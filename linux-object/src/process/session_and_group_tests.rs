//! The two ids job control reads out of a process: its session and its
//! process group, each stored raw with `0` meaning "my own pid".
//!
//! The raw-to-effective resolution is what makes a `Ctrl-C` reach a
//! shell's foreground child, and `getpgid`/`getsid` answer with it, but
//! neither entry point had a test: the suite stopped at the `setsid`
//! verdict and never read a value back out.

use super::*;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap()
}

#[test]
fn an_unset_group_and_session_resolve_to_the_processs_own_pid() {
    let proc = a_process(46_101);
    assert_eq!(proc.linux().pgid_raw(), 0, "stored unset");
    assert_eq!(proc.linux().sid_raw(), 0);
    assert_eq!(effective_pgid(&proc), proc.id());
    assert_eq!(effective_sid(&proc), proc.id());
}

#[test]
fn a_group_that_was_set_is_the_one_that_answers() {
    let proc = a_process(46_102);
    proc.linux().set_pgid_raw(46_102);
    assert_eq!(effective_pgid(&proc), 46_102);
    // `setpgid(0, 0)` stores the argument as given; the resolution turns
    // it back into the own pid rather than into group zero.
    proc.linux().set_pgid_raw(0);
    assert_eq!(effective_pgid(&proc), proc.id());
}

/// `setsid` moves both ids at once: a session leader that kept its old
/// group would leave the new session sharing a group with the old one, and
/// job control reads the group.
#[test]
fn becoming_a_session_leader_moves_both_ids_together() {
    let leader = a_process(46_103);
    leader.linux().set_pgid_raw(46_000);
    assert_eq!(leader.linux().sid_raw(), 0, "no session of its own yet");
    leader.linux().become_session_leader(leader.id());
    assert_eq!(leader.linux().sid_raw(), leader.id());
    assert_eq!(leader.linux().pgid_raw(), leader.id());
    assert_eq!(effective_sid(&leader), leader.id());
    assert_eq!(effective_pgid(&leader), leader.id());
}

/// A fork resolves both ids into concrete numbers for the child, so a
/// parent that never called `setpgid` still passes on a group a `Ctrl-C`
/// can name.
#[test]
fn a_child_inherits_its_parents_group_and_session_concretely() {
    let parent = a_process(46_104);
    assert_eq!(parent.linux().pgid_raw(), 0, "the parent's is unset");
    let child = Process::fork_from(&parent).unwrap();

    assert_eq!(
        child.linux().pgid_raw(),
        parent.id(),
        "the child's group must be a number, not another unset"
    );
    assert_eq!(child.linux().sid_raw(), parent.id());
    assert_eq!(effective_pgid(&child), parent.id());
    assert_eq!(effective_sid(&child), parent.id());
}

/// The two getters are asked about a process whose group and session are
/// NOT its own pid, and whose group and session differ from each other.
/// Asked about a leader, `Ok(proc.id())` is the right answer for every
/// wrong reason: a getter that ignored the stored ids and answered with
/// the pid would pass.
#[test]
fn getpgid_and_getsid_answer_with_the_effective_values() {
    let parent = a_process(46_105);
    // Unset: the own pid is the answer, and here it is the only one.
    assert_eq!(get_process_pgid(parent.id()), Ok(parent.id()));
    assert_eq!(get_process_sid(parent.id()), Ok(parent.id()));

    // A forked child inherits both ids as the PARENT's pid, so its own pid
    // is now the wrong answer to both questions.
    let child = Process::fork_from(&parent).unwrap();
    assert_ne!(child.id(), parent.id());
    assert_eq!(get_process_pgid(child.id()), Ok(parent.id()));
    assert_eq!(get_process_sid(child.id()), Ok(parent.id()));

    // And with the group moved off the session, the two questions have
    // three distinct candidate answers and each getter picks its own.
    let its_own_group: KoID = 46_115;
    child.linux().set_pgid_raw(its_own_group);
    assert_eq!(get_process_pgid(child.id()), Ok(its_own_group));
    assert_eq!(
        get_process_sid(child.id()),
        Ok(parent.id()),
        "setpgid moved the session as well"
    );
}

#[test]
fn getpgid_and_getsid_of_a_pid_nobody_has_are_esrch() {
    let nobody: KoID = 46_199;
    assert_eq!(get_process_pgid(nobody), Err(LxError::ESRCH));
    assert_eq!(get_process_sid(nobody), Err(LxError::ESRCH));
}

/// `setsid` refuses when the caller's pid already names a group, and the
/// list it checks against is this one.
///
/// The member's group is deliberately NOT its own pid: that is the case
/// the list exists for -- a group whose leader has exited while members
/// remain -- and the case a list of pids would get wrong while looking
/// right.
#[test]
fn the_live_group_list_holds_the_group_a_member_was_put_in() {
    let member = a_process(46_106);
    let the_group: KoID = 46_116;
    member.linux().set_pgid_raw(the_group);

    let groups = live_effective_pgids();
    assert!(
        groups.contains(&the_group),
        "the group its only member is in is missing from the list setsid reads"
    );
    assert!(
        !groups.contains(&member.id()),
        "the list answered with the member's pid instead of its group"
    );
    assert!(
        !groups.contains(&46_199),
        "a group nobody is in is in the list"
    );
}

#[test]
fn a_pid_nobody_has_is_not_a_process_that_exists() {
    let proc = a_process(46_107);
    assert!(process_exists(proc.id()));
    assert!(!process_exists(46_199), "a pid nobody has");
}

#[test]
fn the_real_uid_of_a_pid_nobody_has_is_root() {
    let proc = a_process(46_108);
    // Real and effective apart: this answers the REAL uid, which is what
    // a `kill` permission check compares against.
    proc.linux().set_resuid(1000, 1001, 1001).unwrap();
    assert_eq!(real_uid_of(proc.id()), 1000);
    assert_eq!(
        real_uid_of(46_199),
        0,
        "an unknown pid answers root, which is what the callers assume"
    );
}

#[test]
fn the_live_process_list_holds_a_process_that_is_running() {
    let proc = a_process(46_109);
    let ids: Vec<KoID> = all_live_processes().iter().map(|p| p.id()).collect();
    assert!(ids.contains(&proc.id()));
}
