//! What a signal does at the moment it is SENT, before anybody receives
//! it.
//!
//! `prepare_signal()` does this work in the sender's context because the
//! signals it covers exist to act on a process that is not running. Here
//! there was nothing: every bit of it was left to the target thread's own
//! `handle_signal` loop in `loader/src/linux.rs`. A job-control-stopped
//! process's threads are parked in `wait_while_job_stopped`, waiting for
//! a zircon signal that only `job_continue` raises -- and `job_continue`
//! was called from that same loop. So a stopped process could not be
//! resumed by `SIGCONT` and could not be killed by `SIGKILL`: both waited
//! for the one thread that was waiting for them.

use super::*;

#[test]
fn a_continue_resumes_at_the_moment_it_is_sent() {
    assert_eq!(send_effect(LinuxSignal::SIGCONT), SendEffect::Resume);
}

/// `kill -9` on a Ctrl-Z'd process. The default action for SIGKILL runs
/// in the target's own loop, so the target has to be out of the park
/// before it can die -- otherwise the pid stays stopped forever and no
/// signal can ever remove it.
#[test]
fn a_kill_wakes_a_stopped_process_so_it_can_die() {
    assert_eq!(send_effect(LinuxSignal::SIGKILL), SendEffect::WakeToDie);
}

#[test]
fn the_four_stop_signals_are_the_four() {
    for sig in STOP_SIGNALS {
        assert_eq!(send_effect(sig), SendEffect::Stop, "{:?}", sig);
    }
    assert_eq!(
        STOP_SIGNALS,
        [
            LinuxSignal::SIGSTOP,
            LinuxSignal::SIGTSTP,
            LinuxSignal::SIGTTIN,
            LinuxSignal::SIGTTOU
        ]
    );
}

/// Everything else is an ordinary signal: it waits to be received. A
/// SIGTERM must NOT resume a stopped process, or a `kill` of a stopped
/// job would restart it just long enough to run a handler.
#[test]
fn an_ordinary_signal_does_nothing_until_it_is_received() {
    for sig in [
        LinuxSignal::SIGTERM,
        LinuxSignal::SIGINT,
        LinuxSignal::SIGHUP,
        LinuxSignal::SIGCHLD,
        LinuxSignal::SIGUSR1,
        LinuxSignal::SIGWINCH,
    ] {
        assert_eq!(send_effect(sig), SendEffect::None, "{:?}", sig);
    }
}

/// A stop and a continue cancel each other where they are waiting, so the
/// last one sent decides. Carrying both, the outcome depended on which of
/// them the target's loop happened to dequeue first.
#[test]
fn a_continue_cancels_a_stop_that_was_still_waiting() {
    let mut pending = Sigset::default();
    for stop in STOP_SIGNALS {
        pending.insert(stop);
    }
    pending.insert(LinuxSignal::SIGTERM);
    let after = pending_after_send(pending, LinuxSignal::SIGCONT);
    for stop in STOP_SIGNALS {
        assert!(!after.contains(stop), "{:?} survived a SIGCONT", stop);
    }
    assert!(
        after.contains(LinuxSignal::SIGTERM),
        "only the stops are cancelled"
    );
}

#[test]
fn a_stop_cancels_a_continue_that_was_still_waiting() {
    for stop in STOP_SIGNALS {
        let mut pending = Sigset::default();
        pending.insert(LinuxSignal::SIGCONT);
        pending.insert(LinuxSignal::SIGUSR2);
        let after = pending_after_send(pending, stop);
        assert!(
            !after.contains(LinuxSignal::SIGCONT),
            "a pending SIGCONT survived {:?}",
            stop
        );
        assert!(after.contains(LinuxSignal::SIGUSR2));
    }
}

#[test]
fn an_ordinary_signal_cancels_nothing() {
    let mut pending = Sigset::default();
    pending.insert(LinuxSignal::SIGCONT);
    pending.insert(LinuxSignal::SIGTSTP);
    let after = pending_after_send(pending, LinuxSignal::SIGTERM);
    assert!(after.contains(LinuxSignal::SIGCONT));
    assert!(after.contains(LinuxSignal::SIGTSTP));
}

/// The wake-to-die is not a continue. A parent in `wait(WCONTINUED)` must
/// not be told the job resumed at the very moment it was killed.
#[test]
fn a_process_woken_to_die_owes_its_parent_no_continue_notification() {
    let mut inner = LinuxProcessInner {
        job_stopped: true,
        job_stop_sig: LinuxSignal::SIGTSTP as u8,
        job_stop_pending: true,
        ..Default::default()
    };
    assert!(inner.leave_stop(false));
    assert!(!inner.job_stopped);
    assert!(!inner.job_continued_pending, "it is not continuing");
    assert!(
        !inner.job_stop_pending,
        "and the stop it never collected is no longer current"
    );
}

#[test]
fn a_real_continue_does_owe_one() {
    let mut inner = LinuxProcessInner {
        job_stopped: true,
        job_stop_pending: true,
        ..Default::default()
    };
    assert!(inner.leave_stop(true));
    assert!(inner.job_continued_pending);
    assert!(!inner.job_stop_pending);
}

/// A `SIGCONT` to a process that was not stopped is not a state change,
/// so it owes nothing either -- `wait(WCONTINUED)` would otherwise return
/// for a job that never went anywhere.
#[test]
fn a_continue_to_a_running_process_is_not_a_state_change() {
    let mut inner = LinuxProcessInner::default();
    assert!(!inner.leave_stop(true));
    assert!(!inner.job_continued_pending);
}
