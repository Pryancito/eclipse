use super::*;

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

fn verdict(uid: u32, mode: u16, type_: FileType) -> LxResult {
    LinuxProcess::link_verdict(&creds(uid), OWNER, OWNER, mode, type_)
}

#[test]
fn nobody_links_a_directory_not_even_root() {
    // `vfs_link`: `if (S_ISDIR(inode->i_mode)) return -EPERM;`
    assert_eq!(verdict(ROOT_UID, 0o777, FileType::Dir), Err(LxError::EPERM));
    assert_eq!(verdict(OWNER, 0o777, FileType::Dir), Err(LxError::EPERM));
}

#[test]
fn the_owner_and_root_link_whatever_they_own_however_it_is_set() {
    assert_eq!(verdict(OWNER, 0o000, FileType::File), Ok(()));
    assert_eq!(verdict(OWNER, 0o4755, FileType::File), Ok(()));
    assert_eq!(verdict(OWNER, 0o600, FileType::NamedPipe), Ok(()));
    assert_eq!(verdict(ROOT_UID, 0o000, FileType::File), Ok(()));
    assert_eq!(verdict(ROOT_UID, 0o4755, FileType::CharDevice), Ok(()));
}

#[test]
fn someone_else_needs_a_safe_source_they_can_read_and_write() {
    // The `protected_hardlinks` case: `ln /etc/shadow ~/mine`.
    assert_eq!(verdict(OTHER, 0o600, FileType::File), Err(LxError::EPERM));
    assert_eq!(
        verdict(OTHER, 0o644, FileType::File),
        Err(LxError::EPERM),
        "readable is not enough"
    );
    assert_eq!(
        verdict(OTHER, 0o622, FileType::File),
        Err(LxError::EPERM),
        "writable is not enough either"
    );
    assert_eq!(verdict(OTHER, 0o666, FileType::File), Ok(()));
}

#[test]
fn a_setuid_or_setgid_executable_is_never_a_safe_source() {
    assert_eq!(verdict(OTHER, 0o4666, FileType::File), Err(LxError::EPERM));
    assert_eq!(verdict(OTHER, 0o2676, FileType::File), Err(LxError::EPERM));
    // Setgid WITHOUT group-exec is mandatory locking, not privilege:
    // still a safe source (`(S_ISGID | S_IXGRP)` both, or neither counts).
    assert_eq!(verdict(OTHER, 0o2666, FileType::File), Ok(()));
}

#[test]
fn a_special_file_is_never_a_safe_source_for_someone_else() {
    // "Special files should not get pinned to the filesystem."
    assert_eq!(
        verdict(OTHER, 0o666, FileType::NamedPipe),
        Err(LxError::EPERM)
    );
    assert_eq!(
        verdict(OTHER, 0o666, FileType::CharDevice),
        Err(LxError::EPERM)
    );
    assert_eq!(
        verdict(OTHER, 0o777, FileType::SymLink),
        Err(LxError::EPERM)
    );
    assert_eq!(verdict(OTHER, 0o777, FileType::Socket), Err(LxError::EPERM));
}

#[test]
fn the_answer_is_eperm_not_the_eacces_of_the_access_check() {
    // `may_linkat` returns -EPERM whatever `inode_permission` said: the
    // caller is told the link is forbidden, not that the file is.
    assert_eq!(verdict(OTHER, 0o000, FileType::File), Err(LxError::EPERM));
    let mut c = creds(OTHER);
    c.fsuid = OWNER;
    assert_eq!(
        LinuxProcess::link_verdict(&c, OWNER, OWNER, 0o000, FileType::File),
        Ok(()),
        "and ownership is the filesystem uid"
    );
    // The read-and-write test is on the FILESYSTEM ids too
    // (`inode_permission` -> `current_fsuid()`/`current_fsgid()`): a
    // process whose real ids are strangers to the file but whose fs gid
    // is the file's group may link a group-rw file.
    let mut c = creds(3000);
    c.fsuid = OTHER;
    c.fsgid = OTHER;
    assert_eq!(
        LinuxProcess::link_verdict(&c, OWNER, OTHER, 0o660, FileType::File),
        Ok(())
    );
    c.fsgid = 3000;
    assert_eq!(
        LinuxProcess::link_verdict(&c, OWNER, OTHER, 0o660, FileType::File),
        Err(LxError::EPERM)
    );
}
