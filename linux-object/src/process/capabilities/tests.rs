//! The capability sets and the rules that move them, against what Linux
//! 6.18 does (the `cap-survey` probe, 13 checks, run as root there with
//! glibc and musl). The sets used to be one question, "is the effective uid
//! 0?", so every test here that tells root apart from root-without-something
//! failed before.

use super::super::dup_fd_tests::a_process;
use super::super::*;

/// What Linux 6.18 left in `CapEff` after root's `setfsuid(1000)`:
/// everything but [`CAP_FS_SET`].
const AFTER_SETFSUID: u64 = 0x1fe_f7ff_fde0;

fn bit(cap: u32) -> u64 {
    1 << cap
}

fn without(cap: u32) -> u64 {
    CAP_FULL_SET & !bit(cap)
}

/// Root, with `effective` as its effective set and every capability still
/// permitted, as after `capset(effective, FULL, 0)`.
fn root_with(effective: u64) -> Credentials {
    let mut creds = Credentials::default();
    creds.caps.effective = effective;
    creds
}

/// A process of uid 1000 that has not touched its ids.
fn user() -> Credentials {
    Credentials {
        ruid: 1000,
        euid: 1000,
        suid: 1000,
        rgid: 1000,
        egid: 1000,
        sgid: 1000,
        fsuid: 1000,
        fsgid: 1000,
        groups: Vec::new(),
        umask: 0o022,
        caps: Capabilities::for_uids(1000, 1000, 1000),
    }
}

// ------------------------------------------------------------ the numbers

#[test]
fn the_capability_numbers_are_the_ones_capability_h_names() {
    // Wrong by one and a gate asks about somebody else's privilege.
    for (cap, n) in [
        (CAP_CHOWN, 0),
        (CAP_DAC_OVERRIDE, 1),
        (CAP_DAC_READ_SEARCH, 2),
        (CAP_FOWNER, 3),
        (CAP_FSETID, 4),
        (CAP_KILL, 5),
        (CAP_SETGID, 6),
        (CAP_SETUID, 7),
        (CAP_SETPCAP, 8),
        (CAP_LINUX_IMMUTABLE, 9),
        (CAP_SYS_PTRACE, 19),
        (CAP_SYS_ADMIN, 21),
        (CAP_SYS_BOOT, 22),
        (CAP_SYS_NICE, 23),
        (CAP_SYS_RESOURCE, 24),
        (CAP_SYS_TIME, 25),
        (CAP_MKNOD, 27),
        (CAP_MAC_OVERRIDE, 32),
        (CAP_SYSLOG, 34),
    ] {
        assert_eq!(cap, n);
    }
    // CAP_CHECKPOINT_RESTORE, and the bottom 41 bits.
    assert_eq!(CAP_LAST_CAP, 40);
    assert_eq!(CAP_FULL_SET, 0x1ff_ffff_ffff);
}

#[test]
fn the_filesystem_set_is_the_one_setfsuid_takes_away_on_linux() {
    assert_eq!(CAP_FULL_SET & !CAP_FS_SET, AFTER_SETFSUID);
}

#[test]
fn root_holds_every_capability_and_a_user_none() {
    for cap in 0..=CAP_LAST_CAP {
        assert!(Capabilities::ROOT.has(cap), "root lacks {}", cap);
        assert!(Capabilities::ROOT.bounds(cap), "{} is not bounded", cap);
        assert!(!user().capable(cap), "a user holds {}", cap);
        // The bounding set is not the user's to lose by being a user.
        assert!(user().caps.bounds(cap));
    }
    assert_eq!(Capabilities::ROOT.inheritable, 0);
}

#[test]
fn a_number_past_the_last_capability_is_held_and_bounded_by_nobody() {
    // `PR_CAPBSET_READ` of 41 and 63 is EINVAL on Linux 6.18, and `capget`
    // never reports a bit past 40, so no gate may ask about one.
    let all = Capabilities {
        effective: u64::MAX,
        permitted: u64::MAX,
        inheritable: u64::MAX,
        bounding: u64::MAX,
    };
    for cap in [CAP_LAST_CAP + 1, 63, 64, u32::MAX] {
        assert!(!all.has(cap), "cap {}", cap);
        assert!(!all.bounds(cap), "cap {}", cap);
    }
}

