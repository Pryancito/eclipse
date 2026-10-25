//! The leader's scheduling attributes stay on file when it leaves the
//! thread list, and only the leader's.

use super::*;
use crate::task::{Job, SCHED_BATCH, SCHED_FIFO};

/// The thread that wears `proc`'s id, as `create_linux` makes a leader.
fn its_leader(proc: &Arc<Process>) -> Arc<Thread> {
    Thread::create_with_ext_id(proc, "leader", (), Some(proc.id())).unwrap()
}

#[test]
fn the_leader_leaves_what_it_had_and_no_other_thread_does() {
    let proc = Process::create(&Job::root(), "leader-sched").unwrap();
    let leader = its_leader(&proc);
    let other = Thread::create(&proc, "other").unwrap();
    leader.set_sched(SCHED_BATCH, 7, 0);
    leader.set_sched_reset_on_fork(true);
    leader.set_affinity(0b10).unwrap();
    other.set_sched(SCHED_FIFO, 0, 30);
    other.set_affinity(0b100).unwrap();
    assert_eq!(proc.leader_sched(), None, "the leader is still there");

    other.terminate_abandoned();
    assert_eq!(proc.leader_sched(), None, "another thread left it");

    leader.terminate_abandoned();
    assert_eq!(
        proc.leader_sched(),
        Some(SchedSnapshot {
            policy: SCHED_BATCH,
            nice: 7,
            rt_priority: 0,
            reset_on_fork: true,
            affinity: 0b10,
        })
    );
}

#[test]
fn a_leader_that_leaves_last_or_first_leaves_the_same() {
    let last = Process::create(&Job::root(), "leader-last").unwrap();
    let leader = its_leader(&last);
    leader.set_sched(SCHED_BATCH, 3, 0);
    leader.terminate_abandoned();
    assert_eq!(proc_nice(&last), Some(3), "the only thread");

    let first = Process::create(&Job::root(), "leader-first").unwrap();
    let leader = its_leader(&first);
    let other = Thread::create(&first, "other").unwrap();
    leader.set_sched(SCHED_BATCH, -4, 0);
    leader.terminate_abandoned();
    assert_eq!(proc_nice(&first), Some(-4), "before the rest of its group");
    other.terminate_abandoned();
    assert_eq!(proc_nice(&first), Some(-4), "the last thread changed it");
}

fn proc_nice(proc: &Process) -> Option<i8> {
    proc.leader_sched().map(|s| s.nice)
}

#[test]
fn a_setter_changes_what_is_on_file_and_only_when_there_is_something() {
    let proc = Process::create(&Job::root(), "leader-update").unwrap();
    assert!(
        !proc.update_leader_sched(|s| s.nice = 5),
        "a leader still there was changed on file"
    );
    assert_eq!(proc.leader_sched(), None);

    its_leader(&proc).terminate_abandoned();
    assert!(proc.update_leader_sched(|s| s.nice = 5));
    assert_eq!(proc_nice(&proc), Some(5));
    proc.set_leader_sched(SchedSnapshot {
        affinity: 1,
        ..SchedSnapshot::default()
    });
    assert_eq!(
        proc.leader_sched().map(|s| (s.nice, s.affinity)),
        Some((0, 1))
    );
}

#[test]
fn a_snapshot_starts_where_a_thread_does() {
    let proc = Process::create(&Job::root(), "leader-default").unwrap();
    let fresh = Thread::create(&proc, "fresh").unwrap();
    assert_eq!(SchedSnapshot::of(&fresh), SchedSnapshot::default());
}
