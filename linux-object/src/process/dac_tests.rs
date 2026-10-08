//! The discretionary-access decisions of `LinuxProcess`, on pure inputs:
//! who gets which permission bits, what a `chmod` really lands, and which
//! of a caller's three ids an unprivileged id switch may name. Each test
//! cites the Linux rule it pins (`fs/namei.c`, `fs/attr.c`, `kernel/sys.c`).

use super::*;

const OWNER: u32 = 1000;
const GROUP: u32 = 100;
const OTHER_GROUP: u32 = 200;

/// An ordinary user: every id the same, no supplementary groups.
fn user(uid: u32, gid: u32) -> Credentials {
    Credentials {
        ruid: uid,
        euid: uid,
        suid: uid,
        rgid: gid,
        egid: gid,
        sgid: gid,
        fsuid: uid,
        fsgid: gid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

fn may(creds: &Credentials, mode: u16, requested: u16, use_effective: bool) -> bool {
    LinuxProcess::access_verdict(creds, OWNER, GROUP, mode, false, requested, use_effective).is_ok()
}

fn may_dir(creds: &Credentials, mode: u16, requested: u16) -> bool {
    LinuxProcess::access_verdict(creds, OWNER, GROUP, mode, true, requested, true).is_ok()
}

/// `acl_permission_check`: "Are we the owner? If so, ACL's don't matter"
/// -- and neither do the group or other bits. `0o077` gives the owner
/// nothing even though everybody else can do everything.
#[test]
fn the_owner_arm_is_exclusive() {
    let owner = user(OWNER, GROUP);
    assert!(
        !may(&owner, 0o077, 0o4, true),
        "owner denied read by the owner bits"
    );
    assert!(!may(&owner, 0o077, 0o2, true));
    assert!(!may(&owner, 0o077, 0o1, true));
    let stranger = user(4242, 4242);
    assert!(
        may(&stranger, 0o077, 0o7, true),
        "other bits still apply to others"
    );
}

/// `in_group_p()` consults the ACTING gid and the supplementary list. A
/// process that dropped its effective gid must lose the group bits with
/// it: keeping them is keeping the very access the drop gave up.
#[test]
fn a_dropped_effective_gid_loses_group_access() {
    // rgid is still the file's group, egid is not.
    let mut dropped = user(4242, GROUP);
    dropped.set_egid(OTHER_GROUP);
    dropped.sgid = OTHER_GROUP;

    assert!(
        !may(&dropped, 0o060, 0o4, true),
        "the effective path must not read group bits through the REAL gid"
    );
    // `access(2)` asks about the real ids, and there the real gid counts.
    assert!(
        may(&dropped, 0o060, 0o4, false),
        "the real path is the one place the real gid belongs"
    );
}

/// The supplementary list counts on both paths, as `in_group_p` walks it
/// regardless of which primary gid is being asked about.
#[test]
fn a_supplementary_group_counts_on_both_paths() {
    let mut member = user(4242, OTHER_GROUP);
    member.groups = vec![7, GROUP, 9];
    assert!(may(&member, 0o060, 0o6, true));
    assert!(may(&member, 0o060, 0o6, false));
    let outsider = user(4242, OTHER_GROUP);
    assert!(!may(&outsider, 0o060, 0o4, true));
    assert!(
        may(&outsider, 0o004, 0o4, true),
        "falls through to the other bits"
    );
}

/// `generic_permission`: read/write DACs are always overridable by
/// CAP_DAC_OVERRIDE; executing a file needs at least one x bit somewhere;
/// a directory is always searchable.
#[test]
fn root_reads_and_writes_anything_but_executes_only_what_has_an_x_bit() {
    let root = user(ROOT_UID, ROOT_UID);
    assert!(may(&root, 0o000, 0o6, true));
    assert!(
        !may(&root, 0o000, 0o1, true),
        "no x bit anywhere: even root may not exec"
    );
    assert!(
        may(&root, 0o001, 0o1, true),
        "one x bit, any column, is enough"
    );
    assert!(
        may_dir(&root, 0o000, 0o1),
        "a 0700 (or 0000) directory is searchable by root"
    );
    // The override follows the SELECTED uid: a setuid-root program asked
    // about its real ids is not root for `access(2)`.
    let mut setuid_root = user(OWNER, GROUP);
    setuid_root.set_euid(ROOT_UID);
    assert!(may(&setuid_root, 0o000, 0o2, true));
    assert!(!may(&setuid_root, 0o000, 0o2, false));
}

/// `mask & ~mode` with an empty mask is zero: `F_OK` is existence only.
#[test]
fn nothing_requested_is_always_granted() {
    let stranger = user(4242, 4242);
    assert!(may(&stranger, 0o000, 0, true));
    assert!(may(&stranger, 0o000, 0, false));
}

// ---- chmod -------------------------------------------------------------

fn chmod(creds: &Credentials, cur: u16, mode: u16) -> LxResult<u16> {
    LinuxProcess::chmod_bits(creds, OWNER, GROUP, cur, mode)
}

/// `setattr_prepare` never touches `S_ISUID`: this is how a user makes a
/// setuid binary of a file they own. It used to be stripped from every
/// non-root chmod, and the call still returned success, so `chmod 4755`
/// silently left `0755` behind.
#[test]
fn the_owner_keeps_setuid_on_chmod() {
    let owner = user(OWNER, GROUP);
    assert_eq!(chmod(&owner, 0o100_644, 0o4755), Ok(0o104_755));
    // File-type bits above the permission mask are never the caller's to
    // change.
    assert_eq!(chmod(&owner, 0o100_644, 0o7777), Ok(0o107_777));
}

/// "Normal users cannot set the setgid bit if they are not in the group"
/// -- and that is the only bit `setattr_prepare` strips, and only then.
#[test]
fn setgid_is_stripped_only_from_a_caller_outside_the_file_group() {
    let owner_in_group = user(OWNER, GROUP);
    assert_eq!(chmod(&owner_in_group, 0o644, 0o2755), Ok(0o2755));

    let mut owner_outside = user(OWNER, OTHER_GROUP);
    assert_eq!(
        chmod(&owner_outside, 0o644, 0o6755),
        Ok(0o4755),
        "setgid stripped, setuid kept"
    );
    // A supplementary membership is membership.
    owner_outside.groups = vec![GROUP];
    assert_eq!(chmod(&owner_outside, 0o644, 0o2755), Ok(0o2755));
    // And, per `in_group_p`, the REAL gid is not.
    let mut owner_real_only = user(OWNER, GROUP);
    owner_real_only.set_egid(OTHER_GROUP);
    owner_real_only.sgid = OTHER_GROUP;
    assert_eq!(chmod(&owner_real_only, 0o644, 0o2755), Ok(0o755));
}

/// `inode_owner_or_capable`: the owner or root, nobody else; and root is
/// exempt from the setgid rule.
#[test]
fn a_stranger_cannot_chmod_and_root_keeps_every_bit() {
    let stranger = user(4242, GROUP);
    assert_eq!(chmod(&stranger, 0o644, 0o600), Err(LxError::EPERM));
    let root = user(ROOT_UID, ROOT_UID);
    assert_eq!(chmod(&root, 0o644, 0o6755), Ok(0o6755));
}

// ---- which id an unprivileged switch may name --------------------------

/// `sys_setuid`: `!uid_eq(kuid, old->uid) && !uid_eq(kuid, new->suid)`
/// -> EPERM. The effective id is not in the set: with (r=1000, e=2000,
/// s=3000), `setuid(2000)` is refused by Linux.
#[test]
fn setuid_draws_from_real_and_saved_not_effective() {
    assert!(LinuxProcess::setid_allowed(1000, 3000, 1000));
    assert!(LinuxProcess::setid_allowed(1000, 3000, 3000));
    assert!(!LinuxProcess::setid_allowed(1000, 3000, 2000));
    assert!(!LinuxProcess::setid_allowed(1000, 3000, ROOT_UID));
}

/// `sys_setreuid`: the real argument may be the old real or effective id,
/// never the saved one. With (r=1000, e=1000, s=0) -- a daemon that
/// dropped root and kept it in the saved slot -- `setreuid(0, -1)` is
/// EPERM on Linux; letting it through moved the saved id into the REAL
/// slot, where every later rule accepts it. The effective argument, and
/// every `setresuid` argument, may name any of the three.
#[test]
fn the_real_argument_of_setreuid_never_takes_the_saved_id() {
    assert!(!LinuxProcess::set_real_allowed(1000, 1000, ROOT_UID));
    assert!(LinuxProcess::set_real_allowed(1000, 2000, 2000));
    assert!(LinuxProcess::set_real_allowed(1000, 2000, 1000));
    assert!(LinuxProcess::set_any_allowed(
        1000, 1000, ROOT_UID, ROOT_UID
    ));
    assert!(!LinuxProcess::set_any_allowed(1000, 2000, 3000, 4000));
}

/// `if (ruid != -1 || (euid != -1 && !uid_eq(keuid, old->uid))) new->suid
/// = new->euid;` -- there is no privileged term, so a `setreuid(-1, -1)`
/// that asks for nothing leaves the saved id alone, root or not.
#[test]
fn setreuid_with_nothing_to_do_leaves_the_saved_id_alone() {
    assert!(!LinuxProcess::setreid_updates_saved(NO_ID, NO_ID, 1000));
    assert!(
        LinuxProcess::setreid_updates_saved(1000, NO_ID, 1000),
        "a real id is set"
    );
    assert!(
        LinuxProcess::setreid_updates_saved(NO_ID, 2000, 1000),
        "an effective id other than the old real one is set"
    );
    assert!(
        !LinuxProcess::setreid_updates_saved(NO_ID, 1000, 1000),
        "setting the effective id back to the real one is not a new identity"
    );
}
