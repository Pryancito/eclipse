use super::PAGE_SIZE;
use crate::builder::IoMapper;
const PCI_COMMAND: u16 = 0x04;
use crate::{Device, DeviceError, DeviceResult};
use alloc::{sync::Arc, vec::Vec};
use pci::*;
const BAR0: u16 = 0x10;
#[allow(dead_code)]
pub(crate) const BAR5_REG: u16 = 0x24;
#[allow(dead_code)]
const PCI_CAP_PTR: u16 = 0x34;
const PCI_INTERRUPT_LINE: u16 = 0x3c;
#[allow(dead_code)]
const PCI_INTERRUPT_PIN: u16 = 0x3d;

#[allow(dead_code)]
const PCI_MSI_CTRL_CAP: u16 = 0x00;
#[allow(dead_code)]
const PCI_MSI_ADDR: u16 = 0x04;
#[allow(dead_code)]
const PCI_MSI_UPPER_ADDR: u16 = 0x08;
#[allow(dead_code)]
const PCI_MSI_DATA_32: u16 = 0x08;
#[allow(dead_code)]
const PCI_MSI_DATA_64: u16 = 0x0C;

#[allow(dead_code)]
const PCI_COMMAND_INTX_DISABLE: u16 = 0x0400;

#[allow(dead_code)]
const PCI_CAP_ID_MSI: u8 = 0x05;
const PCI_MAX_CAP_TRAVERSAL: usize = 64;

pub struct PortOpsImpl;

/// Read a BAR's physical base address directly from PCI config space.
/// Handles both 32-bit and 64-bit memory BARs without probing (no side effects).
/// `bar_reg` is the config-space byte offset (0x10 for BAR0, 0x14 for BAR1, etc.).
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
pub(crate) unsafe fn read_bar_addr<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    bar_reg: u16,
) -> u64 {
    let lo = am.read32(ops, loc, bar_reg);
    if lo == 0 {
        return 0;
    }
    if (lo & 0x1) != 0 {
        // I/O space BAR: bits 31:2 = port, bits 1:0 = flags
        (lo & !0x3u32) as u64
    } else if (lo & 0x6) == 0x4 {
        // 64-bit memory BAR: combine with the next 32 bits
        let hi = am.read32(ops, loc, bar_reg + 4);
        ((lo & !0xFu32) as u64) | ((hi as u64) << 32)
    } else {
        // 32-bit memory BAR
        (lo & !0xFu32) as u64
    }
}

/// The size a BAR decodes, from the mask it reads back after all-ones.
///
/// The device leaves its writable address bits set and hardwires the rest to
/// zero, so the size is the value of the lowest bit it kept — which is what
/// Linux's `pci_size()` computes, and what makes this correct for a mask that
/// is not one clean run of ones. That case is real: a 64-bit BAR the device
/// restricts to below 4 GiB reads back a zero high half, and
/// `!mask + 1` on the combined value then yields nonsense in the top 32 bits
/// rather than the size. Returns 0 for an unimplemented BAR, whose mask is
/// all zeros.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn size_from_mask(mask: u64) -> u64 {
    mask.isolate_lowest_one()
}

