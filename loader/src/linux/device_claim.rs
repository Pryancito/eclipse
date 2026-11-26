//! [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM): handing a device
//! the kernel left alone to the process that will drive it, and taking it
//! back when that process is gone.
//!
//! A driver that dies leaves its device running: queues set up, pointing at
//! memory the process pinned for it, an interrupt line with a handler. So
//! the kernel keeps hold of that memory past the process, resets the device
//! (the one thing it ever does to it), and only then lets the memory and the
//! line go and the device be claimed again. That is what lets whoever
//! started the driver simply start it again.

use super::{
    CLAIM_WORDS, ECLIPSE_DEVICE_VIRTIO_BLK, ECLIPSE_DEVICE_VIRTIO_NET,
    ECLIPSE_TRANSPORT_VIRTIO_MMIO, ECLIPSE_TRANSPORT_VIRTIO_PCI,
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::time::Duration;
use kernel_hal::drivers::scheme::IrqScheme;
use kernel_hal::drivers::user::{self, Transport};
use kernel_hal::{CachePolicy, PAGE_SIZE};
use zircon_object::dev::{BusTransactionInitiator, Interrupt, Iommu, IrqLine};
use zircon_object::object::{Handle, HandleValue, KernelObject, KoID, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::{ZxError, ZxResult};

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: an Intel
/// e1000e network card (an 82574/82583 or a PCH part: 82579, I217 to
/// I219), left to userspace by `udriver=e1000e@N`; word 4 says which.
pub const ECLIPSE_DEVICE_E1000E: usize = 3;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: an NVMe
/// controller, left to userspace by `udriver=nvme@N`. Its transport is
/// [`ECLIPSE_TRANSPORT_PCI`]: the window is BAR 0, the controller's
/// registers and doorbells.
pub const ECLIPSE_DEVICE_NVME: usize = 4;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: an AHCI
/// controller, left to userspace by `udriver=ahci@N`. Its transport is
/// [`ECLIPSE_TRANSPORT_PCI`]: the window is BAR 5 (ABAR), the HBA's
/// registers and each port's.
pub const ECLIPSE_DEVICE_AHCI: usize = 5;
/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: a virtio
/// input device, left to userspace by `udriver=virtio-input@N`.
pub const ECLIPSE_DEVICE_VIRTIO_INPUT: usize = 6;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kinds: the
/// keyboard and the aux port of the PC's i8042, left to userspace by
/// `udriver=i8042@0`. Their transport is [`ECLIPSE_TRANSPORT_PORTS`]; each
/// has its own line, and one driver holds both.
pub const ECLIPSE_DEVICE_I8042: usize = 7;
pub const ECLIPSE_DEVICE_I8042_AUX: usize = 8;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: an Intel HD
/// Audio controller, left to userspace by `udriver=hda@N`. Its window is
/// its memory BAR ([`ECLIPSE_TRANSPORT_PCI`]).
pub const ECLIPSE_DEVICE_HDA: usize = 9;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: an xHCI USB
/// controller, left to userspace by `udriver=xhci@N`. Its window is its
/// memory BAR ([`ECLIPSE_TRANSPORT_PCI`]).
pub const ECLIPSE_DEVICE_XHCI: usize = 10;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: a PCI
/// function left to userspace by its identity, `udriver=pci-VVVV-DDDD@N`.
/// Argument 3 is that identity, `vendor << 16 | device`, and the index
/// counts the functions that have it. Its window is its first memory BAR
/// ([`ECLIPSE_TRANSPORT_PCI`]). The kernel knows no way to stop it, so once
/// its driver is gone it stays out of use until the next boot.
pub const ECLIPSE_DEVICE_PCI: usize = 11;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) kind: an NVIDIA
/// GPU, left to userspace by `udriver=nvidia@N`. Its window is BAR 0, the
/// GPU's registers ([`ECLIPSE_TRANSPORT_PCI`], word 4 its identity); its
/// other BARs and its configuration space are reached through
/// `ECLIPSE_DEVICE_PCI_FN`.
pub const ECLIPSE_DEVICE_NVIDIA: usize = 12;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) transport: I/O
/// ports. The window is `size` ports from port `offset` (words 2 and 3),
/// which the driver reaches through `ECLIPSE_DEVICE_PORT`; there is no
/// register VMO (word 0 low is 0), and the line is an edge.
pub const ECLIPSE_TRANSPORT_PORTS: u64 = 4;

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM) transport: a PCI
/// function that is not virtio. The window is its first memory BAR, with
/// the registers as its datasheet lays them out; word 4 is the function's
/// `vendor << 16 | device` when the bus recorded it (an e1000e's), else 0,
/// words 5 and 6 are zero, and its line is an MSI on x86_64 and a level
/// elsewhere.
pub const ECLIPSE_TRANSPORT_PCI: u64 = 3;

