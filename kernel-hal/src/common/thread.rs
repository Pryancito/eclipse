use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use super::future::{SleepFuture, YieldFuture};

/// Sleeps until the specified of time.
pub async fn sleep_until(deadline: Duration) {
    SleepFuture::new(deadline).await
}

/// Yields execution back to the async runtime, by the task's own choice.
///
/// This is `sched_yield(2)`: the caller is asking for someone else to go
/// first, so it is filed in the scheduler's voluntary-yield lane, behind every
/// externally woken task.
pub async fn yield_now() {
    YieldFuture::voluntary().await
}

/// Gives the CPU back because the scheduler is taking it, not because the task
/// asked to.
///
/// Used by the trap path when a timeslice expires or a wake-up preemption is
/// honoured. Unlike [`yield_now`], the task re-enters the ordinary run queue:
/// it did not volunteer, so it must not wait behind every notify for as long
/// as any peer keeps waking.
pub async fn preempt_now() {
    YieldFuture::involuntary().await
}

/// Last RUN_TO_PARITY floor each CPU has already armed its timer for.
///
/// Absolute monotonic nanoseconds; `0` is "never armed". Floors only move
/// forward in time (each slice starts after the last one), so the comparison is
/// a plain `>` and a burst of traps inside one floor window costs one timer
/// instead of one per trap.
static FLOOR_ARMED_NS: [AtomicU64; crate::config::MAX_CORE_NUM] =
    [const { AtomicU64::new(0) }; crate::config::MAX_CORE_NUM];

/// Whether a floor at `deadline` still needs a timer on a CPU whose last armed
/// floor was `armed`, read at `now`.
///
/// A floor already in the past needs nothing: the request is honoured by the
/// next interrupt whatever it is, and arming for a deadline behind the clock
/// would only burn a timer. A floor this CPU has already armed for needs
/// nothing either.
pub(crate) fn should_arm_floor(now: u64, deadline: u64, armed: u64) -> bool {
    deadline > now && deadline > armed
}

/// Bring this CPU's timer forward so a *denied* wake-up preemption request is
/// honoured when the running thread's RUN_TO_PARITY floor ends, instead of at
/// the next scheduler tick.
///
/// The trap path checks the pending request on every interrupt vector, so the
/// interrupt is the whole mechanism and the callback has nothing to do. Without
/// this the floor is only nominally 0.75 ms: a thread in a userspace compute
/// loop takes no interrupt at all until the 4 ms tick, so a neighbour that
/// parks briefly and often was served once per tick and lost most of its share
/// (`eclipse-bench --only psched`, the `reparto justo` row).
pub fn arm_wake_preempt_floor(floor: Duration) {
    let cpu = crate::cpu::cpu_id() as usize;
    if cpu >= crate::config::MAX_CORE_NUM {
        return;
    }
    let now = crate::timer::timer_now().as_nanos() as u64;
    let deadline = floor.as_nanos() as u64;
    if !should_arm_floor(now, deadline, FLOOR_ARMED_NS[cpu].load(Ordering::Relaxed)) {
        return;
    }
    FLOOR_ARMED_NS[cpu].store(deadline, Ordering::Relaxed);
    crate::timer::timer_set(floor, alloc::boxed::Box::new(|_| {}));
}

#[cfg(test)]
mod floor_arm_tests {
    //! The dedup in [`should_arm_floor`], which is what keeps one timer per
    //! floor window instead of one per trap.

    use super::should_arm_floor;

    /// A floor ahead of the clock that this CPU has not armed for yet is the
    /// case the whole mechanism exists for.
    #[test]
    fn a_fresh_floor_in_the_future_gets_a_timer() {
        assert!(should_arm_floor(1_000, 1_750, 0));
        assert!(should_arm_floor(1_000, 1_750, 900));
        assert!(should_arm_floor(1_000, 1_001, 0));
    }

    /// The same floor twice is one timer: a syscall-heavy thread traps many
    /// times inside one window and must not arm on every trap.
    #[test]
    fn the_same_floor_twice_is_one_timer() {
        assert!(!should_arm_floor(1_000, 1_750, 1_750));
        assert!(!should_arm_floor(1_000, 1_750, 2_000));
    }

    /// A floor already behind the clock needs nothing: the next interrupt
    /// honours the request whatever it is.
    #[test]
    fn a_floor_already_past_needs_no_timer() {
        assert!(!should_arm_floor(1_000, 1_000, 0));
        assert!(!should_arm_floor(1_000, 999, 0));
        assert!(!should_arm_floor(1_000, 0, 0));
    }
}
