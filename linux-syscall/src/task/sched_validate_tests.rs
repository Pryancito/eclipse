//! The two pure functions that decide which `(policy, priority, nice)` this
//! family of calls accepts, and which numbers it answers when asked.
//!
//! Neither had a single test. Measured by mutation on 16-Oct: all nine
//! mutants of `rt_priority_bounds` and the one on the fair policies'
//! priority check SURVIVED, so neither table was held by anything. And they
//! are not internal tables: `sched_get_priority_max(2)` and
//! `sched_get_priority_min(2)` are how a library finds out what it may ask
//! for before asking, so a number that drifts here does not break a test, it
//! breaks whoever believed the answer.

use super::*;

/// The fair policies have no real-time priority, and what Linux does with one
/// that carries anyway is not to ignore it: `__sched_setscheduler` refuses.
///
/// `sched_setscheduler(pid, SCHED_OTHER, {sched_priority = 1})` has to answer
/// `EINVAL`. Accepting it and storing a zero leaves the caller believing it
/// runs at priority 1 under a policy that does not use them.
#[test]
fn a_fair_policy_refuses_a_real_time_priority() {
    for policy in [SCHED_NORMAL, SCHED_BATCH, SCHED_IDLE] {
        assert_eq!(
            Syscall::sched_validate(policy, 0, 0),
            Ok((policy, 0, 0)),
            "policy {} with priority 0 is the ordinary case and was refused",
            policy
        );
        for priority in [1, 50, 99, -1, i32::MAX] {
            assert_eq!(
                Syscall::sched_validate(policy, priority, 0),
                Err(LxError::EINVAL),
                "policy {} accepted priority {}",
                policy,
                priority
            );
        }
    }
}

/// And the real-time ones want it inside `1..=99`, the band
/// `sched_get_priority_min`/`max` announce.
#[test]
fn a_real_time_policy_wants_its_priority_inside_the_band_it_announces() {
    for policy in [SCHED_FIFO, SCHED_RR] {
        for priority in [MIN_RT_PRIO as i32, 50, MAX_RT_PRIO as i32] {
            assert_eq!(
                Syscall::sched_validate(policy, priority, 0),
                Ok((policy, priority as u8, 0)),
                "policy {} refused priority {}, which is in band",
                policy,
                priority
            );
        }
        // Both outside edges, which are the ones a misplaced `>=` lets
        // through: the 0 of the fair policies and the 100 above the top.
        for priority in [0, MAX_RT_PRIO as i32 + 1, -1, i32::MIN] {
            assert_eq!(
                Syscall::sched_validate(policy, priority, 0),
                Err(LxError::EINVAL),
                "policy {} accepted priority {}, which is out of band",
                policy,
                priority
            );
        }
    }
}

/// A nice value out of range is refused here, not clamped. `sched_setattr`
/// clamps it before it gets here, as Linux does (`sched_copy_attr`): that is
/// `sched_attr_tests`.
#[test]
fn a_nice_outside_the_range_is_refused_and_not_clamped() {
    for nice in [MIN_NICE as i32, 0, MAX_NICE as i32] {
        assert_eq!(
            Syscall::sched_validate(SCHED_NORMAL, 0, nice),
            Ok((SCHED_NORMAL, 0, nice as i8)),
            "nice {} is in range and was refused",
            nice
        );
    }
    for nice in [MIN_NICE as i32 - 1, MAX_NICE as i32 + 1, 100, i32::MIN] {
        assert_eq!(
            Syscall::sched_validate(SCHED_NORMAL, 0, nice),
            Err(LxError::EINVAL),
            "nice {} was accepted instead of refused",
            nice
        );
    }
}

/// `SCHED_DEADLINE`, and any number that is not a policy: `EINVAL`, and
/// before the priority is looked at.
#[test]
fn a_policy_this_kernel_does_not_have_is_einval() {
    for policy in [SCHED_DEADLINE, 4, 7, 42, u8::MAX] {
        assert_eq!(
            Syscall::sched_validate(policy, 0, 0),
            Err(LxError::EINVAL),
            "policy {} was accepted",
            policy
        );
    }
}

/// The table `sched_get_priority_max(2)` and `sched_get_priority_min(2)`
/// answer, policy by policy.
///
/// One row each rather than a loop over the constants, for the reason
/// `abi_tests` gives: a number that drifts has to be named, and a loop over
/// the constant would follow it wherever it went.
#[test]
fn the_priority_bounds_are_the_table_libc_reads() {
    assert_eq!(
        Syscall::rt_priority_bounds(SCHED_FIFO as usize),
        Ok((1, 99)),
        "SCHED_FIFO"
    );
    assert_eq!(
        Syscall::rt_priority_bounds(SCHED_RR as usize),
        Ok((1, 99)),
        "SCHED_RR"
    );
    assert_eq!(
        Syscall::rt_priority_bounds(SCHED_NORMAL as usize),
        Ok((0, 0)),
        "SCHED_NORMAL"
    );
    assert_eq!(
        Syscall::rt_priority_bounds(SCHED_BATCH as usize),
        Ok((0, 0)),
        "SCHED_BATCH"
    );
    assert_eq!(
        Syscall::rt_priority_bounds(SCHED_IDLE as usize),
        Ok((0, 0)),
        "SCHED_IDLE"
    );
    // And the ones that are not here. `SCHED_DEADLINE` exists in Linux and
    // there is no deadline runqueue in this kernel, so it has to answer the
    // same as a made-up number.
    for policy in [SCHED_DEADLINE as usize, 4, 7, 42, usize::MAX] {
        assert_eq!(
            Syscall::rt_priority_bounds(policy),
            Err(LxError::EINVAL),
            "policy {} has a priority band",
            policy
        );
    }
    // The band they announce and the one `sched_validate` accepts are the
    // same band, or a library asks for what it was told and is refused.
    let (lo, hi) = Syscall::rt_priority_bounds(SCHED_FIFO as usize).unwrap();
    assert!(Syscall::sched_validate(SCHED_FIFO, lo as i32, 0).is_ok());
    assert!(Syscall::sched_validate(SCHED_FIFO, hi as i32, 0).is_ok());
    assert_eq!(
        Syscall::sched_validate(SCHED_FIFO, hi as i32 + 1, 0),
        Err(LxError::EINVAL)
    );
}