#[test]
fn a_check_asks_the_effective_set_and_nothing_else() {
    // `capset(FULL & ~CAP_CHOWN, FULL, 0)` and `chown` is EPERM on Linux,
    // with the capability still permitted and still bounded.
    let creds = root_with(without(CAP_CHOWN));
    assert!(!creds.capable(CAP_CHOWN));
    assert!(creds.capable(CAP_FOWNER));
    assert_eq!(creds.euid, 0);
}

// ---------------------------------------------------------- set*uid and keepcaps

#[test]
fn leaving_root_behind_clears_both_sets_unless_keepcaps_says_otherwise() {
    // setresuid(1000, 1000, 1000): eff 0 prm 0 on Linux; with
    // PR_SET_KEEPCAPS, eff 0 and prm the whole set.
    let mut caps = Capabilities::ROOT;
    caps.follow_setuid([0, 0, 0], [1000, 1000, 1000], false);
    assert_eq!((caps.effective, caps.permitted), (0, 0));
    let mut kept = Capabilities::ROOT;
    kept.follow_setuid([0, 0, 0], [1000, 1000, 1000], true);
    assert_eq!((kept.effective, kept.permitted), (0, CAP_FULL_SET));
    // The bounding and inheritable sets are not the setuid's business.
    assert_eq!(caps.bounding, CAP_FULL_SET);
    assert_eq!(kept.bounding, CAP_FULL_SET);
}

#[test]
fn a_saved_root_keeps_the_permitted_set_and_only_the_effective_one_goes() {
    // seteuid(1000) from root: eff 0, prm the whole set; seteuid(0) again:
    // eff is prm again, even what capset had taken out of eff before.
    let mut caps = Capabilities {
        effective: without(CAP_CHOWN),
        ..Capabilities::ROOT
    };
    caps.follow_setuid([0, 0, 0], [0, 1000, 0], false);
    assert_eq!((caps.effective, caps.permitted), (0, CAP_FULL_SET));
    caps.follow_setuid([0, 1000, 0], [0, 0, 0], false);
    assert_eq!(
        (caps.effective, caps.permitted),
        (CAP_FULL_SET, CAP_FULL_SET)
    );
    // A real uid of 0 alone is enough to keep them.
    let mut real = Capabilities::ROOT;
    real.follow_setuid([0, 0, 0], [0, 1000, 1000], false);
    assert_eq!((real.effective, real.permitted), (0, CAP_FULL_SET));
    // So is a saved one.
    assert_eq!(
        Capabilities::for_uids(1000, 1000, 0),
        Capabilities {
            effective: 0,
            ..Capabilities::ROOT
        }
    );
}

#[test]
fn becoming_root_again_makes_the_permitted_set_effective() {
    // Linux: KEEPCAPS, setresuid(1000, 1000, 1000), capset(CAP_SETUID,
    // FULL, 0), setuid(0): uid 0 and eff the whole set.
    let mut caps = Capabilities {
        effective: bit(CAP_SETUID),
        ..Capabilities::ROOT
    };
    caps.follow_setuid([1000, 1000, 1000], [0, 0, 0], false);
    assert_eq!(caps.effective, CAP_FULL_SET);
}

#[test]
fn a_call_that_moves_no_user_id_changes_nothing() {
    // What the group setters run.
    for ids in [[0, 0, 0], [0, 1000, 0], [1000, 1000, 1000]] {
        let mut caps = Capabilities {
            effective: without(CAP_KILL),
            ..Capabilities::ROOT
        };
        let before = caps;
        caps.follow_setuid(ids, ids, false);
        assert_eq!(caps, before, "{:?}", ids);
    }
}

