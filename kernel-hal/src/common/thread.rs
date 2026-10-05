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
