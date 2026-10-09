use super::*;
use core::sync::atomic::AtomicI32;

#[test]
fn first_ctrl_c_arms_sigint_second_escalates_to_sigkill() {
    let armed = AtomicI32::new(0);
    // No live members → ESRCH inside send, but the arm / escalate choice
    // is independent of that.
    assert_eq!(
        interrupt_or_force_pgrp(4242, &armed),
        LinuxSignal::SIGINT,
        "first press is graceful"
    );
    assert_eq!(armed.load(Ordering::Relaxed), 4242);
    assert_eq!(
        interrupt_or_force_pgrp(4242, &armed),
        LinuxSignal::SIGKILL,
        "second press for the same pgrp is forced"
    );
    assert_eq!(armed.load(Ordering::Relaxed), 0, "arm clears after force");
    assert_eq!(
        interrupt_or_force_pgrp(4242, &armed),
        LinuxSignal::SIGINT,
        "a third press starts over with SIGINT"
    );
}

#[test]
fn a_different_pgrp_does_not_inherit_the_force_arm() {
    let armed = AtomicI32::new(0);
    assert_eq!(interrupt_or_force_pgrp(100, &armed), LinuxSignal::SIGINT);
    assert_eq!(
        interrupt_or_force_pgrp(200, &armed),
        LinuxSignal::SIGINT,
        "a new job gets a fresh SIGINT, not an inherited SIGKILL"
    );
    assert_eq!(armed.load(Ordering::Relaxed), 200);
}

/// The split the pty needs: the arm moves and the answer is known, with
/// nothing sent, so the decision may be taken under a lock the send must
/// not be taken under.
#[test]
fn deciding_without_sending_arms_the_same_way() {
    let armed = AtomicI32::new(0);
    assert_eq!(interrupt_escalation(77, &armed), LinuxSignal::SIGINT);
    assert_eq!(armed.load(Ordering::Relaxed), 77);
    assert_eq!(interrupt_escalation(77, &armed), LinuxSignal::SIGKILL);
    assert_eq!(armed.load(Ordering::Relaxed), 0);
}

/// A terminal with no foreground group has nothing to arm: the pty's
/// Ctrl-C reads the group out of the tty and may well find 0.
#[test]
fn with_no_foreground_group_the_arm_is_left_alone() {
    let armed = AtomicI32::new(99);
    assert_eq!(interrupt_escalation(0, &armed), LinuxSignal::SIGINT);
    assert_eq!(interrupt_escalation(-1, &armed), LinuxSignal::SIGINT);
    assert_eq!(armed.load(Ordering::Relaxed), 99, "the armed job is intact");
}

#[test]
fn clear_interrupt_arm_resets_escalation() {
    let armed = AtomicI32::new(0);
    assert_eq!(interrupt_or_force_pgrp(55, &armed), LinuxSignal::SIGINT);
    clear_interrupt_arm(&armed);
    assert_eq!(
        interrupt_or_force_pgrp(55, &armed),
        LinuxSignal::SIGINT,
        "after clear, the next Ctrl-C is SIGINT again"
    );
}
