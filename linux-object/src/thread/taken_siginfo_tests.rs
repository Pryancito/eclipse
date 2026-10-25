//! A signal's `siginfo_t` goes when the signal does.
//!
//! Linux keeps the two in one `sigqueue` entry and frees them together:
//! dequeued, thrown away as ignored, or flushed by the stop or the continue
//! that cancels them. Here they are kept apart, and three ways out cleared
//! only the bit: a delivery that ran no handler (an ignored `SIGCHLD`, a
//! stop, a continue), the discard of an ignored signal while a thread waits
//! in a syscall, and the stop or continue that a `SIGCONT` or a stop
//! cancels. The information stayed, and a non-realtime signal that finds
//! one keeps it, so the next `SIGCHLD` came with the child before it: its
//! `si_pid`, its status, its `si_code` (`siginfo-survey`, 11 checks, Linux
//! 6.18 with glibc and musl).

use super::*;
use crate::process::{
    check_signals_of, prepare_signal, send_signal_to_process_with_info, ProcessExt,
};
use crate::signal::{SigInfo, Signal, SignalAction, SignalCode};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::ROOT_JOB;

/// What a `kill` from `pid` carries.
fn from(signal: Signal, pid: i32) -> SigInfo {
    SigInfo::from_user(signal, pid, 0, SignalCode::USER)
}

/// `si_pid`, which is what tells one `kill` from another here.
fn sender(info: SigInfo) -> i32 {
    let bytes = info.as_bytes();
    i32::from_ne_bytes([bytes[16], bytes[17], bytes[18], bytes[19]])
}

fn one(signal: Signal) -> Sigset {
    let mut set = Sigset::empty();
    set.insert(signal);
    set
}

