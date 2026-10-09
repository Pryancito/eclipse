//! `setpriority(2)`/`getpriority(2)`: who a `which`/`who` pair names, and
//! who is allowed to move the nice value once it has been named.
//!
//! Both halves were missing. `who` was read for `PRIO_PROCESS` only, so
//! `renice -g` and `renice -u` aimed at whoever called them, and no gate
//! asked anything at all, so any task could put itself at nice -20 and
//! renice anybody else's.

use super::*;

const USER: u32 = 1000;
const OTHER: u32 = 1001;
/// The nice range Linux allows, `MIN_NICE..=MAX_NICE`.
const NICE_RANGE: core::ops::RangeInclusive<i8> = -20..=19;

fn creds(ruid: u32, euid: u32) -> Credentials {
    Credentials {
        ruid,
        euid,
        suid: euid,
        rgid: ruid,
        egid: euid,
        sgid: euid,
        fsuid: euid,
        fsgid: euid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

// ---- the encoding -----------------------------------------------------

#[test]
fn the_nice_encoding_runs_backwards_so_the_most_favoured_task_is_the_biggest_number() {
    assert_eq!(LinuxProcess::nice_to_rlimit(19), 1, "the meekest task");
    assert_eq!(LinuxProcess::nice_to_rlimit(0), 20, "the default");
    assert_eq!(LinuxProcess::nice_to_rlimit(-20), 40, "the greediest");
    // Which is why getpriority takes a MAXIMUM over a group: the largest
    // number is the smallest nice.
    let group = [5i8, -3, 11];
    assert_eq!(
        group.iter().map(|&n| LinuxProcess::nice_to_rlimit(n)).max(),
        Some(LinuxProcess::nice_to_rlimit(-3)),
        "the best-off member is the one reported"
    );
}

#[test]
fn every_nice_value_encodes_to_at_least_one() {
    for nice in NICE_RANGE {
        assert!(
            LinuxProcess::nice_to_rlimit(nice) >= 1,
            "nice {} encodes to 0 or less",
            nice
        );
    }
}

// ---- the budget -------------------------------------------------------

#[test]
fn the_default_nice_budget_of_zero_reaches_no_nice_value_at_all() {
    // INIT_RLIMITS gives RLIMIT_NICE {0, 0}, and the encoding never goes
    // below 1, so an unprivileged task cannot lower its own nice by even
    // one step. This is Linux's behaviour and the whole reason
    // CAP_SYS_NICE exists.
    for nice in NICE_RANGE {
        assert!(
            !LinuxProcess::is_nice_reduction(0, nice),
            "budget 0 should reach nothing, but it reached nice {}",
            nice
        );
    }
}

#[test]
fn a_budget_reaches_exactly_down_to_nice_twenty_minus_itself() {
    // Budget 21 == nice_to_rlimit(-1): it reaches -1 and stops there.
    assert!(LinuxProcess::is_nice_reduction(21, -1));
    assert!(!LinuxProcess::is_nice_reduction(21, -2));
    // And the far end: 40 reaches everything.
    assert!(LinuxProcess::is_nice_reduction(40, -20));
}

#[test]
fn raising_the_budget_never_withdraws_a_nice_value_it_already_allowed() {
    for nice in NICE_RANGE {
        for budget in 0..=41u64 {
            if LinuxProcess::is_nice_reduction(budget, nice) {
                assert!(
                    LinuxProcess::is_nice_reduction(budget + 1, nice),
                    "budget {} allowed nice {} and {} did not",
                    budget,
                    nice,
                    budget + 1
                );
            }
        }
    }
}

#[test]
fn the_capability_reaches_where_no_budget_does() {
    let root = creds(ROOT_UID, ROOT_UID);
    let user = creds(USER, USER);
    assert!(
        LinuxProcess::can_nice(&root, 0, -20),
        "root with no budget still reaches the bottom"
    );
    assert!(
        !LinuxProcess::can_nice(&user, 0, -20),
        "a user with no budget reaches nothing"
    );
}

#[test]
fn the_budget_works_without_the_capability() {
    let user = creds(USER, USER);
    assert!(
        LinuxProcess::can_nice(&user, 40, -20),
        "a raised RLIMIT_NICE is the unprivileged way down"
    );
}

#[test]
fn it_is_the_callers_effective_uid_that_carries_the_capability() {
    // A root program that dropped its effective uid has stopped being
    // privileged, exactly as everywhere else in this kernel.
    let dropped = creds(ROOT_UID, USER);
    assert!(!LinuxProcess::can_nice(&dropped, 0, -1));
}

// ---- whose task is it -------------------------------------------------

#[test]
fn a_task_running_as_you_is_yours_to_renice() {
    let caller = creds(USER, USER);
    let target = creds(USER, USER);
    assert!(LinuxProcess::may_set_priority_of(&caller, &target));
}

#[test]
fn a_task_that_merely_turned_into_you_is_yours_too() {
    // target.euid == caller.euid is the second half of set_one_prio_perm.
    let caller = creds(USER, USER);
    let target = creds(OTHER, USER);
    assert!(LinuxProcess::may_set_priority_of(&caller, &target));
}

#[test]
fn a_setuid_program_stays_reniceable_by_the_user_who_started_it() {
    // Its real uid is still yours even though it is running as root: this
    // is why set_one_prio_perm compares the target's REAL uid and not only
    // its effective one.
    let caller = creds(USER, USER);
    let setuid_root = creds(USER, ROOT_UID);
    assert!(LinuxProcess::may_set_priority_of(&caller, &setuid_root));
}

#[test]
fn a_strangers_task_needs_the_capability() {
    let caller = creds(USER, USER);
    let stranger = creds(OTHER, OTHER);
    assert!(!LinuxProcess::may_set_priority_of(&caller, &stranger));
    let root = creds(ROOT_UID, ROOT_UID);
    assert!(LinuxProcess::may_set_priority_of(&root, &stranger));
}

#[test]
fn it_is_the_callers_effective_uid_that_is_compared_not_its_real_one() {
    // A caller that switched to OTHER reaches OTHER's tasks and loses its
    // own, which is the point of comparing cred->euid.
    let switched = creds(USER, OTHER);
    assert!(LinuxProcess::may_set_priority_of(
        &switched,
        &creds(OTHER, OTHER)
    ));
    assert!(!LinuxProcess::may_set_priority_of(
        &switched,
        &creds(USER, USER)
    ));
}

#[test]
fn reniceing_is_a_looser_test_than_touching_limits() {
    // prlimit64 wants every one of the target's ids to be the caller's
    // real id; setpriority wants one of two against the effective one. A
    // target part-way through a set-user-ID dance shows the difference.
    let caller = creds(USER, USER);
    let halfway = creds(USER, ROOT_UID);
    assert!(LinuxProcess::may_set_priority_of(&caller, &halfway));
    assert!(!LinuxProcess::may_touch_limits_of(&caller, &halfway));
}

// ---- the verdict ------------------------------------------------------

fn verdict(
    caller: &Credentials,
    target: &Credentials,
    target_nice: i8,
    budget: u64,
    nice: i8,
) -> LxResult<()> {
    LinuxProcess::set_priority_verdict(caller, target, target_nice, budget, nice)
}

#[test]
fn pushing_a_task_further_down_the_queue_is_free() {
    let user = creds(USER, USER);
    assert_eq!(verdict(&user, &user, 0, 0, 10), Ok(()));
}

#[test]
fn standing_still_is_free_too() {
    // niceval < task_nice(p) is strict: setting the value it already has
    // never asks for the budget.
    let user = creds(USER, USER);
    assert_eq!(verdict(&user, &user, 5, 0, 5), Ok(()));
}

#[test]
fn taking_it_back_is_not_free_and_says_eacces() {
    let user = creds(USER, USER);
    assert_eq!(verdict(&user, &user, 5, 0, 4), Err(LxError::EACCES));
}

#[test]
fn a_task_that_is_not_yours_is_eperm_before_the_budget_is_even_asked() {
    // Both gates would fire; EPERM is the one Linux reports, because
    // "that task is not yours" answers the question first.
    let caller = creds(USER, USER);
    let stranger = creds(OTHER, OTHER);
    assert_eq!(verdict(&caller, &stranger, 0, 0, -20), Err(LxError::EPERM));
}

#[test]
fn a_stranger_is_eperm_even_when_the_nice_value_goes_up() {
    // The first gate does not care which way the value moves.
    let caller = creds(USER, USER);
    let stranger = creds(OTHER, OTHER);
    assert_eq!(verdict(&caller, &stranger, 0, 40, 19), Err(LxError::EPERM));
}

#[test]
fn the_budget_that_opens_the_gate_is_the_targets_and_not_the_callers() {
    // can_nice(p, niceval) reads p's RLIMIT_NICE. The caller here has
    // nothing of its own; the task it is reniceing has room.
    let user = creds(USER, USER);
    assert_eq!(verdict(&user, &user, 0, 40, -20), Ok(()));
}

#[test]
fn root_may_take_any_task_all_the_way_back_to_minus_twenty() {
    let root = creds(ROOT_UID, ROOT_UID);
    let stranger = creds(OTHER, OTHER);
    assert_eq!(verdict(&root, &stranger, 19, 0, -20), Ok(()));
}

// ---- folding a group's verdicts --------------------------------------

fn fold_all(members: &[LxResult<()>]) -> LxResult<()> {
    members
        .iter()
        .copied()
        .fold(Err(LxError::ESRCH), LinuxProcess::fold_priority_verdict)
}

#[test]
fn a_set_nobody_could_be_found_in_stays_esrch() {
    // How a pgid that names no live process comes out as ESRCH: nothing
    // ever clears the value the walk starts with.
    assert_eq!(fold_all(&[]), Err(LxError::ESRCH));
    assert_eq!(
        fold_all(&[Err(LxError::ESRCH), Err(LxError::ESRCH)]),
        Err(LxError::ESRCH),
        "members that vanished mid-walk are no different"
    );
}

#[test]
fn one_success_turns_the_initial_esrch_into_success() {
    let r = LinuxProcess::fold_priority_verdict(Err(LxError::ESRCH), Ok(()));
    assert_eq!(r, Ok(()));
}

#[test]
fn a_failure_survives_every_later_success() {
    // The `if (error == -ESRCH) error = 0;` in set_one_prio only clears
    // the INITIAL value, so a group with one member out of reach reports
    // the failure however many members went through.
    let mut r: LxResult<()> = Err(LxError::ESRCH);
    r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EPERM));
    r = LinuxProcess::fold_priority_verdict(r, Ok(()));
    r = LinuxProcess::fold_priority_verdict(r, Ok(()));
    assert_eq!(r, Err(LxError::EPERM));
}

