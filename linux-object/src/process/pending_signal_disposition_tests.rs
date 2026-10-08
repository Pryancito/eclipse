//! What `check_signals` does with a pending signal that does NOT wake the
//! syscall.
//!
//! One predicate was answering two questions. "Does this interrupt the
//! syscall with EINTR?" and "may this be thrown away?" have the same
//! answer for most of the enum and a different one for exactly four
//! signals -- the job-control stops -- which is why the difference went
//! unnoticed: everything the tests looked at agreed.
//!
//! The consequence was not subtle. A thread parked in `ppoll`/
//! `epoll_wait`/`read` is every idle desktop process, and `check_signals`
//! runs on each turn of that loop: a `kill -STOP` made the bit pending
//! and the next turn of the wait removed it. `kill -STOP` on anything
//! idle did nothing whatsoever, and so did Ctrl-Z on a program blocked in
//! a read, and so did the SIGTTIN a background job gets for reading the
//! terminal.
//!
//! Both predicates are checked over a disposition rather than a process,
//! which is what lets the two be laid side by side here.

use super::*;
use crate::signal::{SIG_DFL, SIG_IGN};
use alloc::vec::Vec;
use core::convert::TryFrom;

/// Some address that is neither `SIG_DFL` (0) nor `SIG_IGN` (1): a
/// handler the program installed.
const CAUGHT: usize = 0x4000_1000;

fn every_signal() -> Vec<LinuxSignal> {
    (1u8..=64)
        .filter_map(|n| LinuxSignal::try_from(n).ok())
        .collect()
}

/// The whole finding, in one assertion: the set of signals that neither
/// interrupt nor may be discarded is exactly the four stop signals. Empty
/// before the split, because one predicate answered both questions.
#[test]
fn the_two_questions_differ_on_exactly_the_four_stop_signals() {
    let mut neither: Vec<LinuxSignal> = every_signal()
        .into_iter()
        .filter(|&s| !interrupts_syscall(SIG_DFL, s) && !discards_when_pending(SIG_DFL, s))
        .collect();
    neither.sort_unstable_by_key(|s| *s as u8);
    assert_eq!(neither, STOP_SIGNALS.to_vec());
}

/// `kill -STOP` on a process sitting in `poll`. The stop itself happens
/// when the syscall returns, in `handle_signal`; what this fixes is the
/// bit being gone by then.
#[test]
fn a_pending_stop_signal_is_not_a_signal_to_throw_away() {
    for sig in STOP_SIGNALS {
        assert!(
            !discards_when_pending(SIG_DFL, sig),
            "{:?} was discarded while the thread waited",
            sig
        );
    }
}

/// And it still does not raise EINTR, which is the half that was right:
/// Linux restarts the syscall around a stop rather than failing it, and
/// this kernel has no restart machinery to do that with.
#[test]
fn a_stop_signal_still_does_not_wake_the_syscall_with_eintr() {
    for sig in STOP_SIGNALS {
        assert!(!interrupts_syscall(SIG_DFL, sig), "{:?}", sig);
    }
}

/// `sig_kernel_ignore()`: these four really are no-ops by default, and
/// dropping them is what keeps a blocking wait from re-scanning the same
/// stale bit on every wake. SIGCHLD in particular: returning EINTR for it
/// is what made a compositor's libinput dispatch fail with "Interrupted
/// system call" every time an autostart child exited.
#[test]
fn the_four_whose_default_is_to_do_nothing_are_still_discarded() {
    for sig in [
        LinuxSignal::SIGCHLD,
        LinuxSignal::SIGURG,
        LinuxSignal::SIGWINCH,
        LinuxSignal::SIGCONT,
    ] {
        assert!(discards_when_pending(SIG_DFL, sig), "{:?}", sig);
        assert!(!interrupts_syscall(SIG_DFL, sig), "{:?}", sig);
    }
}

/// A shell that sets SIGTSTP to `SIG_IGN` so it cannot be suspended means
/// it: an explicitly ignored signal is discardable whatever it is.
#[test]
fn a_signal_the_program_ignores_is_discarded_whatever_it_is() {
    for sig in every_signal() {
        assert!(discards_when_pending(SIG_IGN, sig), "{:?}", sig);
        assert!(!interrupts_syscall(SIG_IGN, sig), "{:?}", sig);
    }
}

/// A signal with a handler has somewhere to go, so it wakes the syscall
/// and is never dropped on the way.
#[test]
fn a_caught_signal_interrupts_and_is_never_discarded() {
    for sig in every_signal() {
        assert!(interrupts_syscall(CAUGHT, sig), "{:?}", sig);
        assert!(!discards_when_pending(CAUGHT, sig), "{:?}", sig);
    }
}

/// SIGKILL never reaches the discard at all: it interrupts first, and
/// `check_signals` returns EINTR before it gets that far. Its disposition
/// cannot be changed (`rt_sigaction` refuses), so `SIG_DFL` is the only
/// case there is.
#[test]
fn a_kill_leaves_the_wait_before_anything_can_drop_it() {
    assert!(interrupts_syscall(SIG_DFL, LinuxSignal::SIGKILL));
    assert!(!discards_when_pending(SIG_DFL, LinuxSignal::SIGKILL));
}

/// The two lists are near-complements and were treated as exact ones.
#[test]
fn the_ignore_list_and_the_interrupt_list_are_not_complements() {
    for sig in every_signal() {
        if signal_default_action_ignores(sig) {
            assert!(!signal_default_action_interrupts(sig), "{:?}", sig);
        }
    }
    // ... and four signals are in neither.
    assert!(!signal_default_action_ignores(LinuxSignal::SIGSTOP));
    assert!(!signal_default_action_interrupts(LinuxSignal::SIGSTOP));
}
