//! What changing a disposition does beyond the table: an exec, and a
//! new disposition that ignores.

extern crate std;

use super::*;
use crate::signal::{SignalAction, SignalActionFlags, SIG_DFL, SIG_IGN};
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

/// A process with two threads, SIGUSR1 blocked in both and pending in
/// the first (blocked at send time, so it was queued rather than
/// delivered), and SIGCHLD pending in the second.
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
    assert!(first.lock_linux().signals.contains(LinuxSignal::SIGUSR1));
    assert!(second.lock_linux().signals.contains(LinuxSignal::SIGCHLD));
    (proc, first, second)
}

/// `SIG_IGN` discards the pending instance in the thread that holds it,
/// blocked though it is, and the `siginfo_t` with it.
#[test]
fn ignoring_a_signal_discards_what_was_pending_in_every_thread() {
    let (proc, first, second) = with_pending(43_501);
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(SIG_IGN));
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

/// `SIG_DFL` discards only for a signal whose default is to ignore.
#[test]
fn default_discards_only_when_the_default_ignores() {
    let (proc, first, second) = with_pending(43_502);
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(SIG_DFL));
    assert!(
        first.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
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
    let (proc, first, _second) = with_pending(43_503);
    set_signal_action_in(&proc, LinuxSignal::SIGUSR1, entry(CAUGHT));
    assert!(first.lock_linux().signals.contains(LinuxSignal::SIGUSR1));
    assert_eq!(
        proc.linux().signal_action(LinuxSignal::SIGUSR1).handler,
        CAUGHT
    );
}
