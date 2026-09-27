use crate::sync::Mutex;
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
use riscv::register::sie;

use crate::prelude::IrqHandler;
use crate::scheme::{IrqScheme, Scheme};
use crate::utils::run_irq_handler;
use crate::{DeviceError, DeviceResult};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU8, Ordering};

const S_SOFT: usize = 1;
const S_TIMER: usize = 5;
const S_EXT: usize = 9;

/// This hart's supervisor interrupt-enable CSR, doubled for the host test build.
///
/// `sie` is a control register, not memory: on an x86_64 host there is nothing
/// to point it at, and that one import is why this whole file stayed out of
/// every `cargo test` -- which is how it kept a `.unwrap()` that panics the
/// kernel from inside the trap handler. The double records the three bits, which
/// is all `mask` and `unmask` do with it.
#[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
mod sie {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// The CSR's bit for each cause is the cause number itself.
    const fn bit(cause: usize) -> usize {
        1 << cause
    }

    static BITS: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn bits() -> usize {
        BITS.load(Ordering::SeqCst)
    }

    pub(super) fn enabled(cause: usize) -> bool {
        bits() & bit(cause) != 0
    }

    pub(super) fn reset() {
        BITS.store(0, Ordering::SeqCst);
    }

    /// # Safety
    ///
    /// None on the host; the signature matches the real CSR accessor so the
    /// driver reads the same either way.
    pub(super) unsafe fn set_ssoft() {
        BITS.fetch_or(bit(super::S_SOFT), Ordering::SeqCst);
    }
    /// # Safety
    /// See [`set_ssoft`].
    pub(super) unsafe fn set_stimer() {
        BITS.fetch_or(bit(super::S_TIMER), Ordering::SeqCst);
    }
    /// # Safety
    /// See [`set_ssoft`].
    pub(super) unsafe fn set_sext() {
        BITS.fetch_or(bit(super::S_EXT), Ordering::SeqCst);
    }
    /// # Safety
    /// See [`set_ssoft`].
    pub(super) unsafe fn clear_ssoft() {
        BITS.fetch_and(!bit(super::S_SOFT), Ordering::SeqCst);
    }
    /// # Safety
    /// See [`set_ssoft`].
    pub(super) unsafe fn clear_stimer() {
        BITS.fetch_and(!bit(super::S_TIMER), Ordering::SeqCst);
    }
    /// # Safety
    /// See [`set_ssoft`].
    pub(super) unsafe fn clear_sext() {
        BITS.fetch_and(!bit(super::S_EXT), Ordering::SeqCst);
    }
}

static INTC_NUM: AtomicU8 = AtomicU8::new(0);

#[repr(usize)]
pub enum ScauseIntCode {
    SupervisorSoft = S_SOFT,
    SupervisorTimer = S_TIMER,
    SupervisorExternal = S_EXT,
}

pub struct Intc {
    name: String,
    soft_handler: Mutex<Option<IrqHandler>>,
    timer_handler: Mutex<Option<IrqHandler>>,
    ext_handler: Mutex<Option<IrqHandler>>,
}

impl Intc {
    pub fn new() -> Self {
        Self {
            name: format!("riscv-intc-cpu{}", INTC_NUM.fetch_add(1, Ordering::Relaxed)),
            soft_handler: Mutex::new(None),
            timer_handler: Mutex::new(None),
            ext_handler: Mutex::new(None),
        }
    }

    /// The slot a SCAUSE value names, or `None` when this controller does not
    /// own that cause.
    ///
    /// The three causes used to be listed in four places -- here, `handle_irq`,
    /// `is_valid_irq` and the two CSR paths -- which is how the set that can be
    /// registered drifts apart from the set that gets dispatched. This is the
    /// one that decides.
    fn handler_slot(&self, cause: usize) -> Option<&Mutex<Option<IrqHandler>>> {
        match cause {
            S_SOFT => Some(&self.soft_handler),
            S_TIMER => Some(&self.timer_handler),
            S_EXT => Some(&self.ext_handler),
            _ => None,
        }
    }

    fn with_handler<F>(&self, cause: usize, op: F) -> DeviceResult
    where
        F: FnOnce(&mut Option<IrqHandler>) -> DeviceResult,
    {
        match self.handler_slot(cause) {
            Some(slot) => op(&mut slot.lock()),
            None => {
                error!("invalid SCAUSE value {:#x}!", cause);
                Err(DeviceError::InvalidParam)
            }
        }
    }
}

