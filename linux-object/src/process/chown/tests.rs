//! Each case is one of the 24 checks of `chown-survey`, with what Linux
//! 6.18 answered for it with glibc and with musl.

use super::super::dup_fd_tests::a_process;
use super::super::*;

const ROOT: u32 = 0;
const NOBODY: u32 = 65534;
const U: u32 = 1000;
const G: u32 = 1000;
const OTHER: u32 = 2000;

/// `uid:gid` with no supplementary groups, without `drop` in its effective
/// set if it is root.
fn as_(uid: u32, gid: u32, drop: Option<u32>) -> LinuxProcess {
    let proc = a_process();
    if uid != ROOT {
        proc.set_resgid(gid, gid, gid).unwrap();
        proc.set_resuid(uid, uid, uid).unwrap();
    }
    if let Some(cap) = drop {
        proc.change_capabilities(|c| {
            c.effective &= !(1 << cap);
            Ok(())
        })
        .unwrap();
    }
    proc
}

fn a(type_: FileType, uid: u32, gid: u32, mode: u16) -> Metadata {
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
        gid: gid as _,
        rdev: 0,
    }
}

fn file(uid: u32, gid: u32, mode: u16) -> Metadata {
    a(FileType::File, uid, gid, mode)
}

/// What `proc`'s `chown(f, uid, gid)` leaves of `f`: its owner, group and
/// mode, or the errno -- and a refusal must leave `f` as it was.
fn chown(proc: &LinuxProcess, mut f: Metadata, uid: u32, gid: u32) -> LxResult<(u32, u32, u16)> {
    let before = (f.uid as u32, f.gid as u32, f.mode);
    match proc.chown_metadata(&mut f, uid, gid) {
        Ok(()) => Ok((f.uid as u32, f.gid as u32, f.mode)),
        Err(e) => {
            assert_eq!(
                (f.uid as u32, f.gid as u32, f.mode),
                before,
                "refused, and changed"
            );
            Err(e)
        }
    }
}

#[test]
fn asking_for_nothing_asks_nobody_unless_a_set_id_bit_goes() {
    let nobody = as_(NOBODY, NOBODY, None);
    assert_eq!(
        chown(&nobody, file(ROOT, ROOT, 0o644), NO_ID, NO_ID),
        Ok((ROOT, ROOT, 0o644))
    );
    // The set-user-ID bit goes, and taking it is a mode change.
    assert_eq!(
        chown(&nobody, file(ROOT, ROOT, 0o4755), NO_ID, NO_ID),
        Err(LxError::EPERM)
    );
    assert_eq!(
        chown(&nobody, file(ROOT, ROOT, 0o644), ROOT, NO_ID),
        Err(LxError::EPERM),
        "naming the owner it already has is still asking for it"
    );
}

#[test]
fn the_owner_may_keep_the_group_the_file_has_without_being_in_it() {
    let owner = as_(U, G, None);
    assert_eq!(
        chown(&owner, file(U, OTHER, 0o644), NO_ID, OTHER),
        Ok((U, OTHER, 0o644))
    );
    assert_eq!(
        chown(&owner, file(U, G, 0o644), NO_ID, OTHER),
        Err(LxError::EPERM)
    );
    assert_eq!(
        chown(&owner, file(U, OTHER, 0o644), NO_ID, G),
        Ok((U, G, 0o644)),
        "to a group it is in"
    );
    assert_eq!(
        chown(&owner, file(U, G, 0o644), U, NO_ID),
        Ok((U, G, 0o644))
    );
    assert_eq!(
        chown(&owner, file(U, G, 0o644), OTHER, NO_ID),
        Err(LxError::EPERM)
    );
}

#[test]
fn a_directory_keeps_its_set_group_id_bit() {
    let dir = |uid, gid| a(FileType::Dir, uid, gid, 0o2775);
    assert_eq!(
        chown(&as_(ROOT, ROOT, None), dir(ROOT, ROOT), NO_ID, OTHER),
        Ok((ROOT, OTHER, 0o2775))
    );
    assert_eq!(
        chown(&as_(U, G, None), dir(U, G), NO_ID, NO_ID),
        Ok((U, G, 0o2775))
    );
}

#[test]
fn set_group_id_without_group_execute_stays_for_its_group() {
    let owner = as_(U, G, None);
    assert_eq!(
        chown(&owner, file(U, G, 0o2644), NO_ID, NO_ID),
        Ok((U, G, 0o2644))
    );
    assert_eq!(
        chown(&owner, file(U, OTHER, 0o2644), NO_ID, NO_ID),
        Ok((U, OTHER, 0o644)),
        "outside the group, it goes"
    );
    assert_eq!(
        chown(&owner, file(U, G, 0o2755), NO_ID, NO_ID),
        Ok((U, G, 0o755)),
        "with group execute, it goes"
    );
    assert_eq!(
        chown(&owner, file(U, G, 0o4755), NO_ID, NO_ID),
        Ok((U, G, 0o755))
    );
    assert_eq!(
        chown(
            &as_(ROOT, ROOT, None),
            file(ROOT, ROOT, 0o2644),
            NO_ID,
            OTHER
        ),
        Ok((ROOT, OTHER, 0o2644))
    );
    assert_eq!(
        chown(&as_(ROOT, ROOT, None), file(ROOT, ROOT, 0o4755), U, NO_ID),
        Ok((U, ROOT, 0o755))
    );
}

#[test]
fn taking_a_set_user_id_bit_away_is_for_the_owner_or_cap_fowner() {
    let no_fowner = as_(ROOT, ROOT, Some(CAP_FOWNER));
    assert_eq!(
        chown(&no_fowner, file(U, G, 0o4755), OTHER, NO_ID),
        Err(LxError::EPERM),
        "CAP_CHOWN gives the file away, but not the bit"
    );
    assert_eq!(
        chown(&no_fowner, file(ROOT, ROOT, 0o4755), U, NO_ID),
        Ok((U, ROOT, 0o755))
    );
    assert_eq!(
        chown(&no_fowner, file(U, G, 0o644), OTHER, NO_ID),
        Ok((OTHER, G, 0o644))
    );
    let no_chown = as_(ROOT, ROOT, Some(CAP_CHOWN));
    assert_eq!(
        chown(&no_chown, file(U, G, 0o644), NO_ID, NO_ID),
        Ok((U, G, 0o644))
    );
    assert_eq!(
        chown(&no_chown, file(U, G, 0o644), ROOT, NO_ID),
        Err(LxError::EPERM)
    );
    assert_eq!(
        chown(&no_chown, file(U, G, 0o4755), NO_ID, NO_ID),
        Ok((U, G, 0o755))
    );
}

#[test]
fn cap_fsetid_keeps_set_group_id_where_the_group_would_not() {
    // Root is not in `OTHER`; `CAP_FSETID` answers for it.
    assert_eq!(
        chown(
            &as_(ROOT, ROOT, None),
            file(ROOT, OTHER, 0o2644),
            NO_ID,
            NO_ID
        ),
        Ok((ROOT, OTHER, 0o2644))
    );
    // The set-user-ID bit makes it a mode change, which asks about the NEW
    // group: root without `CAP_FSETID` is not in it.
    assert_eq!(
        chown(
            &as_(ROOT, ROOT, Some(CAP_FSETID)),
            file(ROOT, ROOT, 0o6644),
            NO_ID,
            OTHER
        ),
        Ok((ROOT, OTHER, 0o644))
    );
}
