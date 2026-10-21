//! The four directory syscalls nobody ever called: `getdents64`, `linkat`,
//! `renameat` and `renameat2`, each one made the way a program makes it.
//!
//! `dir.rs` has nine test modules and every one of them calls a helper:
//! `check_rename_flags`, `at_flags`, `last_component`, `rename_slash_check`,
//! `is_same_or_below`, `DirentBufWriter`. The verdicts are all decided on,
//! and the bodies that reach them are not run at all -- these four were
//! 186 of the file's uncovered lines.
//!
//! What only a syscall can show here:
//!
//! 1. **That a second name is the same file.** `link` promises one inode
//!    under two names, which is a thing to look at (the inode number and
//!    the link count) and not a return value.
//! 2. **The order each one judges its arguments in.** `linkat` reads its
//!    flag word before either path, so a bad flag with two paths that do
//!    not exist is `EINVAL` and not `ENOENT`; `renameat2` resolves the
//!    source before it asks any permission, so a source that is not there
//!    is `ENOENT` and not `EACCES`.
//! 3. **That the directory trap closes.** `rename("a", "a/b/c")` has to be
//!    `EINVAL` before the filesystem is asked: a filesystem that links the
//!    entry before unlinking it (ramfs does) would leave `a` reachable only
//!    from inside itself, with every file under it lost.
//! 4. **What `getdents64` puts in the caller's buffer.** The entries are
//!    written into a kernel buffer and copied out whole, and the names in
//!    them are the only proof the walk ran.
//!
//! Fixed pids live in 48_11x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `AT_FDCWD`, as the number userspace passes.
const AT_FDCWD: usize = -100i32 as usize;
/// `O_DIRECTORY`, which is what an `opendir` adds.
const O_DIRECTORY: usize = 0o200000;
/// `AT_SYMLINK_FOLLOW` and `AT_EMPTY_PATH`, the two `linkat` takes, and a
/// bit it does not.
const AT_SYMLINK_FOLLOW: usize = 0x400;
const AT_EMPTY_PATH: usize = 0x1000;
const A_LINKAT_FLAG_OF_NOBODY: usize = 0x2000;
/// `AT_REMOVEDIR`, which is `unlinkat`'s.
const AT_REMOVEDIR: usize = 0x200;
/// `renameat2`'s three flags, and a bit nobody has.
const RENAME_NOREPLACE: usize = 1;
const RENAME_EXCHANGE: usize = 2;
const RENAME_WHITEOUT: usize = 4;
const A_RENAME_FLAG_OF_NOBODY: usize = 8;

/// A descriptor nobody opened.
const A_FD_OF_NOBODY: usize = 90;

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

/// A file of `dir` with these contents.
fn a_file_named(lp: &LinuxProcess, dir: &str, name: &str, bytes: &[u8]) {
    let dir = lp.lookup_inode(dir).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    inode.write_at(0, bytes).unwrap();
}

/// A directory of `dir`.
fn a_dir_named(lp: &LinuxProcess, dir: &str, name: &str) {
    let dir = lp.lookup_inode(dir).unwrap();
    dir.create(name, FileType::Dir, 0o755).unwrap();
}

/// The (inode number, link count) of a path, which is what `link` is about.
fn inode_and_links(lp: &LinuxProcess, path: &str) -> (usize, usize) {
    let m = lp.lookup_inode(path).unwrap().metadata().unwrap();
    (m.inode, m.nlinks)
}

/// Whether a path is there at all.
fn is_there(lp: &LinuxProcess, path: &str) -> bool {
    lp.lookup_inode(path).is_ok()
}

/// What a file holds, read through the filesystem and not through a
/// descriptor, because the point is the inode and not the description.
fn contents(lp: &LinuxProcess, path: &str, len: usize) -> Vec<u8> {
    let inode = lp.lookup_inode(path).unwrap();
    let mut out = alloc::vec![0u8; len];
    let n = inode.read_at(0, &mut out).unwrap();
    out.truncate(n);
    out
}

/// Whether the bytes of a `getdents64` answer carry this name.
fn names_it(answer: &[u8], name: &[u8]) -> bool {
    let mut wanted = Vec::from(name);
    wanted.push(0);
    answer.windows(wanted.len()).any(|w| w == &wanted[..])
}