#[test]
fn keepcaps_keeps_the_permitted_set_across_a_real_setresuid() {
    let proc = a_process();
    proc.set_keep_caps(true);
    proc.set_resuid(1000, 1000, 1000).unwrap();
    let caps = proc.capabilities();
    assert_eq!((caps.effective, caps.permitted), (0, CAP_FULL_SET));
    // PR_GET_KEEPCAPS is still 1 after the setuid, as on Linux.
    assert!(proc.keep_caps());
    // Nothing effective: a chown of another's file would be EPERM. Then
    // capset(CAP_CHOWN, FULL, 0) is allowed and makes it work.
    assert!(!proc.capable(CAP_CHOWN));
    proc.change_capabilities(|c| {
        c.effective = bit(CAP_CHOWN);
        Ok(())
    })
    .unwrap();
    assert!(proc.capable(CAP_CHOWN));

    let plain = a_process();
    plain.set_resuid(1000, 1000, 1000).unwrap();
    assert_eq!(plain.capabilities().permitted, 0);
}

#[test]
fn the_setid_calls_ask_cap_setuid_and_cap_setgid_not_the_uid() {
    // capset(FULL & ~CAP_SETUID, FULL, 0) as root: setuid(1000) and
    // setresuid(-1, 1000, -1) are EPERM on Linux 6.18; without CAP_SETGID,
    // setgid(1000) is.
    let proc = a_process();
    proc.change_capabilities(|c| {
        c.effective = without(CAP_SETUID);
        Ok(())
    })
    .unwrap();
    assert_eq!(proc.set_uid(1000), Err(LxError::EPERM));
    assert_eq!(
        proc.set_resuid(u32::MAX, 1000, u32::MAX),
        Err(LxError::EPERM)
    );
    assert_eq!(proc.set_reuid(1000, u32::MAX), Err(LxError::EPERM));
    // Its own ids are still its to name.
    assert_eq!(proc.set_uid(0), Ok(()));
    assert_eq!(proc.set_gid(1000), Ok(()), "CAP_SETGID is still held");

    let proc = a_process();
    proc.change_capabilities(|c| {
        c.effective = without(CAP_SETGID);
        Ok(())
    })
    .unwrap();
    assert_eq!(proc.set_gid(1000), Err(LxError::EPERM));
    assert_eq!(proc.set_resgid(1000, 1000, 1000), Err(LxError::EPERM));
    assert_eq!(proc.set_regid(u32::MAX, 1000), Err(LxError::EPERM));
    assert_eq!(proc.set_uid(1000), Ok(()), "CAP_SETUID is still held");
}

#[test]
fn a_user_holding_cap_setuid_may_become_root() {
    let proc = a_process();
    proc.set_keep_caps(true);
    proc.set_resuid(1000, 1000, 1000).unwrap();
    assert_eq!(proc.set_uid(0), Err(LxError::EPERM));
    proc.change_capabilities(|c| {
        c.effective = bit(CAP_SETUID);
        Ok(())
    })
    .unwrap();
    assert_eq!(proc.set_uid(0), Ok(()));
    let creds = proc.credentials();
    assert_eq!((creds.ruid, creds.euid, creds.suid), (0, 0, 0));
    assert_eq!(creds.caps.effective, CAP_FULL_SET);
}

// ---------------------------------------------------------------- setfsuid

#[test]
fn setfsuid_drops_and_raises_the_filesystem_set_alone() {
    let proc = a_process();
    proc.set_fsuid(1000);
    let caps = proc.capabilities();
    assert_eq!(
        (caps.effective, caps.permitted),
        (AFTER_SETFSUID, CAP_FULL_SET)
    );
    assert!(proc.capable(CAP_KILL));
    assert!(!proc.capable(CAP_DAC_OVERRIDE));
    proc.set_fsuid(0);
    assert_eq!(proc.capabilities().effective, CAP_FULL_SET);
}

#[test]
fn setfsuid_back_to_root_raises_only_what_is_permitted() {
    // capset(FULL & ~CAP_CHOWN, FULL & ~CAP_CHOWN, 0), setfsuid(1000),
    // setfsuid(0): eff is FULL & ~CAP_CHOWN on Linux.
    let mut caps = Capabilities {
        effective: without(CAP_CHOWN),
        permitted: without(CAP_CHOWN),
        ..Capabilities::ROOT
    };
    caps.follow_setfsuid(0, 1000);
    caps.follow_setfsuid(1000, 0);
    assert_eq!(caps.effective, without(CAP_CHOWN));
    // Between two ids that are not 0 nothing moves.
    let mut user = Capabilities::for_uids(0, 1000, 0);
    user.follow_setfsuid(1000, 2000);
    assert_eq!(user, Capabilities::for_uids(0, 1000, 0));
}

