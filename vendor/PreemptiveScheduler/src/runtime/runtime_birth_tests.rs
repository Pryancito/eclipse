//! What a per-CPU `ExecutorRuntime` is when it is built, and what a mid-poll
//! preemption does to it.
//!
//! Like `executor::birth_tests`, this module could not exist until
//! `arch::pg_base_register` got a host stand-in: `ExecutorRuntime::new`
//! creates the CPU's strong executor, and that reads CR3.
//!
//! What is under test is the shape of a preemption. A thread the timer
//! interrupts inside a poll cannot be dropped -- its frame is half-way through
//! someone else's future -- so the frame is frozen as a *weak* executor and
//! the CPU carries on in a brand-new strong one. Everything about that handover
//! has a failure behind it: a weak executor that is not retained is a task
//! that can never run again (its frozen frame still holds the task's borrow,
//! and the queue defers a borrowed slot's wakes), and a weak executor that is
//! retained after it finished is a 2.6 MiB stack held out of the pool while
//! the next strong one is carved.

use super::*;

/// A runtime a test may let go of.
///
/// `ExecutorRuntime` panics in its own `Drop` on purpose: there is one per CPU
/// and it is never meant to go away, so dropping one is a bug the panic names.
/// A test therefore holds it in a `ManuallyDrop` and hands the executors back
/// by itself -- the parked frames go, which is what returns their stacks to
/// the pool, and the shell is forgotten.
///
/// The guard is the executor-globals lock: `ExecutorRuntime::new` goes through
/// `Executor::new`, which touches the stack pool, both live-stack registries
/// and the spine. The published weak count is a per-CPU global too, so it is
/// zeroed on the way in and on the way out.
struct Held {
    runtime: core::mem::ManuallyDrop<ExecutorRuntime>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.runtime.weak_executors.clear();
        publish_weak_waiting(0);
    }
}

impl core::ops::Deref for Held {
    type Target = ExecutorRuntime;
    fn deref(&self) -> &ExecutorRuntime {
        &self.runtime
    }
}

impl core::ops::DerefMut for Held {
    fn deref_mut(&mut self) -> &mut ExecutorRuntime {
        &mut self.runtime
    }
}

fn held(cpu: u8) -> Held {
    let guard = crate::executor::executor_globals_lock();
    publish_weak_waiting(0);
    Held {
        runtime: core::mem::ManuallyDrop::new(ExecutorRuntime::new(cpu)),
        _guard: guard,
    }
}

/// **A fresh runtime owns its CPU id, an empty queue and one strong executor.**
///
/// No weak executors: there has been no preemption yet, and
/// `current_executor` is `None` because nothing has been switched into. A
/// runtime born with a weak executor in the list would hand a turn to a frame
/// that was never parked.
#[test]
fn a_fresh_runtime_has_one_strong_executor_and_nothing_parked() {
    let runtime = held(7);
    assert_eq!(runtime.cpu_id(), 7);
    assert_eq!(runtime.task_num(), 0);
    assert_eq!(runtime.weak_executor_num(), 0);
    assert!(runtime.current_executor.is_none());
    // The strong executor is real and has not run: its saved frame is the one
    // `switch.S` would resume, and its queue is the runtime's own.
    assert!(!runtime.strong_executor.is_running_future());
    assert!(!runtime.strong_executor.killed());
    assert_eq!(
        runtime.strong_executor.context.get_pgbr(),
        crate::arch::HOST_PG_BASE
    );
}

