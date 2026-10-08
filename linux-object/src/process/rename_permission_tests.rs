use super::*;
use rcore_fs::vfs::Timespec;

const ALICE: u32 = 1000;
const BOB: u32 = 2000;
const TMP: usize = 40;
const HOME: usize = 41;

fn creds(uid: u32) -> Credentials {
    Credentials {
        ruid: uid,
        euid: uid,
        suid: uid,
        rgid: uid,
        egid: uid,
        sgid: uid,
        fsuid: uid,
        fsgid: uid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

fn meta(inode: usize, type_: FileType, mode: u16, uid: u32) -> Metadata {
    Metadata {
        dev: 1,
        inode,
        size: 0,
        blk_size: 4096,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_,
        mode,
        nlinks: 1,
        uid: uid as _,
        gid: uid as _,
        rdev: 0,
    }
}

/// `/tmp`: root's, world-writable, sticky.
fn sticky_tmp() -> Metadata {
    meta(TMP, FileType::Dir, 0o1777, ROOT_UID)
}

/// A plain shared directory without the sticky bit.
fn plain_dir(inode: usize) -> Metadata {
    meta(inode, FileType::Dir, 0o777, ROOT_UID)
}

fn file(inode: usize, uid: u32) -> Metadata {
    meta(inode, FileType::File, 0o644, uid)
}

fn dir(inode: usize, mode: u16, uid: u32) -> Metadata {
    meta(inode, FileType::Dir, mode, uid)
}

fn verdict(
    uid: u32,
    old_dir: &Metadata,
    old: &Metadata,
    new_dir: &Metadata,
    new: Option<&Metadata>,
) -> LxResult {
    LinuxProcess::rename_verdict(&creds(uid), old_dir, old, new_dir, new)
}

#[test]
fn in_a_sticky_directory_the_target_must_be_yours_too() {
    // `mv mine yours` in /tmp: `may_delete(new_dir, new_dentry)` is the
    // same sticky test `rm yours` fails.
    let tmp = sticky_tmp();
    let mine = file(1, ALICE);
    let yours = file(2, BOB);
    assert_eq!(
        verdict(ALICE, &tmp, &mine, &tmp, Some(&yours)),
        Err(LxError::EPERM)
    );
    // Over a file of one's own, over nothing, by root, or by the owner
    // of the directory: allowed.
    let also_mine = file(3, ALICE);
    assert_eq!(verdict(ALICE, &tmp, &mine, &tmp, Some(&also_mine)), Ok(()));
    assert_eq!(verdict(ALICE, &tmp, &mine, &tmp, None), Ok(()));
    assert_eq!(verdict(ROOT_UID, &tmp, &mine, &tmp, Some(&yours)), Ok(()));
    let bobs_sticky = meta(TMP, FileType::Dir, 0o1777, BOB);
    let carols = file(4, 3000);
    assert_eq!(
        verdict(BOB, &bobs_sticky, &mine, &bobs_sticky, Some(&carols)),
        Ok(())
    );
}

#[test]
fn the_sticky_bit_on_the_source_directory_still_counts() {
    let tmp = sticky_tmp();
    let yours = file(2, BOB);
    assert_eq!(
        verdict(ALICE, &tmp, &yours, &plain_dir(HOME), None),
        Err(LxError::EPERM)
    );
}

#[test]
fn without_the_sticky_bit_anyone_may_replace_anyone() {
    let shared = plain_dir(HOME);
    assert_eq!(
        verdict(
            ALICE,
            &shared,
            &file(1, ALICE),
            &shared,
            Some(&file(2, BOB))
        ),
        Ok(())
    );
}

#[test]
fn a_directory_onto_a_file_is_enotdir_and_a_file_onto_a_directory_eisdir() {
    let home = plain_dir(HOME);
    let d = dir(5, 0o755, ALICE);
    let f = file(6, ALICE);
    assert_eq!(
        verdict(ALICE, &home, &d, &home, Some(&f)),
        Err(LxError::ENOTDIR)
    );
    assert_eq!(
        verdict(ALICE, &home, &f, &home, Some(&d)),
        Err(LxError::EISDIR)
    );
    // A directory onto an (empty) directory is the filesystem's call.
    let e = dir(7, 0o755, ALICE);
    assert_eq!(verdict(ALICE, &home, &d, &home, Some(&e)), Ok(()));
}

#[test]
fn the_sticky_eperm_on_the_target_comes_before_its_type() {
    // `may_delete` checks `check_sticky` before `d_is_dir(victim)`.
    let tmp = sticky_tmp();
    let d = dir(5, 0o755, ALICE);
    let bobs_file = file(2, BOB);
    assert_eq!(
        verdict(ALICE, &tmp, &d, &tmp, Some(&bobs_file)),
        Err(LxError::EPERM)
    );
}

#[test]
fn moving_a_directory_to_another_parent_needs_write_permission_on_it() {
    // `vfs_rename`: `if (is_dir && new_dir != old_dir)
    // error = inode_permission(source, MAY_WRITE);` -- the move rewrites
    // the directory's own `..`.
    let home = plain_dir(HOME);
    let elsewhere = plain_dir(42);
    let bobs_dir = dir(5, 0o755, BOB);
    assert_eq!(
        verdict(ALICE, &home, &bobs_dir, &elsewhere, None),
        Err(LxError::EACCES)
    );
    // Within the same parent it is only a rename: no such requirement.
    assert_eq!(verdict(ALICE, &home, &bobs_dir, &home, None), Ok(()));
    // A file has no `..` to rewrite.
    assert_eq!(
        verdict(ALICE, &home, &file(6, BOB), &elsewhere, None),
        Ok(())
    );
    // A writable directory, its owner, or root: allowed.
    assert_eq!(
        verdict(ALICE, &home, &dir(5, 0o777, BOB), &elsewhere, None),
        Ok(())
    );
    assert_eq!(verdict(BOB, &home, &bobs_dir, &elsewhere, None), Ok(()));
    assert_eq!(
        verdict(ROOT_UID, &home, &bobs_dir, &elsewhere, None),
        Ok(())
    );
}

#[test]
fn the_same_parent_is_the_same_inode_on_the_same_device() {
    // The parents reached through two different paths (`.` and `..`, a
    // bind mount) compare by (dev, inode), not by the path typed.
    let home = plain_dir(HOME);
    let mut other_device = plain_dir(HOME);
    other_device.dev = 2;
    let bobs_dir = dir(5, 0o755, BOB);
    assert_eq!(
        verdict(ALICE, &home, &bobs_dir, &other_device, None),
        Err(LxError::EACCES)
    );
}