// -------------------------------------------------------------------- exec

#[test]
fn an_exec_as_root_makes_the_bounding_set_permitted_and_effective() {
    // capset(FULL & ~CAP_CHOWN, FULL & ~CAP_CHOWN, 0), KEEPCAPS, exec: eff
    // and prm the whole set again on Linux, keepcaps 0.
    let mut inner = LinuxProcessInner {
        keep_caps: true,
        ..Default::default()
    };
    inner.credentials.caps.effective = without(CAP_CHOWN);
    inner.credentials.caps.permitted = without(CAP_CHOWN);
    inner.apply_exec_ids_from_a_normal_mount(0o755, 0, 0);
    let _ = inner.reset_for_exec(false);
    assert_eq!(inner.credentials.caps, Capabilities::ROOT);
    assert!(!inner.keep_caps);
    // PR_CAPBSET_DROP(CAP_CHOWN), exec: all three without it.
    inner.credentials.caps.bounding = without(CAP_CHOWN);
    inner.apply_exec_ids_from_a_normal_mount(0o755, 0, 0);
    let caps = inner.credentials.caps;
    assert_eq!(
        (caps.effective, caps.permitted, caps.bounding),
        (without(CAP_CHOWN), without(CAP_CHOWN), without(CAP_CHOWN))
    );
}

#[test]
fn an_exec_with_only_the_real_uid_root_permits_but_does_not_make_effective() {
    // setresuid(0, 1000, 1000), exec: eff 0, prm the whole set on Linux.
    let mut caps = Capabilities::for_uids(0, 1000, 1000);
    caps.follow_exec(0, 1000, false);
    assert_eq!((caps.effective, caps.permitted), (0, CAP_FULL_SET));
}

#[test]
fn an_exec_as_a_user_clears_what_keepcaps_kept() {
    // KEEPCAPS, setresuid(1000, 1000, 1000), exec: eff 0 prm 0 on Linux.
    let mut caps = Capabilities {
        effective: 0,
        ..Capabilities::ROOT
    };
    caps.follow_exec(1000, 1000, false);
    assert_eq!((caps.effective, caps.permitted), (0, 0));
    assert_eq!(caps.bounding, CAP_FULL_SET);
}

#[test]
fn a_set_user_id_root_image_run_by_a_user_gets_the_bounding_set() {
    let mut inner = LinuxProcessInner::default();
    inner.credentials = user();
    inner.credentials.caps.bounding = without(CAP_SYS_BOOT);
    inner.apply_exec_ids_from_a_normal_mount(0o4755, 0, 0);
    let caps = inner.credentials.caps;
    assert_eq!(inner.credentials.euid, 0);
    assert_eq!(
        (caps.effective, caps.permitted),
        (without(CAP_SYS_BOOT), without(CAP_SYS_BOOT))
    );
    // Under no_new_privs the bit is not honoured, so nothing is gained.
    let mut inner = LinuxProcessInner {
        no_new_privs: true,
        ..Default::default()
    };
    inner.credentials = user();
    inner.apply_exec_ids_from_a_normal_mount(0o4755, 0, 0);
    assert_eq!(inner.credentials.caps.permitted, 0);
}

#[test]
fn the_inheritable_set_joins_the_bounding_one_for_a_root_image() {
    // `handle_privileged_root`: pP' = cap_bset | pI.
    let mut caps = Capabilities {
        inheritable: bit(CAP_CHOWN),
        bounding: without(CAP_CHOWN),
        ..Capabilities::ROOT
    };
    caps.follow_exec(0, 0, false);
    assert_eq!(caps.permitted, CAP_FULL_SET);
    assert_eq!(caps.inheritable, bit(CAP_CHOWN));
}

