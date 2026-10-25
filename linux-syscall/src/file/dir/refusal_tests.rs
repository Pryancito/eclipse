//! The refusals of the directory syscalls: the ones that answer an errno
//! from inside the body, where no helper decides them.
//!
//! `dir.rs` has ten test modules and every one of them calls a helper --
//! `mknod_file_type`, `create_perm`, `rename_slash_check`, `last_component`.
//! The verdicts those helpers reach are all decided on; what was never run
//! is the handful of lines in the bodies that refuse on their own account,
//! and the order they refuse in.
//!
//! What only a syscall can show here:
//!
//! 1. **That the device-node capability is asked at all.** `mknodat` is the
//!    one syscall in this file that makes a device node, and a character or
//!    block node is `CAP_MKNOD`'s to grant. Nothing asked for it until
//!    recently, so any process could make a node naming any major and minor
//!    it liked -- and the check has to come *before* the parent is looked
//!    up, or the answer to an unprivileged `mknod` turns into whatever the
//!    path happens to say.
//! 2. **The order a taken name is judged in.** `mknodat` and `symlinkat`
//!    both look the name up before they ask the parent's mode, as
//!    `filename_create` does: the answer to a name that exists in a
//!    directory the caller may not write is `EEXIST`, not `EACCES`.
//! 3. **That a trailing slash is not part of the name.** `readlink("l/")`
//!    resolves the link and reads a directory, so it is `EINVAL` or the
//!    lookup's own error, never the link's target; and `rename` refuses a
//!    trailing slash on anything that is not a directory.
//! 4. **That a rename will not turn a file into a directory.** The two
//!    sides have to agree on being directories, which `check_rename`
//!    decides and which no test had ever made fail.
//!
//! Fixed pids live in 48_80x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `O_DIRECTORY`, which is what an `opendir` adds.
const O_DIRECTORY: usize = 0o200000;

/// The three type bits of `mknod(2)` this module uses.
const S_IFIFO: usize = 0o010000;
const S_IFCHR: usize = 0o020000;
const S_IFBLK: usize = 0o060000;

/// `makedev(1, 3)`, which is `/dev/null`: the number an unprivileged
/// `mknod` would most like to be allowed to name.
const DEV_NULL: usize = (1 << 8) | 3;

/// An ordinary unprivileged user, as `setresuid` leaves one.
const A_PLAIN_USER: usize = 1000;

/// A process with a `/x` of its own, because a lookup from the root goes
/// through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create("x", FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A path as userspace writes one: NUL-terminated.
fn c(s: &str) -> Vec<u8> {
    let mut bytes = Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

/// Where userspace would say that lives.
fn at(bytes: &[u8]) -> usize {
    bytes.as_ptr() as usize
}

/// A file of `dir`.
fn a_file_named(lp: &LinuxProcess, dir: &str, name: &str) {
    let dir = lp.lookup_inode(dir).unwrap();
    dir.create(name, FileType::File, 0o644).unwrap();
}

/// A directory of `dir`, with the mode asked for: the tests that drop
/// privileges need one they may write and one they may not.
fn a_dir_named(lp: &LinuxProcess, dir: &str, name: &str, mode: u32) {
    let dir = lp.lookup_inode(dir).unwrap();
    dir.create(name, FileType::Dir, mode).unwrap();
}

/// Whether a path is there at all.
fn is_there(lp: &LinuxProcess, path: &str) -> bool {
    lp.lookup_inode(path).is_ok()
}

/// An open descriptor.
fn opened(sys: &Syscall<'_>, path: &[u8], flags: usize) -> SysResult {
    sys.sys_openat(FileDesc::CWD, UserInPtr::from(at(path)), flags, 0)
}

/// `mknodat` from the current directory, the way `mknod(2)` reaches it.
fn mknod(sys: &Syscall<'_>, path: &[u8], mode: usize, dev: usize) -> SysResult {
    sys.sys_mknodat(FileDesc::CWD, UserInPtr::from(at(path)), mode, dev)
}

/// `symlinkat` from the current directory.
fn symlink(sys: &Syscall<'_>, target: &[u8], linkpath: &[u8]) -> SysResult {
    sys.sys_symlinkat(
        UserInPtr::from(at(target)),
        FileDesc::CWD,
        UserInPtr::from(at(linkpath)),
    )
}

/// `readlinkat` from the current directory, into a buffer of its own, and
/// what it wrote there.
fn readlink(sys: &Syscall<'_>, path: &[u8]) -> (SysResult, Vec<u8>) {
    let mut buf = [0u8; 64];
    let ret = sys.sys_readlinkat(
        FileDesc::CWD,
        UserInPtr::from(at(path)),
        UserOutPtr::from(buf.as_mut_ptr() as usize),
        buf.len(),
    );
    let n = match ret {
        Ok(n) => n,
        Err(_) => 0,
    };
    (ret, Vec::from(&buf[..n]))
}

/// `renameat2` from the current directory on both sides, with no flags.
fn rename(sys: &Syscall<'_>, old: &[u8], new: &[u8]) -> SysResult {
    sys.sys_renameat2(
        FileDesc::CWD,
        UserInPtr::from(at(old)),
        FileDesc::CWD,
        UserInPtr::from(at(new)),
        0,
    )
}

#[test]
fn chdir_and_fchdir_both_want_a_directory() {
    // Two bodies, one rule, and the same `ENOTDIR` reached twice: `chdir`
    // looks the path up and `fchdir` reads a descriptor, and neither of
    // them had ever been handed something that is not a directory.
    let proc = a_process(48_800, "dir-chdir");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "f");
            let dir = c("/x");
            let file = c("/x/f");

            assert_eq!(
                sys.sys_chdir(UserInPtr::from(at(&file))),
                Err(LxError::ENOTDIR),
                "chdir into a regular file"
            );
            assert_eq!(
                sys.sys_chdir(UserInPtr::from(at(&dir))),
                Ok(0),
                "and a directory still works"
            );

            let file_fd = opened(sys, &file, 0).expect("a file opens");
            assert_eq!(
                sys.sys_fchdir(FileDesc::from(file_fd)),
                Err(LxError::ENOTDIR),
                "fchdir on a regular file's descriptor"
            );
            let dir_fd = opened(sys, &dir, O_DIRECTORY).expect("a directory opens");
            assert_eq!(
                sys.sys_fchdir(FileDesc::from(dir_fd)),
                Ok(0),
                "and a directory's descriptor still works"
            );
        })
    });
}

