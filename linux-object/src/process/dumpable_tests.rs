//! `prctl(PR_SET_DUMPABLE)` and the two places the kernel decides
//! dumpability on the process's behalf.
//!
//! The flag was stored and never read: `prctl(PR_SET_DUMPABLE, 0)`
//! returned 0, the process believed it had closed itself, and anything of
//! the same user went on reading its memory through `/proc/<pid>/` and
//! taking its open files with `pidfd_getfd`. The rules that ask it now are
//! in `ptrace_access_tests`, and the exec half in `exec_reset_tests`; this
//! module is the third place, `commit_creds()` -- the id change with no
//! exec in sight.

use super::dup_fd_tests::a_process;
use super::*;

#[test]
fn a_process_starts_out_dumpable() {
    // `SUID_DUMP_USER`, the Linux default, and the direction that matters
    // most to get right: a kernel that started everything non-dumpable
    // would leave `gdb` unable to attach to anything at all.
    assert_eq!(a_process().dumpable(), SUID_DUMP_USER);
}

#[test]
fn dropping_privilege_closes_the_process_even_though_the_ids_end_up_even() {
    // `commit_creds()`. A set-user-ID root program that calls
    // `setuid(1000)` lands on ids that look like an ordinary user's, and
    // its memory still holds whatever it read while it was root. Linux
    // shuts the door at the moment the ids move; the user who started it
    // must not be able to read it afterwards.
    let proc = a_process();
    proc.set_uid(1000).unwrap();
    assert_eq!(proc.dumpable(), SUID_DUMP_DISABLE);
}

#[test]
fn an_id_call_that_moves_nothing_leaves_the_process_alone() {
    // The condition is a comparison, not "a `set*id` happened": root
    // setting the id it already has changes nothing about what its memory
    // holds, and closing the process here would make every idle
    // `setuid(geteuid())` a one-way door.
    let proc = a_process();
    proc.set_uid(ROOT_UID).unwrap();
    assert_eq!(proc.dumpable(), SUID_DUMP_USER);
}

#[test]
fn moving_only_the_saved_id_leaves_the_process_alone() {
    // Four of the six ids, and this is the one that says which four:
    // `setresuid(-1, -1, 0)` moves the saved id, which grants nothing and
    // reveals nothing. A condition written as "any id moved" closes the
    // process here, and `is_sugid` -- which IS about any of the six --
    // sits two lines above it.
    let proc = a_process();
    let before = proc.credentials().suid;
    proc.set_resuid(NO_ID, NO_ID, 1000).unwrap();
    let after = proc.credentials();
    assert_ne!(
        after.suid, before,
        "the saved id has to MOVE or this test asks nothing"
    );
    assert_eq!(after.euid, ROOT_UID, "and the effective one must not");
    assert_eq!(proc.dumpable(), SUID_DUMP_USER);
}

#[test]
fn moving_the_group_alone_closes_it_too() {
    // `!gid_eq(old->egid, new->egid)`: a set-group-ID program holds other
    // people's data in its memory just as surely as a set-user-ID one.
    let proc = a_process();
    proc.set_gid(1000).unwrap();
    assert_eq!(proc.dumpable(), SUID_DUMP_DISABLE);
}

#[test]
fn setfsuid_closes_it_as_well() {
    // `!uid_eq(old->fsuid, new->fsuid)` is in the same condition, and
    // `setfsuid` is the one id path that does not go through
    // `note_id_change`, so it is the one that gets missed. The process
    // that calls it is a file server standing in for a user, holding in
    // memory whatever it read as root a moment ago.
    let proc = a_process();
    proc.set_fsuid(1000);
    assert_eq!(proc.credentials().fsuid, 1000);
    assert_eq!(proc.dumpable(), SUID_DUMP_DISABLE);

    let proc = a_process();
    proc.set_fsgid(1000);
    assert_eq!(proc.dumpable(), SUID_DUMP_DISABLE);
}

#[test]
fn a_query_that_moves_no_filesystem_id_leaves_the_process_alone() {
    // `setfsuid(-1)` is the pure query the man page tells a program to
    // make, and `setfsuid` of the id already in force builds no
    // credentials at all. Neither is a move.
    let proc = a_process();
    assert_eq!(proc.set_fsuid(NO_ID), ROOT_UID);
    assert_eq!(proc.set_fsuid(ROOT_UID), ROOT_UID);
    assert_eq!(proc.dumpable(), SUID_DUMP_USER);
}

#[test]
fn an_id_change_drops_the_parent_death_signal() {
    // The other half of the same `if`: `task->pdeath_signal = 0;`. The
    // parent chose that signal while this process was still somebody it
    // could order about, and it must not keep the lever over a process
    // whose privileges have just moved.
    let proc = a_process();
    proc.set_pdeathsig(crate::signal::Signal::SIGTERM as u8);
    proc.set_uid(1000).unwrap();
    assert_eq!(proc.pdeathsig(), 0);
}
