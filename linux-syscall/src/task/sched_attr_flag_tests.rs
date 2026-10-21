//! `sched_setattr(2)`'s `sched_flags`, which were never read.

use super::*;

/// A flag Linux does not know is `EINVAL`; the known ones are not, alone
/// or together. The plan does not look: Linux finds the task first.
#[test]
fn an_unknown_flag_is_einval() {
    assert_eq!(sched_flags_known(0x80), Err(LxError::EINVAL));
    assert_eq!(sched_flags_known(1 << 63), Err(LxError::EINVAL));
    assert_eq!(
        sched_flags_known(SCHED_FLAG_KEEP_POLICY | 0x100),
        Err(LxError::EINVAL)
    );
    assert!(sched_flags_known(SCHED_FLAG_ALL).is_ok());
    assert!(sched_flags_known(SCHED_FLAG_RESET_ON_FORK).is_ok());
    assert!(
        sched_setattr_plan(0x80, 0).is_ok(),
        "not the plan's to refuse"
    );
    assert_eq!(SCHED_FLAG_UTIL_CLAMP, 0x60);
    // Their uapi values.
    assert_eq!(SCHED_FLAG_RESET_ON_FORK, 0x01);
    assert_eq!(SCHED_FLAG_KEEP_POLICY, 0x08);
    assert_eq!(SCHED_FLAG_KEEP_PARAMS, 0x10);
    assert_eq!(SCHED_FLAG_ALL, 0x7f);
}

/// `SCHED_FLAG_KEEP_POLICY` keeps the thread's policy whatever
/// `sched_policy` says; without it `sched_policy` is the policy.
#[test]
fn keep_policy_ignores_the_policy_in_the_struct() {
    assert_eq!(
        sched_setattr_plan(SCHED_FLAG_KEEP_POLICY, 0),
        Ok(SchedAttrPlan {
            policy: None,
            keep_params: false
        })
    );
    assert_eq!(
        sched_setattr_plan(SCHED_FLAG_KEEP_POLICY, 1),
        Ok(SchedAttrPlan {
            policy: None,
            keep_params: false
        })
    );
    assert_eq!(
        sched_setattr_plan(0, 1),
        Ok(SchedAttrPlan {
            policy: Some(1),
            keep_params: false
        })
    );
    assert_eq!(
        sched_setattr_plan(SCHED_FLAG_KEEP_PARAMS, 2),
        Ok(SchedAttrPlan {
            policy: Some(2),
            keep_params: true
        })
    );
    assert_eq!(
        sched_setattr_plan(SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS, 2),
        Ok(SchedAttrPlan {
            policy: None,
            keep_params: true
        })
    );
}

/// `(int)attr.sched_policy < 0` is `EINVAL`, even under `KEEP_POLICY`,
/// and before the flags are looked at.
#[test]
fn a_negative_policy_is_einval_before_the_flags() {
    assert_eq!(sched_setattr_plan(0, 0x8000_0000), Err(LxError::EINVAL));
    assert_eq!(sched_setattr_plan(0, u32::MAX), Err(LxError::EINVAL));
    assert_eq!(
        sched_setattr_plan(SCHED_FLAG_KEEP_POLICY, u32::MAX),
        Err(LxError::EINVAL)
    );
    assert!(
        sched_setattr_plan(0, 0x7fff_ffff).is_ok(),
        "the sign, not the range"
    );
}
