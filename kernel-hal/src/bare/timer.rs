//! Time and clock functions.

use alloc::boxed::Box;
use core::time::Duration;

use lock::Mutex;
use naive_timer::Timer;

#[allow(dead_code)]
pub(super) const TICKS_PER_SEC: u64 = 100;

lazy_static::lazy_static! {
    static ref NAIVE_TIMER: Mutex<Timer> = Mutex::new(Timer::default());
    /// Offset added to monotonic boot time for CLOCK_REALTIME / gettimeofday.
    static ref WALL_CLOCK_OFFSET: Mutex<Duration> = Mutex::new(Duration::ZERO);
}

/// Wall-clock time (Unix epoch): monotonic since boot + adjustable offset.
pub fn wall_clock_now() -> Duration {
    timer_now() + *WALL_CLOCK_OFFSET.lock()
}

/// Set wall-clock instant (`CLOCK_REALTIME` / `settimeofday`).
pub fn wall_clock_set(target: Duration) {
    let mono = timer_now();
    *WALL_CLOCK_OFFSET.lock() = target.saturating_sub(mono);
}

hal_fn_impl! {
    impl mod crate::hal_fn::timer {
        fn timer_enable() {
            super::arch::timer_init();
        }

        fn timer_now() -> Duration {
            super::arch::timer::timer_now()
        }

        fn timer_set(deadline: Duration, callback: Box<dyn FnOnce(Duration) + Send + Sync>) {
            debug!("Set timer at: {:?}", deadline);
            // Mutex::lock() uses push_off/pop_off which already handles interrupt
            // disabling. Manual intr_off/on here would bypass the noff accounting
            // and cause "RefCell already borrowed" panics under SMP.
            NAIVE_TIMER.lock().add(deadline, callback);
        }

        fn timer_tick() {
            // Every CPU's LAPIC timer lands here; polling the (single) xHCI
            // controller from all of them just multiplies MMIO traffic and
            // lock contention by the core count, so only CPU 0 polls.
            #[cfg(all(
                target_arch = "x86_64",
                not(feature = "no-pci")
            ))]
            if crate::cpu::cpu_id() == 0 {
                zcore_drivers::usb::xhci_hid::poll();
            }
            NAIVE_TIMER.lock().expire(timer_now());
        }
    }
}