/// A process with one thread, which blocks nothing.
fn a_process(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

/// What the next delivery of `thread` hands out: the signal, and who sent
/// it by its `siginfo_t`.
fn delivered(thread: &Thread) -> Option<(Signal, i32)> {
    let mut lt = thread.lock_linux();
    lt.handle_signal()
        .map(|(signal, _, info)| (signal, sender(info)))
}

#[test]
fn a_signal_is_delivered_with_its_siginfo_and_leaves_none_behind() {
    let (proc, thread) = a_process(49_101);
    thread
        .lock_linux()
        .queue_signal(Signal::SIGCHLD, Some(from(Signal::SIGCHLD, 11)));
    assert_eq!(delivered(&thread), Some((Signal::SIGCHLD, 11)));
    // Nothing ran for it: an ignored SIGCHLD, as `SIG_DFL` has it.
    {
        let mut lt = thread.lock_linux();
        let mask = lt.signal_mask();
        lt.taken_without_handler(mask);
    }
    thread
        .lock_linux()
        .queue_signal(Signal::SIGCHLD, Some(from(Signal::SIGCHLD, 22)));
    assert_eq!(
        delivered(&thread),
        Some((Signal::SIGCHLD, 22)),
        "the next SIGCHLD came with the child before it"
    );
    proc.exit(0);
}

#[test]
fn one_queued_while_the_last_is_being_delivered_keeps_its_own() {
    // The delivery locks the thread again to run the handler; a signal of
    // the same number queued in between is a new one, with its own sender.
    let (proc, thread) = a_process(49_102);
    let mut lt = thread.lock_linux();
    lt.queue_signal(Signal::SIGUSR1, Some(from(Signal::SIGUSR1, 31)));
    let (_, _, first) = lt.handle_signal().unwrap();
    lt.queue_signal(Signal::SIGUSR1, Some(from(Signal::SIGUSR1, 32)));
    lt.deliver_siginfo(first);
    assert_eq!(sender(first), 31);
    let (_, _, second) = lt.handle_signal().unwrap();
    assert_eq!(sender(second), 32, "the second took the first's");
    drop(lt);
    proc.exit(0);
}

#[test]
fn a_signal_sent_with_nothing_is_bare_whatever_came_before() {
    let (proc, thread) = a_process(49_103);
    let mut lt = thread.lock_linux();
    lt.queue_signal(Signal::SIGWINCH, Some(from(Signal::SIGWINCH, 41)));
    lt.handle_signal().unwrap();
    lt.queue_signal(Signal::SIGWINCH, None);
    let (_, _, info) = lt.handle_signal().unwrap();
    assert!(
        info == SigInfo::bare(Signal::SIGWINCH),
        "from {}",
        sender(info)
    );
    drop(lt);
    proc.exit(0);
}

#[test]
fn an_ignored_signal_thrown_away_in_a_wait_takes_its_siginfo_with_it() {
    // `check_signals_of` is what a sleep, a poll or a read asks; an
    // ignored signal it finds is thrown away there, not delivered.
    let (proc, thread) = a_process(49_104);
    send_signal_to_process_with_info(
        proc.id() as usize,
        Signal::SIGCHLD,
        Some(from(Signal::SIGCHLD, 51)),
    )
    .unwrap();
    assert_eq!(check_signals_of(&thread), Ok(()), "SIGCHLD is ignored");
    assert!(!thread.lock_linux().signals.contains(Signal::SIGCHLD));
    send_signal_to_process_with_info(
        proc.id() as usize,
        Signal::SIGCHLD,
        Some(from(Signal::SIGCHLD, 52)),
    )
    .unwrap();
    assert_eq!(
        sender(thread.lock_linux().take_siginfo(Signal::SIGCHLD)),
        52
    );
    proc.exit(0);
}

#[test]
fn a_continue_throws_away_a_waiting_stop_with_its_siginfo() {
    let (proc, thread) = a_process(49_105);
    // Not ignored, so the thread keeps it while it waits blocked.
    proc.linux().set_signal_action(
        Signal::SIGTSTP,
        SignalAction {
            handler: 0x1000,
            ..SignalAction::default()
        },
    );
    thread
        .lock_linux()
        .queue_signal(Signal::SIGTSTP, Some(from(Signal::SIGTSTP, 61)));
    prepare_signal(&proc, Signal::SIGCONT);
    assert!(!thread.lock_linux().signals.contains(Signal::SIGTSTP));
    thread
        .lock_linux()
        .queue_signal(Signal::SIGTSTP, Some(from(Signal::SIGTSTP, 62)));
    assert_eq!(delivered(&thread), Some((Signal::SIGTSTP, 62)));
    proc.exit(0);
}

#[test]
fn a_continue_throws_away_a_stop_waiting_on_the_process() {
    // Blocked by every thread, a process-directed stop waits on the
    // process, and a SIGCONT has to reach it there.
    let (proc, thread) = a_process(49_106);
    thread.lock_linux().block_signals(&one(Signal::SIGTSTP));
    send_signal_to_process_with_info(
        proc.id() as usize,
        Signal::SIGTSTP,
        Some(from(Signal::SIGTSTP, 71)),
    )
    .unwrap();
    assert!(proc.linux().shared_pending().contains(Signal::SIGTSTP));
    prepare_signal(&proc, Signal::SIGCONT);
    assert!(
        !proc.linux().shared_pending().contains(Signal::SIGTSTP),
        "the stop outlived the continue, to stop the process once unblocked"
    );
    send_signal_to_process_with_info(
        proc.id() as usize,
        Signal::SIGTSTP,
        Some(from(Signal::SIGTSTP, 72)),
    )
    .unwrap();
    let mut lt = thread.lock_linux();
    proc.linux().pull_shared_pending_locked(&mut lt);
    assert!(!lt.signals.contains(Signal::SIGTSTP), "still blocked");
    lt.sigwait = one(Signal::SIGTSTP);
    proc.linux().pull_shared_pending_locked(&mut lt);
    assert_eq!(sender(lt.take_siginfo(Signal::SIGTSTP)), 72);
    drop(lt);
    proc.exit(0);
}

#[test]
fn a_stop_throws_away_a_waiting_continue_and_nothing_else() {
    let (proc, thread) = a_process(49_107);
    let mut both = one(Signal::SIGCONT);
    both.insert(Signal::SIGUSR2);
    thread.lock_linux().block_signals(&both);
    for (signal, pid) in [(Signal::SIGCONT, 81), (Signal::SIGUSR2, 85)] {
        send_signal_to_process_with_info(proc.id() as usize, signal, Some(from(signal, pid)))
            .unwrap();
    }
    {
        let mut lt = thread.lock_linux();
        lt.queue_signal(Signal::SIGCONT, Some(from(Signal::SIGCONT, 82)));
        lt.queue_signal(Signal::SIGUSR2, Some(from(Signal::SIGUSR2, 83)));
    }
    prepare_signal(&proc, Signal::SIGTSTP);
    assert!(!proc.linux().shared_pending().contains(Signal::SIGCONT));
    assert!(
        proc.linux().shared_pending().contains(Signal::SIGUSR2),
        "a stop throws away a continue, and only that"
    );
    let mut lt = thread.lock_linux();
    assert!(!lt.signals.contains(Signal::SIGCONT));
    assert_eq!(sender(lt.take_siginfo(Signal::SIGUSR2)), 83);
    lt.queue_signal(Signal::SIGCONT, Some(from(Signal::SIGCONT, 84)));
    assert_eq!(sender(lt.take_siginfo(Signal::SIGCONT)), 84);
    drop(lt);
    proc.exit(0);
}

#[test]
fn an_ordinary_signal_cancels_nothing() {
    let (proc, thread) = a_process(49_108);
    thread.lock_linux().block_signals(&one(Signal::SIGTSTP));
    send_signal_to_process_with_info(
        proc.id() as usize,
        Signal::SIGTSTP,
        Some(from(Signal::SIGTSTP, 91)),
    )
    .unwrap();
    thread
        .lock_linux()
        .queue_signal(Signal::SIGTSTP, Some(from(Signal::SIGTSTP, 92)));
    prepare_signal(&proc, Signal::SIGUSR1);
    assert!(proc.linux().shared_pending().contains(Signal::SIGTSTP));
    assert_eq!(
        sender(thread.lock_linux().take_siginfo(Signal::SIGTSTP)),
        92
    );
    proc.exit(0);
}
