//! The sixteen resource limits, and who may move them.
//!
//! Twelve of the sixteen used to answer `ENOSYS` and three more answered
//! with a constant, whatever the caller had set. And the hard limit was
//! not a limit: an earlier batch left a test saying so on the record --
//! *"this kernel has no capability model to ask"* -- because Linux's
//! fourth rule needs `CAP_SYS_RESOURCE`. It has one now, so the rule is
//! here and that test has become its opposite.

use super::dup_fd_tests::a_process;
use super::*;

const USER: u32 = 1000;
const OTHER: u32 = 2000;

fn limit(cur: u64, max: u64) -> RLimit {
    RLimit { cur, max }
}

/// Ordinary credentials: every id the same, so two processes built this
/// way with the same numbers can reach each other.
fn creds(ruid: u32, euid: u32, suid: u32) -> Credentials {
    Credentials {
        ruid,
        euid,
        suid,
        rgid: ruid,
        egid: euid,
        sgid: suid,
        fsuid: euid,
        fsgid: euid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

fn check(resource: usize, old: RLimit, new: RLimit, may_raise: bool) -> LxResult {
    LinuxProcess::rlimit_check(resource, old, new, may_raise)
}

// ---- the four rules ----------------------------------------------

#[test]
fn a_soft_limit_above_the_hard_one_is_einval() {
    // The rule that makes the hard limit mean anything at all.
    assert_eq!(
        check(RLIMIT_NOFILE, limit(1024, 1024), limit(4096, 1024), true),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        check(RLIMIT_CORE, limit(0, RLIM_INFINITY), limit(1, 0), true),
        Err(LxError::EINVAL)
    );
}

#[test]
fn a_soft_limit_equal_to_the_hard_one_is_fine() {
    // The boundary is `>`, not `>=`: setting both to the same value is
    // what a process does when it raises itself to its hard limit, the
    // single most common `setrlimit` call there is.
    assert_eq!(
        check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 4096), false),
        Ok(())
    );
}

#[test]
fn the_descriptor_ceiling_is_eperm_and_applies_to_that_resource_alone() {
    // `if (resource == RLIMIT_NOFILE && new_rlim->rlim_max >
    // sysctl_nr_open) return -EPERM;`. EPERM and not EINVAL, and the
    // difference is load-bearing: a caller that sees EPERM retries with a
    // smaller number, one that sees EINVAL concludes it built the struct
    // wrong.
    assert_eq!(
        check(
            RLIMIT_NOFILE,
            limit(0, RLIM_INFINITY),
            limit(NR_OPEN + 1, NR_OPEN + 1),
            true
        ),
        Err(LxError::EPERM)
    );
    assert_eq!(
        check(
            RLIMIT_MEMLOCK,
            limit(0, RLIM_INFINITY),
            limit(NR_OPEN + 1, NR_OPEN + 1),
            true
        ),
        Ok(()),
        "a million is not a lot of bytes, and this rule is about descriptors"
    );
}

#[test]
fn the_descriptor_ceiling_itself_is_accepted() {
    assert_eq!(
        check(
            RLIMIT_NOFILE,
            limit(0, RLIM_INFINITY),
            limit(NR_OPEN, NR_OPEN),
            true
        ),
        Ok(())
    );
}

#[test]
fn the_order_of_the_rules_is_linuxs() {
    // Both wrong at once: `cur > max` is checked first, so this is EINVAL
    // and not EPERM. A caller that retries on EPERM would otherwise spin
    // on a struct that is never going to be accepted.
    assert_eq!(
        check(
            RLIMIT_NOFILE,
            limit(0, RLIM_INFINITY),
            limit(RLIM_INFINITY, NR_OPEN + 1),
            true
        ),
        Err(LxError::EINVAL)
    );
}

#[test]
fn the_hard_limit_is_a_boundary_now() {
    // `if (new_rlim->rlim_max > rlim->rlim_max && !capable(CAP_SYS_RESOURCE))
    //         retval = -EPERM;`
    //
    // Without it a process that wanted a soft limit past its hard one set
    // both at once and got it, so the hard limit was a lid the process
    // could lift. This is the test that used to assert the opposite.
    assert_eq!(
        check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 8192), false),
        Err(LxError::EPERM)
    );
    assert_eq!(
        check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 8192), true),
        Ok(()),
        "and CAP_SYS_RESOURCE is what lifts it"
    );
}

#[test]
fn lowering_the_hard_limit_needs_nothing_and_does_not_come_back() {
    // A one-way ratchet is the point of the thing: a program drops its
    // own ceiling before running something it does not trust, and that
    // something cannot undo it.
    assert_eq!(
        check(RLIMIT_NOFILE, limit(1024, 4096), limit(64, 64), false),
        Ok(())
    );
    assert_eq!(
        check(RLIMIT_NOFILE, limit(64, 64), limit(64, 4096), false),
        Err(LxError::EPERM)
    );
}