impl Default for Intc {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheme for Intc {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn handle_irq(&self, cause: usize) {
        let Some(slot) = self.handler_slot(cause) else {
            // This used to be `.unwrap()` on the `Err` that an unknown cause
            // produces, i.e. a kernel panic reached straight from the trap
            // handler. `TrapReason::from` hands every interrupt cause through
            // untouched (`if is_interrupt { return Self::Interrupt(code) }`), so
            // any cause this file does not know -- a counter-overflow interrupt
            // on a hart with Sscofpmf, a platform-defined cause of 16 or more --
            // took the machine down. And a panic inside an interrupt handler
            // buries its own cause, which is the reason `IrqManager::handle` was
            // fixed for exactly this and wrote it down.
            warn!("invalid SCAUSE value {:#x}!", cause);
            return;
        };

        // Clone the handler out under the lock and run it with the lock
        // RELEASED. This used to call the handler from inside `with_handler`,
        // that is, with the very `Mutex` held that `register_handler` and
        // `unregister` take -- so a handler that registers or unregisters an
        // interrupt from inside itself deadlocked the hart, and `lock::Mutex`
        // holds interrupts off, so that hart then serviced nothing at all. The
        // x86 APIC fixed this first and wrote down why ("a self-deadlock that
        // pinned the CPU ... and froze every other core. This never reproduced
        // under 2 emulated CPUs"); the GIC-400 and the PLIC followed. This was
        // the last of the four, and it is the one that dispatches the timer.
        //
        // Cloning also keeps the closure alive if another hart unregisters it
        // while it runs, and going through `run_irq_handler` is what every other
        // interrupt controller in this crate does: it is the one guard against
        // calling through a handler whose fat pointer has been smashed, which
        // lands as an EXECUTE fault in the null range rather than as anything a
        // log would name.
        let handler = slot.lock().clone();
        if run_irq_handler(cause, handler).is_err() {
            // `trace!`, not `warn!`: a cause with no handler is usually one that
            // keeps firing, because nothing quiesced the source, and a line per
            // occurrence buries every other thing in the log. Same as the
            // GIC-400.
            trace!("no registered handler for SCAUSE {}!", cause);
        }
    }
}

impl IrqScheme for Intc {
    fn is_valid_irq(&self, cause: usize) -> bool {
        self.handler_slot(cause).is_some()
    }

    fn mask(&self, cause: usize) -> DeviceResult {
        unsafe {
            match cause {
                S_SOFT => sie::clear_ssoft(),
                S_TIMER => sie::clear_stimer(),
                S_EXT => sie::clear_sext(),
                _ => return Err(DeviceError::InvalidParam),
            }
        }
        Ok(())
    }

    fn unmask(&self, cause: usize) -> DeviceResult {
        unsafe {
            match cause {
                S_SOFT => sie::set_ssoft(),
                S_TIMER => sie::set_stimer(),
                S_EXT => sie::set_sext(),
                _ => return Err(DeviceError::InvalidParam),
            }
        }
        Ok(())
    }

    fn register_handler(&self, cause: usize, handler: IrqHandler) -> DeviceResult {
        self.with_handler(cause, |opt| {
            if opt.is_some() {
                Err(DeviceError::AlreadyExists)
            } else {
                *opt = Some(handler);
                Ok(())
            }
        })
    }

    fn unregister(&self, cause: usize) -> DeviceResult {
        self.with_handler(cause, |opt| {
            if opt.is_some() {
                *opt = None;
                Ok(())
            } else {
                Err(DeviceError::InvalidParam)
            }
        })
    }
}

/// The per-hart interrupt controller, 131 lines with no tests.
///
/// It is the first thing every interrupt on a RISC-V board goes through, and the
/// only one that dispatches the timer. One `use riscv::register::sie` kept it out
/// of every `cargo test`, and what that hid was a `.unwrap()` in `handle_irq`
/// reached from the trap handler, and a handler called with the lock that
/// registering one takes still held.
#[cfg(all(test, not(any(target_arch = "riscv32", target_arch = "riscv64"))))]
mod intc_tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicUsize;

