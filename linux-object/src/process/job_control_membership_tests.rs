//! Who may move whom between process groups and sessions.
//!
//! A process group is the unit a terminal signals: [`send_signal_to_pgrp`]
//! walks every live process whose effective pgid matches the foreground
//! group and delivers there, which is how one Ctrl-C reaches a pipeline.
//! Membership of that list was writable by anybody: `set_process_pgid`
//! said so in its own doc comment ("Permissive (no session/leader
//! checks)"), took no caller at all, and set the field. `kernel/sys.c`'s
//! `do_setpgid` has four rules and `ksys_setsid` a fifth; these are them.

use super::*;

const SHELL: KoID = 100;
const CHILD: KoID = 101;
const STRANGER: KoID = 900;
const OTHER_SESSION: KoID = 800;

/// A shell that leads its own session, about to group a child of its own
/// that has forked but not yet exec'd -- the job-control idiom, which
/// must keep working.
fn shell_grouping_its_fresh_child() -> SetpgidFacts {
    SetpgidFacts {
        caller_pid: SHELL,
        caller_sid: SHELL,
        target_pid: CHILD,
        target_sid: SHELL,
        target_is_child: true,
        target_has_execed: false,
        target_is_session_leader: false,
        new_pgid: CHILD,
        group_exists_in_caller_session: false,
    }
}

#[test]
fn a_shell_may_put_its_own_child_in_a_group_of_its_own() {
    assert_eq!(setpgid_verdict(&shell_grouping_its_fresh_child()), Ok(()));
}

/// The rule this kernel is missing, and the one that matters: a process
/// group is a kill list, so filing somebody else's process under your own
/// group number hands your terminal's Ctrl-C to a process that never
/// agreed to it. Linux answers ESRCH -- not EPERM -- so the call cannot
/// double as a probe for which pids exist.
#[test]
fn a_stranger_is_not_the_callers_to_move() {
    let mut f = shell_grouping_its_fresh_child();
    f.target_pid = STRANGER;
    f.target_is_child = false;
    f.new_pgid = STRANGER;
    assert_eq!(setpgid_verdict(&f), Err(LxError::ESRCH));
}

/// ...including into the caller's OWN group, which is the shape that
/// actually steals a process: the target then receives every
/// terminal-generated signal aimed at the caller's job.
#[test]
fn a_stranger_cannot_be_dragged_into_the_callers_group() {
    let mut f = shell_grouping_its_fresh_child();
    f.target_pid = STRANGER;
    f.target_is_child = false;
    f.new_pgid = SHELL;
    f.group_exists_in_caller_session = true;
    assert_eq!(setpgid_verdict(&f), Err(LxError::ESRCH));
}

/// `if (!(p->flags & PF_FORKNOEXEC)) return -EACCES`. The parent's window
/// closes at the child's `execve`: after it, the program running in that
/// child is one the parent did not write.
#[test]
fn a_child_that_has_already_execed_is_no_longer_groupable() {
    let mut f = shell_grouping_its_fresh_child();
    f.target_has_execed = true;
    assert_eq!(setpgid_verdict(&f), Err(LxError::EACCES));
}

/// That rule is about a CHILD, not about the caller. A shell has exec'd
/// itself, obviously, and `setpgid(0, 0)` is how it puts itself into a
/// group -- so reading the flag before asking whose process it is would
/// break the ordinary self-call.
#[test]
fn a_process_that_has_execed_may_still_move_itself() {
    let f = SetpgidFacts {
        caller_pid: CHILD,
        caller_sid: SHELL,
        target_pid: CHILD,
        target_sid: SHELL,
        target_is_child: false,
        target_has_execed: true,
        target_is_session_leader: false,
        new_pgid: CHILD,
        group_exists_in_caller_session: false,
    };
    assert_eq!(setpgid_verdict(&f), Ok(()));
}

