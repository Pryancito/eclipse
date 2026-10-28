use crate::arch::timer::set_next_trigger;
use crate::common::vm::GenericPageTable;
use crate::drivers;
use crate::hal_fn::mem::phys_to_virt;
use crate::imp::config::VIRTIO_BASE;
use crate::{CachePolicy, MMUFlags, PhysAddr, VirtAddr, KCONFIG};
use alloc::sync::Arc;
use zcore_drivers::builder::{DevicetreeDriverBuilder, IoMapper};
use zcore_drivers::irq::gic_400;
use zcore_drivers::scheme::IrqScheme;
use zcore_drivers::uart::{BufferedUart, Pl011Uart};
use zcore_drivers::utils::devicetree::{check_fdt_header, FDT_HEADER_SIZE};
use zcore_drivers::virtio::{VirtIOHeader, VirtIoBlk};
use zcore_drivers::Device;

pub fn init_early() {
    // See the x86_64 note: before any probe, so a driver that finds a
    // hot-pluggable device during bring-up has somewhere to publish it.
    drivers::install_hotplug_sink();
    let uart = Pl011Uart::new(phys_to_virt(KCONFIG.uart_base));
    let uart = Arc::new(uart);
    let gic = gic_400::init(
        phys_to_virt(KCONFIG.gic_base + 0x1_0000),
        phys_to_virt(KCONFIG.gic_base),
    );
    gic.irq_enable(30);
    gic.irq_enable(33);
    gic.irq_enable(IPI_SGI);
    gic.register_handler(33, Arc::new(handle_uart_irq)).ok();
    gic.register_handler(30, Arc::new(handle_timer_irq)).ok();
    gic.register_handler(IPI_SGI as usize, Arc::new(handle_ipi))
        .ok();
    drivers::add_device(Device::Irq(Arc::new(gic)));
    drivers::add_device(Device::Uart(BufferedUart::new(uart)));
}

pub fn init() {
    // The QEMU virt machine exposes a bank of virtio-mmio transports, and the
    // firmware boot disk is not guaranteed to occupy the first one.  A block
    // device is optional (the Zircon ZBI is linked into the kernel), so do not
    // abort boot when this slot is empty.
    if let Ok(virtio_blk) =
        VirtIoBlk::new(unsafe { &mut *(phys_to_virt(VIRTIO_BASE) as *mut VirtIOHeader) })
    {
        drivers::add_device(Device::Block(Arc::new(virtio_blk)));
    }
    probe_devicetree();
}

struct IoMapperImpl;

impl IoMapper for IoMapperImpl {
    fn query_or_map(&self, paddr: PhysAddr, size: usize) -> Option<VirtAddr> {
        let vaddr = phys_to_virt(paddr);
        let mut pt = super::vm::kernel_page_table().lock();
        if let Ok((paddr_mapped, _, _)) = pt.query(vaddr) {
            if paddr_mapped == paddr {
                return Some(vaddr);
            }
            warn!("IoMapper::query_or_map: not linear mapping: vaddr={vaddr:#x}, paddr={paddr_mapped:#x}");
            return None;
        }
        let size = crate::addr::align_up(size);
        let flags = MMUFlags::READ
            | MMUFlags::WRITE
            | MMUFlags::HUGE_PAGE
            | MMUFlags::DEVICE
            | MMUFlags::from_bits_truncate(CachePolicy::UncachedDevice as usize);
        match pt.map_cont(vaddr, size, paddr, flags) {
            Ok(()) => Some(vaddr),
            Err(err) => {
                warn!(
                    "IoMapper::query_or_map: failed to map {:#x?} => {paddr:#x}: {err:?}",
                    vaddr..vaddr + size
                );
                None
            }
        }
    }
}

/// Maps `len` bytes of ordinary memory at `paddr`, page by page, leaving
/// alone the pages that are mapped already.
///
/// The windows of a device go through [`IoMapperImpl`], which maps a whole
/// range at once as uncached device memory. A device tree blob is neither:
/// it is bytes in RAM, it wants to be readable and cached, and the range it
/// sits in is **partly mapped already** --- the physical frames the kernel
/// got from the memory map are linear-mapped at boot, and a blob the firmware
/// left below the kernel is not among them. `map_cont` over such a range
/// fails on the first page that is already there, so the pages are done one
/// at a time.
fn map_memory(paddr: PhysAddr, len: usize) -> Option<VirtAddr> {
    use crate::PAGE_SIZE;
    let first = crate::addr::align_down(paddr);
    let last = crate::addr::align_up(paddr + len);
    let mut pt = super::vm::kernel_page_table().lock();
    let mut at = first;
    while at < last {
        let vaddr = phys_to_virt(at);
        match pt.query(vaddr) {
            Ok((mapped, _, _)) if mapped == at => {}
            Ok((mapped, _, _)) => {
                warn!("dtb: {vaddr:#x} is mapped to {mapped:#x}, not to {at:#x}");
                return None;
            }
            Err(_) => {
                if let Err(err) = pt.map_cont(vaddr, PAGE_SIZE, at, MMUFlags::READ) {
                    warn!("dtb: failed to map {vaddr:#x} => {at:#x}: {err:?}");
                    return None;
                }
            }
        }
        at += PAGE_SIZE;
    }
    Some(phys_to_virt(paddr))
}