#[test]
fn a_device_node_needs_cap_mknod_and_the_check_comes_first() {
    // `may_mknod` is asked before the parent is looked up, which is what
    // makes the answer to an unprivileged device node `EPERM` whatever the
    // path says. Asking it afterwards would hand out `ENOENT` for a path
    // that is not there and `EACCES` for a directory the caller may not
    // write, and a program could not tell "you may not" from "it is not
    // there".
    let proc = a_process(48_801, "dir-mknod-cap");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_dir_named(sys.linux_process(), "/x", "w", 0o777);
            let as_root = c("/x/w/root-node");
            let plain = c("/x/w/plain-node");
            let nowhere = c("/x/nothing/here");
            let fifo = c("/x/w/plain-fifo");

            assert_eq!(
                mknod(sys, &as_root, S_IFCHR | 0o666, DEV_NULL),
                Ok(0),
                "root makes a character node"
            );

            assert_eq!(
                sys.sys_setresuid(A_PLAIN_USER, A_PLAIN_USER, A_PLAIN_USER),
                Ok(0),
                "dropping to a plain user"
            );

            assert_eq!(
                mknod(sys, &plain, S_IFCHR | 0o666, DEV_NULL),
                Err(LxError::EPERM),
                "a character node without CAP_MKNOD"
            );
            assert_eq!(
                mknod(sys, &plain, S_IFBLK | 0o666, DEV_NULL),
                Err(LxError::EPERM),
                "a block node without CAP_MKNOD"
            );
            assert_eq!(
                mknod(sys, &nowhere, S_IFCHR | 0o666, DEV_NULL),
                Err(LxError::EPERM),
                "and the capability is asked before the parent is looked up"
            );
            assert!(
                !is_there(sys.linux_process(), "/x/w/plain-node"),
                "the node a plain user was refused must not be there"
            );

            // A fifo is nobody's capability: the same call, the same
            // directory, the same user, and it goes through.
            assert_eq!(
                mknod(sys, &fifo, S_IFIFO | 0o666, 0),
                Ok(0),
                "a fifo needs no capability"
            );
        })
    });
}

#[test]
fn a_taken_name_is_eexist_before_the_parents_mode_is_asked() {
    // `filename_create`'s order, and the one that cost a desktop its sound
    // daemon when it was the other way round. The proof is a directory the
    // caller may NOT write: a name that is taken there answers `EEXIST`,
    // and only a free name reaches the parent's mode and answers `EACCES`.
    let proc = a_process(48_802, "dir-eexist");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // `/x` is 0o755 and owned by root, so a plain user may not
            // write it.
            a_file_named(sys.linux_process(), "/x", "taken");
            let taken = c("/x/taken");
            let free = c("/x/free");
            let link_target = c("/somewhere");

            assert_eq!(
                sys.sys_setresuid(A_PLAIN_USER, A_PLAIN_USER, A_PLAIN_USER),
                Ok(0),
                "dropping to a plain user"
            );

            assert_eq!(
                mknod(sys, &taken, S_IFIFO | 0o666, 0),
                Err(LxError::EEXIST),
                "mknodat answers the lookup, not the parent's mode"
            );
            assert_eq!(
                mknod(sys, &free, S_IFIFO | 0o666, 0),
                Err(LxError::EACCES),
                "and a free name is where the parent's mode is asked"
            );

            assert_eq!(
                symlink(sys, &link_target, &taken),
                Err(LxError::EEXIST),
                "symlinkat answers the lookup too"
            );
            assert_eq!(
                symlink(sys, &link_target, &free),
                Err(LxError::EACCES),
                "and the same free name is still the parent's mode"
            );
        })
    });
}

