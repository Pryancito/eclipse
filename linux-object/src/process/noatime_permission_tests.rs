use super::*;
use rcore_fs::vfs::FileSystem;
use rcore_fs_ramfs::RamFS;

fn creds(euid: u32, fsuid: u32) -> Credentials {
    Credentials {
        ruid: euid,
        euid,
        suid: euid,
        rgid: euid,
        egid: euid,
        sgid: euid,
        fsuid,
        fsgid: fsuid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

#[test]
fn the_owner_and_root_and_nobody_else() {
    assert!(LinuxProcess::owner_or_capable(&creds(1000, 1000), 1000));
    assert!(LinuxProcess::owner_or_capable(
        &creds(ROOT_UID, ROOT_UID),
        1000
    ));
    assert!(!LinuxProcess::owner_or_capable(&creds(2000, 2000), 1000));
}

#[test]
fn ownership_is_judged_by_the_filesystem_uid() {
    // `vfsuid_eq_kuid(vfsuid, current_fsuid())`: a server that switched
    // only its filesystem id to the file's owner counts as the owner,
    // and one that kept the owner's effective id but moved its fsuid
    // away does not.
    assert!(LinuxProcess::owner_or_capable(&creds(2000, 1000), 1000));
    assert!(!LinuxProcess::owner_or_capable(&creds(1000, 2000), 1000));
    // Root is root by the filesystem id too.
    assert!(!LinuxProcess::owner_or_capable(
        &creds(ROOT_UID, 2000),
        1000
    ));
}

#[test]
fn check_owner_or_capable_is_eperm_off_real_metadata() {
    // `may_open`: `return -EPERM`, not the `EACCES` of a permission bit;
    // the file may well be world-readable.
    let inode: Arc<dyn INode> = RamFS::new()
        .root_inode()
        .create("f", FileType::File, 0o644)
        .unwrap();
    let mut meta = inode.metadata().unwrap();
    meta.uid = 1000;
    inode.set_metadata(&meta).unwrap();
    let meta = inode.metadata().unwrap();
    // Root, then a process that dropped to the owner, then one that
    // dropped to somebody else (a dropped process cannot come back).
    let root = super::dup_fd_tests::a_process();
    assert_eq!(root.check_owner_or_capable(&meta), Ok(()));
    let owner = super::dup_fd_tests::a_process();
    owner.set_resuid(1000, 1000, 1000).unwrap();
    assert_eq!(owner.check_owner_or_capable(&meta), Ok(()));
    let other = super::dup_fd_tests::a_process();
    other.set_resuid(2000, 2000, 2000).unwrap();
    assert_eq!(other.check_owner_or_capable(&meta), Err(LxError::EPERM));
}