/// Probe the size of a BAR by writing all-ones and reading back.
/// Handles both 32-bit and 64-bit memory BARs.
/// Temporarily disables Memory/IO decoding (per PCI spec) to avoid bus errors.
/// Returns 0 if the BAR is not implemented or the size cannot be determined.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
pub(crate) unsafe fn probe_bar_size<T: PortOps>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    bar_reg: u16,
) -> u64 {
    let orig_cmd = am.read16(ops, loc, PCI_COMMAND);
    // Disable Memory and I/O decoding while probing (PCI spec requirement)
    am.write16(ops, loc, PCI_COMMAND, orig_cmd & !0x03u16);

    let orig_lo = am.read32(ops, loc, bar_reg);
    am.write32(ops, loc, bar_reg, 0xFFFF_FFFF);
    let mask_lo = am.read32(ops, loc, bar_reg);
    am.write32(ops, loc, bar_reg, orig_lo);

    let size = if (orig_lo & 0x1) != 0 {
        // I/O space BAR: bits 1:0 are flags.
        size_from_mask((mask_lo & !0x3u32) as u64)
    } else if (orig_lo & 0x6) == 0x4 {
        // 64-bit memory BAR: both halves carry writable address bits, so the
        // size is only known once both are probed. A 2060's BAR1 is one of
        // these; everything QEMU shows this kernel is 32-bit, which is why
        // this arm has never run in a VM.
        let orig_hi = am.read32(ops, loc, bar_reg + 4);
        am.write32(ops, loc, bar_reg + 4, 0xFFFF_FFFF);
        let mask_hi = am.read32(ops, loc, bar_reg + 4);
        am.write32(ops, loc, bar_reg + 4, orig_hi);
        let full_mask = ((mask_hi as u64) << 32) | (mask_lo as u64);
        size_from_mask(full_mask & !0xFu64)
    } else {
        // 32-bit memory BAR: bits 3:0 are flags.
        size_from_mask((mask_lo & !0xFu32) as u64)
    };

    am.write16(ops, loc, PCI_COMMAND, orig_cmd);
    size
}

#[cfg(target_arch = "x86_64")]
use x86_64::instructions::port::Port;

#[cfg(target_arch = "x86_64")]
impl PortOps for PortOpsImpl {
    unsafe fn read8(&self, port: u16) -> u8 {
        unsafe { Port::new(port).read() }
    }
    unsafe fn read16(&self, port: u16) -> u16 {
        unsafe { Port::new(port).read() }
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        unsafe { Port::new(port as u16).read() }
    }
    unsafe fn write8(&self, port: u16, val: u8) {
        unsafe {
            Port::new(port).write(val);
        }
    }
    unsafe fn write16(&self, port: u16, val: u16) {
        unsafe {
            Port::new(port).write(val);
        }
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        unsafe {
            Port::new(port as u16).write(val);
        }
    }
}

#[cfg(target_arch = "x86_64")]
const PCI_BASE: usize = 0; //Fix me

#[cfg(any(target_arch = "mips", target_arch = "riscv64", target_arch = "aarch64"))]
use super::{phys_to_virt, read, write};

#[cfg(all(feature = "board_malta", target_arch = "mips"))]
const PCI_BASE: usize = 0xbbe00000;

/// QEMU `virt` PCIe ECAM configuration space base.
///
/// Public because the config accessors below reach it with a bare
/// `phys_to_virt`, with no mapping of their own, so somebody has to put it in
/// the kernel page table first -- see `kernel_hal`'s riscv64 `drivers::init`.
#[cfg(target_arch = "riscv64")]
pub const PCI_BASE: usize = 0x30000000;
/// The whole ECAM window: 256 buses x 32 devices x 8 functions x 4 KiB.
#[cfg(target_arch = "riscv64")]
pub const PCI_CONFIG_SIZE: usize = 256 * 32 * 8 * 4096;
#[cfg(target_arch = "riscv64")]
#[allow(dead_code)]
const E1000_BASE: usize = 0x40000000;
// riscv64 Qemu

// aarch64 QEMU `virt` PCIe ECAM configuration space base.
#[cfg(target_arch = "aarch64")]
const PCI_BASE: usize = 0x40_1000_0000;

#[cfg(target_arch = "x86_64")]
pub const PCI_ACCESS: CSpaceAccessMethod = CSpaceAccessMethod::IO;
#[cfg(not(target_arch = "x86_64"))]
pub const PCI_ACCESS: CSpaceAccessMethod = CSpaceAccessMethod::MemoryMapped(PCI_BASE as *mut u8);