#[test]
fn a_hard_limit_that_does_not_move_is_not_a_raise() {
    // The comparison is `>`, so re-setting the same hard limit while
    // moving the soft one is the ordinary unprivileged call.
    assert_eq!(
        check(RLIMIT_NOFILE, limit(1024, 4096), limit(4096, 4096), false),
        Ok(())
    );
}

// ---- the sixteen resources ---------------------------------------

#[test]
fn the_resource_numbers_are_the_ones_resource_h_names() {
    // The order IS the numbering, so a row out of place is a program
    // asking about its stack and being told about its core dumps.
    const IN_ORDER: [(usize, &str); RLIM_NLIMITS] = [
        (RLIMIT_CPU, "RLIMIT_CPU"),
        (RLIMIT_FSIZE, "RLIMIT_FSIZE"),
        (RLIMIT_DATA, "RLIMIT_DATA"),
        (RLIMIT_STACK, "RLIMIT_STACK"),
        (RLIMIT_CORE, "RLIMIT_CORE"),
        (RLIMIT_RSS, "RLIMIT_RSS"),
        (RLIMIT_NPROC, "RLIMIT_NPROC"),
        (RLIMIT_NOFILE, "RLIMIT_NOFILE"),
        (RLIMIT_MEMLOCK, "RLIMIT_MEMLOCK"),
        (RLIMIT_AS, "RLIMIT_AS"),
        (RLIMIT_LOCKS, "RLIMIT_LOCKS"),
        (RLIMIT_SIGPENDING, "RLIMIT_SIGPENDING"),
        (RLIMIT_MSGQUEUE, "RLIMIT_MSGQUEUE"),
        (RLIMIT_NICE, "RLIMIT_NICE"),
        (RLIMIT_RTPRIO, "RLIMIT_RTPRIO"),
        (RLIMIT_RTTIME, "RLIMIT_RTTIME"),
    ];
    for (index, (number, name)) in IN_ORDER.iter().enumerate() {
        assert_eq!(*number, index, "{} is not number {}", name, index);
    }
    assert_eq!(RLIM_NLIMITS, 16);
}

#[test]
fn every_resource_answers_and_the_seventeenth_is_einval() {
    // `ulimit -a` walks all sixteen. Twelve of them used to answer
    // `ENOSYS`, which a program reads as "this kernel has no such call"
    // rather than "no such resource".
    let proc = a_process();
    for resource in 0..RLIM_NLIMITS {
        assert!(
            proc.rlimit(resource, None, false).is_ok(),
            "resource {} has no answer",
            resource
        );
    }
    assert_eq!(
        proc.rlimit(RLIM_NLIMITS, None, false),
        Err(LxError::EINVAL),
        "past the end is EINVAL, which is what `do_prlimit` says"
    );
    assert_eq!(proc.rlimit(usize::MAX, None, false), Err(LxError::EINVAL));
}

#[test]
fn what_a_process_starts_with_is_the_table_linux_starts_with() {
    let proc = a_process();
    let at = |r| proc.rlimit(r, None, false).unwrap();
    assert_eq!(at(RLIMIT_NOFILE), limit(1024, 4096), "INR_OPEN_CUR/MAX");
    assert_eq!(at(RLIMIT_STACK), limit(USER_STACK_SIZE, RLIM_INFINITY));
    assert_eq!(
        at(RLIMIT_CORE),
        limit(0, RLIM_INFINITY),
        "no core is ever written, so zero is the size and not a policy"
    );
    assert_eq!(at(RLIMIT_NICE), limit(0, 0));
    assert_eq!(at(RLIMIT_RTPRIO), limit(0, 0));
    assert_eq!(at(RLIMIT_AS), limit(RLIM_INFINITY, RLIM_INFINITY));
    assert_eq!(
        at(RLIMIT_NPROC),
        limit(RLIM_INFINITY, RLIM_INFINITY),
        "Linux's zero is a placeholder init overwrites; nothing counts here"
    );
}

#[test]
fn a_limit_that_is_set_is_the_limit_that_is_read_back() {
    // What three of the four answered resources did not do: they reported
    // a constant, so a program that lowered `RLIMIT_AS` and read it back
    // was told its own request had not happened -- and no error said so.
    let proc = a_process();
    let wanted = limit(64 * 1024 * 1024, 128 * 1024 * 1024);
    assert_eq!(
        proc.rlimit(RLIMIT_AS, Some(wanted), false),
        Ok(limit(RLIM_INFINITY, RLIM_INFINITY)),
        "and the answer is the limit as it WAS"
    );
    assert_eq!(proc.rlimit(RLIMIT_AS, None, false), Ok(wanted));
}

