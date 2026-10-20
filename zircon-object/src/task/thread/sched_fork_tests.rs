//! What a cloned task starts with: [`sched_after_fork`] against the rule
//! `sched_fork` follows, and [`Thread::inherit_sched`] against the threads it
//! copies between.

extern crate std;

use super::sched_fork::sched_after_fork;
use super::*;
use crate::task::*;

fn a_thread() -> Arc<Thread> {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("a process");
    Thread::create(&proc, "a thread").expect("a thread")
}

#[test]
fn without_the_flag_a_child_keeps_everything() {
    for (policy, nice, rt) in [
        (SCHED_NORMAL, -20, 0),
        (SCHED_NORMAL, 19, 0),
        (SCHED_BATCH, -5, 0),
        (SCHED_IDLE, 3, 0),
        (SCHED_FIFO, 0, 50),
        (SCHED_RR, -3, 99),
    ] {
        assert_eq!(
            sched_after_fork(policy, nice, rt, false),
            (policy, nice, rt),
            "a child of a task in policy {} at nice {} prio {} without the flag",
            policy,
            nice,
            rt
        );
    }
}

#[test]
fn the_flag_makes_a_real_time_parents_child_an_ordinary_task_at_nice_0() {
    for policy in [SCHED_FIFO, SCHED_RR, SCHED_DEADLINE] {
        assert_eq!(
            sched_after_fork(policy, -7, 40, true),
            (SCHED_NORMAL, 0, 0),
            "policy {}",
            policy
        );
    }
}

#[test]
fn the_flag_leaves_a_fair_class_alone_and_takes_back_a_negative_nice() {
    for policy in [SCHED_NORMAL, SCHED_BATCH, SCHED_IDLE] {
        assert_eq!(sched_after_fork(policy, -20, 0, true), (policy, 0, 0));
        assert_eq!(sched_after_fork(policy, -1, 0, true), (policy, 0, 0));
        assert_eq!(sched_after_fork(policy, 0, 0, true), (policy, 0, 0));
        assert_eq!(
            sched_after_fork(policy, 7, 0, true),
            (policy, 7, 0),
            "a positive nice is not privilege, and stays"
        );
    }
}

#[test]
fn a_cloned_thread_starts_with_its_parents_mask_and_class() {
    let parent = a_thread();
    parent.set_affinity(0b0110).unwrap();
    parent.set_sched(SCHED_BATCH, 5, 0);
    let child = a_thread();

    child.inherit_sched(&parent);

    assert_eq!(child.affinity(), 0b0110);
    assert_eq!(child.sched_policy(), SCHED_BATCH);
    assert_eq!(child.sched_nice(), 5);
    assert!(!child.sched_reset_on_fork());
}

#[test]
fn a_cloned_thread_is_reset_when_its_parent_asked_and_does_not_keep_the_flag() {
    let parent = a_thread();
    parent.set_affinity(0b1000).unwrap();
    parent.set_sched(SCHED_FIFO, 0, 30);
    parent.set_sched_reset_on_fork(true);
    let child = a_thread();

    child.inherit_sched(&parent);

    assert_eq!(
        (
            child.sched_policy(),
            child.sched_nice(),
            child.sched_rt_priority()
        ),
        (SCHED_NORMAL, 0, 0)
    );
    assert!(
        !child.sched_reset_on_fork(),
        "the flag came across: a grandchild would be reset too"
    );
    assert_eq!(child.affinity(), 0b1000, "the flag does not touch the mask");
    assert!(
        parent.sched_reset_on_fork(),
        "the parent lost its flag by forking"
    );
}

#[test]
fn a_new_thread_starts_without_the_flag() {
    let t = a_thread();
    assert!(!t.sched_reset_on_fork());
    t.set_sched_reset_on_fork(true);
    assert!(t.sched_reset_on_fork());
    t.set_sched_reset_on_fork(false);
    assert!(!t.sched_reset_on_fork());
}

/// A real-time parent that did NOT ask for a reset hands its priority down
/// with its class. Only `SCHED_RESET_ON_FORK` takes it away, and every test
/// here with a priority in it set that flag: a child cloned from a
/// `SCHED_FIFO` thread at priority 30 kept the class and started at 0, which
/// in a fixed-priority class is not a thread that runs a little later, it is a
/// thread the scheduler never picks over any other FIFO task.
#[test]
fn a_cloned_thread_of_a_real_time_parent_keeps_its_priority() {
    let parent = a_thread();
    parent.set_sched(SCHED_FIFO, 0, 30);
    let child = a_thread();

    child.inherit_sched(&parent);

    assert_eq!(
        (
            child.sched_policy(),
            child.sched_nice(),
            child.sched_rt_priority()
        ),
        (SCHED_FIFO, 0, 30),
        "the child of a SCHED_FIFO task that never asked for a reset"
    );
}