#[cfg(any(target_arch = "mips", target_arch = "riscv64", target_arch = "aarch64"))]
impl PortOps for PortOpsImpl {
    unsafe fn read8(&self, port: u16) -> u8 {
        read(phys_to_virt(PCI_BASE) + port as usize)
    }
    unsafe fn read16(&self, port: u16) -> u16 {
        read(phys_to_virt(PCI_BASE) + port as usize)
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        read(phys_to_virt(PCI_BASE) + port as usize)
    }
    unsafe fn write8(&self, port: u16, val: u8) {
        write(phys_to_virt(PCI_BASE) + port as usize, val);
    }
    unsafe fn write16(&self, port: u16, val: u16) {
        write(phys_to_virt(PCI_BASE) + port as usize, val);
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        write(phys_to_virt(PCI_BASE) + port as usize, val);
    }
}

/// Enable the pci device and its interrupt
/// Return assigned MSI interrupt number when applicable
#[allow(dead_code)]
unsafe fn enable(loc: Location, paddr: u64) -> Option<usize> {
    unsafe {
        let ops = &PortOpsImpl;
        //let am = CSpaceAccessMethod::IO;
        let am = PCI_ACCESS;

        if paddr != 0 {
            // reveal PCI regs by setting paddr
            let bar0_raw = am.read32(ops, loc, BAR0);
            am.write32(ops, loc, BAR0, (paddr & !0xfff) as u32); //Only for 32-bit decoding
            warn!(
                "BAR0 set from {:#x} to {:#x}",
                bar0_raw,
                am.read32(ops, loc, BAR0)
            );
        }

        // 23 and lower are used. Atomic, not `static mut`: device probe can run
        // concurrently, and a plain `+=` on a `static mut` is a data race (UB).
        static MSI_IRQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(23);

        let orig = am.read16(ops, loc, PCI_COMMAND);
        // Always enable MEM space + Bus Mastering so DMA devices (e.g. AHCI) work
        // regardless of whether MSI is available.
        am.write16(ops, loc, PCI_COMMAND, orig | 0x7);

        // find MSI cap
        let mut msi_found = false;
        let mut cap_ptr = am.read8(ops, loc, PCI_CAP_PTR) as u16;
        let mut assigned_irq = None;
        let mut cap_steps = 0usize;
        let mut prev_cap_ptr = 0u16;
        while cap_ptr > 0 {
            if cap_steps >= PCI_MAX_CAP_TRAVERSAL {
                warn!(
                    "PCI capability chain too long or cyclic at {:?}, aborting traversal",
                    loc
                );
                break;
            }
            if cap_ptr == prev_cap_ptr {
                warn!(
                    "PCI capability chain stuck at {:#x} for {:?}, aborting traversal",
                    cap_ptr, loc
                );
                break;
            }
            if cap_ptr < 0x40 {
                warn!(
                    "PCI capability pointer out of spec ({:#x}) for {:?}, aborting traversal",
                    cap_ptr, loc
                );
                break;
            }

            cap_steps = cap_steps.saturating_add(1);
            let cap_id = am.read8(ops, loc, cap_ptr);
            if cap_id == PCI_CAP_ID_MSI {
                let orig_ctrl = am.read32(ops, loc, cap_ptr + PCI_MSI_CTRL_CAP);
                // SDM Vol. 3 §10.11 Message Signalled Interrupts: address
                // bits 19:12 carry the destination APIC id (physical mode).
                // This used to be hardcoded to 0 with "0 is (usually) the
                // apic id of the bsp" -- on firmware that gives the BSP any
                // other id, every MSI (xHCI keyboard/mouse, e1000e RX) went to
                // a CPU that may never come up and was silently lost. Route
                // MSIs to the CPU that actually runs the interrupt handlers.
                #[cfg(target_arch = "x86_64")]
                let dest = crate::irq::x86::Apic::bsp_apic_id() as u32;
                #[cfg(not(target_arch = "x86_64"))]
                let dest = 0u32;
                am.write32(ops, loc, cap_ptr + PCI_MSI_ADDR, 0xfee0_0000 | (dest << 12));
                let irq = MSI_IRQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                assigned_irq = Some(irq as usize);
                // we offset all our irq numbers by 32
                if (orig_ctrl >> 16) & (1 << 7) != 0 {
                    // 64bit: the message-address upper dword must be programmed too
                    // (to 0 for the 0xFEE00000 LAPIC window), or the device latches
                    // an undefined high address and MSIs go astray.
                    am.write32(ops, loc, cap_ptr + PCI_MSI_UPPER_ADDR, 0);
                    am.write32(ops, loc, cap_ptr + PCI_MSI_DATA_64, irq + 32);
                } else {
                    // 32bit
                    am.write32(ops, loc, cap_ptr + PCI_MSI_DATA_32, irq + 32);
                }

                // enable MSI interrupt, assuming 64bit for now
                am.write32(ops, loc, cap_ptr + PCI_MSI_CTRL_CAP, orig_ctrl | 0x10000);
                debug!(
                    "MSI control {:#b}, enabling MSI interrupt {}",
                    orig_ctrl >> 16,
                    irq
                );
                msi_found = true;
            }
            debug!("PCI device has cap id {} at {:#X}", cap_id, cap_ptr);
            prev_cap_ptr = cap_ptr;
            cap_ptr = am.read8(ops, loc, cap_ptr + 1) as u16;
        }

        if !msi_found {
            am.write32(ops, loc, PCI_INTERRUPT_LINE, 33);
            debug!("MSI not found, using PCI interrupt");
        }

        debug!("pci device enable done");

        assigned_irq
    }
}

