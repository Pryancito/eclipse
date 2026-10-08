//! `user_check_sched_setscheduler()`: who may change a task's scheduling
//! policy, its real-time priority, or its nice value through the
//! `sched_set*` family.
//!
//! Nothing asked before this. Any process could put itself on `SCHED_FIFO`
//! at priority 99 and keep the machine to itself, and could do it to
//! another user's threads too.

use super::*;

const USER: u32 = 1000;
const OTHER: u32 = 1001;

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

/// A task of USER's, on the default policy, with the boot limits: no nice
/// budget and no real-time budget at all.
fn on_the_default_policy() -> SchedFacts {
    SchedFacts {
        policy: SCHED_NORMAL,
        nice: 0,
        rt_priority: 0,
        rlimit_nice: 0,
        rlimit_rtprio: 0,
    }
}

fn want(policy: u8, nice: i8, rt_priority: u8) -> SchedRequest {
    SchedRequest {
        policy,
        nice,
        rt_priority,
    }
}

/// USER asking about a task of USER's.
fn verdict(now: &SchedFacts, want: &SchedRequest) -> LxResult<()> {
    let user = creds(USER, USER);
    LinuxProcess::may_set_scheduler(&user, &user, now, want)
}

// ---- the policy classes ----------------------------------------------

#[test]
fn sched_idle_is_not_one_of_the_fair_policies() {
    // Linux counts it apart, and that is what makes LEAVING it cost
    // something: idle sits below nice 19, so everything else is a step up.
    assert!(is_fair_policy(SCHED_NORMAL));
    assert!(is_fair_policy(SCHED_BATCH));
    assert!(!is_fair_policy(SCHED_IDLE));
    assert!(!is_fair_policy(SCHED_FIFO));
}

#[test]
fn only_fifo_and_rr_are_real_time() {
    assert!(is_rt_policy(SCHED_FIFO));
    assert!(is_rt_policy(SCHED_RR));
    assert!(!is_rt_policy(SCHED_NORMAL));
    assert!(!is_rt_policy(SCHED_BATCH));
    assert!(!is_rt_policy(SCHED_IDLE));
    assert!(!is_rt_policy(SCHED_DEADLINE));
}

// ---- the fair half ---------------------------------------------------

#[test]
fn asking_for_what_the_task_already_has_needs_nothing() {
    assert_eq!(
        verdict(&on_the_default_policy(), &want(SCHED_NORMAL, 0, 0)),
        Ok(())
    );
}

#[test]
fn a_task_may_always_make_itself_meeker() {
    assert_eq!(
        verdict(&on_the_default_policy(), &want(SCHED_NORMAL, 19, 0)),
        Ok(())
    );
}

#[test]
fn lowering_a_nice_value_this_way_says_eperm_where_setpriority_says_eacces() {
    // The same budget, spent through a different syscall, and Linux
    // reports it differently: `sched_setattr` has one errno for every one
    // of its reasons.
    let now = on_the_default_policy();
    assert_eq!(
        verdict(&now, &want(SCHED_NORMAL, -1, 0)),
        Err(LxError::EPERM)
    );
    let user = creds(USER, USER);
    assert_eq!(
        LinuxProcess::set_priority_verdict(&user, &user, now.nice, now.rlimit_nice, -1),
        Err(LxError::EACCES),
        "setpriority's own answer, for the same move"
    );
}

#[test]
fn a_nice_budget_pays_for_the_fair_half() {
    let generous = SchedFacts {
        rlimit_nice: LinuxProcess::nice_to_rlimit(-5),
        ..on_the_default_policy()
    };
    assert_eq!(verdict(&generous, &want(SCHED_NORMAL, -5, 0)), Ok(()));
    assert_eq!(
        verdict(&generous, &want(SCHED_NORMAL, -6, 0)),
        Err(LxError::EPERM),
        "one step past what the budget covers"
    );
}

// ---- the real-time half ----------------------------------------------

#[test]
fn entering_a_real_time_policy_with_no_budget_is_privileged() {
    assert_eq!(
        verdict(&on_the_default_policy(), &want(SCHED_FIFO, 0, 1)),
        Err(LxError::EPERM),
        "the lowest real-time priority there is, and still refused"
    );
}

#[test]
fn a_task_already_running_real_time_may_keep_its_priority_without_a_budget() {
    // The zero-budget rule only stops a CHANGE of policy. A task that is
    // already on SCHED_FIFO -- put there by root -- can call
    // sched_setparam with the priority it has.
    let running = SchedFacts {
        policy: SCHED_FIFO,
        rt_priority: 30,
        ..on_the_default_policy()
    };
    assert_eq!(verdict(&running, &want(SCHED_FIFO, 0, 30)), Ok(()));
}

#[test]
fn switching_between_the_two_real_time_policies_still_needs_a_budget() {
    // The zero-budget rule bars a CHANGE of policy, and FIFO to RR is one
    // even though the priority does not move -- so this is refused where
    // asking for the very same policy and priority would go through.
    let running = SchedFacts {
        policy: SCHED_FIFO,
        rt_priority: 30,
        ..on_the_default_policy()
    };
    assert_eq!(
        verdict(&running, &want(SCHED_RR, 0, 30)),
        Err(LxError::EPERM)
    );
    assert_eq!(
        verdict(&running, &want(SCHED_FIFO, 0, 30)),
        Ok(()),
        "staying put is the case this one is being told apart from"
    );
}

#[test]
fn a_real_time_task_may_always_give_priority_back() {
    let running = SchedFacts {
        policy: SCHED_FIFO,
        rt_priority: 50,
        ..on_the_default_policy()
    };
    assert_eq!(verdict(&running, &want(SCHED_FIFO, 0, 1)), Ok(()));
}