/// Walks a device tree blob, if the command line said where one is, and adds
/// what it finds.
///
/// **Nothing happens without `dtb=0x…`.** `rayboot` passes no device tree
/// pointer, so without the hatch there is no address to walk and this returns
/// having done nothing: the boot is exactly the one above, on the bases in
/// `KCONFIG`. That is deliberate --- the hardcoded bases stay the fallback,
/// and a tree that is missing, unreadable or wrong must not cost the machine
/// the console it already has.
///
/// Two kinds of device are **not** taken from the tree even when it describes
/// them: the interrupt controller and the UART. Both are up before there is a
/// tree to read, `init_early` registered them, and a second driver for either
/// would be a second driver for the same silicon. The controller the tree
/// names is handed to the walk instead, which is what `adopt_intc` is for.
fn probe_devicetree() {
    let Some(paddr) = crate::cmdline::dtb_paddr(KCONFIG.cmdline) else {
        return;
    };
    info!("dtb: the command line puts a device tree blob at {paddr:#x}");
    // The header first, and only as much as it says afterwards: `map_memory`
    // is the one thing here that touches the page table, and the blob decides
    // how much of it. Mapping the header alone and handing the address
    // straight to the builder would fault on the first byte past it, because
    // the builder reads `totalsize` and walks all of it.
    let Some(vaddr) = map_memory(paddr, FDT_HEADER_SIZE) else {
        return;
    };
    // SAFETY: `map_memory` just made these bytes readable.
    let header = unsafe { core::slice::from_raw_parts(vaddr as *const u8, FDT_HEADER_SIZE) };
    let totalsize = match check_fdt_header(header) {
        Ok(totalsize) => totalsize,
        Err(err) => {
            warn!("dtb: {paddr:#x} does not hold a blob this kernel can read: {err:?}");
            return;
        }
    };
    if map_memory(paddr, totalsize).is_none() {
        return;
    }
    let gic: Arc<dyn IrqScheme> = match drivers::all_irq().first() {
        Some(gic) => gic.clone(),
        None => {
            warn!("dtb: no interrupt controller is up yet, so the tree cannot be walked");
            return;
        }
    };
    let builder = match DevicetreeDriverBuilder::new(vaddr, IoMapperImpl) {
        Ok(builder) => builder
            // The Pi 4 writes the first and the `virt` machine of QEMU the
            // second; a kernel that boots on both says both.
            .adopt_intc("arm,gic-400", gic.clone())
            .adopt_intc("arm,cortex-a15-gic", gic),
        Err(err) => {
            warn!("dtb: {vaddr:#x} does not hold a blob this kernel can read: {err:?}");
            return;
        }
    };
    let devices = match builder.build() {
        Ok(devices) => devices,
        Err(err) => {
            warn!("dtb: the walk failed: {err:?}");
            return;
        }
    };
    for dev in devices {
        match dev {
            Device::Irq(_) | Device::Uart(_) => {
                info!("dtb: {dev:?} is already up, leaving it alone");
            }
            dev => {
                info!("dtb: adding {dev:?}");
                drivers::add_device(dev);
            }
        }
    }
}

/// The SGI `send_ipi` raises for a TLB shootdown (GICD_SGIR with SGIINTID 0).
const IPI_SGI: u32 = 0;

fn handle_timer_irq() {
    set_next_trigger();
    crate::timer::timer_tick();
}

/// Drain and acknowledge this CPU's TLB-shootdown queue.
///
/// Nothing was registered for this interrupt. `send_ipi` published the request
/// and rang SGI 0, and on the receiving side the GIC dispatched it to no one:
/// the entry stayed queued and no watermark was ever published, so the
/// initiator -- whose wait has no timeout, and whose NMI escalation is
/// x86-only -- spun on it. What made this survive at all is the ticket lock's
/// spin pump, which drains the queue for a CPU that happens to be contending
/// on a lock; a target running user code, or idle, starved. That is the
/// intermittent `[tlb-shootdown] slow ack wait spins=16777216` on this
/// architecture, and why it depends on what the guest was doing.
fn handle_ipi() {
    crate::common::ipi::tlb_shootdown_ack();
}

fn handle_uart_irq() {
    crate::drivers::all_uart().first_unwrap().handle_irq(0);
}