/// The controllers a reserved device's interrupt can be on, by the name
/// each one gives itself: the PLIC before anything else on riscv64, where
/// the per-hart local controllers are in the same list and do not take
/// device lines.
const CONTROLLERS: [&str; 3] = ["riscv-plic", "ARM Generic Interrupt Controller", "x86-apic"];

/// One line of an interrupt controller, as an [`Interrupt`] drives it.
struct ControllerLine {
    controller: Arc<dyn IrqScheme>,
    irq: usize,
}

impl IrqLine for ControllerLine {
    fn mask(&self) {
        let _ = self.controller.mask(self.irq);
    }

    fn unmask(&self) {
        let _ = self.controller.unmask(self.irq);
    }

    fn register_handler(&self, handler: Box<dyn Fn() + Send + Sync>) -> ZxResult {
        self.controller
            .register_handler(self.irq, Arc::from(handler))
            .map_err(|_| ZxError::ALREADY_BOUND)
    }

    fn unregister_handler(&self) -> ZxResult {
        self.controller
            .unregister(self.irq)
            .map_err(|_| ZxError::NOT_FOUND)
    }
}

/// The device's interrupt as an object, when there is one to make: a line
/// the kernel knows, on a controller it has. Not having one is not an
/// error -- the driver can poll, as it did before -- so it is only logged.
///
/// Closing its handle does not tear it down: the handler it registers holds
/// it. [`Held::take_back`] does, when the driver is gone.
fn device_interrupt(device: &user::UserDevice) -> Option<Arc<Interrupt>> {
    let irq = device.irq?;
    let all = kernel_hal::drivers::all_irq();
    let controller = CONTROLLERS.iter().find_map(|name| all.find(name))?;
    if !controller.is_valid_irq(irq) {
        warn!("udriver: irq {irq} is not one {} takes", controller.name());
        return None;
    }
    // virtio-mmio raises a level that stays up until the driver acks it in
    // the device; an MSI is a message, and there is nothing to hold down.
    // A PCI function's line is an MSI on x86_64 and its INTx elsewhere,
    // which is a level too.
    let level = match device.transport {
        Transport::VirtioMmio => true,
        Transport::VirtioPci { .. } => false,
        Transport::Pci => !cfg!(target_arch = "x86_64"),
        // An ISA line, which the i8042 pulses.
        Transport::Ports => false,
    };
    let line = Box::new(ControllerLine { controller, irq });
    match Interrupt::new_line(line, level) {
        Ok(interrupt) => Some(interrupt),
        Err(e) => {
            warn!("udriver: irq {irq} could not be taken: {e:?}");
            None
        }
    }
}

/// What the device will reach through its initiator: a domain of its own
/// when an IOMMU is in use and covers it, so it sees only what its driver
/// pins; otherwise passthrough, every byte, as on a machine without one.
fn dma_view(device: &user::UserDevice) -> Arc<Iommu> {
    watch_faults();
    let domain = device.pci_place().and_then(|place| {
        kernel_hal::iommu::domain_for(kernel_hal::iommu::PciRequester {
            segment: 0,
            bus: place.bus,
            device: place.device,
            function: place.function,
        })
    });
    match domain {
        Some(domain) => Iommu::with_domain(domain),
        None => {
            if kernel_hal::iommu::unit().is_some() {
                let (kind, index) = (device.kind, device.index);
                warn!("udriver: {kind} #{index} has no IOMMU domain; its DMA reaches all memory");
            }
            Iommu::create()
        }
    }
}

/// How often the IOMMU's fault records are read: it raises no interrupt.
const FAULT_POLL: Duration = Duration::from_millis(250);

