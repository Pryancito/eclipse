//! The leader kept after its `exit(2)`, and the two ways it is let go. The
//! thread holds its process, so a leader nobody let go would keep the
//! process, and itself, alive for good.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

/// A process with its leader gone the way `exit(2)` takes it, and one
/// worker left running.
fn the_leader_gone_first(pid: KoID) -> (Arc<Process>, Weak<Thread>, Arc<Thread>) {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    let leader = Thread::create_linux(&proc).unwrap();
    let worker = Thread::create_linux(&proc).unwrap();
    assert_eq!(leader.id(), pid, "the premise: the leader's tid is the pid");
    proc.linux().keep_exited_leader(leader.clone());
    leader.terminate_abandoned();
    (proc, Arc::downgrade(&leader), worker)
}

#[test]
fn an_exited_leader_is_a_task_until_the_last_thread_goes() {
    let (proc, leader, worker) = the_leader_gone_first(46_911);
    let pid = proc.id();
    assert_eq!(proc.thread_ids(), [worker.id()]);
    assert_eq!(task_count(&proc), 2);
    assert_eq!(task_ids(&proc), [pid, worker.id()]);
    assert!(Arc::ptr_eq(
        &task_of(&proc, pid).unwrap(),
        &leader.upgrade().unwrap()
    ));
    assert!(Arc::ptr_eq(&task_of(&proc, worker.id()).unwrap(), &worker));
    assert!(task_of(&proc, pid + 1_000).is_none());

    worker.terminate_abandoned();
    assert!(matches!(proc.status(), Status::Exited(_)));
    assert!(
        exited_leader_of(&proc).is_none(),
        "the process died holding it"
    );
    assert!(leader.upgrade().is_none(), "something else still holds it");
}

/// `de_thread`: an `execve` from the worker gives the pid to a new leader,
/// and the old one, already gone, is let go -- or the pid would name two
/// tasks.
#[test]
fn an_exec_from_another_thread_lets_the_old_leader_go() {
    let (proc, leader, worker) = the_leader_gone_first(46_912);
    let pid = proc.id();

    let successor = worker.successor_as_leader().unwrap();

    assert!(exited_leader_of(&proc).is_none());
    assert!(leader.upgrade().is_none(), "something else still holds it");
    let mut ids = task_ids(&proc);
    ids.sort_unstable();
    let mut expected = alloc::vec![pid, worker.id()];
    expected.sort_unstable();
    assert_eq!(ids, expected, "the pid once, the worker until it exits");
    assert!(Arc::ptr_eq(&task_of(&proc, pid).unwrap(), &successor));
    worker.terminate_abandoned();
    successor.terminate_abandoned();
}

/// The same for a forked child, whose process dies by another path.
#[test]
fn a_forked_child_lets_its_leader_go_when_it_dies() {
    let parent = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 46_913).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let leader = Thread::create_linux(&child).unwrap();
    let worker = Thread::create_linux(&child).unwrap();
    assert_eq!(leader.id(), child.id());
    child.linux().keep_exited_leader(leader.clone());
    leader.terminate_abandoned();
    let kept = Arc::downgrade(&leader);
    drop(leader);
    assert_eq!(task_count(&child), 2);

    worker.terminate_abandoned();
    assert!(
        exited_leader_of(&child).is_none(),
        "the process died holding it"
    );
    assert!(kept.upgrade().is_none(), "something else still holds it");
    parent.exit(0);
}

/// A `setpriority` or `sched_setaffinity` that found the kept leader
/// changed the object; the zombie the process becomes answers with that,
/// not with what the leader had when it left the thread list.
#[test]
fn what_the_kept_leader_was_given_is_what_its_zombie_keeps() {
    let (proc, leader, worker) = the_leader_gone_first(46_914);
    assert_eq!(
        proc.leader_sched().map(|s| s.nice),
        Some(0),
        "what it had when it left the list"
    );
    let kept = leader.upgrade().unwrap();
    kept.set_sched(SCHED_BATCH, 9, 0);
    kept.set_affinity(0b100).unwrap();
    drop(kept);

    worker.terminate_abandoned();
    let filed = proc.leader_sched().unwrap();
    assert_eq!(
        (filed.policy, filed.nice, filed.affinity),
        (SCHED_BATCH, 9, 0b100)
    );
    assert!(leader.upgrade().is_none());
}
