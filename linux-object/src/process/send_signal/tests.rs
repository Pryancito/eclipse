//! A signal sent to a process, queued as Linux queues it: each `sigqueue` of
//! a real-time signal an entry of its own, counted against the receiving
//! user, refused past its `RLIMIT_SIGPENDING`.
//!
//! Fixed pids live in 51_1xx; the per-user count is one table for the
//! machine, so each test's process has a uid of its own, the same number.

use super::*;
use crate::signal::queue::queued_for_user;
use crate::signal::{SignalCode, SIG_IGN};
use crate::thread::ThreadExt;
use core::convert::TryInto;
use rcore_fs_ramfs::RamFS;

const RT: LinuxSignal = LinuxSignal::SIGRT34;

/// A process of real user `pid`, with one thread that blocks `RT`.
fn a_process(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "q", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let uid = pid as u32;
    proc.linux().set_resuid(uid, uid, uid).unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    let mut rt = Sigset::empty();
    rt.insert(RT);
    thread.lock_linux().block_signals(&rt);
    (proc, thread)
}

fn sigqueue(proc: &Process, signal: LinuxSignal, value: usize) -> LxResult<()> {
    let info = SigInfo::queued(signal, 3, 0, value);
    send_signal_to_process_with_info(proc.id() as usize, signal, Some(info))
}

/// The `si_value` of a `sigqueue`d info.
fn value(info: &SigInfo) -> usize {
    usize::from_ne_bytes(info.as_bytes()[24..32].try_into().unwrap())
}

/// Take `signal` on `thread` the way `sigtimedwait` does: what waits on the
/// process first comes onto the thread.
fn take(proc: &Process, thread: &Arc<Thread>, signal: LinuxSignal) -> Option<SigInfo> {
    let mut lt = thread.lock_linux();
    lt.sigwait.insert(signal);
    proc.linux().pull_shared_pending_locked(&mut lt);
    lt.sigwait = Sigset::empty();
    lt.signals.contains(signal).then(|| lt.take_siginfo(signal))
}

#[test]
fn three_sigqueues_of_a_blocked_signal_wait_on_the_process_and_come_out_in_order() {
    let (proc, thread) = a_process(51_101);
    for v in 1..=3 {
        sigqueue(&proc, RT, v).unwrap();
    }
    assert!(proc.linux().shared_pending().contains(RT));
    assert_eq!(queued_for_user(51_101), 3);
    for v in 1..=3 {
        assert_eq!(take(&proc, &thread, RT).map(|i| value(&i)), Some(v));
    }
    assert!(take(&proc, &thread, RT).is_none(), "a fourth");
    assert_eq!(queued_for_user(51_101), 0);
}

#[test]
fn past_the_limit_a_sigqueue_is_eagain_and_a_kill_still_succeeds() {
    let (proc, thread) = a_process(51_102);
    let two = RLimit { cur: 2, max: 2 };
    proc.linux()
        .rlimit(RLIMIT_SIGPENDING, Some(two), false)
        .unwrap();
    sigqueue(&proc, RT, 1).unwrap();
    sigqueue(&proc, RT, 2).unwrap();
    assert_eq!(sigqueue(&proc, RT, 3), Err(LxError::EAGAIN));
    let kill = SigInfo::from_user(RT, 3, 0, SignalCode::USER);
    assert_eq!(
        send_signal_to_process_with_info(proc.id() as usize, RT, Some(kill)),
        Ok(())
    );
    assert_eq!(queued_for_user(51_102), 2);
    assert_eq!(take(&proc, &thread, RT).map(|i| value(&i)), Some(1));
    assert_eq!(take(&proc, &thread, RT).map(|i| value(&i)), Some(2));
    assert!(take(&proc, &thread, RT).is_none(), "the kill folded in");
    // On a thread that has it unblocked, the same.
    thread.lock_linux().set_signal_mask(Sigset::empty());
    sigqueue(&proc, RT, 4).unwrap();
    sigqueue(&proc, RT, 5).unwrap();
    assert_eq!(sigqueue(&proc, RT, 6), Err(LxError::EAGAIN));
    assert_eq!(thread.lock_linux().queued_entries_of(RT), 2);
}

