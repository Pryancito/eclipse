//! Everything a `Thread` is besides its scheduling slices: the state it
//! reports while suspended, the signals each transition publishes, the
//! generic blocked marker, its affinity, its time counters and its flags.

use super::*;
use crate::object::KernelObject;
use crate::task::*;

fn a_thread() -> Arc<Thread> {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("process");
    Thread::create(&proc, "thread").expect("thread")
}

/// Suspension does not overwrite what the thread was actually doing.
///
/// The comment on the gate says the order is `Dying > Exception > Suspend >
/// Blocked`: a thread stopped in an exception reports the exception even
/// while a suspend token is held over it, because that is what the debugger
/// on the other end of the exception channel needs to see. Only a thread
/// with nothing more specific to say reports `Suspended`.
#[test]
fn a_suspended_thread_still_reports_what_stopped_it() {
    let t = a_thread();
    {
        let mut inner = t.lock_inner();
        inner.suspend_count = 1;
        inner.state = ThreadState::BlockedException;
    }
    assert_eq!(
        t.state(),
        ThreadState::BlockedException,
        "a suspend hid the exception the thread is stopped in"
    );

    t.lock_inner().state = ThreadState::BlockedChannel;
    assert_eq!(
        t.state(),
        ThreadState::Suspended,
        "a plain blocked thread under a suspend reports the suspend"
    );

    t.lock_inner().state = ThreadState::Dying;
    assert_eq!(
        t.state(),
        ThreadState::Dying,
        "a dying thread outranks the suspend over it"
    );

    t.lock_inner().suspend_count = 0;
    assert_eq!(t.state(), ThreadState::Dying);
}

/// A thread with no context saved is not suspended, whatever the count
/// says: there is nothing parked to hold.
#[test]
fn a_thread_with_no_context_is_never_reported_suspended() {
    let t = a_thread();
    let mut inner = t.lock_inner();
    inner.suspend_count = 3;
    inner.state = ThreadState::Running;
    inner.context = None;
    assert_eq!(inner.state(), ThreadState::Running);
}

/// A suspend that lands while the thread is out in user mode shows when
/// the context comes back, which is the moment the thread really stops:
/// `put_context` flips the signals then. Its shortcut for the plain
/// running case must not swallow that one.
#[test]
fn a_suspend_taken_while_running_shows_when_the_context_comes_back() {
    let t = CurrentThread(a_thread());
    let cx = t
        .lock_inner()
        .context
        .take()
        .expect("a new thread has its context");
    t.lock_inner().change_state(ThreadState::Running, &t.base);
    t.suspend();
    assert!(
        t.signal().contains(Signal::THREAD_RUNNING),
        "still in user mode: nothing has stopped yet"
    );

    t.put_context(cx);

    assert!(
        t.signal().contains(Signal::THREAD_SUSPENDED),
        "the thread stopped and its waiters were never told"
    );
    assert!(!t.signal().contains(Signal::THREAD_RUNNING));
}

/// A dying thread is neither running nor suspended, and its signals say
/// so: a debugger waiting for THREAD_SUSPENDED must not take a thread
/// being killed for one it may inspect.
#[test]
fn a_dying_thread_drops_the_running_and_suspended_signals() {
    let t = a_thread();
    t.suspend();
    assert!(t.signal().contains(Signal::THREAD_SUSPENDED));

    t.kill();

    assert_eq!(t.state(), ThreadState::Dying);
    assert!(
        !t.signal().intersects(
            Signal::THREAD_RUNNING | Signal::THREAD_SUSPENDED | Signal::THREAD_TERMINATED
        ),
        "{:?}",
        t.signal()
    );
}

