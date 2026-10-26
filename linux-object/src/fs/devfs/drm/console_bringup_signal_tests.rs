//! Tests for the one-way signals that tell userspace and the kernel about the
//! console GPU's bring-up.
//!
//! They exist because the first version of the wait deadlocked both ends
//! against each other: `eclipse-init` held `labwc` back until
//! `/proc/gpuready` said the card served, and the kernel's deferred bring-up
//! waited for the compositor to claim the VT before it would start. Neither
//! could move, so both ran out their timeouts -- 25 s of black screen, then
//! pixman anyway -- and on a box booted with `nvidia.noconsole_gpu` the file
//! said `waiting` forever about a bring-up nobody had scheduled.

use super::*;

/// A boot that will not bring the console GPU up must say so, because
/// `/proc/gpuready` saying `waiting` is a promise that it will.
#[test]
fn nothing_is_expected_until_a_bring_up_is_scheduled() {
    set_console_bringup_expected(false);
    assert!(
        !console_bringup_expected(),
        "no scheduled bring-up means there is nothing for a waiter to wait for"
    );
    set_console_bringup_expected(true);
    assert!(console_bringup_expected());
    set_console_bringup_expected(false);
    assert!(
        !console_bringup_expected(),
        "the answer must follow the boot, not latch on the first `true`"
    );
}

/// The read signal is a latch on purpose: the deferred task polls it every
/// 500 ms, and init reads `/proc/gpuready` once before each poll could see it.
/// A flag that cleared itself would be missed and both ends would wait again.
#[test]
fn asking_once_is_remembered() {
    note_gpu_ready_read();
    assert!(
        gpu_ready_was_read(),
        "the question must survive the poll gap"
    );
    note_gpu_ready_read();
    assert!(
        gpu_ready_was_read(),
        "asking twice is not a reason to forget that anyone asked"
    );
}

/// A client parked in `GEM_NEW` is the third signal, and the strongest: the
/// other two are guesses about whether boot progress is over, this one is the
/// session stopped dead. A latch for the same reason as the read above -- the
/// task looks every 500 ms and the client parks once -- and never cleared,
/// because a boot that has had one blocked client has nothing left to protect
/// with the courtesy sleep.
#[test]
fn a_parked_client_is_remembered() {
    note_cold_client_waiting();
    assert!(
        cold_client_waiting(),
        "the parked client must survive the poll gap"
    );
    note_cold_client_waiting();
    assert!(
        cold_client_waiting(),
        "a second client parking is not a reason to forget the first"
    );
}

/// Whether an ioctl may park, in the only four states that exist. `expected`
/// alone is not enough: it latches at boot and never clears, so after a
/// bring-up that did NOT attach the RM the card answers `rm=no` for the rest
/// of the boot and every single ioctl would pay the whole 60 s wait before
/// the refusal it would have had at once.
#[test]
fn only_a_bring_up_that_is_still_coming_is_worth_waiting_for() {
    assert!(
        bringup_still_coming(true, false),
        "scheduled and not finished: the one case a client must wait out"
    );
    assert!(
        !bringup_still_coming(true, true),
        "finished means nothing more is coming, attached or not"
    );
    assert!(
        !bringup_still_coming(false, false),
        "nothing scheduled: `nvidia.noconsole_gpu`, or a box with one card"
    );
    assert!(
        !bringup_still_coming(false, true),
        "neither scheduled nor pending is still not something to wait for"
    );
}
