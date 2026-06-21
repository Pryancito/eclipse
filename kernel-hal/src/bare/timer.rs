//! Time and clock functions.

use alloc::boxed::Box;
use core::convert::TryFrom;
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

/// Earliest pending timer deadline (in monotonic nanoseconds), or `u64::MAX`
/// when no timer is registered. Maintained alongside the heap inside the
/// `NAIVE_TIMER` lock, but readable lock-free. Lets every CPU's per-tick
/// `timer_tick` skip the spinlock when there is nothing to expire — the
/// common case under multi-CPU where all CPUs would otherwise contend on
/// the timer mutex 250 times a second.
static NEXT_DEADLINE_NS: AtomicU64 = AtomicU64::new(u64::MAX);

#[inline]
fn duration_to_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

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
            let mut t = NAIVE_TIMER.lock();
            t.add(deadline, callback);
            // Republish the new earliest deadline so other CPUs' fast-path
            // ticks observe it. Done under the lock so concurrent updates
            // can't race with `timer_tick`'s post-expire publish.
            let next = t.next().map(duration_to_ns).unwrap_or(u64::MAX);
            NEXT_DEADLINE_NS.store(next, Ordering::Release);
        }

        fn timer_tick() {
            // Blink the framebuffer text cursor. Cheap (one atomic load) on most
            // ticks; must run before the lock-free deadline fast-path below so it
            // keeps blinking while the system is idle with no pending timers.
            crate::console::cursor_blink_tick();

            #[cfg(all(
                target_arch = "x86_64",
                not(feature = "no-pci")
            ))]
            zcore_drivers::usb::xhci_hid::poll();
            // Lock-free fast path: if the earliest pending deadline hasn't
            // arrived yet, skip the mutex entirely. Saves a spinlock acquire
            // per CPU per tick (250 Hz × N CPUs), which is the dominant
            // contention on the timer mutex under SMP.
            let now = timer_now();
            if duration_to_ns(now) < NEXT_DEADLINE_NS.load(Ordering::Acquire) {
                return;
            }
            let mut t = NAIVE_TIMER.lock();
            t.expire(now);
            let next = t.next().map(duration_to_ns).unwrap_or(u64::MAX);
            NEXT_DEADLINE_NS.store(next, Ordering::Release);
        }
    }
}