/// Only a thread stopped in an exception has a report to hand out, which
/// is what `ZX_INFO_THREAD_EXCEPTION_REPORT` reads.
#[test]
fn only_a_thread_stopped_in_an_exception_has_a_report_to_give() {
    let t = a_thread();
    t.lock_inner().exception = Some(Exception::new(&t, ExceptionType::ThreadStarting, None));
    assert_eq!(
        t.get_thread_exception_info().err(),
        Some(ZxError::BAD_STATE),
        "the thread is not stopped in it"
    );

    t.lock_inner().state = ThreadState::BlockedException;
    assert!(t.get_thread_exception_info().is_ok());
}

/// The guards around `ext` read intact on a thread nothing has written
/// over. A report that called every thread smashed would bury the one
/// that is.
#[test]
fn a_fresh_thread_reports_both_ext_guards_intact() {
    assert_eq!(a_thread().ext_canaries(), (true, true));
}

/// `wait_for_run` takes the context away, and the only thing that makes the
/// thread runnable again is a `put_context` handing it back.
///
/// Every round of `run_user` that enters user mode does that. The Linux
/// job-control stop is the one path that goes round the loop WITHOUT
/// entering user mode (SIGTSTP/SIGTTIN/SIGTTOU park the thread and
/// `continue`), and it must park the context all the same: without it the
/// registers are dropped on the floor and the next `wait_for_run` finds
/// nothing to hand out.
#[test]
fn a_context_parked_without_running_is_handed_out_again() {
    let t = a_thread();
    {
        let mut inner = t.lock_inner();
        inner.context = Some(Box::new(UserContext::default()));
        inner.state = ThreadState::Running;
    }
    let current = CurrentThread(t.clone());

    let ctx = current
        .wait_for_run()
        .now_or_never()
        .expect("a runnable thread hands out its context at once");
    assert!(
        t.lock_inner().context.is_none(),
        "the context is away while the thread runs"
    );

    // What the job-control stop does: hand it straight back, never having
    // entered user mode.
    current.put_context(ctx);
    assert!(t.lock_inner().context.is_some());

    let ctx = current
        .wait_for_run()
        .now_or_never()
        .expect("a parked context is handed out again, not lost");
    current.put_context(ctx);
}

/// Each transition publishes the signal set `zx_object_wait_one` is
/// waiting on, and clears the ones that no longer hold. Leaving a stale
/// bit behind is a waiter that wakes on a thread state that is over.
#[test]
fn every_transition_leaves_exactly_the_signals_that_still_hold() {
    let t = a_thread();

    t.lock_inner().change_state(ThreadState::Running, &t.base);
    let s = t.base.signal();
    assert!(s.contains(Signal::THREAD_RUNNING));
    assert!(!s.contains(Signal::THREAD_SUSPENDED));
    assert!(!s.contains(Signal::THREAD_TERMINATED));

    t.suspend();
    let s = t.base.signal();
    assert!(
        s.contains(Signal::THREAD_SUSPENDED),
        "a suspended thread has to publish THREAD_SUSPENDED"
    );
    assert!(
        !s.contains(Signal::THREAD_RUNNING),
        "and stop publishing THREAD_RUNNING"
    );

    t.resume();
    let s = t.base.signal();
    assert!(s.contains(Signal::THREAD_RUNNING));
    assert!(
        !s.contains(Signal::THREAD_SUSPENDED),
        "a resumed thread still looked suspended to anyone waiting"
    );

    t.lock_inner().change_state(ThreadState::Dead, &t.base);
    let s = t.base.signal();
    assert!(s.contains(Signal::THREAD_TERMINATED));
    assert!(!s.contains(Signal::THREAD_RUNNING));
    assert!(!s.contains(Signal::THREAD_SUSPENDED));
}

