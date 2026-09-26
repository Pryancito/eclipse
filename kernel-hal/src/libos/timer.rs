//! Time and clock functions.

use async_std::task;
use core::sync::atomic::{AtomicI64, Ordering};
use nix::time::{clock_gettime, ClockId};
use std::time::{Duration, SystemTime};

hal_fn_impl! {
    impl mod crate::hal_fn::timer {
        fn timer_now() -> Duration {
            let now = clock_gettime(ClockId::CLOCK_MONOTONIC).unwrap();
            Duration::new(now.tv_sec() as u64, now.tv_nsec() as u32)
        }

        fn timer_now_realtime() -> Duration {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
        }

        fn timer_set(deadline: Duration, callback: Box<dyn FnOnce(Duration) + Send + Sync>) {
            task::spawn(async move {
                let now = timer_now();
                if deadline > now {
                    task::sleep(deadline - now).await;
                }
                callback(timer_now());
            });
        }
    }
}

/// Nanoseconds `settimeofday` has shifted the wall clock by, relative to the
/// host's own `CLOCK_REALTIME`. Signed: the clock may legitimately be set back.
static WALL_CLOCK_OFFSET_NS: AtomicI64 = AtomicI64::new(0);

/// Wall-clock time (Unix epoch).
///
/// This used to return `timer_now()`, i.e. `CLOCK_MONOTONIC` -- the host's
/// UPTIME, a few thousand seconds. So `time()`, `gettimeofday()` and every
/// timestamp the kernel stamps on a file said 1970 while hostfs handed back the
/// host's real mtimes, and `functional/stat.exe` failed on exactly that:
///
/// ```text
/// st.st_ctime<=t failed: 1790001664 > 2497
/// ```
///
/// Anything that compares a file's time against the clock -- `make`, `find
/// -newer`, a cache's freshness check -- saw the whole filesystem dated decades
/// into the future.
pub fn wall_clock_now() -> Duration {
    let host = timer_now_realtime();
    let offset = WALL_CLOCK_OFFSET_NS.load(Ordering::Relaxed);
    if offset >= 0 {
        host + Duration::from_nanos(offset as u64)
    } else {
        host.saturating_sub(Duration::from_nanos(offset.unsigned_abs()))
    }
}

/// Adjust the wall clock (`settimeofday`). The host's clock is never touched --
/// the kernel only remembers how far its own clock has been moved from it.
pub fn wall_clock_set(target: Duration) {
    let host = timer_now_realtime().as_nanos() as i128;
    let delta = (target.as_nanos() as i128 - host).clamp(i64::MIN as i128, i64::MAX as i128);
    WALL_CLOCK_OFFSET_NS.store(delta as i64, Ordering::Relaxed);
}

/// Wall-clock offset in nanoseconds, for the vDSO. libos has no vDSO (processes
/// are host threads), so nothing reads this; it reports the offset against the
/// host clock rather than against monotonic time.
pub fn wall_clock_offset_ns() -> u64 {
    WALL_CLOCK_OFFSET_NS.load(Ordering::Relaxed).max(0) as u64
}

/// TSC multiplier for the vDSO (libos: there is no vDSO — processes are host
/// threads and read the host's own clock).
pub fn vdso_tsc_mult() -> Option<u64> {
    None
}

/// Register the clock-parameter observer (libos: nothing publishes a clock to
/// userspace, so the registration is inert).
pub fn set_clock_observer(_observer: fn()) {}

/// Force the TSC to be considered usable by userspace (libos: no vDSO).
pub fn set_force_tsc_invariant(_force: bool) {}

/// Bare-metal timer-callback containment flag (libos: always false).
pub fn in_timer_callback() -> bool {
    false
}
