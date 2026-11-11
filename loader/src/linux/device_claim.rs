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
use alloc::{boxed::Box, sync::Arc};
use core::time::Duration;
use kernel_hal::drivers::scheme::IrqScheme;
use kernel_hal::drivers::user::{self, Transport};
use kernel_hal::{CachePolicy, PAGE_SIZE};
use zircon_object::dev::{BusTransactionInitiator, Interrupt, Iommu, IrqLine};
use zircon_object::object::{Handle, HandleValue, KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::{ZxError, ZxResult};

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
    let level = device.transport == Transport::VirtioMmio;
    let line = Box::new(ControllerLine { controller, irq });
    match Interrupt::new_line(line, level) {
        Ok(interrupt) => Some(interrupt),
        Err(e) => {
            warn!("udriver: irq {irq} could not be taken: {e:?}");
            None
        }
    }
}

/// How long a dead driver's threads are given to be gone before its device
/// is reset regardless, and how often that is looked at.
const THREADS_GRACE: Duration = Duration::from_millis(500);
const THREADS_STEP: Duration = Duration::from_millis(1);

/// What a driver was given of its device, kept by the kernel as well so it
/// outlasts the driver.
struct Held {
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
        self.silence();
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
    let kind = match args[0] {
        ECLIPSE_DEVICE_VIRTIO_BLK => user::VIRTIO_BLK,
        ECLIPSE_DEVICE_VIRTIO_NET => user::VIRTIO_NET,
        _ => return Err(ZxError::INVALID_ARGS),
    };
    let mut out = kernel_hal::user::UserOutPtr::<u64>::from(args[2]);
    // Look before taking anything: a bad pointer must not cost the device.
    out.write_array(&[0; CLAIM_WORDS])
        .map_err(|_| ZxError::INVALID_ARGS)?;
    let device = user::find(kind, args[1]).ok_or(ZxError::NOT_FOUND)?;
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
    let regs = VmObject::new_physical(base, (end - base) / PAGE_SIZE);
    regs.set_cache_policy(CachePolicy::UncachedDevice)?;
    let held = Held {
        bti: BusTransactionInitiator::create(Iommu::create(), 0),
        interrupt: device_interrupt(&device),
        device: device.clone(),
    };
    let regs = proc.add_handle(Handle::new(regs, Rights::DEFAULT_VMO));
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
    };
    let irq = device.irq.map_or(u32::MAX, |irq| irq as u32);
    let words: [u64; CLAIM_WORDS] = [
        pair(regs, bti),
        transport | (irq as u64) << 32,
        (device.paddr - base) as u64,
        device.size as u64,
        extra[0],
        extra[1],
        extra[2],
        interrupt.unwrap_or(0) as u64,
    ];
    if out.write_array(&words).is_err() {
        let _ = proc.remove_handle(regs);
        let _ = proc.remove_handle(bti);
        if let Some(interrupt) = interrupt {
            let _ = proc.remove_handle(interrupt);
        }
        // Nobody drives it after all: it is there for the next to ask.
        held.take_back();
        return Err(ZxError::INVALID_ARGS);
    }
    held.until_gone(proc);
    Ok(())
}