#[test]
fn symlinkat_wants_a_target_and_asks_for_it_first() {
    // `do_symlinkat` reads the target with `getname`, which is `ENOENT` for
    // an empty string, before the link's own name is looked at. A name that
    // is already taken is the discriminator: with the empty target judged
    // first the answer is `ENOENT`, and with it judged second it would be
    // `EEXIST`.
    let proc = a_process(48_803, "dir-symlink");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "taken");
            let empty = c("");
            let taken = c("/x/taken");
            let fresh = c("/x/fresh");
            let target = c("/x/taken");

            assert_eq!(
                symlink(sys, &empty, &taken),
                Err(LxError::ENOENT),
                "an empty target, on a name that exists"
            );
            assert_eq!(
                symlink(sys, &empty, &fresh),
                Err(LxError::ENOENT),
                "an empty target, on a name that does not"
            );
            assert!(
                !is_there(sys.linux_process(), "/x/fresh"),
                "a refused symlink must leave no name behind"
            );
            assert_eq!(
                symlink(sys, &target, &fresh),
                Ok(0),
                "and a target that is not empty makes the link"
            );
        })
    });
}

#[test]
fn readlink_of_a_trailing_slash_resolves_the_link_instead_of_reading_it() {
    // A trailing slash means "this must be a directory", so the link is
    // followed and what comes back is the lookup's verdict or `EINVAL` --
    // never the link's target. Splitting the name off and reading the link
    // `l` for `readlink("l/")` is exactly the bug this closes.
    let proc = a_process(48_804, "dir-readlink-slash");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "f");
            a_dir_named(sys.linux_process(), "/x", "d", 0o755);
            let to_file = c("/x/to-file");
            let to_dir = c("/x/to-dir");
            assert_eq!(symlink(sys, &c("/x/f"), &to_file), Ok(0));
            assert_eq!(symlink(sys, &c("/x/d"), &to_dir), Ok(0));

            let (ret, wrote) = readlink(sys, &to_file);
            assert_eq!(ret, Ok(4), "without a slash the target comes back");
            assert_eq!(&wrote[..], b"/x/f", "and it is the target");

            let with_slash = c("/x/to-file/");
            assert_eq!(
                readlink(sys, &with_slash).0,
                Err(LxError::ENOTDIR),
                "a link to a file, with a slash, is the lookup's ENOTDIR"
            );

            let dir_with_slash = c("/x/to-dir/");
            assert_eq!(
                readlink(sys, &dir_with_slash).0,
                Err(LxError::EINVAL),
                "a link to a directory resolves, and a directory is no link"
            );
        })
    });
}

#[test]
fn readlink_of_something_that_is_not_a_link_is_einval() {
    // The last component is looked up without following it, so what comes
    // back is the thing named and not what it points at; everything that is
    // not a symlink is `EINVAL`, which is what `readlink(2)` promises.
    let proc = a_process(48_805, "dir-readlink-type");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "f");
            let file = c("/x/f");
            let dir = c("/x");

            assert_eq!(
                readlink(sys, &file).0,
                Err(LxError::EINVAL),
                "a regular file is not a link"
            );
            assert_eq!(
                readlink(sys, &dir).0,
                Err(LxError::EINVAL),
                "a directory is not a link either"
            );
        })
    });
}

#[test]
fn rename_refuses_a_trailing_slash_on_anything_but_a_directory() {
    // `rename("f", "g/")` is `ENOTDIR`: the slash says the name is a
    // directory and the source is not one. The source's type is read first,
    // so the verdict does not depend on whether `g` is there.
    let proc = a_process(48_806, "dir-rename-slash");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "f");
            a_dir_named(sys.linux_process(), "/x", "d", 0o755);

            assert_eq!(
                rename(sys, &c("/x/f"), &c("/x/g/")),
                Err(LxError::ENOTDIR),
                "a file moved onto a name with a slash"
            );
            assert!(
                is_there(sys.linux_process(), "/x/f"),
                "and the file stays where it was"
            );
            assert_eq!(
                rename(sys, &c("/x/d"), &c("/x/e/")),
                Ok(0),
                "a directory may be named with a slash"
            );
            assert!(is_there(sys.linux_process(), "/x/e"), "it moved");
        })
    });
}