/// The same `Syscall` with a mutable borrow, which the dispatch table needs.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A syscall made the way userspace makes it: by number, through the table.
async fn by_number(sys: &Syscall<'_>, which: crate::Sys, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

/// What an errno looks like on the way back out of the table.
fn errno(e: LxError) -> isize {
    -(e as isize)
}

/// An open descriptor, which is what `getdents64` and `AT_EMPTY_PATH` want.
fn opened(sys: &Syscall<'_>, path: &[u8], flags: usize) -> usize {
    sys.sys_openat(FileDesc::CWD, UserInPtr::from(at(path)), flags, 0)
        .expect("a path that is there opens")
}

#[test]
fn getdents64_wants_a_directory_and_copies_out_what_the_walk_found() {
    // The entries are built in a kernel buffer and copied out in one go, so
    // the names that reach the caller are the only proof that the walk ran
    // and that the copy kept them.
    let proc = a_process(48_110, "dir-dents");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "one", b"1");
            a_file_named(sys.linux_process(), "/x", "two", b"2");
            a_dir_named(sys.linux_process(), "/x", "sub");
            let dir = c("/x");
            let fd = opened(sys, &dir, O_DIRECTORY);

            let mut buf = [0u8; 4096];
            let n = sys
                .sys_getdents64(
                    FileDesc::from(fd),
                    UserOutPtr::from(buf.as_mut_ptr() as usize),
                    buf.len(),
                )
                .expect("a directory descriptor can be read");
            assert!(n > 0, "getdents64 found nothing in a directory of three");
            let answer = &buf[..n];
            for name in [&b"one"[..], b"two", b"sub", b".", b".."] {
                assert!(
                    names_it(answer, name),
                    "getdents64 left {:?} out of its answer",
                    core::str::from_utf8(name).unwrap()
                );
            }

            // A length no allocator could honour is capped at 256 KiB before
            // anything is allocated, so this is an ordinary answer and not
            // ENOMEM.
            let fd2 = opened(sys, &dir, O_DIRECTORY);
            let mut big = [0u8; 4096];
            assert_eq!(
                sys.sys_getdents64(
                    FileDesc::from(fd2),
                    UserOutPtr::from(big.as_mut_ptr() as usize),
                    usize::MAX,
                ),
                Ok(n),
                "a huge buffer length was taken at its word"
            );

            // And a buffer with no room for even the first entry is EINVAL,
            // not a short answer: `getdents64`'s `if (!lastdirent) error =
            // -EINVAL;`. Zero bytes is how the end of a directory is
            // reported, so answering it here would make `ls` stop on a
            // buffer it never grew instead of growing it.
            let fd3 = opened(sys, &dir, O_DIRECTORY);
            let mut tiny = [0u8; 64];
            assert_eq!(
                sys.sys_getdents64(
                    FileDesc::from(fd3),
                    UserOutPtr::from(tiny.as_mut_ptr() as usize),
                    8,
                ),
                Err(LxError::EINVAL),
                "a buffer too small for one entry answered as an empty directory"
            );
            assert_eq!(tiny[..8], [0u8; 8], "the refused call copied bytes out");

            // A descriptor that is not a directory, and one that is not a
            // descriptor.
            let file = c("/x/one");
            let plain = opened(sys, &file, 0);
            assert_eq!(
                sys.sys_getdents64(
                    FileDesc::from(plain),
                    UserOutPtr::from(buf.as_mut_ptr() as usize),
                    buf.len()
                ),
                Err(LxError::ENOTDIR)
            );
            assert_eq!(
                sys.sys_getdents64(
                    FileDesc::from(A_FD_OF_NOBODY),
                    UserOutPtr::from(buf.as_mut_ptr() as usize),
                    buf.len()
                ),
                Err(LxError::EBADF)
            );
        })
    });
}

