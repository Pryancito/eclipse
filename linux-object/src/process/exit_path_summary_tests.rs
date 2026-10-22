//! The lock-free mirror the return to user mode reads: that it is never
//! wrong, and that the fast path it buys never swallows a signal.
//!
//! [`ExitPathSummary`] trades a lock for an invariant -- every mutator
//! re-publishes both words before releasing the guard -- and an invariant
//! that nothing checks is a comment. So each test here mutates through a
//! **production** path and then compares the mirror against the field it
//! mirrors, read under the lock. A mutator that forgets the refresh leaves
//! the fast path answering "nothing pending" while a signal sits in the set,
//! which is a signal that never arrives: the failure these tests exist for.
extern crate std;

use super::*;
use crate::signal::{SignalAction, SignalActionFlags};
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn usr1() -> Sigset {
    Sigset::new(1 << (LinuxSignal::SIGUSR1 as u64 - 1))
}

/// A process with one thread that blocks SIGUSR1, so a process-directed
/// SIGUSR1 lands on `shared_pending` instead of on the thread.
fn a_process_blocking_usr1(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "mirror",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    thread.lock_linux().set_signal_mask(usr1());
    (proc, thread)
}

/// What the mirror says, without taking the lock.
fn mirror(lp: &LinuxProcess) -> (u64, bool) {
    (
        lp.exit_path.shared_pending_bits.load(Ordering::Acquire),
        lp.is_job_stopped(),
    )
}

/// What is true, read under the lock.
fn truth(lp: &LinuxProcess) -> (u64, bool) {
    let inner = lp.inner.lock();
    (inner.shared_pending.val(), inner.job_stopped)
}

/// The whole invariant in one assertion, named so a failure says which word.
fn assert_mirror_agrees(lp: &LinuxProcess, after: &str) {
    let (m_sig, m_stop) = mirror(lp);
    let (t_sig, t_stop) = truth(lp);
    assert_eq!(
        m_sig, t_sig,
        "after {after}: the shared-pending mirror says {m_sig:#x} and the field says {t_sig:#x} \
         -- a syscall return would read the mirror and skip the signal"
    );
    assert_eq!(
        m_stop, t_stop,
        "after {after}: the job-stop mirror says {m_stop} and the field says {t_stop}"
    );
}

#[test]
fn a_new_process_starts_with_both_answers_clear() {
    let (proc, _t) = a_process_blocking_usr1(78_001);
    assert_eq!(mirror(proc.linux()), (0, false));
    assert_mirror_agrees(proc.linux(), "creation");
}

#[test]
fn a_forked_child_starts_with_a_clean_mirror() {
    let (parent, _t) = a_process_blocking_usr1(78_002);
    // The parent has a signal waiting; the child must not inherit the bit
    // (`forked_child` clears the set) and so must not inherit the mirror.
    parent
        .linux()
        .queue_shared_signal(LinuxSignal::SIGUSR1, None);
    let child = Process::fork_from(&parent).unwrap();
    assert_eq!(
        mirror(child.linux()),
        (0, false),
        "the child's mirror came from somewhere other than its own fields"
    );
    assert_mirror_agrees(child.linux(), "fork");
    assert_mirror_agrees(parent.linux(), "fork (parent)");
}

#[test]
fn a_queued_shared_signal_is_visible_without_the_lock() {
    let (proc, _t) = a_process_blocking_usr1(78_003);
    let lp = proc.linux();
    lp.queue_shared_signal(LinuxSignal::SIGUSR1, None);
    assert_ne!(
        mirror(lp).0,
        0,
        "the fast path would answer `nothing pending` with SIGUSR1 on the set"
    );
    assert_mirror_agrees(lp, "queue_shared_signal");
}

/// The behavioural test, and the one that has to hold whatever the mirror
/// does: a signal queued while the thread blocked it is delivered once the
/// thread stops blocking it. This is what the fast path can break.
#[test]
fn the_fast_path_does_not_swallow_a_pending_signal() {
    let (proc, thread) = a_process_blocking_usr1(78_004);
    let lp = proc.linux();
    lp.queue_shared_signal(LinuxSignal::SIGUSR1, None);
    thread.lock_linux().set_signal_mask(Sigset::empty());
    lp.pull_shared_pending_into(&thread);
    assert!(
        thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the signal was on shared_pending and the thread no longer blocks it"
    );
    assert_mirror_agrees(lp, "the pull that delivered it");
}

#[test]
fn pulling_the_last_shared_signal_clears_the_mirror() {
    let (proc, thread) = a_process_blocking_usr1(78_005);
    let lp = proc.linux();
    lp.queue_shared_signal(LinuxSignal::SIGUSR1, None);
    thread.lock_linux().set_signal_mask(Sigset::empty());
    lp.pull_shared_pending_into(&thread);
    assert_eq!(
        mirror(lp).0,
        0,
        "nothing is pending any more, so every later return must take no lock"
    );
    assert_mirror_agrees(lp, "pull_shared_pending_into");
}

