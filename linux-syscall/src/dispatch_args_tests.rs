//! What each arm of the syscall table hands over, and in what order.
//!
//! [`crate::dispatch_tests`] asks whether a number reaches the handler that
//! number names. This asks the next question: whether the six words arrive
//! where that handler expects them. Between the number and the handler sits
//! a one-line arm typed out by hand -- `a0.into(), a1 as i64, a2`,
//! `a3 as u32`, a flag word registered for one number and ignored for its
//! neighbour -- and 143 of those arms had never been run at all. A wrong
//! index or a wrong cast in one of them is invisible to every test that
//! calls the handler by name, because by name the compiler puts the
//! arguments in place.
//!
//! So each test here makes the call by number and asks for something that
//! can only be right if the words landed where the arm says:
//!
//! 1. **A positioned read or write proves its own length and offset.**
//!    `pread64(fd, buf, 3, 4)` and `pread64(fd, buf, 4, 3)` both succeed and
//!    both return a count; what tells them apart is the bytes that come out.
//! 2. **`symlinkat` takes its target FIRST and the directory second**, which
//!    is the opposite shape to every other `*at` call in the table.
//! 3. **A user and a group are two different numbers.** `fchownat(path, 7,
//!    11, 0)` landing as `fchownat(path, 11, 7, 0)` is a file handed to the
//!    wrong owner, and both calls answer `0`.
//! 4. **A flag word belongs to one number and not to its neighbour.**
//!    `fchmodat` has no flags and `faccessat` has none either, while
//!    `faccessat2` does, and all three share an arm shape.
//! 5. **The struct a `stat` fills has to be the same one the handler fills
//!    when it is called by name**, which is the reference these tests
//!    compare against rather than reading a private field.
//!
//! Fixed pids live in 48_12x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `AT_FDCWD`, as the number userspace passes.
const AT_FDCWD: usize = -100i32 as usize;
/// `O_RDWR`, so one descriptor can both read and write.
const O_RDWR: usize = 2;
/// `lseek`'s two whences used here.
const SEEK_SET: usize = 0;
const SEEK_CUR: usize = 1;
/// `AT_REMOVEDIR`, `unlinkat`'s only flag.
const AT_REMOVEDIR: usize = 0x200;
/// `AT_SYMLINK_NOFOLLOW`, which `fstatat` and `fchownat` take.
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
/// `R_OK`, and a bit no `faccessat2` flag word has.
const R_OK: usize = 4;
const A_FACCESSAT_FLAG_OF_NOBODY: usize = 0x2000;
/// `S_IFCHR` and the major:minor of `/dev/null`, which is what `mknod`
/// keeps in `rdev` and `mkdir` has no word for.
const S_IFCHR: usize = 0o020000;
const NULL_DEVICE: usize = 0x0103;

/// A process at `pid` whose root holds a `/<top>` of its own, because a
/// lookup from the root goes through a path cache every process in this
/// binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
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