#[test]
fn a_second_name_is_the_same_file_and_the_flag_word_is_read_first() {
    // `link` promises one inode under two names. The link count and the
    // inode number are what say so; the return value cannot.
    let proc = a_process(48_111, "dir-links");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "file", b"shared");
            let file = c("/x/file");
            let link = c("/x/link");
            let (inode, links) = inode_and_links(sys.linux_process(), "/x/file");
            assert_eq!(links, 1, "a fresh file starts with one name");

            assert_eq!(
                sys.sys_link(UserInPtr::from(at(&file)), UserInPtr::from(at(&link))),
                Ok(0)
            );
            assert_eq!(
                inode_and_links(sys.linux_process(), "/x/link"),
                (inode, 2),
                "the second name is a different file"
            );
            assert_eq!(
                contents(sys.linux_process(), "/x/link", 16),
                b"shared",
                "the second name reads something else"
            );

            // The name that is already there is refused before the parent's
            // mode is looked at.
            assert_eq!(
                sys.sys_link(UserInPtr::from(at(&file)), UserInPtr::from(at(&link))),
                Err(LxError::EEXIST)
            );

            // The flag word is read before either path: with a bit nobody
            // defines and two paths that do not exist, the answer is still
            // EINVAL.
            let nowhere = c("/x/nothing-here");
            let elsewhere = c("/x/nothing-there");
            assert_eq!(
                sys.sys_linkat(
                    FileDesc::CWD,
                    UserInPtr::from(at(&nowhere)),
                    FileDesc::CWD,
                    UserInPtr::from(at(&elsewhere)),
                    A_LINKAT_FLAG_OF_NOBODY,
                ),
                Err(LxError::EINVAL),
                "the paths were resolved before the flags were read"
            );
            // And with a flag it does take, the paths are reached.
            assert_eq!(
                sys.sys_linkat(
                    FileDesc::CWD,
                    UserInPtr::from(at(&nowhere)),
                    FileDesc::CWD,
                    UserInPtr::from(at(&elsewhere)),
                    AT_SYMLINK_FOLLOW,
                ),
                Err(LxError::ENOENT)
            );

            // A directory never gets a second name, whoever asks.
            let dir = c("/x");
            let dir_link = c("/x/dir-link");
            assert_eq!(
                sys.sys_link(UserInPtr::from(at(&dir)), UserInPtr::from(at(&dir_link))),
                Err(LxError::EPERM),
                "a directory was given a hard link"
            );
            // And a name that is TAKEN is EEXIST even for a directory, which
            // the refusal above cannot show: the new name is looked up before
            // this file's right to a second name is weighed. Without that
            // order the answer would be the EPERM of `check_link`, and it is
            // the only way to tell the lookup from the filesystem's own
            // `EntryExist`, which answers the same thing one call later.
            assert_eq!(
                sys.sys_link(UserInPtr::from(at(&dir)), UserInPtr::from(at(&link))),
                Err(LxError::EEXIST),
                "the file was weighed before the name was looked up"
            );

            // `AT_EMPTY_PATH` links the file the descriptor is open on, with
            // no path at all.
            let fd = opened(sys, &file, 0);
            let empty = c("");
            let third = c("/x/third");
            assert_eq!(
                sys.sys_linkat(
                    FileDesc::from(fd),
                    UserInPtr::from(at(&empty)),
                    FileDesc::CWD,
                    UserInPtr::from(at(&third)),
                    AT_EMPTY_PATH,
                ),
                Ok(0)
            );
            assert_eq!(inode_and_links(sys.linux_process(), "/x/third"), (inode, 3));
            // Without the flag, an empty path is the lookup's answer.
            let fourth = c("/x/fourth");
            assert_eq!(
                sys.sys_linkat(
                    FileDesc::from(fd),
                    UserInPtr::from(at(&empty)),
                    FileDesc::CWD,
                    UserInPtr::from(at(&fourth)),
                    0,
                ),
                Err(LxError::ENOENT)
            );
        })
    });
}

