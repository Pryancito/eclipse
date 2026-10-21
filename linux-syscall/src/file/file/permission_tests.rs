//! What `access`, `fchmod` and `fchown` do once the verdict is in, which is
//! where a gate goes missing.
//!
//! The three gates themselves -- `access_verdict`, `chmod_bits`,
//! `chown_metadata` -- are pure and have tests of their own in
//! `linux-object`. What has none is the **wiring**: whether this crate asks
//! the gate the question userspace asked, whether its refusal arrives before
//! anything is changed, and whether what it decided reaches the inode. A
//! unit test of a verdict cannot see any of that, and that is exactly the
//! shape the missing `mount` gate had: the answer was right and it was asked
//! too late.
//!
//! Six of these syscalls -- `access`, `faccessat`, `fchmod`, `fchmodat`,
//! `fchown`, `fchownat` -- are not named by any test in the crate.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `R_OK`, from `/usr/include/unistd.h`. The numbers are written here and
/// not read back out of `access_mode`, which is the code this asks about.
const R_OK: usize = 4;

/// `AT_EACCESS`: "answer for the EFFECTIVE ids, not the real ones".
/// `/usr/include/fcntl.h` gives it 0x200.
const AT_EACCESS: usize = 0x200;

/// `AT_EMPTY_PATH`, 0x1000: "the path is empty and names the file `dirfd` is
/// already open on".
const AT_EMPTY_PATH: usize = 0x1000;

/// `(uid, gid)` of -1 in a `chown`, which means "leave this one as it is".
/// It arrives as an unsigned word, so it is the largest one there is.
const LEAVE_IT: usize = u32::MAX as usize;

/// The set-user-ID and set-group-ID bits, as `chmod(1)` writes them.
const SETUID: u16 = 0o4000;
const SETGID: u16 = 0o2000;

/// Somebody who is not root.
const A_USER: u32 = 1000;
/// Somebody who is neither root nor `A_USER`.
const ANOTHER_USER: u32 = 2000;

/// A NUL-terminated path, which is what `as_c_str` reads.
fn c(s: &str) -> alloc::vec::Vec<u8> {
    let mut bytes = alloc::vec::Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A file of `dir_path` with exactly this owner and mode, and the descriptor
/// it got, opened read-write.
///
/// Owner and mode are written through `set_metadata` rather than left to
/// whatever `create` puts there, because every answer below is read off
/// them.
fn a_file_owned_by(lp: &LinuxProcess, dir_path: &str, uid: u32, gid: u32, mode: u16) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create("f", FileType::File, 0o644).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = gid as _;
    metadata.mode = mode;
    inode.set_metadata(&metadata).unwrap();
    let path = alloc::format!("{}/f", dir_path);
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    fd as usize
}

/// The owner and mode the file really has, asked of the filesystem and not
/// of the syscalls under test.
fn owner_and_mode(lp: &LinuxProcess, dir_path: &str) -> (u32, u32, u16) {
    let metadata = lp
        .lookup_inode(&alloc::format!("{}/f", dir_path))
        .unwrap()
        .metadata()
        .unwrap();
    (metadata.uid as u32, metadata.gid as u32, metadata.mode)
}