/// An `iovec` as userspace lays one out: a pointer and a length, which is
/// what `IoVec` is with `repr(C)`.
fn an_iovec(base: usize, len: usize) -> [usize; 2] {
    [base, len]
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

/// What a file holds, read through the filesystem rather than through a
/// description, because the point is the inode.
fn contents(lp: &LinuxProcess, path: &str, len: usize) -> Vec<u8> {
    let inode = lp.lookup_inode(path).unwrap();
    let mut out = alloc::vec![0u8; len];
    let n = inode.read_at(0, &mut out).unwrap();
    out.truncate(n);
    out
}

/// The owner, the group, the permission bits and the device of a path: the
/// four things these arms carry in words that look alike.
fn who_owns(lp: &LinuxProcess, path: &str) -> (u32, u32, u16, u64) {
    let m = lp.lookup_inode(path).unwrap().metadata().unwrap();
    (m.uid as u32, m.gid as u32, m.mode & 0o7777, m.rdev as u64)
}

/// Whether a path is there at all.
fn is_there(lp: &LinuxProcess, path: &str) -> bool {
    lp.lookup_inode(path).is_ok()
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
async fn by_number(sys: &Syscall<'_>, which: Sys, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

/// What an errno looks like on the way back out of the table.
fn errno(e: LxError) -> isize {
    -(e as isize)
}

/// An open descriptor on a path that is there.
fn opened(sys: &Syscall<'_>, path: &[u8], flags: usize) -> usize {
    sys.sys_openat(FileDesc::CWD, UserInPtr::from(at(path)), flags, 0)
        .expect("a path that is there opens")
}

#[test]
fn the_length_and_the_offset_of_a_positioned_read_do_not_change_places() {
    // Seven arms whose words are all plain integers: `lseek(fd, offset,
    // whence)`, `pread64`/`pwrite64(fd, buf, len, offset)`, `writev(fd, iov,
    // count)`, `preadv`/`pwritev(fd, iov, count, offset)` and
    // `ftruncate(fd, len)`. Every one of them answers a byte count, so the
    // return value says nothing about where the words went; the bytes in the
    // file and in the buffer do.
    let proc = a_process(48_120, "args-io", "args_io");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/args_io", "f", b"0123456789");
            let path = c("/args_io/f");
            let fd = opened(sys, &path, O_RDWR);

            // The offset is the second word and the whence the third: with
            // the two swapped this would be `SEEK_HOLE` from zero.
            assert_eq!(
                by_number(sys, Sys::LSEEK, [fd, 4, SEEK_SET, 0, 0, 0]).await,
                4,
                "lseek did not take the fourth byte as its offset"
            );

            let mut buf = [0u8; 8];
            assert_eq!(
                by_number(
                    sys,
                    Sys::PREAD64,
                    [fd, buf.as_mut_ptr() as usize, 3, 4, 0, 0]
                )
                .await,
                3
            );
            assert_eq!(
                &buf[..3],
                b"456",
                "pread64 read the wrong three bytes, so its length and its \
                 offset are the wrong way round"
            );
            // And a positioned read leaves the description's own position
            // where it was, which is the whole difference from `read`.
            assert_eq!(
                by_number(sys, Sys::LSEEK, [fd, 0, SEEK_CUR, 0, 0, 0]).await,
                4,
                "pread64 moved the file position"
            );

            let ab = b"AB";
            assert_eq!(
                by_number(sys, Sys::PWRITE64, [fd, ab.as_ptr() as usize, 2, 1, 0, 0]).await,
                2
            );
            assert_eq!(
                contents(sys.linux_process(), "/args_io/f", 16),
                b"0AB3456789",
                "pwrite64 wrote two bytes somewhere else"
            );

            // `writev` writes at the position, which `pwrite64` left at 4,
            // and it writes the vectors in the order they are given.
            let x = b"XY";
            let z = b"Z";
            let iov = [
                an_iovec(x.as_ptr() as usize, 2),
                an_iovec(z.as_ptr() as usize, 1),
            ];
            assert_eq!(
                by_number(sys, Sys::WRITEV, [fd, iov.as_ptr() as usize, 2, 0, 0, 0]).await,
                3
            );
            assert_eq!(
                contents(sys.linux_process(), "/args_io/f", 16),
                b"0AB3XYZ789",
                "writev did not write both vectors at the file position"
            );

            // `preadv`'s offset is its FOURTH word, after the vector count.
            let mut first = [0u8; 2];
            let mut second = [0u8; 1];
            let out = [
                an_iovec(first.as_mut_ptr() as usize, 2),
                an_iovec(second.as_mut_ptr() as usize, 1),
            ];
            assert_eq!(
                by_number(sys, Sys::PREADV, [fd, out.as_ptr() as usize, 2, 1, 0, 0]).await,
                3
            );
            assert_eq!(&first[..], b"AB", "preadv filled the first vector wrong");
            assert_eq!(
                &second[..],
                b"3",
                "preadv did not go on to the second vector"
            );

            let q = b"QR";
            let s = b"S";
            let back = [
                an_iovec(q.as_ptr() as usize, 2),
                an_iovec(s.as_ptr() as usize, 1),
            ];
            assert_eq!(
                by_number(sys, Sys::PWRITEV, [fd, back.as_ptr() as usize, 2, 7, 0, 0]).await,
                3
            );
            assert_eq!(
                contents(sys.linux_process(), "/args_io/f", 16),
                b"0AB3XYZQRS",
                "pwritev wrote its vectors at the wrong offset"
            );

            assert_eq!(by_number(sys, Sys::FTRUNCATE, [fd, 4, 0, 0, 0, 0]).await, 0);
            assert_eq!(
                contents(sys.linux_process(), "/args_io/f", 16),
                b"0AB3",
                "ftruncate took something other than its second word as the \
                 length"
            );
        })
    });
}

