//! `__ptrace_may_access(PTRACE_MODE_ATTACH_REALCREDS)`: who may reach INTO
//! another process rather than merely signal it.
//!
//! `pidfd_getfd(2)` asked nothing, so any process could take any open file
//! out of any other. It is the fifth syscall in a row to touch another
//! process with no gate, and the fourth DIFFERENT set of ids: `kill` reads
//! two of the target's, the renice two others, `prlimit64` all six, and
//! this one all six again but for another capability. Copying the
//! neighbour's rule is the bug this module exists to catch.

use super::*;

const USER: u32 = 1000;
const GROUP: u32 = 1000;
const OTHER: u32 = 1001;
const OTHER_GROUP: u32 = 1001;

fn ids(ruid: u32, euid: u32, suid: u32, rgid: u32, egid: u32, sgid: u32) -> Credentials {
    Credentials {
        ruid,
        euid,
        suid,
        rgid,
        egid,
        sgid,
        fsuid: euid,
        fsgid: egid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

/// A process that has done nothing clever: one uid and one gid, three times
/// over.
fn plain(uid: u32, gid: u32) -> Credentials {
    ids(uid, uid, uid, gid, gid, gid)
}

/// The same, after a `setfsuid`/`setfsgid` that moved the filesystem pair
/// off the effective one.
fn with_fs(mut creds: Credentials, fsuid: u32, fsgid: u32) -> Credentials {
    creds.fsuid = fsuid;
    creds.fsgid = fsgid;
    creds
}

/// The two rules under test, asked about a target that is DUMPABLE.
///
/// That is the state every process is in until it hardens itself or an exec
/// raises it, so it is what every test here is about except the dumpability
/// pair at the end -- which calls [`LinuxProcess::may_attach_to`] and
/// [`LinuxProcess::may_read_process_innards`] by name, with the argument
/// these two pin.
fn attach_allowed(caller: &Credentials, target: &Credentials, same_thread_group: bool) -> bool {
    LinuxProcess::may_attach_to(caller, target, same_thread_group, SUID_DUMP_USER)
}

fn read_allowed(caller: &Credentials, target: &Credentials, same_thread_group: bool) -> bool {
    LinuxProcess::may_read_process_innards(caller, target, same_thread_group, SUID_DUMP_USER)
}

// ---- the six comparisons ----------------------------------------------

#[test]
fn a_process_of_yours_is_yours_to_reach_into() {
    assert!(attach_allowed(
        &plain(USER, GROUP),
        &plain(USER, GROUP),
        false
    ));
}

#[test]
fn another_users_process_is_not() {
    assert!(!attach_allowed(
        &plain(USER, GROUP),
        &plain(OTHER, OTHER_GROUP),
        false
    ));
}

#[test]
fn every_one_of_the_targets_three_uids_has_to_match() {
    // Not "same user": "that process holds no id I do not already hold".
    // One id out of six is enough to put it out of reach, so each is named
    // on its own rather than swept in a loop -- a loop would pass if the
    // predicate read one field six times.
    let caller = plain(USER, GROUP);
    assert!(!attach_allowed(
        &caller,
        &ids(OTHER, USER, USER, GROUP, GROUP, GROUP),
        false
    ));
    assert!(!attach_allowed(
        &caller,
        &ids(USER, OTHER, USER, GROUP, GROUP, GROUP),
        false
    ));
    assert!(!attach_allowed(
        &caller,
        &ids(USER, USER, OTHER, GROUP, GROUP, GROUP),
        false
    ));
}

#[test]
fn and_every_one_of_its_three_gids() {
    // The groups are half the rule and the half a signal check does not
    // have at all: `may_signal_cred` never looks at a gid. A caller in
    // another group may kill the process and may not read its files.
    let caller = plain(USER, GROUP);
    assert!(!attach_allowed(
        &caller,
        &ids(USER, USER, USER, OTHER_GROUP, GROUP, GROUP),
        false
    ));
    assert!(!attach_allowed(
        &caller,
        &ids(USER, USER, USER, GROUP, OTHER_GROUP, GROUP),
        false
    ));
    assert!(!attach_allowed(
        &caller,
        &ids(USER, USER, USER, GROUP, GROUP, OTHER_GROUP),
        false
    ));
    assert!(LinuxProcess::may_signal_cred(
        &caller,
        &ids(USER, USER, USER, OTHER_GROUP, OTHER_GROUP, OTHER_GROUP)
    ));
}

// ---- which of the CALLER's ids ----------------------------------------

#[test]
fn it_is_the_callers_real_pair_that_is_compared() {
    // `PTRACE_MODE_REALCREDS`, and Linux's own comment says using the
    // effective uid "would make more sense here" -- but userland relies on
    // the old behaviour, so the real one it is. A process whose effective
    // uid has moved keeps the reach its real one gives it.
    let moved_euid = ids(USER, OTHER, USER, GROUP, OTHER_GROUP, GROUP);
    assert!(attach_allowed(&moved_euid, &plain(USER, GROUP), false));
}

#[test]
fn an_effective_uid_you_borrowed_does_not_reach_into_anything() {
    // The mirror image, and the one that matters: `kill` accepts the
    // caller's EFFECTIVE uid against the target's real one, so this same
    // pair may be killed. Two rules over one pair of credentials, two
    // answers.
    let borrowed = ids(OTHER, USER, OTHER, OTHER_GROUP, GROUP, OTHER_GROUP);
    let target = plain(USER, GROUP);
    assert!(LinuxProcess::may_signal_cred(&borrowed, &target));
    assert!(!attach_allowed(&borrowed, &target, false));
}

#[test]
fn a_setuid_program_is_out_of_reach_for_the_user_who_started_it() {
    // The headline case, and why the rule is this strict: the user's own
    // shell started a set-user-ID root program, so the user may still KILL
    // it -- its real and saved uids are theirs. Taking its open files would
    // be taking root's files, so the reach stops at the effective uid it
    // holds.
    let user = plain(USER, GROUP);
    let setuid_root = ids(USER, ROOT_UID, ROOT_UID, GROUP, ROOT_UID, ROOT_UID);
    assert!(LinuxProcess::may_signal_cred(&user, &setuid_root));
    assert!(!attach_allowed(&user, &setuid_root, false));
}

// ---- the capability, and the escape -----------------------------------

#[test]
fn cap_sys_ptrace_is_nineteen() {
    // From the ABI (`include/uapi/linux/capability.h`), so asserted
    // against the literal and not against another name for it.
    assert_eq!(CAP_SYS_PTRACE, 19);
}

#[test]
fn the_capability_lifts_the_six_comparisons() {
    assert!(attach_allowed(
        &plain(ROOT_UID, ROOT_UID),
        &plain(OTHER, OTHER_GROUP),
        false
    ));
}

#[test]
fn the_capability_is_read_from_the_callers_effective_uid() {
    // Not the real one the comparisons use: the ids ask who you ARE and
    // the capability asks what you may DO, and in this kernel the second
    // is the effective uid. A root program that dropped to a user keeps
    // neither.
    let root_by_real_uid_only = ids(ROOT_UID, USER, USER, ROOT_UID, GROUP, GROUP);
    assert!(!attach_allowed(
        &root_by_real_uid_only,
        &plain(OTHER, OTHER_GROUP),
        false
    ));
    let root_by_effective_uid = ids(USER, ROOT_UID, USER, GROUP, ROOT_UID, GROUP);
    assert!(attach_allowed(
        &root_by_effective_uid,
        &plain(OTHER, OTHER_GROUP),
        false
    ));
}

#[test]
fn your_own_thread_group_is_reached_without_reading_an_id() {
    // Linux short-circuits the whole check for your own thread group, and
    // it is kept because a pidfd on yourself is how `pidfd_getfd` spells
    // `dup`. With one credential set per thread group the ids would say
    // yes anyway, so this cannot change an answer here -- it is Linux's
    // seventh test, the non-dumpable one this kernel does not have, that
    // the escape exists to get past.
    let me = ids(USER, ROOT_UID, ROOT_UID, GROUP, ROOT_UID, ROOT_UID);
    assert!(attach_allowed(&me, &me, true));
    assert!(attach_allowed(&me, &me, false));
}

// ---- READ_FSCREDS: the same six comparisons, the other pair -----------

/// A caller whose filesystem pair has moved off its real one: a
/// set-user-ID-`OTHER` program, started by `USER`, that called
/// `setfsuid(OTHER)`. That is the only way the two rules can disagree, and
/// `setfsuid(2)` only ever moves the pair to an id the process already
/// holds.
fn moved_fs_pair() -> Credentials {
    with_fs(
        ids(USER, OTHER, OTHER, GROUP, OTHER_GROUP, OTHER_GROUP),
        OTHER,
        OTHER_GROUP,
    )
}

#[test]
fn reading_a_process_goes_by_the_filesystem_pair() {
    // `PTRACE_MODE_READ_FSCREDS`, which every gated file of `/proc/<pid>/`
    // asks for: a path through the filesystem is judged on the identity the
    // filesystem uses.
    assert!(read_allowed(
        &moved_fs_pair(),
        &plain(OTHER, OTHER_GROUP),
        false
    ));
}

#[test]
fn taking_its_files_goes_by_the_real_pair() {
    // The same caller, the same target, the other answer. `REALCREDS`
    // exists for exactly this: a syscall that names a process is judged on
    // the identity the caller cannot lay down with `setfsuid`.
    assert!(!attach_allowed(
        &moved_fs_pair(),
        &plain(OTHER, OTHER_GROUP),
        false
    ));
}

#[test]
fn so_does_moving_its_limits() {
    assert!(!LinuxProcess::may_touch_limits_of(
        &moved_fs_pair(),
        &plain(OTHER, OTHER_GROUP)
    ));
}

#[test]
fn and_the_mirror_image_answers_the_other_way_round() {
    // A caller whose real pair is the target's and whose filesystem pair
    // has moved away: now the procfs rule refuses and the other two allow.
    // Without this half, reading `fsuid` where `ruid` was meant would still
    // pass the three tests above.
    let moved_off = with_fs(plain(OTHER, OTHER_GROUP), USER, GROUP);
    let target = plain(OTHER, OTHER_GROUP);
    assert!(!read_allowed(&moved_off, &target, false));
    assert!(attach_allowed(&moved_off, &target, false));
    assert!(LinuxProcess::may_touch_limits_of(&moved_off, &target));
}

#[test]
fn a_process_that_never_called_setfsuid_gets_one_answer() {
    // The ordinary case, and why a wrong pair hides: `fsuid` follows `euid`
    // everywhere else, so for almost every process in the machine the two
    // rules agree and the wrong argument reads as correct code.
    for (caller, target) in [
        (plain(USER, GROUP), plain(USER, GROUP)),
        (plain(USER, GROUP), plain(OTHER, OTHER_GROUP)),
        (
            plain(USER, GROUP),
            ids(USER, OTHER, USER, GROUP, GROUP, GROUP),
        ),
        (plain(ROOT_UID, ROOT_UID), plain(OTHER, OTHER_GROUP)),
    ] {
        assert_eq!(
            read_allowed(&caller, &target, false),
            attach_allowed(&caller, &target, false),
            "{:?} against {:?}",
            caller,
            target
        );
    }
}

#[test]
fn your_own_process_is_readable_whatever_its_ids() {
    // `/proc/self/environ` is how a program reads its own environment and
    // `/proc/self/maps` how it reads its own map: the thread group goes
    // through first, as in Linux.
    let me = with_fs(
        ids(USER, ROOT_UID, ROOT_UID, GROUP, ROOT_UID, ROOT_UID),
        OTHER,
        OTHER_GROUP,
    );
    assert!(read_allowed(&me, &me, true));
}

#[test]
fn the_capability_lifts_the_procfs_rule_from_the_effective_uid() {
    // Still the effective uid, not the filesystem one: `setfsuid` moves
    // what you are for a file, never what you may do.
    let dropped_fsuid = with_fs(plain(ROOT_UID, ROOT_UID), USER, GROUP);
    assert!(read_allowed(
        &dropped_fsuid,
        &plain(OTHER, OTHER_GROUP),
        false
    ));
    let root_by_fsuid_only = with_fs(plain(USER, GROUP), ROOT_UID, ROOT_UID);
    assert!(!read_allowed(
        &root_by_fsuid_only,
        &plain(OTHER, OTHER_GROUP),
        false
    ));
}

// ---- dumpability -------------------------------------------------------
//
// The last thing `__ptrace_may_access()` does, and the only one of its
// tests that is about the TARGET's own choice rather than about ids:
//
//     mm = task->mm;
//     if (mm && ((get_dumpable(mm) != SUID_DUMP_USER) &&
//                !ptrace_has_cap(mm->user_ns, mode)))
//             return -EPERM;
//
// Nothing asked it here, so `prctl(PR_SET_DUMPABLE, 0)` was a call this
// kernel accepted and did not act on -- which is worse than refusing it,
// because the program that makes it (`ssh-agent`, `gpg-agent`, anything
// holding a key) carries on believing the other processes of its own user
// can no longer read its memory.

#[test]
fn a_target_that_made_itself_non_dumpable_is_out_of_reach_even_for_its_owner() {
    let me = plain(USER, GROUP);
    assert!(
        !LinuxProcess::may_attach_to(&me, &me, false, SUID_DUMP_DISABLE),
        "pidfd_getfd takes an open file out of an agent that asked not to be read"
    );
    assert!(
        !LinuxProcess::may_read_process_innards(&me, &me, false, SUID_DUMP_DISABLE),
        "/proc/<pid>/maps hands out the address map of the same agent"
    );
}

#[test]
fn only_the_capability_goes_past_a_non_dumpable_target() {
    // `!ptrace_has_cap(...)`: the one way through, and the reason a root
    // `gdb` can still attach to a hardened process.
    assert!(LinuxProcess::may_attach_to(
        &plain(ROOT_UID, ROOT_UID),
        &plain(USER, GROUP),
        false,
        SUID_DUMP_DISABLE
    ));
    assert!(LinuxProcess::may_read_process_innards(
        &plain(ROOT_UID, ROOT_UID),
        &plain(USER, GROUP),
        false,
        SUID_DUMP_DISABLE
    ));
}

#[test]
fn a_process_is_still_allowed_to_read_itself_while_non_dumpable() {
    // Linux's `if (same_thread_group(task, current)) return 0;` returns
    // before this check instead of jumping to it, and the difference is
    // whether a hardened program can still read its own `/proc/self/maps`
    // -- which musl's malloc and every crash handler do.
    let me = plain(USER, GROUP);
    assert!(LinuxProcess::may_attach_to(
        &me,
        &me,
        true,
        SUID_DUMP_DISABLE
    ));
    assert!(LinuxProcess::may_read_process_innards(
        &me,
        &me,
        true,
        SUID_DUMP_DISABLE
    ));
}

#[test]
fn the_test_is_against_suid_dump_user_and_not_against_zero() {
    // `get_dumpable(mm) != SUID_DUMP_USER`, so the root-only value 2 is
    // not dumpable either. Written down because the obvious shape --
    // "dumpable == 0 means no" -- lets `SUID_DUMP_ROOT` through to any
    // process that matches the ids, which is who it is there to keep out.
    const SUID_DUMP_ROOT: u8 = 2;
    let me = plain(USER, GROUP);
    assert!(!LinuxProcess::may_attach_to(
        &me,
        &me,
        false,
        SUID_DUMP_ROOT
    ));
    assert!(LinuxProcess::may_attach_to(&me, &me, false, SUID_DUMP_USER));
}