/// Start reading the IOMMU's fault records into the log, once, when there
/// is an IOMMU and a device is first claimed.
fn watch_faults() {
    static STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    let Some(unit) = kernel_hal::iommu::unit() else {
        return;
    };
    if STARTED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        return;
    }
    kernel_hal::thread::spawn(async move {
        loop {
            unit.poll_faults();
            kernel_hal::thread::sleep_until(kernel_hal::timer::timer_now() + FAULT_POLL).await;
        }
    });
}

/// How long a dead driver's threads are given to be gone before its device
/// is reset regardless, and how often that is looked at.
const THREADS_GRACE: Duration = Duration::from_millis(500);
const THREADS_STEP: Duration = Duration::from_millis(1);

/// Which process has which device: `(process, kind, index)`, from the claim
/// until the device is taken back.
/// Who holds what: the holder, the device's kind and index, and its PCI
/// identity (what tells apart two `pci` functions of the same index).
static HOLDERS: lock::Mutex<Vec<(KoID, &'static str, usize, u32)>> = lock::Mutex::new(Vec::new());

/// Each holder's bus transaction initiator: what it pinned through it is
/// its own memory, which it may name to the kernel ([`owns_memory`]).
static PINS: lock::Mutex<Vec<(KoID, alloc::sync::Weak<BusTransactionInitiator>)>> =
    lock::Mutex::new(Vec::new());

/// Whether `proc` holds a GPU: a device of kind `nvidia`, or a PCI
/// function by identity (what `udrv-nvidia check` claims).
pub(super) fn holds_gpu(proc: &Arc<Process>) -> bool {
    let id = proc.id();
    HOLDERS
        .lock()
        .iter()
        .any(|&(holder, k, _, _)| holder == id && (k == user::NVIDIA || k == user::PCI))
}

/// Whose `len` bytes of physical memory at `phys` are, as far as `proc`
/// may say: `Some(false)` for memory it pinned through a device it holds,
/// `Some(true)` for one of that device's memory BARs (device memory, to be
/// mapped write-combining), `None` for memory that is not its own.
pub(super) fn owns_memory(proc: &Arc<Process>, phys: u64, len: u64) -> Option<bool> {
    let id = proc.id();
    let end = phys.checked_add(len)?;
    let pins: Vec<_> = PINS
        .lock()
        .iter()
        .filter(|(holder, _)| *holder == id)
        .filter_map(|(_, bti)| bti.upgrade())
        .collect();
    if pins
        .iter()
        .any(|bti| bti.pins_cover(phys as usize, len as usize))
    {
        return Some(false);
    }
    let held: Vec<_> = HOLDERS
        .lock()
        .iter()
        .filter(|(holder, ..)| *holder == id)
        .map(|&(_, kind, index, pci_id)| (kind, index, pci_id))
        .collect();
    let in_a_bar = held.into_iter().any(|(kind, index, pci_id)| {
        let device = if kind == user::PCI {
            user::find_pci(pci_id, index)
        } else {
            user::find(kind, index)
        };
        device.and_then(|d| d.pci).is_some_and(|place| {
            place.bars.iter().any(|&(base, size)| {
                size > 0 && phys >= base as u64 && end <= base as u64 + size as u64
            })
        })
    });
    in_a_bar.then_some(true)
}

/// The device of `kind` that `proc` holds, by its place among the reserved
/// ones of that kind. What a driver serves to the kernel is known by this:
/// the next driver of the same device is the same disk's.
pub(super) fn held_by(proc: &Arc<Process>, kind: &str) -> Option<usize> {
    let id = proc.id();
    let holders = HOLDERS.lock();
    holders
        .iter()
        .find(|(holder, k, _, _)| *holder == id && *k == kind)
        .map(|(_, _, index, _)| *index)
}

/// The device `index` of `kind` that `proc` holds, of identity `pci_id` for
/// a function named by its identity ([`ECLIPSE_DEVICE_PCI`]).
pub(super) fn held_device(
    proc: &Arc<Process>,
    kind: &str,
    index: usize,
    pci_id: u32,
) -> Option<Arc<user::UserDevice>> {
    let id = proc.id();
    let held = HOLDERS.lock().iter().any(|&(holder, k, i, p)| {
        holder == id && k == kind && i == index && (kind != user::PCI || p == pci_id)
    });
    if !held {
        return None;
    }
    if kind == user::PCI {
        user::find_pci(pci_id, index)
    } else {
        user::find(kind, index)
    }
}

/// The claim kind of [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM)'s
/// number.
pub(super) fn kind_of(number: usize) -> Option<&'static str> {
    Some(match number {
        ECLIPSE_DEVICE_VIRTIO_BLK => user::VIRTIO_BLK,
        ECLIPSE_DEVICE_VIRTIO_NET => user::VIRTIO_NET,
        ECLIPSE_DEVICE_E1000E => user::E1000E,
        ECLIPSE_DEVICE_NVME => user::NVME,
        ECLIPSE_DEVICE_AHCI => user::AHCI,
        ECLIPSE_DEVICE_VIRTIO_INPUT => user::VIRTIO_INPUT,
        ECLIPSE_DEVICE_I8042 => user::I8042,
        ECLIPSE_DEVICE_I8042_AUX => user::I8042_AUX,
        ECLIPSE_DEVICE_HDA => user::HDA,
        ECLIPSE_DEVICE_XHCI => user::XHCI,
        ECLIPSE_DEVICE_PCI => user::PCI,
        ECLIPSE_DEVICE_NVIDIA => user::NVIDIA,
        _ => return None,
    })
}

