//! Time and clock functions.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use lock::Mutex;
use naive_timer::Timer;

/// Timer interrupt frequency in Hz.
/// 250 Hz gives a 4 ms tick granularity — a good balance between
/// scheduler responsiveness and interrupt overhead for a desktop/interactive
/// workload. (Previous value was 100 Hz / 10 ms which caused noticeable lag.)
pub(super) const TICKS_PER_SEC: u64 = 250;

lazy_static::lazy_static! {
    static ref NAIVE_TIMER: Mutex<Timer> = Mutex::new(Timer::default());
}

/// Offset (in nanoseconds) added to monotonic boot time for
/// `CLOCK_REALTIME` / `gettimeofday`. Stored as a raw `u64` so the read path
/// (`clock_gettime` is on libc's critical path for almost every interactive
/// program) hits a single relaxed load instead of acquiring a spinlock.
/// `u64` nanoseconds covers ~584 years from the Unix epoch — more than
/// enough for any wall-clock we care about.
static WALL_CLOCK_OFFSET_NS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock time (Unix epoch): monotonic since boot + adjustable offset.
pub fn wall_clock_now() -> Duration {
    let offset = Duration::from_nanos(WALL_CLOCK_OFFSET_NS.load(Ordering::Relaxed));
    timer_now() + offset
}

/// Set wall-clock instant (`CLOCK_REALTIME` / `settimeofday`).
pub fn wall_clock_set(target: Duration) {
    let mono = timer_now();
    let offset = target.saturating_sub(mono);
    // `Duration::as_nanos` is u128; truncate to u64. Anything beyond ~584
    // years would already be a nonsensical wall-clock value here, so
    // clamping is fine.
    let ns = u64::try_from(offset.as_nanos()).unwrap_or(u64::MAX);
    WALL_CLOCK_OFFSET_NS.store(ns, Ordering::Relaxed);
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
            #[cfg(all(
                target_arch = "x86_64",
                not(feature = "no-pci")
            ))]
            zcore_drivers::usb::xhci_hid::poll();
            NAIVE_TIMER.lock().expire(timer_now());
        }
    }
}
