//! What a new task takes from the one that cloned it, as far as the
//! scheduler goes: `dup_task_struct` copies the CPU mask and the scheduling
//! class, and `sched_fork` takes the class back when the parent asked for
//! `SCHED_RESET_ON_FORK`.
//!
//! A forked child and a new thread used to start on the defaults, whatever
//! their parent had: `nice -n 10 make` built at nice 0 from the first
//! compiler down, and `taskset` pinned only the process it started.

use super::*;

/// The policy, nice value and real-time priority of a task cloned from one
/// with `(policy, nice, rt_priority)`.
///
/// Without `SCHED_RESET_ON_FORK` it is all three. With it, `sched_fork` does
///
/// ```c
/// if (task_has_dl_policy(p) || task_has_rt_policy(p)) {
///         p->policy = SCHED_NORMAL;
///         p->static_prio = NICE_TO_PRIO(0);
///         p->rt_priority = 0;
/// } else if (PRIO_TO_NICE(p->static_prio) < 0)
///         p->static_prio = NICE_TO_PRIO(0);
/// ```
///
/// so a real-time parent has ordinary children at nice 0, and a fair one
/// keeps its class and its nice unless that nice was below 0. What the flag
/// takes away is what took privilege to get.
pub(super) fn sched_after_fork(
    policy: u8,
    nice: i8,
    rt_priority: u8,
    reset_on_fork: bool,
) -> (u8, i8, u8) {
    if !reset_on_fork {
        (policy, nice, rt_priority)
    } else if matches!(policy, SCHED_FIFO | SCHED_RR | SCHED_DEADLINE) {
        (SCHED_NORMAL, 0, 0)
    } else {
        (policy, nice.max(0), rt_priority)
    }
}

impl Thread {
    /// Give this thread, not yet started, what `parent` hands down to a task
    /// it clones: its CPU mask, and its scheduling class as
    /// [`sched_after_fork`] leaves it. `SCHED_RESET_ON_FORK` itself does not
    /// come across: `sched_fork` clears it in the child, and a new thread
    /// starts without it.
    ///
    /// The mask goes in by hand and not through [`Thread::set_affinity`]:
    /// there is no running thread to move off a CPU yet.
    pub fn inherit_sched(&self, parent: &Thread) {
        self.affinity.store(parent.affinity(), Ordering::Relaxed);
        let (policy, nice, rt_priority) = sched_after_fork(
            parent.sched_policy(),
            parent.sched_nice(),
            parent.sched_rt_priority(),
            parent.sched_reset_on_fork(),
        );
        self.set_sched(policy, nice, rt_priority);
    }
}