#[test]
fn access_asks_about_the_real_user_and_at_eaccess_about_the_effective_one() {
    // The whole reason `AT_EACCESS` exists, and the reason `access(2)` is the
    // one permission call that does NOT use the credentials everything else
    // uses. A set-user-ID-root program asks `access()` "could the person who
    // ran me do this?", so answering it for the effective ids is the classic
    // set-user-ID bypass: the program would be told yes about a file its
    // caller cannot touch, and would then go and open it on their behalf.
    //
    // Only the final check changes. The path walk uses the filesystem ids
    // either way -- a quirk of `access()` that Linux keeps -- so the two
    // answers below differ by the gate alone and not by the lookup.
    let proc = a_process(45_590, "perm-eaccess", "perm_eaccess");
    let dir_path = String::from("/perm_eaccess");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            // Root's file, readable by nobody else.
            a_file_owned_by(&lp, &dir_path, 0, 0, 0o600);
            let path = c(&alloc::format!("{}/f", dir_path));
            let at = || UserInPtr::from(path.as_ptr() as usize);
            // Real 1000, effective 0: a set-user-ID-root program.
            lp.set_resuid(A_USER, 0, 0).unwrap();

            assert_eq!(
                sys.sys_access(at(), R_OK),
                Err(LxError::EACCES),
                "access answered for the effective user, which is the bypass"
            );
            assert_eq!(
                sys.sys_faccessat(FileDesc::CWD, at(), R_OK, AT_EACCESS),
                Ok(0),
                "AT_EACCESS did not reach the effective user"
            );
            // And without the flag `faccessat` is `access`, which is how
            // `access` is implemented and what keeps the two in step.
            assert_eq!(
                sys.sys_faccessat(FileDesc::CWD, at(), R_OK, 0),
                Err(LxError::EACCES),
                "faccessat without AT_EACCESS did not answer for the real user"
            );
        })
    });
}

#[test]
fn chown_clears_the_setuid_and_setgid_bits_and_the_clearing_reaches_the_inode() {
    // Giving away a set-user-ID-root file without stripping the bit hands
    // the recipient root: they own an executable that runs as root and they
    // can rewrite its contents. Linux clears both bits on every successful
    // ownership change, and the clearing has to survive the trip back to
    // the inode -- a `chown` that worked out the right mode and then wrote
    // the old one back is the same hole.
    let proc = a_process(45_591, "perm-setuid", "perm_setuid");
    let dir_path = String::from("/perm_setuid");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_owned_by(&lp, &dir_path, 0, 0, SETUID | SETGID | 0o755);
            assert_eq!(
                sys.sys_fchown(FileDesc::from(fd), A_USER as usize, A_USER as usize),
                Ok(0),
                "root could not give the file away"
            );

            let (uid, gid, mode) = owner_and_mode(&lp, &dir_path);
            assert_eq!((uid, gid), (A_USER, A_USER), "the file was not given away");
            assert_eq!(
                mode & (SETUID | SETGID),
                0,
                "the file was given away WITH its set-user-ID bit, which is root for the taking"
            );
            assert_eq!(mode, 0o755, "the permission bits were changed too");
        })
    });
}

#[test]
fn a_refused_chown_changes_nothing_at_all() {
    // The ordering property, which is what the `mount` gate got wrong: the
    // refusal has to come before any of the work. Here there are two pieces
    // of work to be too late for -- the new owner and the stripping of the
    // set-user-ID bit -- and a gate asked after either one leaves a file
    // that was not given away but did lose the bit, or worse.
    let proc = a_process(45_592, "perm-refused", "perm_refused");
    let dir_path = String::from("/perm_refused");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_file_owned_by(&lp, &dir_path, A_USER, A_USER, SETUID | 0o644);
            // Our own file, but we are not root, so we may not pass it on.
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            assert_eq!(
                sys.sys_fchown(FileDesc::from(fd), ANOTHER_USER as usize, LEAVE_IT),
                Err(LxError::EPERM),
                "an unprivileged caller gave a file away"
            );
            assert_eq!(
                owner_and_mode(&lp, &dir_path),
                (A_USER, A_USER, SETUID | 0o644),
                "a refused chown changed something anyway"
            );
        })
    });
}

