use alloc::sync::Arc;
use core::ops::Range;

use super::Scheme;
use crate::DeviceResult;

/// An interrupt handler closure.
///
/// `Arc`, not `Box`, so a dispatcher can clone the handler out of its table,
/// release the table lock, and only THEN invoke it — the clone keeps the
/// closure alive even if another CPU unregisters it mid-call. Running a handler
/// while still holding the dispatch lock deadlocks: the x86 timer handler
/// re-enters the IRQ path and re-takes the same global APIC lock on the same
/// CPU, freezing it (and the timer heap lock) forever. See `x86_apic::handle_irq`.
pub type IrqHandler = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug)]
pub enum IrqTriggerMode {
    Edge,
    Level,
}

#[derive(Debug)]
pub enum IrqPolarity {
    ActiveHigh,
    ActiveLow,
}

pub trait IrqScheme: Scheme {
    /// Is a valid IRQ number.
    fn is_valid_irq(&self, irq_num: usize) -> bool;

    /// Disable IRQ.
    fn mask(&self, irq_num: usize) -> DeviceResult;

    /// Enable IRQ.
    fn unmask(&self, irq_num: usize) -> DeviceResult;

    /// Configure the specified interrupt vector. If it is invoked, it must be
    /// invoked prior to interrupt registration.
    fn configure(&self, _irq_num: usize, _tm: IrqTriggerMode, _pol: IrqPolarity) -> DeviceResult {
        unimplemented!()
    }

    /// Add an interrupt handler to an IRQ.
    fn register_handler(&self, irq_num: usize, handler: IrqHandler) -> DeviceResult;

    /// Register the device to delivery an IRQ.
    fn register_device(&self, irq_num: usize, dev: Arc<dyn Scheme>) -> DeviceResult {
        self.register_handler(irq_num, Arc::new(move || dev.handle_irq(irq_num)))
    }

    /// Remove the interrupt handler to an IRQ.
    fn unregister(&self, irq_num: usize) -> DeviceResult;

    /// Method used for platform allocation of blocks of MSI and MSI-X compatible
    /// IRQ targets.
    fn msi_alloc_block(&self, _requested_irqs: usize) -> DeviceResult<Range<usize>> {
        unimplemented!()
    }

    /// Method used to free a block of MSI IRQs previously allocated by msi_alloc_block().
    /// This does not unregister IRQ handlers.
    fn msi_free_block(&self, _block: Range<usize>) -> DeviceResult {
        unimplemented!()
    }

    /// Register a handler function for a given msi_id within an msi_block_t. Passing a
    /// NULL handler will effectively unregister a handler for a given msi_id within the
    /// block.
    fn msi_register_handler(
        &self,
        _block: Range<usize>,
        _msi_id: usize,
        _handler: IrqHandler,
    ) -> DeviceResult {
        unimplemented!()
    }

    /// Init irq for current cpu.
    /// Some IRQ hardware requires per-CPU initialization.
    fn init_hart(&self) {
        unimplemented!()
    }

    /// [for x86_64] enable apic timer
    fn apic_timer_enable(&self) {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Mutex;
    use crate::DeviceError;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// A controller that keeps whatever `register_handler` is given instead of
    /// touching hardware, so the default `register_device` can be observed
    /// from the other side of the call it makes.
    struct TableIrq {
        registered: Mutex<Vec<(usize, IrqHandler)>>,
    }

    impl TableIrq {
        fn new() -> Self {
            Self {
                registered: Mutex::new(Vec::new()),
            }
        }

        /// Invoke the handler stored for `irq_num`, the way a dispatcher
        /// would.
        fn fire(&self, irq_num: usize) {
            let handler = self
                .registered
                .lock()
                .iter()
                .find(|(n, _)| *n == irq_num)
                .map(|(_, h)| h.clone());
            handler.expect("no handler for that irq")();
        }
    }

    impl Scheme for TableIrq {
        fn name(&self) -> &str {
            "table-irq"
        }
    }

    impl IrqScheme for TableIrq {
        fn is_valid_irq(&self, irq_num: usize) -> bool {
            irq_num < 256
        }
        fn mask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn register_handler(&self, irq_num: usize, handler: IrqHandler) -> DeviceResult {
            self.registered.lock().push((irq_num, handler));
            Ok(())
        }
        fn unregister(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
    }

    /// A controller whose `register_handler` refuses, so the default
    /// `register_device` can be checked for swallowing the refusal.
    struct RefusingIrq;

    impl Scheme for RefusingIrq {
        fn name(&self) -> &str {
            "refusing-irq"
        }
    }

    impl IrqScheme for RefusingIrq {
        fn is_valid_irq(&self, _irq_num: usize) -> bool {
            false
        }
        fn mask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn register_handler(&self, _irq_num: usize, _handler: IrqHandler) -> DeviceResult {
            Err(DeviceError::AlreadyExists)
        }
        fn unregister(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
    }

    /// A device that records which IRQ number it was told about.
    struct CountingDevice {
        calls: AtomicUsize,
        last_irq: AtomicUsize,
    }

    impl CountingDevice {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                last_irq: AtomicUsize::new(usize::MAX),
            })
        }
    }

