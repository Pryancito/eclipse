//! The orphaned process group, and the `SIGHUP` + `SIGCONT` it is owed.
//!
//! The case, in one line: `sleep 1000`, Ctrl-Z, and then the shell dies. The
//! job is in a job-control stop and every relative it has left is inside its
//! own group, so no process in the system will ever continue it -- it holds
//! its memory, its descriptors and its pid for as long as the machine is up.
//! POSIX makes that the kernel's problem and Linux solves it in
//! `kill_orphaned_pgrp`. Nothing here did.
//!
//! The predicate is tested without processes because that is where the two
//! halves of the parent test can be separated, and each half fails in its own
//! direction. The end-to-end test is what pins that the predicate is actually
//! consulted at the one moment it matters.
//!
//! Fixed pids live in 46_6xx here; they are a global table, so every file that
//! pins one has its own range.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

/// A member whose parent is outside the group and in the same session: the
/// shell, from the job's point of view. One of these in a group is what makes
/// the group not orphaned.
fn protected_by_a_shell(stopped: bool) -> GroupMember {
    GroupMember {
        pgid: 800,
        sid: 700,
        parent: Some((700, 700)),
        stopped,
    }
}

/// A member with no parent left to speak for it.
fn orphan(stopped: bool) -> GroupMember {
    GroupMember {
        pgid: 800,
        sid: 700,
        parent: None,
        stopped,
    }
}

#[test]
fn a_group_whose_member_still_has_its_shell_is_not_orphaned() {
    assert!(!group_is_orphaned(&[protected_by_a_shell(true)]));
    assert!(!orphaned_group_owes_a_wake(&[
        orphan(true),
        protected_by_a_shell(false),
    ]));
}

#[test]
fn a_group_with_nobody_left_outside_it_is_orphaned() {
    assert!(group_is_orphaned(&[orphan(false), orphan(true)]));
}

/// The "different group" half of the parent test. Without it, a group whose
/// members are each other's parents -- which is every pipeline a shell builds,
/// since the job leader forks the rest -- protects itself for ever, so no
/// group is ever orphaned and no stopped job is ever freed.
#[test]
fn a_parent_inside_the_group_protects_nothing() {
    let member_whose_parent_is_the_job_leader = GroupMember {
        pgid: 800,
        sid: 700,
        parent: Some((800, 700)),
        stopped: true,
    };
    assert!(group_is_orphaned(&[member_whose_parent_is_the_job_leader]));
}

/// The "same session" half. Without it, a job whose shell has already left for
/// a session of its own counts as protected by it -- which is the very case
/// this exists for, since a `setsid` is how a process stops being able to do
/// job control for anybody.
#[test]
fn a_parent_in_another_session_protects_nothing() {
    let member_whose_parent_daemonized = GroupMember {
        pgid: 800,
        sid: 700,
        parent: Some((900, 900)),
        stopped: true,
    };
    assert!(group_is_orphaned(&[member_whose_parent_daemonized]));
}

/// INIT is written out as `None` rather than as its ids, so this is what says
/// the two are treated alike: a child handed to PID 1 is on its own.
#[test]
fn a_child_of_init_is_on_its_own() {
    assert!(orphaned_group_owes_a_wake(&[orphan(true)]));
}

/// An orphaned group with nothing stopped in it is ordinary -- every daemon's
/// group is orphaned for its whole life -- so it is owed nothing. Waking those
/// would mean a `SIGHUP` to every daemon every time any process exits.
#[test]
fn an_orphaned_group_with_nothing_stopped_is_owed_nothing() {
    assert!(group_is_orphaned(&[orphan(false)]));
    assert!(!orphaned_group_owes_a_wake(&[orphan(false)]));
}

/// A group that no longer has members is vacuously orphaned and has nothing
/// stopped, so it is owed nothing. It is the common case at every process
/// exit: the group the dying process was the last member of.
#[test]
fn an_empty_group_is_owed_nothing() {
    assert!(group_is_orphaned(&[]));
    assert!(!group_has_stopped_member(&[]));
    assert!(!orphaned_group_owes_a_wake(&[]));
}

/// One stopped member is enough, whatever the rest of the group is doing: a
/// pipeline is stopped as a unit, but a single member still parked is a member
/// nobody will continue.
#[test]
fn one_stopped_member_is_enough() {
    assert!(orphaned_group_owes_a_wake(&[
        orphan(false),
        orphan(false),
        orphan(true),
    ]));
}