#[test]
fn no_new_privs_cuts_the_new_permitted_set_to_the_old_one() {
    // "they get no more than they had, and maybe less".
    let mut caps = Capabilities {
        effective: without(CAP_CHOWN),
        permitted: without(CAP_CHOWN),
        ..Capabilities::ROOT
    };
    caps.follow_exec(0, 0, true);
    assert_eq!(
        (caps.effective, caps.permitted),
        (without(CAP_CHOWN), without(CAP_CHOWN))
    );
    // The exec of a process asks its own no_new_privs.
    let mut inner = LinuxProcessInner {
        no_new_privs: true,
        ..Default::default()
    };
    inner.credentials.caps.effective = without(CAP_CHOWN);
    inner.credentials.caps.permitted = without(CAP_CHOWN);
    inner.apply_exec_ids_from_a_normal_mount(0o755, 0, 0);
    assert_eq!(inner.credentials.caps.permitted, without(CAP_CHOWN));
}

#[test]
fn a_fork_copies_the_sets() {
    let mut parent = LinuxProcessInner::default();
    parent.credentials.caps.effective = without(CAP_CHOWN);
    parent.credentials.caps.bounding = without(CAP_KILL);
    let child = parent.forked_child(41, 42, 999);
    assert_eq!(child.credentials.caps, parent.credentials.caps);
}

// ------------------------------------------------------------ changing them

#[test]
fn a_refused_change_leaves_the_sets_as_they_were() {
    let proc = a_process();
    let refused = proc.change_capabilities(|c| {
        c.effective = 0;
        Err(LxError::EPERM)
    });
    assert_eq!(refused, Err(LxError::EPERM));
    assert_eq!(proc.capabilities(), Capabilities::ROOT);
}

// -------------------------------------------------------- the file checks

fn access(creds: &Credentials, mode: u16, is_dir: bool, requested: u16) -> LxResult {
    LinuxProcess::access_verdict(creds, 1000, 1000, mode, is_dir, requested, true)
}

#[test]
fn without_either_dac_capability_root_is_checked_by_the_bits() {
    // A 0600 file and a 0700 directory of uid 1000: EACCES on Linux.
    let creds = root_with(CAP_FULL_SET & !bit(CAP_DAC_OVERRIDE) & !bit(CAP_DAC_READ_SEARCH));
    assert_eq!(
        access(&creds, 0o600, false, ACCESS_READ),
        Err(LxError::EACCES)
    );
    assert_eq!(
        access(&creds, 0o700, true, ACCESS_EXEC),
        Err(LxError::EACCES)
    );
    // What the bits give still goes.
    assert_eq!(access(&creds, 0o604, false, ACCESS_READ), Ok(()));
    // And they are read as for anyone (`acl_permission_check`): the owner's
    // bits on root's own file, the group's when root is in the file's group,
    // the others' only after both.
    let verdict = |owner_uid, owner_gid, mode| {
        LinuxProcess::access_verdict(&creds, owner_uid, owner_gid, mode, false, ACCESS_READ, true)
    };
    assert_eq!(verdict(0, 0, 0o600), Ok(()));
    assert_eq!(verdict(1000, 0, 0o040), Ok(()));
    assert_eq!(verdict(1000, 0, 0o604), Err(LxError::EACCES));
}

#[test]
fn dac_read_search_reads_and_searches_but_never_writes() {
    let creds = root_with(without(CAP_DAC_OVERRIDE));
    assert_eq!(access(&creds, 0o600, false, ACCESS_READ), Ok(()));
    assert_eq!(
        access(&creds, 0o600, false, ACCESS_WRITE),
        Err(LxError::EACCES)
    );
    assert_eq!(
        access(&creds, 0o700, false, ACCESS_READ | ACCESS_EXEC),
        Err(LxError::EACCES)
    );
    assert_eq!(
        access(&creds, 0o700, true, ACCESS_READ | ACCESS_EXEC),
        Ok(())
    );
    assert_eq!(
        access(&creds, 0o700, true, ACCESS_WRITE | ACCESS_EXEC),
        Err(LxError::EACCES)
    );
}