#[test]
fn a_failure_after_a_success_is_reported_too() {
    let mut r: LxResult<()> = Err(LxError::ESRCH);
    r = LinuxProcess::fold_priority_verdict(r, Ok(()));
    assert_eq!(r, Ok(()), "the first member went through");
    r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EPERM));
    assert_eq!(r, Err(LxError::EPERM));
}

#[test]
fn the_last_failure_is_the_one_reported() {
    // set_one_prio overwrites `error` outright on a failure, so of two
    // different failures it is the later one that reaches userspace.
    let mut r: LxResult<()> = Err(LxError::ESRCH);
    r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EACCES));
    r = LinuxProcess::fold_priority_verdict(r, Err(LxError::EPERM));
    assert_eq!(r, Err(LxError::EPERM));
}

// ---- who the pair names ----------------------------------------------

const OWN_TID: KoID = 3;
const OWN_PGID: KoID = 7;

#[test]
fn zero_means_mine_in_each_of_the_three_flavours() {
    assert_eq!(
        prio_target(PRIO_PROCESS, 0, OWN_TID, OWN_PGID, USER),
        Ok(PrioTarget::Thread(OWN_TID))
    );
    assert_eq!(
        prio_target(PRIO_PGRP, 0, OWN_TID, OWN_PGID, USER),
        Ok(PrioTarget::Group(OWN_PGID))
    );
    assert_eq!(
        prio_target(PRIO_USER, 0, OWN_TID, OWN_PGID, USER),
        Ok(PrioTarget::User(USER))
    );
}