/// `New` and `Dying` publish nothing at all — and that includes taking
/// `THREAD_TERMINATED` back down. A thread that reached `Dead` and is then
/// moved to `Dying` (a kill racing teardown) would otherwise stay
/// terminated for every waiter while it is still being taken apart.
#[test]
fn a_dying_thread_publishes_none_of_the_three_signals() {
    let t = a_thread();
    t.lock_inner().change_state(ThreadState::Dead, &t.base);
    assert!(t.base.signal().contains(Signal::THREAD_TERMINATED));

    t.lock_inner().change_state(ThreadState::Dying, &t.base);
    let s = t.base.signal();
    assert!(
        !s.contains(Signal::THREAD_TERMINATED),
        "a dying thread was still publishing THREAD_TERMINATED"
    );
    assert!(!s.contains(Signal::THREAD_RUNNING));
    assert!(!s.contains(Signal::THREAD_SUSPENDED));
}

/// Only a `Running` thread becomes `Blocked`, and only the generic
/// `Blocked` goes back to `Running`.
///
/// This is the marker `/proc/<pid>/status` reads. It defers to everything
/// more specific on purpose: `blocking_run` records *why* a thread waits
/// and asserts on return that nobody moved the state under it, so
/// overwriting `BlockedFutex` both loses that reason and trips the
/// assertion. Teardown wins for the same reason — an unblock racing a kill
/// must not resurrect a dying thread into `Running`.
#[test]
fn the_generic_blocked_marker_defers_to_every_more_specific_state() {
    let t = a_thread();
    t.lock_inner().state = ThreadState::Running;
    assert!(t.set_blocked(true), "a running thread blocks");
    assert_eq!(t.lock_inner().state, ThreadState::Blocked);

    assert!(
        !t.set_blocked(true),
        "blocking an already blocked thread changes nothing"
    );

    assert!(t.set_blocked(false), "and the generic block comes back");
    assert_eq!(t.lock_inner().state, ThreadState::Running);

    for specific in [
        ThreadState::BlockedFutex,
        ThreadState::BlockedChannel,
        ThreadState::BlockedPort,
        ThreadState::Dying,
        ThreadState::Dead,
        ThreadState::New,
        ThreadState::Suspended,
    ] {
        t.lock_inner().state = specific;
        assert!(
            !t.set_blocked(true),
            "{:?} was overwritten by the generic blocked marker",
            specific
        );
        assert!(
            !t.set_blocked(false),
            "{:?} was resurrected into Running by an unblock",
            specific
        );
        assert_eq!(t.lock_inner().state, specific, "{:?} moved", specific);
    }
}

/// An all-zero affinity mask would make the thread unschedulable, so it is
/// refused; every other mask is stored as given.
#[test]
fn a_thread_may_not_be_pinned_to_no_cpu_at_all() {
    let t = a_thread();
    let before = t.affinity();

    assert_eq!(t.set_affinity(0), Err(ZxError::INVALID_ARGS));
    assert_eq!(t.affinity(), before, "the refused mask was stored anyway");

    assert!(t.set_affinity(0b1010).is_ok());
    assert_eq!(t.affinity(), 0b1010);
    assert!(t.set_affinity(u64::MAX).is_ok());
    assert_eq!(t.affinity(), u64::MAX);
    assert!(t.set_affinity(1).is_ok());
    assert_eq!(t.affinity(), 1);
}

/// `ZX_INFO_THREAD` publishes the mask in the FIRST word of the eight it
/// carries, which is where `zx_object_get_info` and `sched_getaffinity`
/// read CPUs 0..63. Putting it anywhere else reports a thread pinned to no
/// CPU at all.
#[test]
fn the_affinity_mask_is_published_in_the_first_word_of_the_info() {
    let t = a_thread();
    t.set_affinity(0b1101).unwrap();
    let info = t.get_thread_info();
    assert_eq!(info.cpu_affinity_mask[0], 0b1101);
    assert!(
        info.cpu_affinity_mask[1..].iter().all(|w| *w == 0),
        "the mask leaked into a word nobody reads"
    );
    assert_eq!(info.state, ThreadState::New as u32);
    assert_eq!(
        info.wait_exception_channel_type, 0,
        "a thread in no exception waits on no channel"
    );
}