#[test]
fn dac_override_does_all_but_execute_a_file_nobody_may_execute() {
    let creds = root_with(without(CAP_DAC_READ_SEARCH));
    assert_eq!(
        access(&creds, 0o600, false, ACCESS_READ | ACCESS_WRITE),
        Ok(())
    );
    assert_eq!(
        access(&creds, 0o700, true, ACCESS_WRITE | ACCESS_EXEC),
        Ok(())
    );
    assert_eq!(
        access(&creds, 0o600, false, ACCESS_EXEC),
        Err(LxError::EACCES)
    );
    assert_eq!(access(&creds, 0o610, false, ACCESS_EXEC), Ok(()));
    // A 0700 directory is searched: apk's /lib/apk/exec.
    assert_eq!(access(&creds, 0o700, true, ACCESS_EXEC), Ok(()));
}

#[test]
fn access_checks_with_what_the_real_uid_would_hold() {
    // `access_override_creds`: the permitted set when the real uid is 0,
    // nothing otherwise -- whatever the effective set says.
    let saved_root = Credentials {
        ruid: 0,
        caps: Capabilities::for_uids(0, 1000, 1000),
        ..user()
    };
    let check = |creds: &Credentials| {
        LinuxProcess::access_verdict(creds, 2000, 2000, 0o600, false, ACCESS_READ, false)
    };
    assert_eq!(check(&saved_root), Ok(()));
    assert_eq!(
        LinuxProcess::access_verdict(&saved_root, 2000, 2000, 0o600, false, ACCESS_READ, true),
        Err(LxError::EACCES)
    );
    let set_user_id_root = Credentials {
        euid: 0,
        fsuid: 0,
        caps: Capabilities::ROOT,
        ..user()
    };
    assert_eq!(check(&set_user_id_root), Err(LxError::EACCES));
}

#[test]
fn cap_fowner_is_what_lets_root_treat_a_file_as_its_own() {
    // chmod and explicit times of a file of uid 1000: EPERM on Linux once
    // capset has taken CAP_FOWNER out of the effective set.
    let creds = root_with(without(CAP_FOWNER));
    assert_eq!(
        LinuxProcess::chmod_bits(&creds, 1000, 1000, 0o100600, 0o644),
        Err(LxError::EPERM)
    );
    assert_eq!(
        LinuxProcess::utimes_verdict(&creds, 1000, 1000, 0o600, false, true),
        Err(LxError::EPERM)
    );
    assert!(!LinuxProcess::owner_or_capable(&creds, 1000));
    // A set-user-ID file is not a safe source to link.
    assert_eq!(
        LinuxProcess::link_verdict(&creds, 1000, 1000, 0o4755, FileType::File),
        Err(LxError::EPERM)
    );
    let root = Credentials::default();
    assert_eq!(
        LinuxProcess::chmod_bits(&root, 1000, 1000, 0o100600, 0o644),
        Ok(0o100644)
    );
    assert_eq!(
        LinuxProcess::link_verdict(&root, 1000, 1000, 0o4755, FileType::File),
        Ok(())
    );
}

fn meta(type_: FileType, mode: u16, uid: u32) -> Metadata {
    use rcore_fs::vfs::Timespec;
    let never = Timespec { sec: 0, nsec: 0 };
    Metadata {
        dev: 1,
        inode: 2,
        size: 0,
        blk_size: 4096,
        blocks: 0,
        atime: never,
        mtime: never,
        ctime: never,
        type_,
        mode,
        nlinks: 1,
        uid: uid as _,
        gid: uid as _,
        rdev: 0,
    }
}

#[test]
fn the_sticky_bit_holds_against_root_without_cap_fowner() {
    let (dir, entry) = (
        meta(FileType::Dir, 0o1777, 1000),
        meta(FileType::File, 0o644, 2000),
    );
    let creds = root_with(without(CAP_FOWNER));
    assert_eq!(
        LinuxProcess::sticky_verdict(&creds, &dir, &entry),
        Err(LxError::EPERM)
    );
    assert_eq!(
        LinuxProcess::sticky_verdict(&Credentials::default(), &dir, &entry),
        Ok(())
    );
}