#[test]
fn a_named_who_is_the_one_that_gets_used() {
    // This is the bug: `who` used to be read for PRIO_PROCESS only, so
    // `renice -g 99` and `renice -u 1001` aimed at the caller instead.
    assert_eq!(
        prio_target(PRIO_PROCESS, 42, OWN_TID, OWN_PGID, USER),
        Ok(PrioTarget::Thread(42))
    );
    assert_eq!(
        prio_target(PRIO_PGRP, 99, OWN_TID, OWN_PGID, USER),
        Ok(PrioTarget::Group(99)),
        "a named group is not the caller's group"
    );
    assert_eq!(
        prio_target(PRIO_USER, OTHER as usize, OWN_TID, OWN_PGID, USER),
        Ok(PrioTarget::User(OTHER)),
        "a named user is not the caller"
    );
}

#[test]
fn the_three_flavours_do_not_borrow_each_others_ids() {
    // Three different own-ids, so a flavour reaching for the wrong one
    // shows up instead of coinciding.
    for (which, want) in [
        (PRIO_PROCESS, PrioTarget::Thread(OWN_TID)),
        (PRIO_PGRP, PrioTarget::Group(OWN_PGID)),
        (PRIO_USER, PrioTarget::User(USER)),
    ] {
        assert_eq!(
            prio_target(which, 0, OWN_TID, OWN_PGID, USER),
            Ok(want),
            "which={} took the wrong id",
            which
        );
    }
}

#[test]
fn a_which_that_is_not_one_of_the_three_is_einval() {
    assert_eq!(
        prio_target(3, 0, OWN_TID, OWN_PGID, USER),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        prio_target(usize::MAX, 0, OWN_TID, OWN_PGID, USER),
        Err(LxError::EINVAL)
    );
}
