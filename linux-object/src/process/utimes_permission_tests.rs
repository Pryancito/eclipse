use super::*;
use rcore_fs::vfs::FileSystem;
use rcore_fs_ramfs::RamFS;

const OWNER: u32 = 1000;
const OTHER: u32 = 2000;

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

fn verdict(uid: u32, mode: u16, explicit: bool) -> LxResult {
    LinuxProcess::utimes_verdict(&creds(uid), OWNER, OWNER, mode, false, explicit)
}

#[test]
fn the_owner_and_root_may_set_any_time_on_a_read_only_file() {
    assert_eq!(verdict(OWNER, 0o444, true), Ok(()));
    assert_eq!(verdict(OWNER, 0o444, false), Ok(()));
    assert_eq!(verdict(ROOT_UID, 0o444, true), Ok(()));
    assert_eq!(verdict(ROOT_UID, 0o000, false), Ok(()));
}

#[test]
fn explicit_times_on_someone_elses_file_are_eperm_even_with_write_access() {
    // `touch -d yesterday /tmp/theirs` on a 0666 file: writable, yet the
    // times are the owner's to forge, not the world's (`setattr_prepare`:
    // `ATTR_MTIME_SET` without `inode_owner_or_capable` is EPERM).
    assert_eq!(verdict(OTHER, 0o666, true), Err(LxError::EPERM));
    assert_eq!(verdict(OTHER, 0o644, true), Err(LxError::EPERM));
}

#[test]
fn a_touch_needs_write_access_and_nothing_more() {
    // `touch /tmp/theirs` (times NULL): `ATTR_TOUCH` falls back on
    // `inode_permission(MAY_WRITE)`, so a writable file may be touched by
    // anyone and a read-only one answers EACCES, not EPERM.
    assert_eq!(verdict(OTHER, 0o666, false), Ok(()));
    assert_eq!(verdict(OTHER, 0o644, false), Err(LxError::EACCES));
}

#[test]
fn ownership_is_the_filesystem_uid_not_the_effective_one() {
    // `inode_owner_or_capable()`: `vfsuid_eq_kuid(vfsuid, current_fsuid())`.
    let mut c = creds(OTHER);
    c.fsuid = OWNER;
    assert_eq!(
        LinuxProcess::utimes_verdict(&c, OWNER, OWNER, 0o444, false, true),
        Ok(())
    );
    let mut c = creds(OWNER);
    c.fsuid = OTHER;
    assert_eq!(
        LinuxProcess::utimes_verdict(&c, OWNER, OWNER, 0o444, false, true),
        Err(LxError::EPERM)
    );
}

#[test]
fn check_utimes_reads_the_verdict_off_real_metadata() {
    let proc = super::dup_fd_tests::a_process();
    proc.set_resuid(OTHER, OTHER, OTHER).unwrap();
    let inode: Arc<dyn INode> = RamFS::new()
        .root_inode()
        .create("f", FileType::File, 0o644)
        .unwrap();
    let mut meta = inode.metadata().unwrap();
    meta.uid = OWNER as usize;
    meta.gid = OWNER as usize;
    inode.set_metadata(&meta).unwrap();
    let meta = inode.metadata().unwrap();
    assert_eq!(proc.check_utimes(&meta, true), Err(LxError::EPERM));
    assert_eq!(proc.check_utimes(&meta, false), Err(LxError::EACCES));
    let mut meta = meta;
    meta.mode = 0o666;
    assert_eq!(proc.check_utimes(&meta, false), Ok(()));
}