pub fn init_driver(dev: &PCIDevice, mapper: &Option<Arc<dyn IoMapper>>) -> DeviceResult<Device> {
    // Enable Memory Space and Bus Mastering
    let irq = unsafe { enable(dev.loc, 0) };

    // Try modular PCI drivers (ArceOS style)
    if let Ok(device) = super::pci_drivers::probe_pci_device(dev, mapper, irq) {
        return Ok(device);
    }

    Err(DeviceError::NotSupported)
}

pub fn detach_driver(_loc: &Location) -> bool {
    false
}

pub fn init(mapper: Option<Arc<dyn IoMapper>>) -> DeviceResult<Vec<Device>> {
    let _mapper_driver = if let Some(m) = mapper.clone() {
        m.query_or_map(PCI_BASE, PAGE_SIZE * 256 * 32 * 8);
        Some(m)
    } else {
        None
    };

    let mut dev_list = Vec::new();
    let pci_iter = unsafe { scan_bus(&PortOpsImpl, PCI_ACCESS) };
    info!("");
    info!("--------- PCI bus:device:function ---------");
    for dev in pci_iter {
        info!(
            "pci: {}:{}:{} {:04x}:{:04x} ({} {}) irq: {}:{:?}",
            dev.loc.bus,
            dev.loc.device,
            dev.loc.function,
            dev.id.vendor_id,
            dev.id.device_id,
            dev.id.class,
            dev.id.subclass,
            dev.pic_interrupt_line,
            dev.interrupt_pin,
        );
        let res = init_driver(&dev, &mapper);
        match res {
            Ok(d) => dev_list.push(d),
            // debug!, not warn!: `NotSupported` is the NORMAL outcome for every
            // PCI function without a driver (bridges, SMBus, audio, …), and at
            // LOG=warn each line costs ~10 ms of per-byte UART spin. Real
            // driver failures log at their own probe sites.
            Err(e) => debug!(
                "{:?}, failed to initialize PCI device: {:04x}:{:04x}",
                e, dev.id.vendor_id, dev.id.device_id
            ),
        }
    }
    // One AHCI controller can host several disks but the probe only returns the
    // first as a `Device`; collect the remaining SATA disks it parked.
    for extra in crate::ata::ahci::take_extra_disks() {
        dev_list.push(extra);
    }
    info!("---------");
    info!("");

    Ok(dev_list)
}

pub fn find_device(vendor: u16, product: u16) -> Option<Location> {
    let pci_iter = unsafe { scan_bus(&PortOpsImpl, PCI_ACCESS) };
    for dev in pci_iter {
        if dev.id.vendor_id == vendor && dev.id.device_id == product {
            return Some(dev.loc);
        }
    }
    None
}