#[test]
fn a_timer_s_expiry_is_never_refused() {
    let (proc, thread) = a_process(51_103);
    let none = RLimit { cur: 0, max: 0 };
    proc.linux()
        .rlimit(RLIMIT_SIGPENDING, Some(none), false)
        .unwrap();
    let info = SigInfo::timer(RT, 9, 0, 0);
    send_timer_signal_to_process(proc.id() as usize, RT, info).unwrap();
    assert_eq!(
        queued_for_user(51_103),
        0,
        "counted when the timer was made"
    );
    assert_eq!(take(&proc, &thread, RT).and_then(|i| i.timer_id()), Some(9));
}

#[test]
fn the_thread_s_own_entries_come_before_the_process_s() {
    let (proc, thread) = a_process(51_104);
    sigqueue(&proc, RT, 1).unwrap();
    let to = proc.linux().signal_receiver();
    let own = SigInfo::queued(RT, 3, 0, 2);
    thread
        .lock_linux()
        .send_signal(RT, Some(own), Some(to))
        .unwrap();
    assert_eq!(take(&proc, &thread, RT).map(|i| value(&i)), Some(2));
    assert_eq!(take(&proc, &thread, RT).map(|i| value(&i)), Some(1));
}

#[test]
fn ignoring_a_signal_throws_every_entry_away_and_gives_it_back() {
    let (proc, thread) = a_process(51_105);
    for v in 1..=3 {
        sigqueue(&proc, RT, v).unwrap();
    }
    let to = proc.linux().signal_receiver();
    let own = SigInfo::queued(RT, 3, 0, 4);
    thread
        .lock_linux()
        .send_signal(RT, Some(own), Some(to))
        .unwrap();
    let ignore = SignalAction {
        handler: SIG_IGN,
        ..SignalAction::default()
    };
    set_signal_action_in(&proc, RT, ignore);
    assert!(!proc.linux().shared_pending().contains(RT));
    assert!(!thread.lock_linux().signals.contains(RT));
    assert_eq!(queued_for_user(51_105), 0);
    assert_eq!(thread.lock_linux().queued_entries_of(RT), 0);
}

#[test]
fn the_default_limit_is_half_of_linux_s_max_threads() {
    const GIB: usize = 1 << 30;
    assert_eq!(default_sigpending_limit(16 * GIB), 65_536);
    assert_eq!(default_sigpending_limit(GIB), 4_096);
    assert_eq!(default_sigpending_limit(0), 10, "MIN_THREADS / 2");
    assert_eq!(default_sigpending_limit(usize::MAX), 0x3fff_ffff / 2);
    let (proc, _) = a_process(51_106);
    let limit = proc.linux().rlimit(RLIMIT_SIGPENDING, None, false).unwrap();
    let want = default_sigpending_limit(kernel_hal::mem::memory_usage().1);
    assert_eq!(
        limit,
        RLimit {
            cur: want,
            max: want
        }
    );
}

#[test]
fn a_dying_process_gives_back_what_waited_on_it() {
    // `create_linux`, for the exit callbacks a real process has.
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 51_107).unwrap();
    proc.linux().set_resuid(51_107, 51_107, 51_107).unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    let mut rt = Sigset::empty();
    rt.insert(RT);
    thread.lock_linux().block_signals(&rt);
    sigqueue(&proc, RT, 1).unwrap();
    assert_eq!(queued_for_user(51_107), 1);
    proc.exit(0);
    assert_eq!(queued_for_user(51_107), 0);
    // A thread's own, when its run loop ends (`loader`): see `flush_pending`.
    let to = proc.linux().signal_receiver();
    thread.lock_linux().send_signal(RT, None, Some(to)).unwrap();
    assert_eq!(queued_for_user(51_107), 1);
    thread.lock_linux().flush_pending();
    assert!(!thread.lock_linux().signals.contains(RT));
    assert_eq!(queued_for_user(51_107), 0);
}
