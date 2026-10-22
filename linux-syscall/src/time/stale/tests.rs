//! A timer's signal still waiting when the timer is deleted, set again or
//! done away with by an `execve` is thrown away, and a signal of the same
//! number from anybody else is not.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::sync::Arc;
use linux_object::process::{send_signal_to_process_with_info, LinuxProcess};
use linux_object::signal::Sigset;
use rcore_fs_ramfs::RamFS;

const CLOCK_MONOTONIC: usize = 1;
const CLOCK_PROCESS_CPUTIME_ID: usize = 2;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A timer of the caller's on `clock` that sends `SIGALRM` to the process,
/// with `SIGALRM` blocked so that its signal waits, set to go off in an
/// hour: the test brings its expiries itself.
fn a_timer(sys: &Syscall<'_>, clock: usize) -> usize {
    let mut id = [0i32];
    sys.sys_timer_create(clock, 0, id.as_mut_ptr() as usize)
        .unwrap();
    let mut blocked = Sigset::empty();
    blocked.insert(Signal::SIGALRM);
    sys.thread.lock_linux().block_signals(&blocked);
    settime(sys, id[0] as usize, 3_600_000).unwrap();
    id[0] as usize
}

/// `timer_settime` of `id` to go off once in `ms` milliseconds; 0 disarms.
fn settime(sys: &Syscall<'_>, id: usize, ms: u64) -> SysResult {
    let spec = ITimerSpec {
        interval: TimeSpec::from_duration(Duration::ZERO),
        value: TimeSpec::from_duration(Duration::from_millis(ms)),
    };
    sys.sys_timer_settime(
        id,
        0,
        (&spec as *const ITimerSpec as usize).into(),
        0.into(),
    )
}

/// The wheel getting to timer `id`, at the generation it has now.
fn expires(id: usize) {
    let gen = POSIX_TIMERS.lock()[&id].generation;
    expire_posix_timer(id, gen, kernel_hal::timer::timer_now());
}

/// Whether a `SIGALRM` waits for the caller's process, on it or on the
/// caller's thread.
fn waiting(sys: &Syscall<'_>) -> bool {
    sys.thread.lock_linux().signals.contains(Signal::SIGALRM)
        || sys
            .linux_process()
            .shared_pending()
            .contains(Signal::SIGALRM)
}

#[test]
fn timer_delete_drops_the_signal_it_left_waiting() {
    let proc = a_process(46_721);
    let (before, deleted, after) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let id = a_timer(sys, CLOCK_MONOTONIC);
            expires(id);
            let before = waiting(sys);
            let deleted = sys.sys_timer_delete(id);
            (before, deleted, waiting(sys))
        })
    });
    assert!(before, "the test needs the signal waiting");
    assert_eq!(deleted, Ok(0));
    assert!(!after, "timer_delete left its signal to be taken");
}

#[test]
fn timer_settime_drops_the_signal_from_before_and_not_the_next() {
    let proc = a_process(46_722);
    let (set_again, disarmed, next) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let id = a_timer(sys, CLOCK_MONOTONIC);
            expires(id);
            settime(sys, id, 3_600_000).unwrap();
            let set_again = waiting(sys);
            expires(id);
            settime(sys, id, 0).unwrap();
            let disarmed = waiting(sys);
            settime(sys, id, 3_600_000).unwrap();
            expires(id);
            let next = waiting(sys);
            sys.sys_timer_delete(id).unwrap();
            (set_again, disarmed, next)
        })
    });
    assert!(!set_again, "set again, and the signal from before waits on");
    assert!(!disarmed, "disarmed, and the signal from before waits on");
    assert!(next, "the expiry after the setting sent nothing");
}

#[test]
fn a_cpu_clock_timer_set_again_drops_its_signal_too() {
    let proc = a_process(46_723);
    let (fired, after) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let id = a_timer(sys, CLOCK_PROCESS_CPUTIME_ID);
            settime(sys, id, 50).unwrap();
            sys.thread.time_add(60_000_000);
            run_cpu_timers(sys.thread);
            let fired = waiting(sys);
            settime(sys, id, 3_600_000).unwrap();
            let after = waiting(sys);
            sys.sys_timer_delete(id).unwrap();
            (fired, after)
        })
    });
    assert!(fired, "the test needs the CPU timer to have gone off");
    assert!(!after, "set again, and the signal from before waits on");
}

#[test]
fn a_signal_of_the_same_number_from_anybody_else_stays() {
    let proc = a_process(46_724);
    let after = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let id = a_timer(sys, CLOCK_MONOTONIC);
            // An alarm(2), already waiting when the timer would go off.
            let alarm = SigInfo::from_kernel(Signal::SIGALRM);
            let pid = sys.zircon_process().id() as usize;
            send_signal_to_process_with_info(pid, Signal::SIGALRM, Some(alarm)).unwrap();
            expires(id);
            sys.sys_timer_delete(id).unwrap();
            waiting(sys)
        })
    });
    assert!(after, "deleting the timer threw away the alarm");
}

#[test]
fn timer_delete_of_another_process_s_timer_is_einval_and_leaves_it() {
    let proc = a_process(46_726);
    // Process 46_727's, which this test never makes: only its number counts.
    let theirs = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    POSIX_TIMERS.lock().insert(
        theirs,
        PosixTimer {
            owner: 46_727,
            notify: TimerNotify::SIGALRM_TO_PROCESS,
            clock: PosixTimerClock::Base(ClockBase::Monotonic),
            interval: Duration::ZERO,
            next: Duration::ZERO,
            generation: 0,
            overrun: Overrun::default(),
        },
    );
    let deleted = on_a_thread(&proc, move |sys| {
        Box::pin(async move { sys.sys_timer_delete(theirs) })
    });
    let left = POSIX_TIMERS.lock().remove(&theirs).is_some();
    assert_eq!(deleted, Err(LxError::EINVAL));
    assert!(left, "one process deleted another's timer");
}

#[test]
fn an_execve_drops_the_signals_of_the_timers_it_deletes() {
    let proc = a_process(46_725);
    let (before, after, left) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let id = a_timer(sys, CLOCK_MONOTONIC);
            expires(id);
            let before = waiting(sys);
            let owner = sys.zircon_process().id();
            delete_posix_timers_at_exec(owner);
            let left = POSIX_TIMERS.lock().values().any(|t| t.owner == owner);
            (before, waiting(sys), left)
        })
    });
    assert!(before, "the test needs the signal waiting");
    assert!(!after, "the new program got the deleted timer's signal");
    assert!(!left, "a timer outlived the execve");
}