pub fn get_bar0_mem(loc: Location) -> Option<(usize, usize)> {
    unsafe { probe_function(&PortOpsImpl, loc, PCI_ACCESS) }
        .and_then(|dev| dev.bars[0])
        .map(|bar| match bar {
            BAR::Memory(addr, len, _, _) => (addr as usize, len as usize),
            _ => unimplemented!(),
        })
}

#[cfg(all(test, target_arch = "x86_64"))]
mod bar_tests {
    //! Host tests for BAR decoding.
    //!
    //! `read_bar_addr` is what tells the NVIDIA driver where BAR0 is and the
    //! AHCI driver where BAR5 is — the first thing either does on a real
    //! machine, and a number that is simply handed to us in a VM. QEMU shows
    //! this kernel 32-bit BARs below 4 GiB; a 2060 has a 64-bit prefetchable
    //! BAR1 of 256 MiB. So the 64-bit arm has never once run in a VM, which
    //! is the whole reason for a fake config space.
    //!
    //! The fake models the one thing that makes sizing work: a device leaves
    //! its writable address bits set and hardwires the rest, so writing
    //! all-ones and reading back returns the size mask with the type flags
    //! still in place.

    use super::*;
    use lock::Mutex;

    const CONFIG_ADDRESS: u32 = 0x0CF8;
    const CONFIG_DATA: u32 = 0x0CFC;

    struct FakeConfig {
        /// 256 bytes of configuration space, as 64 dwords.
        space: Mutex<[u32; 64]>,
        /// Which bits of each dword the device lets software change.
        writable: Mutex<[u32; 64]>,
        latched: Mutex<u32>,
    }

    impl FakeConfig {
        fn new() -> Self {
            let mut writable = [0u32; 64];
            // The command register is writable, which is what the probe
            // toggles to stop the device decoding while its BAR reads back
            // all-ones.
            writable[1] = 0xFFFF_FFFF;
            Self {
                space: Mutex::new([0u32; 64]),
                writable: Mutex::new(writable),
                latched: Mutex::new(0),
            }
        }

        /// Install a memory BAR of `size` bytes based at `base`. `flags` are
        /// the hardwired low bits: `0x0` 32-bit, `0x4` 64-bit, `0x8`
        /// prefetchable, `0x1` I/O.
        fn bar(&self, reg: u16, base: u64, size: u64, flags: u32) {
            let i = reg as usize / 4;
            let keep = if flags & 0x1 != 0 { 0x3u64 } else { 0xFu64 };
            let addr_mask = !(size - 1);
            let mut space = self.space.lock();
            let mut writable = self.writable.lock();
            space[i] = ((base & !keep) as u32) | flags;
            writable[i] = (addr_mask & !keep) as u32;
            if flags & 0x6 == 0x4 {
                space[i + 1] = (base >> 32) as u32;
                writable[i + 1] = (addr_mask >> 32) as u32;
            }
        }

        /// A 64-bit BAR the device will not let software place above 4 GiB:
        /// the upper dword is hardwired to zero. Real devices do this, and it
        /// is the case where a size taken as `!mask + 1` comes out nonsense.
        fn bar64_below_4g(&self, reg: u16, base: u64, size: u64) {
            self.bar(reg, base, size, 0x4);
            self.writable.lock()[reg as usize / 4 + 1] = 0;
            self.space.lock()[reg as usize / 4 + 1] = 0;
        }

        fn raw(&self, reg: u16) -> u32 {
            self.space.lock()[reg as usize / 4]
        }
    }

    impl PortOps for FakeConfig {
        unsafe fn read8(&self, _port: u16) -> u8 {
            unreachable!("config space is reached a dword at a time")
        }
        unsafe fn read16(&self, _port: u16) -> u16 {
            unreachable!("config space is reached a dword at a time")
        }
        unsafe fn read32(&self, port: u32) -> u32 {
            assert_eq!(
                port, CONFIG_DATA,
                "read from a port that is not the data one"
            );
            let off = (*self.latched.lock() & 0xFC) as usize;
            self.space.lock()[off / 4]
        }
        unsafe fn write8(&self, _port: u16, _val: u8) {
            unreachable!("config space is reached a dword at a time")
        }
        unsafe fn write16(&self, _port: u16, _val: u16) {
            unreachable!("config space is reached a dword at a time")
        }
        unsafe fn write32(&self, port: u32, val: u32) {
            if port == CONFIG_ADDRESS {
                *self.latched.lock() = val;
                return;
            }
            assert_eq!(
                port, CONFIG_DATA,
                "wrote to a port that is not the data one"
            );
            let off = (*self.latched.lock() & 0xFC) as usize / 4;
            let w = self.writable.lock()[off];
            let mut space = self.space.lock();
            space[off] = (val & w) | (space[off] & !w);
        }
    }