/// **A preemption parks the old strong executor and carves a new one.**
///
/// The old frame goes into the weak list -- marked weak, so the frame knows it
/// is waiting for a turn -- and the new strong executor is a different one with
/// a stack of its own. Reusing the same executor here would resume a frame that
/// is still inside a poll.
#[test]
fn downgrading_the_strong_executor_parks_it_and_builds_another() {
    let mut runtime = held(0);
    let old_id = runtime.strong_executor.id();
    let old_stack = runtime.strong_executor.stack_base();
    let (created_before, _, _) = sched_weak_stats();

    runtime.downgrade_strong_executor();

    assert_eq!(runtime.weak_executor_num(), 1, "el marco no se aparco");
    assert_ne!(
        runtime.strong_executor.id(),
        old_id,
        "se siguio usando el ejecutor que estaba dentro de un poll"
    );
    assert_ne!(runtime.strong_executor.stack_base(), old_stack);
    assert_eq!(sched_weak_stats().0, created_before + 1);
    let parked = runtime.weak_executors[0].as_ref().expect("el hueco");
    assert_eq!(parked.id(), old_id);
}

/// **A weak executor that finished is dropped before the next strong is
/// carved.**
///
/// Without that, a burst of mid-poll preempts piles killed weaks in the vector
/// until the strong one finally yields with no task, and by then peak live
/// stacks have already ballooned -- each one 2.6 MiB held out of the pool while
/// the next is allocated from the heap.
#[test]
fn a_finished_weak_executor_is_reclaimed_before_the_next_one_is_built() {
    let mut runtime = held(0);
    runtime.downgrade_strong_executor();
    assert_eq!(runtime.weak_executor_num(), 1);

    // The parked frame reached the end of its poll and killed itself.
    runtime.weak_executors[0]
        .as_ref()
        .expect("el hueco")
        .force_replace_executor();
    assert!(runtime.weak_executors[0].as_ref().unwrap().killed());

    runtime.downgrade_strong_executor();
    assert_eq!(
        runtime.weak_executor_num(),
        1,
        "el marco muerto se quedo en la lista junto al nuevo"
    );
    assert!(!runtime.weak_executors[0].as_ref().unwrap().killed());
}

/// **Past the soft cap the preemption still happens, and says so.**
///
/// A future caught mid-poll cannot be dropped, so there is no ceiling to
/// enforce here -- the count is the signal that preempt churn is outrunning
/// weak drain, which is a reason to look at timeslices and not at `STACK_SIZE`.
#[test]
fn the_weak_soft_cap_is_counted_and_not_enforced() {
    let mut runtime = held(0);
    for _ in 0..MAX_WEAK_PER_CPU {
        runtime.downgrade_strong_executor();
    }
    assert_eq!(runtime.weak_executor_num(), MAX_WEAK_PER_CPU);

    let (_, _, hits_before) = sched_weak_stats();
    runtime.downgrade_strong_executor();
    assert_eq!(
        runtime.weak_executor_num(),
        MAX_WEAK_PER_CPU + 1,
        "el tope blando rechazo una preempcion"
    );
    assert_eq!(
        sched_weak_stats().2,
        hits_before + 1,
        "pasar el tope no se conto"
    );
    // And the peak is at least what this CPU just reached.
    assert!(sched_weak_stats().1 >= (MAX_WEAK_PER_CPU + 1) as u64);
}

/// **The count this CPU publishes is the length of its own weak list.**
///
/// It is read at the head of `Executor::run` to decide whether to hand the CPU
/// to a parked frame, so a stale count is either a frame that never gets a turn
/// or a stack switch on every pass that buys nothing.
#[test]
fn the_published_weak_count_follows_the_list() {
    let mut runtime = held(0);
    assert_eq!(weak_waiting_here(), 0);
    runtime.downgrade_strong_executor();
    assert_eq!(weak_waiting_here(), 1);
    runtime.downgrade_strong_executor();
    assert_eq!(weak_waiting_here(), 2);

    // And a pass that finds them all dead publishes zero, which is what stops
    // the turn from being handed out for nothing.
    for slot in runtime.weak_executors.iter() {
        slot.as_ref().unwrap().force_replace_executor();
    }
    runtime.reclaim_finished_weaks();
    assert_eq!(runtime.weak_executor_num(), 0);
    assert_eq!(weak_waiting_here(), 0);
}
