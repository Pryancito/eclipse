//! [`ECLIPSE_DEVICE_CLAIM`](super::ECLIPSE_DEVICE_CLAIM): handing a device
//! the kernel left alone to the process that will drive it.

use super::{
    CLAIM_WORDS, ECLIPSE_DEVICE_VIRTIO_BLK, ECLIPSE_DEVICE_VIRTIO_NET,
    ECLIPSE_TRANSPORT_VIRTIO_MMIO, ECLIPSE_TRANSPORT_VIRTIO_PCI,
};
use alloc::{boxed::Box, sync::Arc};
use kernel_hal::drivers::scheme::IrqScheme;
use kernel_hal::drivers::user::{self, Transport};
use kernel_hal::{CachePolicy, PAGE_SIZE};
use zircon_object::dev::{BusTransactionInitiator, Interrupt, Iommu, IrqLine};
use zircon_object::object::{Handle, HandleValue, Rights};
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
/// Nothing tears it down: the handler it registers holds it, and the device
/// it belongs to is never handed back (`UserDevice::claim`).
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
    let bti = BusTransactionInitiator::create(Iommu::create(), 0);
    let regs = proc.add_handle(Handle::new(regs, Rights::DEFAULT_VMO));
    let bti = proc.add_handle(Handle::new(bti, Rights::DEFAULT_BTI));
    let interrupt: Option<HandleValue> = device_interrupt(&device)
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
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(())
}