/// The two time counters are separate: user time and kernel time, each
/// accumulating rather than replacing. `/proc/<pid>/stat` fields 14 and 15
/// are these two, and `times(2)` adds them up.
#[test]
fn the_two_clocks_of_a_thread_accumulate_and_do_not_mix() {
    let t = a_thread();
    assert_eq!((t.get_time(), t.get_sys_time()), (0, 0));

    t.time_add(700);
    t.time_add(300);
    assert_eq!(t.get_time(), 1000, "user time replaced instead of adding");
    assert_eq!(t.get_sys_time(), 0, "user time landed in the kernel clock");

    t.sys_time_add(40);
    t.sys_time_add(2);
    assert_eq!(
        t.get_sys_time(),
        42,
        "kernel time replaced instead of adding"
    );
    assert_eq!(t.get_time(), 1000, "kernel time landed in the user clock");
}

/// Runtime info reports the CPU time twice, as cpu and queue time, because
/// zCore does not measure run-queue latency separately yet. Reporting zero
/// for the second is a thread that looks like it never waited.
#[test]
fn runtime_info_carries_the_cpu_time_in_both_of_the_fields_it_fills() {
    let t = a_thread();
    t.time_add(5_000);
    let info = t.get_runtime_info();
    assert_eq!(info.cpu_time, 5_000);
    assert_eq!(info.queue_time, 5_000);
    assert_eq!(info.page_fault_time, 0);
    assert_eq!(info.lock_contention_time, 0);
}

/// `ZX_INFO_TASK_RUNTIME` on a Process sums its live threads (not
/// `WRONG_TYPE`).
#[test]
fn process_runtime_info_sums_its_threads() {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("process");
    let t1 = Thread::create(&proc, "t1").expect("thread");
    let t2 = Thread::create(&proc, "t2").expect("thread");
    t1.time_add(1_000);
    t2.time_add(2_000);
    let info = proc.get_runtime_info();
    assert_eq!(info.cpu_time, 3_000);
    assert_eq!(info.queue_time, 3_000);
}

/// A thread that exits hands both of its clocks to its process before it
/// leaves the thread list, so `getrusage`, `times` and the parent's
/// `wait4` still see the time it burned.
#[test]
fn a_dying_thread_hands_both_its_clocks_to_its_process() {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("process");
    let t = Thread::create(&proc, "thread").expect("thread");
    t.time_add(9_000);
    t.sys_time_add(1_500);

    assert_eq!(
        (proc.dead_threads_time(), proc.dead_threads_sys_time()),
        (0, 0)
    );
    t.terminate_abandoned();

    assert_eq!(
        proc.dead_threads_time(),
        9_000,
        "the thread's user time died with it"
    );
    assert_eq!(
        proc.dead_threads_sys_time(),
        1_500,
        "the thread's kernel time died with it"
    );
}

/// Only the thread a process was started with is its first thread; every
/// later one is not. `Process::exit` and the initial-thread rules hang off
/// this.
#[test]
fn only_the_thread_a_process_started_with_is_its_first() {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("process");
    let first = Thread::create(&proc, "first").expect("thread");
    let second = Thread::create(&proc, "second").expect("thread");

    assert!(!first.is_first_thread(), "no thread is first before start");
    first.set_first_thread();
    assert!(first.is_first_thread());
    assert!(
        !second.is_first_thread(),
        "a later thread came out as the first one"
    );
}

/// The flags a thread carries are its own, and `update_flags` is the only
/// way they move.
#[test]
fn a_threads_flags_are_what_was_last_written_into_them() {
    let t = a_thread();
    assert_eq!(t.flags(), ThreadFlag::empty());

    t.update_flags(|f| f.insert(ThreadFlag::VCPU));
    assert_eq!(
        t.flags(),
        ThreadFlag::VCPU,
        "the flag went in but the reader does not see it"
    );

    t.update_flags(|f| f.remove(ThreadFlag::VCPU));
    assert_eq!(t.flags(), ThreadFlag::empty());
}