/// A process with a thread, which is what a signal can be queued to.
fn a_process(pid: KoID) -> Arc<Process> {
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// End to end, and the whole reason for the module: a shell with a stopped job
/// in a group of its own dies, and the job comes back out of its stop.
///
/// Without the wake, `is_job_stopped()` here stays `true` for ever -- there is
/// no other process left that could call `job_continue`, which is exactly what
/// makes this the kernel's job.
#[test]
fn a_stopped_job_whose_shell_dies_is_continued() {
    let shell = a_process(46_601);
    shell.linux().become_session_leader(shell.id());
    let job = Process::fork_from(&shell).unwrap();
    Thread::create_linux(&job).unwrap();
    // The job-control idiom: the shell puts the job in a group of its own so a
    // Ctrl-C reaches the job and not the shell.
    job.linux().set_pgid_raw(job.id());
    job.linux().job_stop(&job, LinuxSignal::SIGTSTP as u8);
    assert!(job.linux().is_job_stopped(), "Ctrl-Z stopped it");

    shell.exit(0);

    assert!(
        !job.linux().is_job_stopped(),
        "nobody but the kernel was left to continue it"
    );
}

/// The SIGHUP is the other half of the pair and it is not decoration: the
/// SIGCONT only gets the job running again, and a job that comes back from a
/// Ctrl-Z with its terminal's owner gone and no hangup to show for it has been
/// told nothing at all. POSIX names both signals and Linux sends both.
#[test]
fn the_job_is_told_about_the_hangup_and_not_only_resumed() {
    let shell = a_process(46_603);
    shell.linux().become_session_leader(shell.id());
    let job = Process::fork_from(&shell).unwrap();
    let job_thread = Thread::create_linux(&job).unwrap();
    job.linux().set_pgid_raw(job.id());
    job.linux().job_stop(&job, LinuxSignal::SIGTSTP as u8);

    shell.exit(0);

    assert!(
        job_thread
            .lock_linux()
            .signals
            .contains(LinuxSignal::SIGHUP),
        "resumed without ever being told why"
    );
}

/// The same thing when the process that dies is itself a forked child, which
/// is every shell in the system: `eclipse-init` forks them, so a shell's own
/// death runs the OTHER termination callback -- the one `fork_from` installs,
/// not the one `create_linux` does. Both have to sweep, and only one of them
/// being wired is a bug that shows up in the only case that ever happens.
#[test]
fn a_forked_shell_dying_wakes_its_job_too() {
    let login = a_process(46_604);
    login.linux().become_session_leader(login.id());
    let shell = Process::fork_from(&login).unwrap();
    Thread::create_linux(&shell).unwrap();
    let job = Process::fork_from(&shell).unwrap();
    Thread::create_linux(&job).unwrap();
    job.linux().set_pgid_raw(job.id());
    job.linux().job_stop(&job, LinuxSignal::SIGTSTP as u8);

    shell.exit(0);

    assert!(
        !job.linux().is_job_stopped(),
        "the shell `init` forked is the one whose death this really is"
    );
}

/// The other side of the same event: a job whose shell is still alive is left
/// exactly where it is. A kernel that woke every stopped group on any exit
/// would resume every suspended job in the system the first time a background
/// command finished.
#[test]
fn a_stopped_job_whose_shell_is_alive_stays_stopped() {
    let shell = a_process(46_602);
    shell.linux().become_session_leader(shell.id());
    let job = Process::fork_from(&shell).unwrap();
    Thread::create_linux(&job).unwrap();
    job.linux().set_pgid_raw(job.id());
    job.linux().job_stop(&job, LinuxSignal::SIGTSTP as u8);

    // Some other child of the same shell exits, which is the event that runs
    // the sweep.
    let other = Process::fork_from(&shell).unwrap();
    Thread::create_linux(&other).unwrap();
    other.exit(0);

    assert!(
        job.linux().is_job_stopped(),
        "the shell is still there to run `fg`, so the stop is still the user's"
    );
}

/// The candidate list is the group the dying process is ITSELF in as well as
/// its children's, and the first half is the one every end-to-end test above
/// gets for free: in all of them the stopped job is a CHILD of the process
/// that dies, so its group arrives through the children either way. A member
/// of a stopped pipeline that dies while a sibling is still parked in the
/// same group leaves a group that nothing else would have named.
#[test]
fn the_group_the_dying_process_is_in_is_a_candidate_too() {
    let login = a_process(46_605);
    login.linux().become_session_leader(login.id());
    let dying = Process::fork_from(&login).unwrap();
    Thread::create_linux(&dying).unwrap();
    dying.linux().set_pgid_raw(dying.id());
    let child = Process::fork_from(&dying).unwrap();
    Thread::create_linux(&child).unwrap();
    child.linux().set_pgid_raw(child.id());

    let candidates = groups_this_death_could_orphan(&dying);

    assert!(
        candidates.contains(&dying.id()),
        "the group the dying process is itself in was not named"
    );
    assert!(
        candidates.contains(&child.id()),
        "the group of a child it still holds was not named"
    );
}