/// What a driver was given of its device, kept by the kernel as well so it
/// outlasts the driver.
struct Held {
    /// The process it was handed to.
    holder: KoID,
    device: Arc<user::UserDevice>,
    /// Through this the driver pinned what the device reads and writes; the
    /// pins live as long as it does.
    bti: Arc<BusTransactionInitiator>,
    interrupt: Option<Arc<Interrupt>>,
}

impl Held {
    /// The line, so a device nobody answers for cannot interrupt, and so a
    /// driver thread asleep on it wakes up to find it is dead. A driver
    /// that destroyed it itself is no trouble.
    fn silence(&self) {
        if let Some(interrupt) = &self.interrupt {
            if let Err(e) = interrupt.destroy() {
                let (kind, index) = (self.device.kind, self.device.index);
                debug!("udriver: {kind} #{index}: its interrupt was already gone ({e:?})");
            }
        }
    }

    /// The driver is gone: stop the device and make it claimable again.
    fn take_back(self) {
        let (kind, index) = (self.device.kind, self.device.index);
        let pci_id = self.device.pci_id;
        HOLDERS
            .lock()
            .retain(|held| *held != (self.holder, kind, index, pci_id));
        PINS.lock().retain(|(holder, bti)| {
            *holder != self.holder || bti.upgrade().is_some_and(|b| !Arc::ptr_eq(&b, &self.bti))
        });
        self.silence();
        // Its fences can never land: their waiters are let go, and the
        // kernel stops reading their landing zones, before those are
        // anyone else's.
        let fences = super::syncobj_serve::driver_gone(self.holder);
        if fences > 0 {
            info!("udriver: {kind} #{index}: {fences} fences of its driver abandoned");
        }
        // GEM objects it named are in memory it pinned, and a client may
        // still have one mapped: the pins stay, for good, so that memory
        // is never anyone else's while a mapping of it lives.
        if super::gem_serve::driver_gone(self.holder) {
            warn!("udriver: {kind} #{index}: its driver named GEM objects; what it pinned stays pinned");
            if self.device.reset() {
                self.device.release();
            }
            core::mem::forget(self.bti);
            return;
        }
        if !self.device.reset() {
            // It may still be writing to what it was given, so that stays
            // pinned for good, and nobody else gets a device in that state.
            warn!("udriver: {kind} #{index} lost its driver and could not be reset; it stays out of use");
            core::mem::forget(self.bti);
            return;
        }
        // Reset: it names no memory any more. Dropping the initiator drops
        // the pins the dead process left in it.
        drop(self.bti);
        self.device.release();
        info!("udriver: {kind} #{index} lost its driver; reset, and free to be claimed again");
    }

    /// Take the device back when `proc` terminates.
    fn until_gone(self, proc: &Arc<Process>) {
        let proc = proc.clone();
        kernel_hal::thread::spawn(async move {
            let object: Arc<dyn KernelObject> = proc.clone();
            object.wait_signal(Signal::PROCESS_TERMINATED).await;
            // A Linux process is terminated when its exit status is on
            // file, and its threads may still be on their way out: one that
            // was running can touch the registers until it next enters the
            // kernel. So the reset waits for them, but not for ever -- a
            // thread asleep in the kernel touches nothing.
            self.silence();
            let deadline = kernel_hal::timer::timer_now() + THREADS_GRACE;
            while proc.thread_count() != 0 && kernel_hal::timer::timer_now() < deadline {
                kernel_hal::thread::sleep_until(kernel_hal::timer::timer_now() + THREADS_STEP)
                    .await;
            }
            self.take_back();
        });
    }
}

