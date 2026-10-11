//! What changing a disposition does beyond the table: an exec, and a
//! new disposition that ignores.

extern crate std;

use super::*;
use crate::signal::{SignalAction, SignalActionFlags, SignalCode, SIG_DFL, SIG_IGN};
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

const CAUGHT: usize = 0x4000_1000;

fn entry(handler: usize) -> SignalAction {
    SignalAction {
        handler,
        flags: SignalActionFlags::NOCLDWAIT | SignalActionFlags::RESTART,
        restorer: 0x5000,
        mask: Sigset::new(0xff),
    }
}

/// An exec puts a caught signal back to default, keeps an ignored one
/// ignored, and strips flags, mask and restorer from all three.
#[test]
fn an_exec_keeps_only_whether_a_signal_is_ignored() {
    let mut table = [entry(CAUGHT), entry(SIG_IGN), entry(SIG_DFL)];
    flush_signal_handlers(&mut table);
    assert_eq!(table[0].handler, SIG_DFL, "caught goes to default");
    assert_eq!(table[1].handler, SIG_IGN, "ignored stays ignored");
    assert_eq!(table[2].handler, SIG_DFL);
    for (i, action) in table.iter().enumerate() {
        assert!(action.flags.is_empty(), "entry {} kept its flags", i);
        assert_eq!(action.restorer, 0, "entry {}", i);
        assert!(action.mask.is_empty(), "entry {} kept its mask", i);
    }
}

/// A process with two threads, SIGUSR1 blocked in both and pending on
/// `shared_pending` (process-directed and blocked everywhere at send time,
/// so it was queued once for the process rather than delivered), and SIGCHLD
/// pending privately in the second thread.
///
/// The two halves are deliberate: a disposition change has to reach BOTH
/// pending sets, and they are cleared by two different pieces of
/// [`set_signal_action_in`] (the `shared_pending` / `shared_pending_info`
/// pair under the process lock, and the per-thread `take_siginfo` sweep).
/// A fixture that only filled one of them would pass with the other half
/// deleted.
///
/// This used to assert the SIGUSR1 bit on the FIRST THREAD's private set,
/// which is where `send_signal_to_process` put it until the all-blocked
/// branch was rewritten to queue it once on `shared_pending` and never also
/// on the first tid -- a double-delivery fix, with the pending set the only
/// visible difference. Nothing caught the drift because the workspace's
/// `default-members` is `xtask` alone, so `cargo test` in CI never builds
/// this crate; the three tests below had been red since.
fn with_pending(pid: KoID) -> (Arc<Process>, Arc<Thread>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "pending",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let first = Thread::create_linux(&proc).unwrap();
    let second = Thread::create_linux(&proc).unwrap();
    let usr1 = Sigset::new(1 << (LinuxSignal::SIGUSR1 as u64 - 1));
    first.lock_linux().set_signal_mask(usr1);
    second.lock_linux().set_signal_mask(usr1);
    proc.linux()
        .set_signal_action(LinuxSignal::SIGCHLD, entry(CAUGHT));
    send_signal_to_process(pid as usize, LinuxSignal::SIGUSR1).unwrap();
    second.lock_linux().queue_signal(
        LinuxSignal::SIGCHLD,
        Some(SigInfo::bare(LinuxSignal::SIGCHLD)),
    );
    assert!(
        proc.linux().shared_pending().contains(LinuxSignal::SIGUSR1),
        "a process-directed signal blocked in every thread must still become \
         pending, once, on the process"
    );
    assert!(
        !first.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "a private copy on the first tid is the double delivery the shared \
         queue exists to avoid"
    );
    assert!(second.lock_linux().signals.contains(LinuxSignal::SIGCHLD));
    (proc, first, second)
}

/// `SIG_IGN` discards what was pending wherever it was held -- the process's
/// shared queue as much as a thread's own -- blocked though it is, and the
/// `siginfo_t` with it.
#[test]
fn ignoring_a_signal_discards_what_was_pending_in_every_thread() {
    let (proc, first, second) = with_pending(43_501);
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(SIG_IGN));
    assert!(
        !proc.linux().shared_pending().contains(LinuxSignal::SIGUSR1),
        "a signal left on the shared queue is delivered by the next unblock, \
         to a disposition that says to ignore it"
    );
    assert!(!first.lock_linux().signals.contains(LinuxSignal::SIGUSR1));
    assert!(
        second.lock_linux().signals.contains(LinuxSignal::SIGCHLD),
        "others stay"
    );
    assert_eq!(
        proc.linux().signal_action(LinuxSignal::SIGUSR1).handler,
        SIG_IGN
    );
}

/// And the `siginfo_t` goes with it, not just the bit: `shared_pending_info`
/// keeps one entry per signal and a later instance of the same signal does
/// `or_insert`, so an entry left behind by the discard is what the NEXT
/// SIGUSR1 is delivered with -- the sender and cause of a signal that was
/// thrown away.
///
/// The first one queued here is `SI_KERNEL`, which [`SigInfo::bare`] never
/// is, so the code that comes back says which of the two the second delivery
/// got.
#[test]
fn the_discarded_signal_does_not_leave_its_siginfo_behind() {
    let (proc, first, _second) = with_pending(43_504);
    proc.linux().queue_shared_signal(
        LinuxSignal::SIGUSR1,
        Some(SigInfo::from_kernel(LinuxSignal::SIGUSR1)),
    );
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(SIG_IGN));

    // Unblocking is what pulls the shared queue onto a thread. With the
    // discard done properly there is nothing left to pull.
    first.lock_linux().set_signal_mask(Sigset::empty());
    proc.linux().pull_shared_pending_into(&first);
    assert!(
        !first.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the discarded signal came back on the next unblock"
    );

    // A handler again, and a fresh SIGUSR1 that carries no information of
    // its own: what it is delivered with has to be a bare `SI_USER`.
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(CAUGHT));
    proc.linux().queue_shared_signal(LinuxSignal::SIGUSR1, None);
    proc.linux().pull_shared_pending_into(&first);
    assert!(first.lock_linux().signals.contains(LinuxSignal::SIGUSR1));
    assert_eq!(
        first.lock_linux().take_siginfo(LinuxSignal::SIGUSR1).code,
        SignalCode::USER,
        "the new SIGUSR1 was delivered with the discarded one's cause"
    );
}

/// `SIG_DFL` discards only for a signal whose default is to ignore.
#[test]
fn default_discards_only_when_the_default_ignores() {
    let (proc, _first, second) = with_pending(43_502);
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(SIG_DFL));
    assert!(
        proc.linux().shared_pending().contains(LinuxSignal::SIGUSR1),
        "SIGUSR1's default kills"
    );
    set_signal_action_in(&proc, LinuxSignal::SIGCHLD, entry(SIG_DFL));
    assert!(
        !second.lock_linux().signals.contains(LinuxSignal::SIGCHLD),
        "SIGCHLD's default ignores"
    );
}

/// A handler keeps what is pending: it is about to be delivered.
#[test]
fn a_handler_keeps_what_is_pending() {
    let (proc, _first, _second) = with_pending(43_503);
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(CAUGHT));
    assert!(proc.linux().shared_pending().contains(LinuxSignal::SIGUSR1));
    assert_eq!(
        proc.linux().signal_action(LinuxSignal::SIGUSR1).handler,
        CAUGHT
    );
}