/// A pull that delivers only *some* of the bits has to leave the rest
/// visible: the mirror is not a boolean.
#[test]
fn a_pull_that_leaves_a_signal_behind_leaves_the_mirror_set() {
    let (proc, thread) = a_process_blocking_usr1(78_006);
    let lp = proc.linux();
    lp.queue_shared_signal(LinuxSignal::SIGUSR1, None);
    lp.queue_shared_signal(LinuxSignal::SIGUSR2, None);
    // Unblock only USR1, so USR2 stays on the shared set.
    thread
        .lock_linux()
        .set_signal_mask(Sigset::new(1 << (LinuxSignal::SIGUSR2 as u64 - 1)));
    lp.pull_shared_pending_into(&thread);
    assert!(
        thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "USR1 was unblocked and should have been pulled"
    );
    assert_ne!(
        mirror(lp).0,
        0,
        "USR2 is still on the shared set and a later return has to look"
    );
    assert_mirror_agrees(lp, "a partial pull");
}

#[test]
fn a_shared_signal_discarded_by_sig_ign_clears_the_mirror() {
    let (proc, _t) = a_process_blocking_usr1(78_007);
    let lp = proc.linux();
    lp.queue_shared_signal(LinuxSignal::SIGUSR1, None);
    // `SIG_IGN` discards what is already pending (this is the path
    // `set_signal_action_in` exists for).
    set_signal_action_in(
        &proc,
        LinuxSignal::SIGUSR1,
        SignalAction {
            handler: 1, // SIG_IGN
            flags: SignalActionFlags::empty(),
            mask: Sigset::empty(),
            restorer: 0,
        },
    );
    assert_eq!(
        mirror(lp).0,
        0,
        "the bit was discarded, so the fast path must stop taking the lock"
    );
    assert_mirror_agrees(lp, "set_signal_action_in(SIG_IGN)");
}

#[test]
fn a_stop_a_continue_throws_away_on_the_process_clears_the_mirror() {
    let (proc, _t) = a_process_blocking_usr1(78_013);
    let lp = proc.linux();
    lp.queue_shared_signal(LinuxSignal::SIGTSTP, None);
    prepare_signal(&proc, LinuxSignal::SIGCONT);
    assert_eq!(
        mirror(lp).0,
        0,
        "the stop was thrown away, so the fast path must stop taking the lock"
    );
    assert_mirror_agrees(lp, "prepare_signal(SIGCONT)");
}

#[test]
fn a_job_stop_is_visible_without_the_lock_and_a_continue_clears_it() {
    let (proc, _t) = a_process_blocking_usr1(78_008);
    let lp = proc.linux();
    lp.job_stop(&proc, LinuxSignal::SIGTSTP as u8);
    assert!(
        mirror(lp).1,
        "a stopped process must not be re-entered, and this is what says so"
    );
    assert_mirror_agrees(lp, "job_stop");
    assert!(lp.job_continue(&proc), "the process was stopped");
    assert!(!mirror(lp).1, "SIGCONT leaves no stop behind");
    assert_mirror_agrees(lp, "job_continue");
}

#[test]
fn a_stop_broken_to_kill_the_process_clears_it_too() {
    let (proc, _t) = a_process_blocking_usr1(78_009);
    let lp = proc.linux();
    lp.job_stop(&proc, LinuxSignal::SIGTSTP as u8);
    lp.job_wake_to_die(&proc);
    assert!(
        !mirror(lp).1,
        "the thread has to run the default SIGKILL action, which it cannot do while parked"
    );
    assert_mirror_agrees(lp, "job_wake_to_die");
}

/// A second stop of an already-stopped process returns early, before the
/// refresh. That is correct only because the word is already published; the
/// assertion keeps it that way.
#[test]
fn stopping_an_already_stopped_process_leaves_the_mirror_set() {
    let (proc, _t) = a_process_blocking_usr1(78_010);
    let lp = proc.linux();
    lp.job_stop(&proc, LinuxSignal::SIGTSTP as u8);
    lp.job_stop(&proc, LinuxSignal::SIGTSTP as u8);
    assert!(mirror(lp).1, "the second stop unpublished the first");
    assert_mirror_agrees(lp, "a second job_stop");
}

/// The two words are refreshed together, from one source of truth, so a
/// mutator of one cannot leave the other stale. Mutating both and checking
/// both is what says so.
#[test]
fn a_signal_and_a_stop_at_once_both_come_out_right() {
    let (proc, _t) = a_process_blocking_usr1(78_011);
    let lp = proc.linux();
    lp.queue_shared_signal(LinuxSignal::SIGUSR1, None);
    lp.job_stop(&proc, LinuxSignal::SIGTSTP as u8);
    let (sig, stop) = mirror(lp);
    assert_ne!(sig, 0, "the stop refresh overwrote the signal word");
    assert!(stop, "the signal refresh overwrote the stop word");
    assert_mirror_agrees(lp, "a signal and a stop");
}