/// [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM).
pub(super) fn claim_device(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    let kind = kind_of(args[0]).ok_or(ZxError::INVALID_ARGS)?;
    let mut out = kernel_hal::user::UserOutPtr::<u64>::from(args[2]);
    // Look before taking anything: a bad pointer must not cost the device.
    out.write_array(&[0; CLAIM_WORDS])
        .map_err(|_| ZxError::INVALID_ARGS)?;
    let device = if kind == user::PCI {
        let pci_id = <u32 as core::convert::TryFrom<usize>>::try_from(args[3])
            .map_err(|_| ZxError::INVALID_ARGS)?;
        user::find_pci(pci_id, args[1])
    } else {
        user::find(kind, args[1])
    }
    .ok_or(ZxError::NOT_FOUND)?;
    let base = device.paddr & !(PAGE_SIZE - 1);
    let end = device
        .paddr
        .checked_add(device.size)
        .and_then(|end| end.checked_add(PAGE_SIZE - 1))
        .ok_or(ZxError::OUT_OF_RANGE)?
        & !(PAGE_SIZE - 1);
    if !device.claim() {
        return Err(ZxError::ALREADY_BOUND);
    }
    // A device on I/O ports has no memory to hand out: its window is
    // ports, which the kernel reads and writes for it.
    let ports = device.transport == Transport::Ports;
    let regs = if ports {
        None
    } else {
        let regs = VmObject::new_physical(base, (end - base) / PAGE_SIZE);
        regs.set_cache_policy(CachePolicy::UncachedDevice)?;
        Some(regs)
    };
    let held = Held {
        holder: proc.id(),
        bti: BusTransactionInitiator::create(dma_view(&device), 0),
        interrupt: device_interrupt(&device),
        device: device.clone(),
    };
    let regs: HandleValue = regs.map_or(0, |regs| {
        proc.add_handle(Handle::new(regs, Rights::DEFAULT_VMO))
    });
    let bti = proc.add_handle(Handle::new(held.bti.clone(), Rights::DEFAULT_BTI));
    let interrupt: Option<HandleValue> = held
        .interrupt
        .clone()
        .map(|irq| proc.add_handle(Handle::new(irq, Rights::DEFAULT_INTERRUPT)));
    let pair = |lo: u32, hi: u32| lo as u64 | (hi as u64) << 32;
    let (transport, extra) = match device.transport {
        Transport::VirtioMmio => (ECLIPSE_TRANSPORT_VIRTIO_MMIO, [0; 3]),
        Transport::VirtioPci {
            common,
            notify,
            isr,
            device,
            notify_multiplier,
        } => (
            ECLIPSE_TRANSPORT_VIRTIO_PCI,
            [
                pair(common, notify),
                pair(isr, device),
                notify_multiplier as u64,
            ],
        ),
        Transport::Pci => (ECLIPSE_TRANSPORT_PCI, [device.pci_id as u64, 0, 0]),
        Transport::Ports => (ECLIPSE_TRANSPORT_PORTS, [0; 3]),
    };
    let irq = device.irq.map_or(u32::MAX, |irq| irq as u32);
    let words: [u64; CLAIM_WORDS] = [
        pair(regs, bti),
        transport | (irq as u64) << 32,
        if ports {
            device.paddr as u64
        } else {
            (device.paddr - base) as u64
        },
        device.size as u64,
        extra[0],
        extra[1],
        extra[2],
        interrupt.unwrap_or(0) as u64,
    ];
    if out.write_array(&words).is_err() {
        if regs != 0 {
            let _ = proc.remove_handle(regs);
        }
        let _ = proc.remove_handle(bti);
        if let Some(interrupt) = interrupt {
            let _ = proc.remove_handle(interrupt);
        }
        // Nobody drives it after all: it is there for the next to ask.
        held.take_back();
        return Err(ZxError::INVALID_ARGS);
    }
    HOLDERS
        .lock()
        .push((held.holder, device.kind, device.index, device.pci_id));
    PINS.lock().push((held.holder, Arc::downgrade(&held.bti)));
    held.until_gone(proc);
    Ok(())
}