#[test]
fn renameat2_closes_the_directory_trap_before_it_asks_anything() {
    // `rename("a", "a/b/c")` reached the filesystem as an ordinary move.
    // ramfs links the new entry before unlinking the old one, so `a` came
    // out reachable only from inside itself: a cycle no path from `/` enters,
    // every file under it lost, and every inode in it kept alive by its own
    // reference. Linux answers EINVAL in `do_renameat2`, before any
    // permission is asked, and so has this to.
    let proc = a_process(48_112, "dir-trap");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_dir_named(sys.linux_process(), "/x", "a");
            a_dir_named(sys.linux_process(), "/x/a", "b");
            a_file_named(sys.linux_process(), "/x", "file", b"here");
            a_file_named(sys.linux_process(), "/x", "other", b"there");
            let a = c("/x/a");
            let under_a = c("/x/a/b/c");

            assert_eq!(
                sys.sys_rename(UserInPtr::from(at(&a)), UserInPtr::from(at(&under_a))),
                Err(LxError::EINVAL),
                "a directory was moved inside itself"
            );
            assert!(
                is_there(sys.linux_process(), "/x/a/b"),
                "the refusal happened after the move"
            );

            // A directory into ITSELF is the same answer.
            let a_again = c("/x/a/again");
            assert_eq!(
                sys.sys_rename(UserInPtr::from(at(&a)), UserInPtr::from(at(&a_again))),
                Err(LxError::EINVAL)
            );

            // And the trap closes BEFORE the filesystem is asked, which the
            // two shapes above cannot show: ramfs walks up from the
            // destination itself and answers EINVAL too, so dropping this
            // check changes no answer there. Name a target that is there and
            // has something in it and the two part company: the filesystem
            // gets as far as looking at the target and answers ENOTEMPTY,
            // while the trap refuses without asking it anything. A
            // filesystem with no walk of its own -- the point of having the
            // check here at all -- would have moved the directory.
            a_file_named(sys.linux_process(), "/x/a/b", "deep", b"kept");
            let into_b = c("/x/a/b");
            assert_eq!(
                sys.sys_rename(UserInPtr::from(at(&a)), UserInPtr::from(at(&into_b))),
                Err(LxError::EINVAL),
                "the filesystem was asked, and answered about the target"
            );
            assert!(
                is_there(sys.linux_process(), "/x/a/b/deep"),
                "something under the target was unlinked on the way"
            );

            // The source is resolved before either parent's mode is asked,
            // so a source that is not there is ENOENT.
            let nowhere = c("/x/nothing-here");
            let target = c("/x/target");
            assert_eq!(
                sys.sys_rename(UserInPtr::from(at(&nowhere)), UserInPtr::from(at(&target))),
                Err(LxError::ENOENT)
            );

            // The flag word is judged before the paths are resolved: every
            // one of these is EINVAL with a source that does not exist.
            let file = c("/x/file");
            let other = c("/x/other");
            for flags in [
                RENAME_EXCHANGE,
                RENAME_WHITEOUT,
                A_RENAME_FLAG_OF_NOBODY,
                RENAME_NOREPLACE | RENAME_EXCHANGE,
            ] {
                assert_eq!(
                    sys.sys_renameat2(
                        FileDesc::CWD,
                        UserInPtr::from(at(&nowhere)),
                        FileDesc::CWD,
                        UserInPtr::from(at(&target)),
                        flags,
                    ),
                    Err(LxError::EINVAL),
                    "flags {:#x} were read after the paths",
                    flags
                );
            }

            // `RENAME_NOREPLACE` is the one flag this kernel takes, and it
            // is the target already being there that it refuses.
            assert_eq!(
                sys.sys_renameat2(
                    FileDesc::CWD,
                    UserInPtr::from(at(&file)),
                    FileDesc::CWD,
                    UserInPtr::from(at(&other)),
                    RENAME_NOREPLACE,
                ),
                Err(LxError::EEXIST)
            );
            assert_eq!(
                sys.sys_renameat2(
                    FileDesc::CWD,
                    UserInPtr::from(at(&file)),
                    FileDesc::CWD,
                    UserInPtr::from(at(&target)),
                    RENAME_NOREPLACE,
                ),
                Ok(0),
                "a target that is not there is what NOREPLACE wants"
            );
        })
    });
}

#[test]
fn renameat_moves_the_name_and_the_file_stays_the_same_file() {
    // What a move is: the old name gone, the new name on the SAME inode,
    // with the same bytes behind it.
    let proc = a_process(48_113, "dir-rename");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "from", b"moved");
            a_dir_named(sys.linux_process(), "/x", "sub");
            let from = c("/x/from");
            let to = c("/x/to");
            let (inode, _) = inode_and_links(sys.linux_process(), "/x/from");

            assert_eq!(
                sys.sys_rename(UserInPtr::from(at(&from)), UserInPtr::from(at(&to))),
                Ok(0)
            );
            assert!(
                !is_there(sys.linux_process(), "/x/from"),
                "the old name outlived the move"
            );
            assert_eq!(inode_and_links(sys.linux_process(), "/x/to").0, inode);
            assert_eq!(contents(sys.linux_process(), "/x/to", 16), b"moved");

            // And across directories, which is the same call with another
            // parent.
            let deeper = c("/x/sub/back");
            assert_eq!(
                sys.sys_renameat(
                    FileDesc::CWD,
                    UserInPtr::from(at(&to)),
                    FileDesc::CWD,
                    UserInPtr::from(at(&deeper)),
                ),
                Ok(0)
            );
            assert!(!is_there(sys.linux_process(), "/x/to"));
            assert_eq!(inode_and_links(sys.linux_process(), "/x/sub/back").0, inode);
        })
    });
}