#[test]
fn a_refused_change_leaves_the_limit_where_it_was() {
    let proc = a_process();
    let before = proc.rlimit(RLIMIT_NOFILE, None, false).unwrap();
    assert_eq!(
        proc.rlimit(RLIMIT_NOFILE, Some(limit(8192, 8192)), false),
        Err(LxError::EPERM)
    );
    assert_eq!(proc.rlimit(RLIMIT_NOFILE, None, false), Ok(before));
}

#[test]
fn the_soft_limit_is_what_closes_the_descriptor_table() {
    // The hard limit is a ceiling on what may be ASKED for; the soft one
    // is the budget in force. A table that checked the hard limit would
    // hand out descriptors a program had deliberately stopped itself
    // taking, and `EMFILE` would arrive four thousand files later than
    // the program arranged.
    let proc = a_process();
    proc.rlimit(RLIMIT_NOFILE, Some(limit(2, 4096)), false)
        .unwrap();
    let open =
        || super::dup_fd_tests::an_open_log(super::dup_fd_tests::Log::new(), OpenFlags::WRONLY);
    assert!(proc.add_file(open()).is_ok());
    assert!(proc.add_file(open()).is_ok());
    assert_eq!(proc.add_file(open()), Err(LxError::EMFILE));
}

#[test]
fn the_descriptor_limit_that_is_stored_is_the_one_that_is_enforced() {
    // One table, one row: there is no second place for the number the fd
    // table checks to drift away from the number `getrlimit` reports.
    let proc = a_process();
    proc.rlimit(RLIMIT_NOFILE, Some(limit(64, 4096)), false)
        .unwrap();
    assert_eq!(proc.file_limit(), limit(64, 4096));
}

// ---- whose limits they are ---------------------------------------

#[test]
fn a_process_with_the_very_same_ids_is_reachable() {
    let caller = creds(USER, USER, USER);
    let target = creds(USER, USER, USER);
    assert!(LinuxProcess::may_touch_limits_of(&caller, &target));
}

#[test]
fn another_users_process_is_not() {
    let caller = creds(USER, USER, USER);
    let target = creds(OTHER, OTHER, OTHER);
    assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
}

#[test]
fn a_target_part_way_through_a_set_user_id_dance_is_out_of_reach() {
    // `id_match` is all six comparisons, not "same user": a target that
    // still holds root in its saved uid is beyond the user who started
    // it, because raising its limits would raise the limits of whatever
    // it is about to become.
    let caller = creds(USER, USER, USER);
    let target = creds(USER, USER, ROOT_UID);
    assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
    let target = creds(USER, ROOT_UID, USER);
    assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
}

#[test]
fn it_is_the_callers_real_ids_that_are_compared() {
    // `cred->uid`, not `cred->euid`. A set-user-ID program does not get
    // to reach further than the user who ran it just by holding an
    // effective id; what it gets instead is `CAP_SYS_RESOURCE`, and only
    // if that effective id is root.
    //
    // The caller here differs from the target in its real UID and in
    // NOTHING else, so the effective ids alone would say yes.
    let mut caller = creds(USER, USER, USER);
    caller.ruid = OTHER;
    let target = creds(USER, USER, USER);
    assert!(
        !LinuxProcess::may_touch_limits_of(&caller, &target),
        "the effective id matches and the real one does not"
    );
    let mut caller = creds(USER, USER, USER);
    caller.rgid = OTHER;
    assert!(
        !LinuxProcess::may_touch_limits_of(&caller, &target),
        "and the same on the group side"
    );
}

#[test]
fn the_group_half_is_compared_too() {
    let caller = creds(USER, USER, USER);
    let mut target = creds(USER, USER, USER);
    target.sgid = OTHER;
    assert!(!LinuxProcess::may_touch_limits_of(&caller, &target));
}

#[test]
fn cap_sys_resource_reaches_anybody() {
    let root = creds(ROOT_UID, ROOT_UID, ROOT_UID);
    let target = creds(OTHER, USER, ROOT_UID);
    assert!(LinuxProcess::may_touch_limits_of(&root, &target));
}

#[test]
fn a_root_real_uid_without_a_root_effective_one_does_not() {
    // The capability comes from the EFFECTIVE id, so a root-owned program
    // that has dropped to a user is on the id-match path like anyone
    // else.
    let dropped = creds(ROOT_UID, USER, USER);
    let target = creds(OTHER, OTHER, OTHER);
    assert!(!LinuxProcess::may_touch_limits_of(&dropped, &target));
}