    const LOC: Location = Location {
        bus: 1,
        device: 0,
        function: 0,
    };
    const IO: CSpaceAccessMethod = CSpaceAccessMethod::IO;
    const BAR1: u16 = 0x14;
    const MIB: u64 = 1 << 20;

    fn addr(c: &FakeConfig, reg: u16) -> u64 {
        unsafe { read_bar_addr(c, IO, LOC, reg) }
    }

    fn size(c: &FakeConfig, reg: u16) -> u64 {
        unsafe { probe_bar_size(c, IO, LOC, reg) }
    }

    #[test]
    fn a_32_bit_memory_bar_gives_its_base_and_its_size() {
        let c = FakeConfig::new();
        c.bar(BAR0, 0xF000_0000, 16 * MIB, 0x0);
        assert_eq!(addr(&c, BAR0), 0xF000_0000);
        assert_eq!(size(&c, BAR0), 16 * MIB);
    }

    #[test]
    fn the_type_flags_are_not_part_of_the_address() {
        let c = FakeConfig::new();
        // Prefetchable 32-bit: bit 3 set. Reading it as part of the address
        // would put the aperture 8 bytes off and every register with it.
        c.bar(BAR0, 0xF000_0000, 16 * MIB, 0x8);
        assert_eq!(c.raw(BAR0) & 0xF, 0x8, "the fake kept the flag bits");
        assert_eq!(addr(&c, BAR0), 0xF000_0000);
        assert_eq!(size(&c, BAR0), 16 * MIB);
    }

    #[test]
    fn a_64_bit_bar_is_read_from_both_halves() {
        let c = FakeConfig::new();
        // What a 2060 presents: a 256 MiB prefetchable BAR1 placed above
        // 4 GiB. Reading only the low half puts the aperture at 0 and every
        // access through it faults.
        c.bar(BAR1, 0x0000_0007_8000_0000, 256 * MIB, 0x4 | 0x8);
        assert_eq!(addr(&c, BAR1), 0x0000_0007_8000_0000);
        assert_eq!(size(&c, BAR1), 256 * MIB);
    }

    #[test]
    fn a_64_bit_bar_whose_low_half_is_all_flags_is_still_found() {
        let c = FakeConfig::new();
        // Base exactly on a 4 GiB boundary: the low dword holds nothing but
        // the type bits, so an "is the low dword zero" shortcut would drop a
        // perfectly good aperture.
        c.bar(BAR1, 0x0000_0001_0000_0000, 256 * MIB, 0x4);
        assert_eq!(addr(&c, BAR1), 0x0000_0001_0000_0000);
        assert_eq!(size(&c, BAR1), 256 * MIB);
    }

    #[test]
    fn a_resizable_bar_larger_than_four_gigabytes_is_sized_from_its_high_half() {
        let c = FakeConfig::new();
        // Resizable BAR: the whole 8 GiB of VRAM exposed through BAR1. Its
        // size mask has no bits at all in the low dword, so treating it as a
        // 32-bit BAR reports an aperture of zero -- and every smaller BAR
        // hides that, because for those the 32-bit arm happens to give the
        // same answer.
        c.bar(BAR1, 0x0000_0010_0000_0000, 8 * 1024 * MIB, 0x4 | 0x8);
        assert_eq!(addr(&c, BAR1), 0x0000_0010_0000_0000);
        assert_eq!(size(&c, BAR1), 8 * 1024 * MIB);
    }