#[test]
fn the_target_comes_before_the_descriptor_in_symlinkat() {
    // `symlinkat(target, newdirfd, linkpath)` is the one `*at` call in the
    // table whose descriptor is not the first word, so the arm that spells
    // it out is the only thing standing between `ln -s` and a link made in
    // the wrong place. The link's own contents are what say which way the
    // words went.
    let proc = a_process(48_121, "args-link", "args_link");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/args_link", "one", b"first");
            a_dir_named(sys.linux_process(), "/args_link", "d");
            let target = c("one");
            let link = c("/args_link/to-one");
            assert_eq!(
                by_number(
                    sys,
                    Sys::SYMLINKAT,
                    [at(&target), AT_FDCWD, at(&link), 0, 0, 0]
                )
                .await,
                0,
                "symlinkat refused a target and a path that are both there"
            );

            let mut buf = [0u8; 16];
            assert_eq!(
                by_number(
                    sys,
                    Sys::READLINKAT,
                    [AT_FDCWD, at(&link), buf.as_mut_ptr() as usize, 16, 0, 0]
                )
                .await,
                3
            );
            assert_eq!(
                &buf[..3],
                b"one",
                "the link does not hold the target symlinkat was given"
            );

            // `symlink` and `readlink` are the same two calls with no
            // descriptor, so every word moves one place along.
            let other = c("/args_link/also-one");
            assert_eq!(
                by_number(sys, Sys::SYMLINK, [at(&target), at(&other), 0, 0, 0, 0]).await,
                0
            );
            let mut again = [0u8; 16];
            assert_eq!(
                by_number(
                    sys,
                    Sys::READLINK,
                    [at(&other), again.as_mut_ptr() as usize, 16, 0, 0, 0]
                )
                .await,
                3
            );
            assert_eq!(&again[..3], b"one");

            // `unlinkat`'s third word is a flag word with one bit in it, and
            // that bit is what says whether a directory or a file is meant.
            let d = c("/args_link/d");
            assert_eq!(
                by_number(sys, Sys::UNLINKAT, [AT_FDCWD, at(&d), 0, 0, 0, 0]).await,
                errno(LxError::EISDIR),
                "unlinkat took a directory without AT_REMOVEDIR"
            );
            assert_eq!(
                by_number(
                    sys,
                    Sys::UNLINKAT,
                    [AT_FDCWD, at(&d), AT_REMOVEDIR, 0, 0, 0]
                )
                .await,
                0
            );
            assert!(
                !is_there(sys.linux_process(), "/args_link/d"),
                "the directory is still there"
            );

            // And the two calls that have no flag word at all.
            assert_eq!(
                by_number(sys, Sys::UNLINK, [at(&other), 0, 0, 0, 0, 0]).await,
                0
            );
            assert!(!is_there(sys.linux_process(), "/args_link/also-one"));
            a_dir_named(sys.linux_process(), "/args_link", "e");
            let e = c("/args_link/e");
            assert_eq!(by_number(sys, Sys::RMDIR, [at(&e), 0, 0, 0, 0, 0]).await, 0);
            assert!(!is_there(sys.linux_process(), "/args_link/e"));
        })
    });
}