#[test]
fn a_uid_or_gid_of_minus_one_leaves_that_one_alone() {
    // `chown(path, -1, gid)` is how `chgrp` is written, and it is the only
    // way to change one of the two without knowing the other. A kernel that
    // took the -1 as an id would set the owner to 4294967295, which is
    // nobody, and the file would belong to no account at all.
    let proc = a_process(45_593, "perm-leave-it", "perm_leave_it");
    let dir_path = String::from("/perm_leave_it");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_owned_by(&lp, &dir_path, 0, 0, 0o644));

            assert_eq!(
                sys.sys_fchown(fd, A_USER as usize, LEAVE_IT),
                Ok(0),
                "changing only the owner failed"
            );
            let (uid, gid, _) = owner_and_mode(&lp, &dir_path);
            assert_eq!(uid, A_USER, "the owner did not change");
            assert_eq!(gid, 0, "the group changed when it was told not to");

            assert_eq!(
                sys.sys_fchown(fd, LEAVE_IT, ANOTHER_USER as usize),
                Ok(0),
                "changing only the group failed"
            );
            let (uid, gid, _) = owner_and_mode(&lp, &dir_path);
            assert_eq!(uid, A_USER, "the owner changed when it was told not to");
            assert_eq!(gid, ANOTHER_USER, "the group did not change");
        })
    });
}

#[test]
fn fchmod_of_someone_elses_file_is_eperm_and_leaves_the_mode_alone() {
    // `inode_owner_or_capable`: the mode of a file is the owner's and
    // root's to set, and nobody else's -- having write access to it is not
    // enough, because the mode is what grants the access in the first place.
    // A file left world-writable by mistake must not be one `chmod 777`
    // away from being world-anything-else.
    let proc = a_process(45_594, "perm-chmod", "perm_chmod");
    let dir_path = String::from("/perm_chmod");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            // Root's file, and writable by everyone.
            let fd = a_file_owned_by(&lp, &dir_path, 0, 0, 0o666);
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            assert_eq!(
                sys.sys_fchmod(fd.into(), 0o777),
                Err(LxError::EPERM),
                "someone who is not the owner set the mode"
            );
            assert_eq!(
                owner_and_mode(&lp, &dir_path),
                (0, 0, 0o666),
                "a refused chmod changed the mode anyway"
            );
        })
    });
}

#[test]
fn at_empty_path_names_the_file_the_descriptor_is_open_on_and_without_it_it_is_enoent() {
    // What `AT_EMPTY_PATH` is for: `fchmodat(fd, "", mode, AT_EMPTY_PATH)`
    // reaches a file through a descriptor alone, which is how a program
    // changes a file it opened but cannot name -- one in a directory it has
    // since lost, or named by a path that is no longer there. Without the
    // flag the empty path is a path, and an empty path names nothing.
    //
    // Asked of `fchmodat` and `fchownat` separately, because each carries
    // its own copy of the three lines that decide it.
    let proc = a_process(45_595, "perm-empty", "perm_empty");
    let dir_path = String::from("/perm_empty");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_owned_by(&lp, &dir_path, 0, 0, 0o644));
            let empty = c("");
            let at = || UserInPtr::from(empty.as_ptr() as usize);

            assert_eq!(
                sys.sys_fchmodat(fd, at(), 0o600, 0),
                Err(LxError::ENOENT),
                "an empty path without the flag named something"
            );
            assert_eq!(
                sys.sys_fchmodat(fd, at(), 0o600, AT_EMPTY_PATH),
                Ok(0),
                "AT_EMPTY_PATH did not reach the file the descriptor is open on"
            );
            assert_eq!(
                owner_and_mode(&lp, &dir_path).2,
                0o600,
                "the mode did not change"
            );

            assert_eq!(
                sys.sys_fchownat(fd, at(), A_USER as usize, LEAVE_IT, 0),
                Err(LxError::ENOENT),
                "an empty path without the flag named something"
            );
            assert_eq!(
                sys.sys_fchownat(fd, at(), A_USER as usize, LEAVE_IT, AT_EMPTY_PATH),
                Ok(0),
                "AT_EMPTY_PATH did not reach the file the descriptor is open on"
            );
            assert_eq!(
                owner_and_mode(&lp, &dir_path).0,
                A_USER,
                "the owner did not change"
            );
        })
    });
}