    #[test]
    fn a_64_bit_bar_pinned_below_four_gigabytes_still_reports_its_size() {
        let c = FakeConfig::new();
        // A 64-bit BAR whose upper dword the device hardwires to zero. Its
        // size mask is then not one clean run of ones, and `!mask + 1` over
        // the combined value yields a number with the top 32 bits set —
        // gigabytes of aperture that do not exist. The size is the lowest bit
        // the device kept, which is right either way.
        c.bar64_below_4g(BAR1, 0xE000_0000, 256 * MIB);
        assert_eq!(addr(&c, BAR1), 0xE000_0000);
        assert_eq!(size(&c, BAR1), 256 * MIB);
    }

    #[test]
    fn an_io_bar_gives_its_port_and_its_size() {
        let c = FakeConfig::new();
        // 32 ports at 0xC000, which is what a NIC or an IDE function looks
        // like. The size arithmetic here used to parse as `!(mask + 1)`
        // rather than `!mask + 1`, so every I/O BAR came back two short.
        c.bar(BAR0, 0xC000, 32, 0x1);
        assert_eq!(addr(&c, BAR0), 0xC000);
        assert_eq!(size(&c, BAR0), 32);
        // And a one-port BAR, where being two short would underflow.
        c.bar(BAR0, 0xCF8, 4, 0x1);
        assert_eq!(size(&c, BAR0), 4);
    }

    #[test]
    fn an_unimplemented_bar_is_zero_rather_than_a_guess() {
        let c = FakeConfig::new();
        // Nothing installed: the register reads zero and stays zero however
        // hard it is written, which is how a device says "no BAR here".
        assert_eq!(addr(&c, BAR0), 0);
        assert_eq!(size(&c, BAR0), 0);
    }

    #[test]
    fn probing_leaves_the_device_exactly_as_it_found_it() {
        let c = FakeConfig::new();
        c.bar(BAR1, 0x0000_0007_8000_0000, 256 * MIB, 0x4 | 0x8);
        // Memory and I/O decoding enabled, plus bus master.
        unsafe { IO.write16(&c, LOC, PCI_COMMAND, 0x0007) };
        let before = (c.raw(BAR1), c.raw(BAR1 + 4), c.raw(PCI_COMMAND));

        assert_eq!(size(&c, BAR1), 256 * MIB);

        // The probe writes all-ones into the BAR and turns decoding off to do
        // it. Leaving either behind is not a wrong number, it is a device
        // that answers at the wrong address or not at all — and on the AHCI
        // path, a disk that never comes back.
        assert_eq!(
            (c.raw(BAR1), c.raw(BAR1 + 4), c.raw(PCI_COMMAND)),
            before,
            "the probe did not put the device back"
        );
        assert_eq!(addr(&c, BAR1), 0x0000_0007_8000_0000);
    }

    #[test]
    fn reading_an_address_never_disturbs_the_device() {
        let c = FakeConfig::new();
        c.bar(BAR0, 0xF000_0000, 16 * MIB, 0x0);
        unsafe { IO.write16(&c, LOC, PCI_COMMAND, 0x0007) };
        let before = (c.raw(BAR0), c.raw(PCI_COMMAND));
        // `read_bar_addr` exists precisely so the address can be had without
        // the side effects of a probe: some GPUs hang if BAR0 is written
        // during bring-up.
        for _ in 0..3 {
            assert_eq!(addr(&c, BAR0), 0xF000_0000);
        }
        assert_eq!((c.raw(BAR0), c.raw(PCI_COMMAND)), before);
    }

    #[test]
    fn the_size_of_a_mask_is_the_lowest_bit_the_device_kept() {
        // Every power of two a real BAR can be, and the degenerate mask.
        assert_eq!(size_from_mask(0), 0);
        for shift in 4..48 {
            let size = 1u64 << shift;
            assert_eq!(size_from_mask(!(size - 1)), size);
        }
        // A mask with a gap in it — what a hardwired upper dword produces —
        // still yields the size rather than a number built from the gap.
        assert_eq!(size_from_mask(0x0000_0000_F000_0000), 256 * MIB);
    }
}