#[test]
fn rename_will_not_turn_a_file_into_a_directory_either_way_round() {
    // `check_rename`'s own refusals, which nothing had ever made fail: the
    // two sides must agree on being directories. `rename(2)` names both
    // errnos -- `EISDIR` for a file onto a directory and `ENOTDIR` for a
    // directory onto a file -- and getting them the wrong way round is the
    // kind of thing a shell's `mv` reports verbatim.
    let proc = a_process(48_807, "dir-rename-type");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "f");
            a_dir_named(sys.linux_process(), "/x", "d", 0o755);

            assert_eq!(
                rename(sys, &c("/x/f"), &c("/x/d")),
                Err(LxError::EISDIR),
                "a file onto an existing directory"
            );
            assert_eq!(
                rename(sys, &c("/x/d"), &c("/x/f")),
                Err(LxError::ENOTDIR),
                "a directory onto an existing file"
            );
            assert!(
                is_there(sys.linux_process(), "/x/f") && is_there(sys.linux_process(), "/x/d"),
                "both sides stay as they were"
            );
        })
    });
}

#[test]
fn unlinkat_rewrites_the_one_path_it_is_asked_to() {
    // `/dev/shm/testshm` is answered from `/testshm`, which is the only
    // path in this file the kernel rewrites. The rewrite happens before the
    // flags are read and before anything is looked up, so the proof is that
    // the file at the rewritten path is the one that goes.
    let proc = a_process(48_808, "dir-unlink-shm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/", "testshm");
            assert!(is_there(sys.linux_process(), "/testshm"));

            assert_eq!(
                sys.sys_unlinkat(
                    FileDesc::CWD,
                    UserInPtr::from(at(&c("/dev/shm/testshm"))),
                    0
                ),
                Ok(0),
                "the shm path answers, with no /dev/shm anywhere"
            );
            assert!(
                !is_there(sys.linux_process(), "/testshm"),
                "and what went is the file at the rewritten path"
            );
        })
    });
}

#[test]
fn an_error_from_the_directory_that_is_not_the_end_comes_through() {
    // `next_entry` turns `ENOENT` into "end of directory" and has to pass
    // everything else out: swallowing it would end the listing early and
    // report success, which is how a short `ls` happens. `openat` will not
    // hand out a write-only directory (`EISDIR`, as Linux refuses it too),
    // so the description is built here: a readable one lists the directory
    // and the same inode without the read bit answers `EBADF` from the
    // first entry.
    let proc = a_process(48_809, "dir-dents-error");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "one");
            let inode = sys.linux_process().lookup_inode("/x").unwrap();

            let mut readable = File::new(inode.clone(), OpenFlags::RDONLY, String::from("/x"));
            let mut names = Vec::new();
            let pushed = collect_dirents(&mut readable, |_, _, name| {
                names.push(String::from(name));
                true
            })
            .expect("a readable directory lists");
            assert!(pushed >= 3, "., .. and one, and got {:?}", names);

            let mut write_only = File::new(inode, OpenFlags::WRONLY, String::from("/x"));
            assert_eq!(
                collect_dirents(&mut write_only, |_, _, _| true),
                Err(LxError::EBADF),
                "the error from the first entry is the answer, not the end"
            );
        })
    });
}

#[test]
fn chroot_gates_and_updates_root() {
    let proc = a_process(48_810, "dir-chroot");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            lp.root_inode()
                .create("jail", FileType::Dir, 0o755)
                .unwrap();
            let jail_inode = lp.lookup_inode("/jail").unwrap();
            jail_inode
                .create("inner_file", FileType::File, 0o644)
                .unwrap();

            let at = |s: &[u8]| UserInPtr::<u8>::from(s.as_ptr() as usize);

            // 1. Non-existent path returns ENOENT
            assert_eq!(sys.sys_chroot(at(b"/missing\0")), Err(LxError::ENOENT));

            // 2. Regular file returns ENOTDIR
            assert_eq!(
                sys.sys_chroot(at(b"/jail/inner_file\0")),
                Err(LxError::ENOTDIR)
            );

            // 3. Root can chroot into /jail
            assert_eq!(sys.sys_chroot(at(b"/jail\0")), Ok(0));

            // Inside the chroot, / is now /jail
            assert!(lp.lookup_inode("/inner_file").is_ok());
            // And outside path /jail no longer exists from inside
            assert_eq!(lp.lookup_inode("/jail").err(), Some(LxError::ENOENT));

            // 4. Once unprivileged, calling chroot returns EPERM
            lp.set_resuid(1000, 1000, 1000).unwrap();
            assert_eq!(sys.sys_chroot(at(b"/\0")), Err(LxError::EPERM));
        })
    });
}
