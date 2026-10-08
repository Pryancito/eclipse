//! A child that exits, stops or resumes pulsed the zircon `SIGCHLD` bit
//! at its parent, which wakes a blocked `wait*`, and nothing else: no
//! Linux `SIGCHLD` was ever queued, so a handler, a signalfd or a
//! `sigwait` on it never ran. `do_notify_parent` and
//! `do_notify_parent_cldstop` are the two places Linux sends it.

use super::*;
use crate::signal::{SignalAction, SignalActionFlags, SignalCode};
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_parent(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "parent",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

fn pending_sigchld(thread: &Arc<Thread>) -> bool {
    thread.lock_linux().signals.contains(LinuxSignal::SIGCHLD)
}

fn clear_pending(thread: &Arc<Thread>) {
    thread.lock_linux().signals = Sigset::default();
}

/// The `siginfo_t` of the pending SIGCHLD as `(si_code, si_pid, si_uid,
/// si_status)`, read where glibc reads them.
fn sigchld_info(thread: &Arc<Thread>) -> (SignalCode, i32, i32, i32) {
    let info = thread.lock_linux().take_siginfo(LinuxSignal::SIGCHLD);
    let b = info.as_bytes();
    let word = |at: usize| i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    (info.code, word(16), word(20), word(24))
}

#[test]
fn the_sigchld_says_which_child_whose_and_how_it_ended() {
    let (parent, thread) = a_parent(43_004);
    let child = Process::fork_from(&parent).unwrap();
    child.linux().set_resuid(1000, 1000, 1000).unwrap();
    child.exit(7);
    assert_eq!(
        sigchld_info(&thread),
        (SignalCode::CLD_EXITED, child.id() as i32, 1000, 7)
    );
}

#[test]
fn a_child_killed_by_a_signal_is_reported_as_killed_by_that_signal() {
    let (parent, thread) = a_parent(43_005);
    let child = Process::fork_from(&parent).unwrap();
    child.exit(exit_code_killed_by(LinuxSignal::SIGKILL as u8));
    assert_eq!(
        sigchld_info(&thread),
        (
            SignalCode::CLD_KILLED,
            child.id() as i32,
            0,
            LinuxSignal::SIGKILL as i32
        )
    );
}

#[test]
fn a_stop_and_a_continue_say_so_in_the_sigchld() {
    let (parent, thread) = a_parent(43_006);
    let child = Process::fork_from(&parent).unwrap();
    child.linux().job_stop(&child, LinuxSignal::SIGTSTP as u8);
    assert_eq!(
        sigchld_info(&thread),
        (
            SignalCode::CLD_STOPPED,
            child.id() as i32,
            0,
            LinuxSignal::SIGTSTP as i32
        )
    );
    child.linux().job_continue(&child);
    let (code, pid, _, _) = sigchld_info(&thread);
    assert_eq!((code, pid), (SignalCode::CLD_CONTINUED, child.id() as i32));
}

#[test]
fn a_child_that_exits_sends_its_parent_a_linux_sigchld() {
    let (parent, thread) = a_parent(43_001);
    let child = Process::fork_from(&parent).unwrap();
    assert!(!pending_sigchld(&thread));
    child.exit(0);
    assert!(
        pending_sigchld(&thread),
        "the parent never heard the child die"
    );
}

#[test]
fn a_child_that_stops_and_resumes_sends_sigchld_both_times() {
    let (parent, thread) = a_parent(43_002);
    let child = Process::fork_from(&parent).unwrap();
    child.linux().job_stop(&child, LinuxSignal::SIGSTOP as u8);
    assert!(pending_sigchld(&thread), "no SIGCHLD for the stop");
    clear_pending(&thread);
    assert!(child.linux().job_continue(&child));
    assert!(pending_sigchld(&thread), "no SIGCHLD for the continue");
}

#[test]
fn sa_nocldstop_keeps_stops_quiet_but_not_deaths() {
    let (parent, thread) = a_parent(43_003);
    parent.linux().set_signal_action(
        LinuxSignal::SIGCHLD,
        SignalAction {
            handler: 0x1000,
            flags: SignalActionFlags::NOCLDSTOP,
            restorer: 0,
            mask: Sigset::default(),
        },
    );
    let child = Process::fork_from(&parent).unwrap();
    child.linux().job_stop(&child, LinuxSignal::SIGSTOP as u8);
    assert!(!pending_sigchld(&thread), "SA_NOCLDSTOP was ignored");
    child.linux().job_continue(&child);
    assert!(
        !pending_sigchld(&thread),
        "SA_NOCLDSTOP was ignored on continue"
    );
    child.exit(0);
    assert!(
        pending_sigchld(&thread),
        "a death is never covered by SA_NOCLDSTOP"
    );
}

fn sigchld_action(parent: &Arc<Process>, handler: usize, flags: SignalActionFlags) {
    parent.linux().set_signal_action(
        LinuxSignal::SIGCHLD,
        SignalAction {
            handler,
            flags,
            restorer: 0,
            mask: Sigset::default(),
        },
    );
}

/// A daemon that sets SIGCHLD to SIG_IGN and never waits (sigaction(2):
/// "children that terminate do not become zombies") used to leave every
/// child as a zombie in `reaped_children`, for good, and a later
/// `wait4(-1)` handed back a child the program had said it would never
/// collect.
#[test]
fn sig_ign_on_sigchld_releases_the_child_with_no_zombie_and_no_signal() {
    let (parent, thread) = a_parent(43_007);
    sigchld_action(&parent, crate::signal::SIG_IGN, SignalActionFlags::empty());
    let child = Process::fork_from(&parent).unwrap();
    child.exit(3);
    assert!(
        !parent.linux().is_zombie_child(child.id()),
        "the child stayed a zombie although SIGCHLD is ignored"
    );
    assert!(!parent.linux().has_child(child.id()));
    assert!(!pending_sigchld(&thread), "SIG_IGN still queued a SIGCHLD");
    let r = async_std::task::block_on(wait_child_any(&parent, true, true));
    assert_eq!(r.err(), Some(LxError::ECHILD));
}

#[test]
fn sa_nocldwait_releases_the_child_but_the_handler_still_runs() {
    let (parent, thread) = a_parent(43_008);
    sigchld_action(&parent, 0x1000, SignalActionFlags::NOCLDWAIT);
    let child = Process::fork_from(&parent).unwrap();
    child.exit(0);
    assert!(!parent.linux().has_child(child.id()), "zombie left behind");
    assert!(
        pending_sigchld(&thread),
        "SA_NOCLDWAIT is not SIG_IGN: the signal is still sent"
    );
}

#[test]
fn a_parent_that_wants_its_zombies_keeps_them() {
    let (parent, _thread) = a_parent(43_009);
    sigchld_action(&parent, 0x1000, SignalActionFlags::NOCLDSTOP);
    let child = Process::fork_from(&parent).unwrap();
    child.exit(5);
    assert!(parent.linux().is_zombie_child(child.id()));
    let (pid, status, _) = async_std::task::block_on(wait_child_any(&parent, true, true)).unwrap();
    assert_eq!((pid, status), (child.id(), wait_status_exited(5)));
}