#[test]
fn raising_a_real_time_priority_is_capped_by_the_budget() {
    let running = SchedFacts {
        policy: SCHED_FIFO,
        rt_priority: 10,
        rlimit_rtprio: 20,
        ..on_the_default_policy()
    };
    assert_eq!(verdict(&running, &want(SCHED_FIFO, 0, 20)), Ok(()));
    assert_eq!(
        verdict(&running, &want(SCHED_FIFO, 0, 21)),
        Err(LxError::EPERM)
    );
}

#[test]
fn a_real_time_budget_lets_a_task_in_without_the_capability() {
    let ready = SchedFacts {
        rlimit_rtprio: 10,
        ..on_the_default_policy()
    };
    assert_eq!(verdict(&ready, &want(SCHED_RR, 0, 10)), Ok(()));
    assert_eq!(
        verdict(&ready, &want(SCHED_RR, 0, 11)),
        Err(LxError::EPERM),
        "above the budget, even on the way in"
    );
}

#[test]
fn the_nice_value_carried_along_is_not_checked_under_a_real_time_policy() {
    // fair_policy(policy) is false for FIFO, so the nice clause does not
    // run; the value is stored and means nothing until the task goes back
    // to a fair policy. This is Linux's behaviour, not an oversight here.
    let ready = SchedFacts {
        rlimit_rtprio: 10,
        ..on_the_default_policy()
    };
    assert_eq!(verdict(&ready, &want(SCHED_FIFO, -20, 10)), Ok(()));
}

// ---- leaving SCHED_IDLE ----------------------------------------------

#[test]
fn staying_in_sched_idle_is_free() {
    let idle = SchedFacts {
        policy: SCHED_IDLE,
        ..on_the_default_policy()
    };
    assert_eq!(verdict(&idle, &want(SCHED_IDLE, 0, 0)), Ok(()));
}

#[test]
fn leaving_sched_idle_costs_the_nice_value_the_task_already_has() {
    // Not the nice value asked for: the one it has. Coming off idle is
    // itself the step up, so the budget has to cover where the task
    // lands.
    let idle = SchedFacts {
        policy: SCHED_IDLE,
        ..on_the_default_policy()
    };
    assert_eq!(
        verdict(&idle, &want(SCHED_NORMAL, 0, 0)),
        Err(LxError::EPERM)
    );
    let idle_with_budget = SchedFacts {
        rlimit_nice: LinuxProcess::nice_to_rlimit(0),
        ..idle
    };
    assert_eq!(
        verdict(&idle_with_budget, &want(SCHED_NORMAL, 0, 0)),
        Ok(())
    );
}

#[test]
fn leaving_sched_idle_is_judged_on_the_nice_it_has_and_not_the_one_asked_for() {
    // A task parked on SCHED_IDLE at nice -5, asking for SCHED_NORMAL at
    // nice 19, is asking to be MEEKER -- and is still refused. Coming off
    // idle lands it at -5 whatever it says, and -5 is what the budget has
    // to cover.
    let idle = SchedFacts {
        policy: SCHED_IDLE,
        nice: -5,
        rlimit_nice: LinuxProcess::nice_to_rlimit(19),
        ..on_the_default_policy()
    };
    assert_eq!(
        verdict(&idle, &want(SCHED_NORMAL, 19, 0)),
        Err(LxError::EPERM)
    );
    let enough = SchedFacts {
        rlimit_nice: LinuxProcess::nice_to_rlimit(-5),
        ..idle
    };
    assert_eq!(verdict(&enough, &want(SCHED_NORMAL, 19, 0)), Ok(()));
}

// ---- whose task, and the way past ------------------------------------

#[test]
fn another_users_task_is_eperm_however_modest_the_request() {
    // Same policy, same nice, nothing asked for: it is still not yours.
    let caller = creds(USER, USER);
    let stranger = creds(OTHER, OTHER);
    assert_eq!(
        LinuxProcess::may_set_scheduler(
            &caller,
            &stranger,
            &on_the_default_policy(),
            &want(SCHED_NORMAL, 0, 0)
        ),
        Err(LxError::EPERM)
    );
}

#[test]
fn a_setuid_program_stays_schedulable_by_the_user_who_started_it() {
    let caller = creds(USER, USER);
    let setuid_root = creds(USER, ROOT_UID);
    assert_eq!(
        LinuxProcess::may_set_scheduler(
            &caller,
            &setuid_root,
            &on_the_default_policy(),
            &want(SCHED_NORMAL, 0, 0)
        ),
        Ok(())
    );
}

#[test]
fn the_capability_is_the_one_way_past_every_one_of_these() {
    let root = creds(ROOT_UID, ROOT_UID);
    let stranger = creds(OTHER, OTHER);
    let idle = SchedFacts {
        policy: SCHED_IDLE,
        ..on_the_default_policy()
    };
    // Another user's task, coming off idle, straight to the top of the
    // real-time range, with every budget at zero.
    assert_eq!(
        LinuxProcess::may_set_scheduler(&root, &stranger, &idle, &want(SCHED_FIFO, -20, 99)),
        Ok(())
    );
}

#[test]
fn sched_deadline_is_privileged_outright() {
    // Unreachable through the syscalls today -- the parameter check
    // refuses it with EINVAL first, there being no deadline runqueue --
    // but the rule belongs with the others.
    assert_eq!(
        verdict(&on_the_default_policy(), &want(SCHED_DEADLINE, 0, 0)),
        Err(LxError::EPERM)
    );
    let root = creds(ROOT_UID, ROOT_UID);
    assert_eq!(
        LinuxProcess::may_set_scheduler(
            &root,
            &root,
            &on_the_default_policy(),
            &want(SCHED_DEADLINE, 0, 0)
        ),
        Ok(())
    );
}