#[test]
fn cap_fsetid_keeps_the_set_group_id_bit_on_another_group() {
    // `in_group_or_capable`: root is not in group 1000.
    let creds = root_with(without(CAP_FSETID));
    assert_eq!(
        LinuxProcess::chmod_bits(&creds, 1000, 1000, 0o100600, 0o2755),
        Ok(0o100755)
    );
    assert_eq!(
        LinuxProcess::chmod_bits(&Credentials::default(), 1000, 1000, 0o100600, 0o2755),
        Ok(0o102755)
    );
}

#[test]
fn cap_chown_is_what_lets_a_process_give_a_file_away() {
    // capset(FULL & ~CAP_CHOWN, FULL, 0) and chown of its own file to 1:1
    // is EPERM on Linux; a user that kept CAP_CHOWN gives root's file away.
    let proc = a_process();
    proc.change_capabilities(|c| {
        c.effective = without(CAP_CHOWN);
        Ok(())
    })
    .unwrap();
    let mut file = meta(FileType::File, 0o644, 0);
    assert_eq!(proc.chown_metadata(&mut file, 1, 1), Err(LxError::EPERM));
    assert_eq!(file.uid, 0);

    let kept = a_process();
    kept.set_keep_caps(true);
    kept.set_resuid(1000, 1000, 1000).unwrap();
    assert_eq!(
        kept.chown_metadata(&mut file, 2000, 2000),
        Err(LxError::EPERM)
    );
    kept.change_capabilities(|c| {
        c.effective = bit(CAP_CHOWN);
        Ok(())
    })
    .unwrap();
    assert_eq!(kept.chown_metadata(&mut file, 2000, 2000), Ok(()));
    assert_eq!((file.uid, file.gid), (2000, 2000));
}

#[test]
fn the_status_lines_and_capget_read_the_kept_sets() {
    let proc = a_process();
    proc.change_capabilities(|c| {
        c.effective = without(CAP_CHOWN);
        c.inheritable = bit(CAP_KILL);
        c.bounding = without(CAP_SYS_BOOT);
        Ok(())
    })
    .unwrap();
    let caps = proc.capabilities();
    assert_eq!(caps.effective, without(CAP_CHOWN));
    assert_eq!(caps.permitted, CAP_FULL_SET);
    assert_eq!(caps.inheritable, bit(CAP_KILL));
    assert!(!caps.bounds(CAP_SYS_BOOT));
    assert!(proc.capable(CAP_KILL) && !proc.capable(CAP_CHOWN));
}

#[test]
fn each_gate_asks_its_own_capability_and_not_the_uid() {
    // Root that put one capability down keeps every other gate.
    let target = user();
    let gates = |c: &Credentials| {
        [
            LinuxProcess::may_attach_to(c, &target, false, SUID_DUMP_USER),
            LinuxProcess::may_touch_limits_of(c, &target),
            LinuxProcess::may_signal_cred(c, &target),
            LinuxProcess::may_set_priority_of(c, &target),
        ]
    };
    assert_eq!(gates(&Credentials::default()), [true; 4]);
    assert_eq!(
        gates(&root_with(without(CAP_SYS_PTRACE))),
        [false, true, true, true]
    );
    assert_eq!(
        gates(&root_with(without(CAP_SYS_RESOURCE))),
        [true, false, true, true]
    );
    assert_eq!(
        gates(&root_with(without(CAP_KILL))),
        [true, true, false, true]
    );
    assert_eq!(
        gates(&root_with(without(CAP_SYS_NICE))),
        [true, true, true, false]
    );
    // And a user holding one passes that gate alone.
    let mut kill_only = user();
    kill_only.caps.effective = bit(CAP_KILL);
    let other = Credentials {
        ruid: 2000,
        euid: 2000,
        suid: 2000,
        ..user()
    };
    assert!(LinuxProcess::may_signal_cred(&kill_only, &other));
    assert!(!LinuxProcess::may_touch_limits_of(&kill_only, &other));
}