    impl Scheme for CountingDevice {
        fn name(&self) -> &str {
            "counting-device"
        }
        fn handle_irq(&self, irq_num: usize) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.last_irq.store(irq_num, Ordering::Relaxed);
        }
    }

    /// `register_device` is sugar over `register_handler`: it must register
    /// under the IRQ it was asked for, and once.
    #[test]
    fn registering_a_device_registers_one_handler_for_that_irq() {
        let ctrl = TableIrq::new();
        ctrl.register_device(42, CountingDevice::new()).unwrap();
        let table = ctrl.registered.lock();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].0, 42);
    }

    /// And the closure it builds must hand the device the SAME number: a
    /// driver that multiplexes sources off one vector decides what to service
    /// from this argument, so an off-by-one here services the wrong device.
    #[test]
    fn the_registered_closure_passes_the_irq_number_through() {
        let ctrl = TableIrq::new();
        let dev = CountingDevice::new();
        ctrl.register_device(7, dev.clone()).unwrap();
        assert_eq!(
            dev.calls.load(Ordering::Relaxed),
            0,
            "fired at register time"
        );
        ctrl.fire(7);
        assert_eq!(dev.calls.load(Ordering::Relaxed), 1);
        assert_eq!(dev.last_irq.load(Ordering::Relaxed), 7);
    }

    /// Nothing is invoked until the controller fires, and each fire is one
    /// call -- a handler that ran twice per interrupt would double every
    /// device's work.
    #[test]
    fn each_fire_invokes_the_device_once() {
        let ctrl = TableIrq::new();
        let dev = CountingDevice::new();
        ctrl.register_device(3, dev.clone()).unwrap();
        ctrl.fire(3);
        ctrl.fire(3);
        ctrl.fire(3);
        assert_eq!(dev.calls.load(Ordering::Relaxed), 3);
    }

    /// A refusal from `register_handler` must reach the caller: a driver that
    /// believed its device was wired up would wait forever for interrupts
    /// nobody delivers.
    #[test]
    fn a_refused_registration_is_reported_to_the_caller() {
        assert_eq!(
            RefusingIrq.register_device(1, CountingDevice::new()),
            Err(DeviceError::AlreadyExists)
        );
    }

    /// The handler is an `Arc` on purpose: a dispatcher clones it out of its
    /// table, drops the table lock, and only then calls it. Cloning must not
    /// detach it from the device.
    #[test]
    fn a_cloned_handler_still_reaches_the_device() {
        let ctrl = TableIrq::new();
        let dev = CountingDevice::new();
        ctrl.register_device(9, dev.clone()).unwrap();
        let handler = ctrl.registered.lock()[0].1.clone();
        // The table is unlocked here, which is the whole point of the clone.
        handler();
        assert_eq!(dev.calls.load(Ordering::Relaxed), 1);
        assert_eq!(dev.last_irq.load(Ordering::Relaxed), 9);
    }

    /// The two enums are what `configure` is told; they must stay distinct
    /// (and printable, since bring-up logs them).
    #[test]
    fn the_trigger_mode_and_polarity_names_are_distinct() {
        use alloc::format;
        assert_ne!(
            format!("{:?}", IrqTriggerMode::Edge),
            format!("{:?}", IrqTriggerMode::Level)
        );
        assert_ne!(
            format!("{:?}", IrqPolarity::ActiveHigh),
            format!("{:?}", IrqPolarity::ActiveLow)
        );
    }

    /// The optional corners of the trait are `unimplemented!()`, which is a
    /// panic, not an error return. That is deliberate -- a caller asking an
    /// 8259 for an MSI block has a bug, not a runtime condition -- but it
    /// means nothing may call them speculatively, so pin it.
    #[test]
    #[should_panic(expected = "not implemented")]
    fn asking_a_plain_controller_for_an_msi_block_panics() {
        let _ = TableIrq::new().msi_alloc_block(1);
    }

    /// Same for the per-CPU init: a controller that needs none must not be
    /// assumed to tolerate the call.
    #[test]
    #[should_panic(expected = "not implemented")]
    fn init_hart_on_a_controller_that_needs_none_panics() {
        TableIrq::new().init_hart();
    }
}