#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    // Seven numbers, each one in a shape that has to SUCCEED: by number, a
    // call that fails does not tell an argument swap from an ordinary
    // refusal, because both ends of a swap are usually refused too.
    let proc = a_process(48_114, "dir-by-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/x", "one", b"1");
            a_dir_named(sys.linux_process(), "/x", "sub");
            let dir = c("/x");
            let one = c("/x/one");
            let two = c("/x/two");
            let three = c("/x/three");
            let four = c("/x/four");
            let five = c("/x/five");
            let sub = c("/x/sub");
            let fd = opened(sys, &dir, O_DIRECTORY);

            // `getdents64(fd, buf, size)`: the answer goes in the second
            // word and its length is the third.
            let mut buf = [0u8; 4096];
            let n = by_number(
                sys,
                crate::Sys::GETDENTS64,
                [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
            )
            .await;
            assert!(n > 0, "getdents64 by number answered {}", n);
            assert!(names_it(&buf[..n as usize], b"one"));

            // `link(old, new)`, and the second name has to be the new one.
            assert_eq!(
                by_number(sys, crate::Sys::LINK, [at(&one), at(&two), 0, 0, 0, 0]).await,
                0
            );
            assert!(is_there(sys.linux_process(), "/x/two"));

            // `linkat(olddirfd, old, newdirfd, new, flags)`.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::LINKAT,
                    [AT_FDCWD, at(&one), AT_FDCWD, at(&three), 0, 0]
                )
                .await,
                0
            );
            assert!(is_there(sys.linux_process(), "/x/three"));

            // `rename(old, new)`: the old name has to be the one that goes.
            assert_eq!(
                by_number(sys, crate::Sys::RENAME, [at(&two), at(&four), 0, 0, 0, 0]).await,
                0
            );
            assert!(!is_there(sys.linux_process(), "/x/two"));
            assert!(is_there(sys.linux_process(), "/x/four"));

            // `renameat(olddirfd, old, newdirfd, new)`.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::RENAMEAT,
                    [AT_FDCWD, at(&four), AT_FDCWD, at(&five), 0, 0]
                )
                .await,
                0
            );
            assert!(is_there(sys.linux_process(), "/x/five"));

            // `renameat2`: the flags are the FIFTH word. With the target
            // already there, NOREPLACE is EEXIST -- and read from anywhere
            // else that bit would not be set and the move would happen.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::RENAMEAT2,
                    [
                        AT_FDCWD,
                        at(&five),
                        AT_FDCWD,
                        at(&three),
                        RENAME_NOREPLACE,
                        0
                    ]
                )
                .await,
                errno(LxError::EEXIST)
            );
            assert!(is_there(sys.linux_process(), "/x/five"), "it moved anyway");

            // `unlinkat(dirfd, path, flags)`: the flag word is the third,
            // and AT_REMOVEDIR is what lets a directory go.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::UNLINKAT,
                    [AT_FDCWD, at(&three), 0, 0, 0, 0]
                )
                .await,
                0
            );
            assert!(!is_there(sys.linux_process(), "/x/three"));
            assert_eq!(
                by_number(sys, crate::Sys::UNLINKAT, [AT_FDCWD, at(&sub), 0, 0, 0, 0]).await,
                errno(LxError::EISDIR),
                "a directory went without AT_REMOVEDIR"
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::UNLINKAT,
                    [AT_FDCWD, at(&sub), AT_REMOVEDIR, 0, 0, 0]
                )
                .await,
                0
            );
            assert!(!is_there(sys.linux_process(), "/x/sub"));
        })
    });
}