/// A child that has left for a session of its own (a daemon that called
/// `setsid`) is out of this shell's reach, even though it is still its
/// child. EPERM, and it is checked BEFORE the exec rule: a child that is
/// both gone and exec'd answers for the session, which is the reason it
/// can never come back.
#[test]
fn a_child_in_another_session_is_out_of_reach() {
    let mut f = shell_grouping_its_fresh_child();
    f.target_sid = OTHER_SESSION;
    assert_eq!(setpgid_verdict(&f), Err(LxError::EPERM));
    f.target_has_execed = true;
    assert_eq!(
        setpgid_verdict(&f),
        Err(LxError::EPERM),
        "the session rule is the one that answers"
    );
}

/// A session leader's pid IS its session id. Letting it wander into
/// another group would leave the session named after a group its leader
/// is not in -- and `getsid` and `getpgid` would stop agreeing about it.
#[test]
fn a_session_leader_is_pinned_to_its_own_group() {
    let f = SetpgidFacts {
        caller_pid: SHELL,
        caller_sid: SHELL,
        target_pid: SHELL,
        target_sid: SHELL,
        target_is_child: false,
        target_has_execed: true,
        target_is_session_leader: true,
        new_pgid: SHELL,
        group_exists_in_caller_session: true,
    };
    assert_eq!(setpgid_verdict(&f), Err(LxError::EPERM));
}

/// Joining an EXISTING group means the group has to exist, and in the
/// caller's session. This is what stops a second job from being filed
/// under a number that belongs to another terminal's pipeline.
#[test]
fn joining_a_group_that_exists_nowhere_is_refused() {
    let mut f = shell_grouping_its_fresh_child();
    f.new_pgid = 555;
    f.group_exists_in_caller_session = false;
    assert_eq!(setpgid_verdict(&f), Err(LxError::EPERM));
}

#[test]
fn joining_a_group_of_the_callers_own_session_is_allowed() {
    let mut f = shell_grouping_its_fresh_child();
    f.new_pgid = 555;
    f.group_exists_in_caller_session = true;
    assert_eq!(setpgid_verdict(&f), Ok(()));
}

/// Creating one is always allowed, precisely because `pgid == pid` cannot
/// collide with a group that is already there: the pid is the target's
/// own. This is the second half of the same rule and the first half is
/// useless without it -- a shell's first job would have nowhere to go.
#[test]
fn a_group_named_after_the_target_needs_no_group_to_exist() {
    let mut f = shell_grouping_its_fresh_child();
    f.new_pgid = f.target_pid;
    f.group_exists_in_caller_session = false;
    assert_eq!(setpgid_verdict(&f), Ok(()));
}

/// Order, again: a stranger with an impossible group answers for WHOSE
/// process it is, not for the group. Otherwise the error tells an
/// unrelated caller whether that group exists in its session.
#[test]
fn whose_process_it_is_is_answered_before_which_group() {
    let mut f = shell_grouping_its_fresh_child();
    f.target_pid = STRANGER;
    f.target_is_child = false;
    f.new_pgid = 555;
    f.group_exists_in_caller_session = false;
    assert_eq!(setpgid_verdict(&f), Err(LxError::ESRCH));
}

/// `ksys_setsid`: refused while the caller's pid already names a group,
/// because the new session would claim that same number for its group.
/// The everyday case is the caller's own group, which is exactly why the
/// daemonize idiom `fork`s before calling `setsid`.
#[test]
fn a_group_leader_may_not_start_a_session() {
    assert_eq!(
        setsid_verdict(SHELL, &[SHELL, CHILD]),
        Err(LxError::EPERM),
        "the caller's own group carries its pid"
    );
}

#[test]
fn a_process_that_leads_no_group_may() {
    assert_eq!(setsid_verdict(CHILD, &[SHELL, SHELL]), Ok(()));
}

/// And the case the old check could not see: it read only the CALLER's
/// own pgid, so a caller that had moved itself elsewhere while a child of
/// its own still carried its pid as a group number passed -- and the new
/// session's group would then have had two unrelated members.
#[test]
fn a_pid_that_names_a_group_somebody_else_is_in_counts_too() {
    // The caller sits in group 555; only its child still carries SHELL.
    assert_eq!(setsid_verdict(SHELL, &[555, SHELL]), Err(LxError::EPERM));
}