    /// There is one `sie` per process here, so the tests that read it take
    /// turns. Zeroes the register on the way in.
    #[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
    fn alone_with_sie() -> crate::sync::MutexGuard<'static, ()> {
        static TURNSTILE: crate::sync::Mutex<()> = crate::sync::Mutex::new(());
        let guard = TURNSTILE.lock();
        sie::reset();
        guard
    }

    /// A handler and the count of how many times it has run.
    fn counter() -> (IrqHandler, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let mine = count.clone();
        (
            Arc::new(move || {
                mine.fetch_add(1, Ordering::SeqCst);
            }),
            count,
        )
    }

    /// The three causes this controller owns.
    const CAUSES: [usize; 3] = [S_SOFT, S_TIMER, S_EXT];

    /// Causes a real hart can raise that this controller does not own. 13 is the
    /// counter-overflow interrupt of Sscofpmf; 16 and up are platform-defined,
    /// which is where the XuanTie cores put their own; 2 and 6 belong to the
    /// hypervisor extension. `TrapReason::from` passes every one of them straight
    /// through to `handle_irq`.
    const FOREIGN_CAUSES: [usize; 7] = [0, 2, 6, 13, 16, 17, usize::MAX];

    /// The fix. `handle_irq` used to `.unwrap()` the `Err` an unknown cause
    /// produces, and it is called from the trap handler, where a panic takes the
    /// machine and buries its own cause.
    #[test]
    fn a_cause_this_controller_does_not_own_does_not_take_the_machine_down() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        for cause in FOREIGN_CAUSES {
            intc.handle_irq(cause);
        }
    }

    /// And an unknown cause is still refused everywhere it can be refused with a
    /// return value.
    #[test]
    fn a_cause_this_controller_does_not_own_is_refused_by_every_other_entry() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        for cause in FOREIGN_CAUSES {
            assert_eq!(
                intc.register_handler(cause, counter().0),
                Err(DeviceError::InvalidParam),
                "cause {:#x} must not be registrable",
                cause
            );
            assert_eq!(intc.unregister(cause), Err(DeviceError::InvalidParam));
            assert_eq!(intc.mask(cause), Err(DeviceError::InvalidParam));
            assert_eq!(intc.unmask(cause), Err(DeviceError::InvalidParam));
            assert!(!intc.is_valid_irq(cause));
        }
    }

    #[test]
    fn the_handler_registered_for_a_cause_is_the_one_that_runs() {
        let _alone = alone_with_sie();
        for cause in CAUSES {
            let intc = Intc::new();
            let (handler, count) = counter();
            intc.register_handler(cause, handler).unwrap();

            intc.handle_irq(cause);
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "cause {} ran nothing",
                cause
            );

            for other in CAUSES.iter().filter(|c| **c != cause) {
                intc.handle_irq(*other);
            }
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "a handler ran for a cause it was never registered for"
            );
        }
    }

    /// The other fix, and the one that hangs if it comes undone rather than
    /// failing. The handler used to run from inside `with_handler`, holding the
    /// same `Mutex` that `unregister` takes -- and quiescing the source from
    /// inside the handler is what a device driver does. `lock::Mutex` holds
    /// interrupts off, so the hart then serviced nothing at all.
    #[test]
    fn a_handler_that_unregisters_itself_does_not_wedge_the_hart() {
        let _alone = alone_with_sie();
        let intc = Arc::new(Intc::new());
        let reached_the_end = Arc::new(AtomicUsize::new(0));

        let weak = Arc::downgrade(&intc);
        let marker = reached_the_end.clone();
        intc.register_handler(
            S_TIMER,
            Arc::new(move || {
                if let Some(intc) = weak.upgrade() {
                    intc.unregister(S_TIMER).unwrap();
                }
                // After the unregister on purpose: the handler is cloned out of
                // the slot before it runs, so dropping the slot's `Arc` mid-call
                // must not pull the captured state out from under it.
                marker.fetch_add(7, Ordering::SeqCst);
            }),
        )
        .unwrap();

        intc.handle_irq(S_TIMER);

        assert_eq!(
            reached_the_end.load(Ordering::SeqCst),
            7,
            "the handler did not run to completion"
        );
        assert!(
            intc.register_handler(S_TIMER, counter().0).is_ok(),
            "the slot the handler unregistered must be free again"
        );
    }

    /// Registering a *different* cause from inside a handler takes a different
    /// lock, which never deadlocked -- but it is the shape a driver that chains
    /// interrupts has, so it is worth pinning.
    #[test]
    fn a_handler_that_registers_another_cause_from_inside_completes() {
        let _alone = alone_with_sie();
        let intc = Arc::new(Intc::new());
        let weak = Arc::downgrade(&intc);
        intc.register_handler(
            S_EXT,
            Arc::new(move || {
                if let Some(intc) = weak.upgrade() {
                    intc.register_handler(S_SOFT, Arc::new(|| {})).unwrap();
                }
            }),
        )
        .unwrap();

        intc.handle_irq(S_EXT);

        assert!(!intc.is_valid_irq(usize::MAX));
        assert_eq!(
            intc.register_handler(S_SOFT, counter().0),
            Err(DeviceError::AlreadyExists),
            "the handler the first one registered must be there"
        );
    }

    /// A cause whose source keeps firing with nothing registered is the normal
    /// way this is reached, so it has to be cheap and silent, not fatal.
    #[test]
    fn a_cause_with_no_handler_is_harmless() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        for _ in 0..3 {
            for cause in CAUSES {
                intc.handle_irq(cause);
            }
        }
    }

    #[test]
    fn a_second_handler_for_one_cause_is_refused_and_the_first_one_survives() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        let (first, count) = counter();
        intc.register_handler(S_EXT, first).unwrap();

        assert_eq!(
            intc.register_handler(S_EXT, counter().0),
            Err(DeviceError::AlreadyExists)
        );

        intc.handle_irq(S_EXT);
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "the refused registration replaced the handler that was already there"
        );
    }

    #[test]
    fn a_cause_can_be_registered_again_once_it_is_unregistered() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        let (first, first_count) = counter();
        intc.register_handler(S_SOFT, first).unwrap();
        intc.unregister(S_SOFT).unwrap();

        let (second, second_count) = counter();
        intc.register_handler(S_SOFT, second).unwrap();
        intc.handle_irq(S_SOFT);

        assert_eq!(first_count.load(Ordering::SeqCst), 0, "the old handler ran");
        assert_eq!(second_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unregistering_a_cause_that_has_no_handler_is_an_error() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        assert_eq!(intc.unregister(S_TIMER), Err(DeviceError::InvalidParam));
    }

    /// One source of truth for the three causes. They used to be listed in four
    /// places, and a set that can be registered but not dispatched is a handler
    /// that is never called.
    #[test]
    fn the_causes_it_calls_valid_are_exactly_the_causes_it_accepts() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        for cause in 0..64usize {
            let valid = intc.is_valid_irq(cause);
            assert_eq!(
                valid,
                intc.register_handler(cause, counter().0).is_ok(),
                "cause {}: is_valid_irq and register_handler disagree",
                cause
            );
            if valid {
                intc.unregister(cause).unwrap();
            }
            assert_eq!(
                valid,
                intc.unmask(cause).is_ok(),
                "cause {}: is_valid_irq and unmask disagree, so a line can be \
                 called valid and never actually enabled",
                cause
            );
        }
    }

    #[test]
    fn unmasking_a_cause_enables_exactly_its_own_line() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        for cause in CAUSES {
            sie::reset();
            intc.unmask(cause).unwrap();
            assert!(sie::enabled(cause), "cause {} was not enabled", cause);
            assert_eq!(
                sie::bits().count_ones(),
                1,
                "unmasking cause {} enabled another line too: sie = {:#x}",
                cause,
                sie::bits()
            );
        }
    }

    #[test]
    fn masking_a_cause_leaves_the_other_two_alone() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        for cause in CAUSES {
            sie::reset();
            for each in CAUSES {
                intc.unmask(each).unwrap();
            }
            assert_eq!(sie::bits().count_ones(), 3, "all three must be enabled");

            intc.mask(cause).unwrap();

            assert!(!sie::enabled(cause), "cause {} stayed enabled", cause);
            for other in CAUSES.iter().filter(|c| **c != cause) {
                assert!(
                    sie::enabled(*other),
                    "masking cause {} also masked cause {}",
                    cause,
                    other
                );
            }
        }
    }

    /// Both directions are idempotent, which the boot path relies on: it unmasks
    /// the soft and timer lines on every hart, and a hart that comes up twice
    /// must not end up with something else enabled.
    #[test]
    fn masking_and_unmasking_twice_is_the_same_as_once() {
        let _alone = alone_with_sie();
        let intc = Intc::new();
        intc.unmask(S_TIMER).unwrap();
        let after_one = sie::bits();
        intc.unmask(S_TIMER).unwrap();
        assert_eq!(sie::bits(), after_one);

        intc.mask(S_TIMER).unwrap();
        let after_mask = sie::bits();
        intc.mask(S_TIMER).unwrap();
        assert_eq!(sie::bits(), after_mask);
        assert_eq!(after_mask, 0);
    }

    /// `kernel-hal` registers and unmasks through this enum
    /// (`bare/arch/riscv/drivers.rs`), and the controller dispatches on the
    /// constants. A drift between the two is a handler registered for a cause
    /// nobody delivers.
    #[test]
    fn the_public_cause_numbers_are_the_ones_the_controller_dispatches_on() {
        let _alone = alone_with_sie();
        assert_eq!(ScauseIntCode::SupervisorSoft as usize, S_SOFT);
        assert_eq!(ScauseIntCode::SupervisorTimer as usize, S_TIMER);
        assert_eq!(ScauseIntCode::SupervisorExternal as usize, S_EXT);

        let intc = Intc::new();
        for cause in [
            ScauseIntCode::SupervisorSoft as usize,
            ScauseIntCode::SupervisorTimer as usize,
            ScauseIntCode::SupervisorExternal as usize,
        ] {
            assert!(
                intc.is_valid_irq(cause),
                "cause {} is not dispatched",
                cause
            );
        }
    }

    /// The name is how `kernel-hal` finds the controller belonging to a hart, so
    /// two of them must not share one.
    #[test]
    fn each_controller_gets_a_name_of_its_own() {
        let _alone = alone_with_sie();
        let first = Intc::new();
        let second = Intc::new();
        assert!(first.name().starts_with("riscv-intc-cpu"));
        assert_ne!(first.name(), second.name());
    }
}