#[test]
fn the_user_and_the_group_do_not_change_places() {
    // Nine arms that carry a pair of numbers nothing in the answer
    // distinguishes: `fchownat(dirfd, path, uid, gid, flags)` and its two
    // shorter forms, the three `chmod` forms, and the three `access` forms,
    // where a flag word belongs to `faccessat2` and to neither of the other
    // two. Every value here is different from every other on purpose.
    let proc = a_process(48_122, "args-own", "args_own");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/args_own", "f", b"mine");
            let path = c("/args_own/f");
            let fd = opened(sys, &path, O_RDWR);

            assert_eq!(
                by_number(sys, Sys::FCHOWNAT, [AT_FDCWD, at(&path), 7, 11, 0, 0]).await,
                0
            );
            let (uid, gid, _, _) = who_owns(sys.linux_process(), "/args_own/f");
            assert_eq!(
                (uid, gid),
                (7, 11),
                "fchownat put the user where the group goes"
            );

            // `chown` is the same handler with `AT_FDCWD` and no flags put
            // in by the arm itself, so its two ids move one word along.
            assert_eq!(
                by_number(sys, Sys::CHOWN, [at(&path), 13, 17, 0, 0, 0]).await,
                0
            );
            let (uid, gid, _, _) = who_owns(sys.linux_process(), "/args_own/f");
            assert_eq!((uid, gid), (13, 17), "chown read the wrong two words");

            assert_eq!(by_number(sys, Sys::FCHOWN, [fd, 19, 23, 0, 0, 0]).await, 0);
            let (uid, gid, _, _) = who_owns(sys.linux_process(), "/args_own/f");
            assert_eq!((uid, gid), (19, 23), "fchown read the wrong two words");

            // The three `chmod` forms. `fchmodat` has NO flags argument --
            // `fchmodat2` would -- so its fourth word is nobody's, and a
            // flag nothing defines sitting there must not refuse the call.
            assert_eq!(
                by_number(sys, Sys::FCHMOD, [fd, 0o604, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(who_owns(sys.linux_process(), "/args_own/f").2, 0o604);
            assert_eq!(
                by_number(sys, Sys::CHMOD, [at(&path), 0o611, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(who_owns(sys.linux_process(), "/args_own/f").2, 0o611);
            assert_eq!(
                by_number(
                    sys,
                    Sys::FCHMODAT,
                    [AT_FDCWD, at(&path), 0o640, A_FACCESSAT_FLAG_OF_NOBODY, 0, 0]
                )
                .await,
                0,
                "fchmodat read a flag word it does not have"
            );
            assert_eq!(
                who_owns(sys.linux_process(), "/args_own/f").2,
                0o640,
                "fchmodat took the wrong word as the mode"
            );

            // The three `access` forms end the same way: `faccessat2` is the
            // only one of the three with a flag word, and the arm registers
            // it for that number alone.
            assert_eq!(
                by_number(sys, Sys::ACCESS, [at(&path), R_OK, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                by_number(
                    sys,
                    Sys::FACCESSAT,
                    [AT_FDCWD, at(&path), R_OK, A_FACCESSAT_FLAG_OF_NOBODY, 0, 0]
                )
                .await,
                0,
                "faccessat read a flag word it does not have"
            );
            assert_eq!(
                by_number(
                    sys,
                    Sys::FACCESSAT2,
                    [AT_FDCWD, at(&path), R_OK, A_FACCESSAT_FLAG_OF_NOBODY, 0, 0]
                )
                .await,
                errno(LxError::EINVAL),
                "faccessat2 ignored the flag word that is its own"
            );
        })
    });
}

#[test]
fn the_mode_and_the_device_of_a_node_do_not_change_places() {
    // `mknodat(dirfd, path, mode, dev)` keeps the device only for a
    // character or block node, and the mode decides which kind it is, so the
    // two words are told apart by what ends up in `rdev`. `mkdirat` has no
    // fourth word at all.
    let proc = a_process(48_123, "args-node", "args_node");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let made = c("/args_node/made");
            assert_eq!(
                by_number(sys, Sys::MKDIRAT, [AT_FDCWD, at(&made), 0o700, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                who_owns(sys.linux_process(), "/args_node/made").2,
                0o700,
                "mkdirat took the wrong word as the mode"
            );

            let plain = c("/args_node/plain");
            assert_eq!(
                by_number(sys, Sys::MKDIR, [at(&plain), 0o750, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(who_owns(sys.linux_process(), "/args_node/plain").2, 0o750);

            let node = c("/args_node/zero");
            assert_eq!(
                by_number(
                    sys,
                    Sys::MKNODAT,
                    [AT_FDCWD, at(&node), S_IFCHR | 0o644, NULL_DEVICE, 0, 0]
                )
                .await,
                0
            );
            let (_, _, mode, rdev) = who_owns(sys.linux_process(), "/args_node/zero");
            assert_eq!(mode, 0o644, "mknodat took the device as the mode");
            assert_eq!(
                rdev, NULL_DEVICE as u64,
                "mknodat took the mode as the device"
            );

            let legacy = c("/args_node/zero-too");
            assert_eq!(
                by_number(
                    sys,
                    Sys::MKNOD,
                    [at(&legacy), S_IFCHR | 0o600, NULL_DEVICE, 0, 0, 0]
                )
                .await,
                0
            );
            let (_, _, mode, rdev) = who_owns(sys.linux_process(), "/args_node/zero-too");
            assert_eq!((mode, rdev), (0o600, NULL_DEVICE as u64));
        })
    });
}

#[test]
fn the_struct_each_stat_fills_is_the_one_the_table_names() {
    // The `stat` family writes a struct whose fields this module cannot
    // read, so the reference is the same handler called by name, where the
    // compiler puts the arguments in place. Byte for byte, the two answers
    // have to agree -- and a swapped buffer would not be a different answer
    // but a write to a word that is not a pointer.
    let proc = a_process(48_124, "args-stat", "args_stat");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/args_stat", "f", b"0123456789");
            let path = c("/args_stat/f");
            let fd = opened(sys, &path, 0);

            let mut by_num = [0u8; 256];
            let mut by_name = [0u8; 256];
            assert_eq!(
                by_number(
                    sys,
                    Sys::FSTAT,
                    [fd, by_num.as_mut_ptr() as usize, 0, 0, 0, 0]
                )
                .await,
                0
            );
            sys.sys_fstat(
                FileDesc::from(fd),
                UserOutPtr::from(by_name.as_mut_ptr() as usize),
            )
            .expect("fstat by name");
            assert_eq!(
                by_num[..],
                by_name[..],
                "the struct fstat filled by number is not the one it fills by name"
            );

            let mut at_num = [0u8; 256];
            let mut at_name = [0u8; 256];
            assert_eq!(
                by_number(
                    sys,
                    Sys::NEWFSTATAT,
                    [
                        AT_FDCWD,
                        at(&path),
                        at_num.as_mut_ptr() as usize,
                        AT_SYMLINK_NOFOLLOW,
                        0,
                        0
                    ]
                )
                .await,
                0
            );
            sys.sys_fstatat(
                FileDesc::CWD,
                UserInPtr::from(at(&path)),
                UserOutPtr::from(at_name.as_mut_ptr() as usize),
                AT_SYMLINK_NOFOLLOW,
            )
            .expect("fstatat by name");
            assert_eq!(at_num[..], at_name[..], "fstatat's third word is a buffer");

            // `statx` is the only arm in the table with five words in it,
            // and the buffer is the fifth: the mask in front of it is cast
            // to a `u32` on the way through.
            let mut x_num = [0u8; 512];
            let mut x_name = [0u8; 512];
            assert_eq!(
                by_number(
                    sys,
                    Sys::STATX,
                    [
                        AT_FDCWD,
                        at(&path),
                        0,
                        0xfff,
                        x_num.as_mut_ptr() as usize,
                        0
                    ]
                )
                .await,
                0
            );
            sys.sys_statx(
                FileDesc::CWD,
                UserInPtr::from(at(&path)),
                0,
                0xfff,
                UserOutPtr::from(x_name.as_mut_ptr() as usize),
            )
            .expect("statx by name");
            assert_eq!(
                x_num[..],
                x_name[..],
                "statx did not write into its fifth word"
            );

            let mut fs_num = [0u8; 256];
            let mut fs_name = [0u8; 256];
            assert_eq!(
                by_number(
                    sys,
                    Sys::FSTATFS,
                    [fd, fs_num.as_mut_ptr() as usize, 0, 0, 0, 0]
                )
                .await,
                0
            );
            sys.sys_fstatfs(
                FileDesc::from(fd),
                UserOutPtr::from(fs_name.as_mut_ptr() as usize),
            )
            .expect("fstatfs by name");
            assert_eq!(fs_num[..], fs_name[..]);
        })
    });
}

#[test]
fn the_directory_a_descriptor_names_becomes_the_working_one() {
    // `fchdir(fd)` and `getcwd(buf, len)`: the proof that the first took the
    // descriptor is what the second copies out, and the proof that the
    // second read its length is that a buffer one byte short is `ERANGE`.
    let proc = a_process(48_125, "args-cwd", "args_cwd");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_dir_named(sys.linux_process(), "/args_cwd", "sub");
            let sub = c("/args_cwd/sub");
            let fd = opened(sys, &sub, 0o200000);

            assert_eq!(by_number(sys, Sys::FCHDIR, [fd, 0, 0, 0, 0, 0]).await, 0);
            let mut buf = [0u8; 32];
            let n = by_number(
                sys,
                Sys::GETCWD,
                [buf.as_mut_ptr() as usize, 32, 0, 0, 0, 0],
            )
            .await;
            assert!(n > 0, "getcwd answered {}", n);
            assert_eq!(
                &buf[..13],
                b"/args_cwd/sub",
                "fchdir did not take the descriptor's directory"
            );

            // The length is the second word: with the two swapped this would
            // be a write of 32 bytes to address 32.
            assert_eq!(
                by_number(sys, Sys::GETCWD, [buf.as_mut_ptr() as usize, 4, 0, 0, 0, 0]).await,
                errno(LxError::ERANGE),
                "getcwd wrote a path longer than the buffer it was given"
            );
        })
    });
}
