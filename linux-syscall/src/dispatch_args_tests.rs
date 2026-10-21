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
//! 6. **A round trip is the strongest thing a pair of arms can be asked
//!    for.** `set_robust_list` then `get_robust_list` has to give the same
//!    address and the same length back; `prctl(PR_SET_NAME)` then
//!    `PR_GET_NAME` the same name.
//! 7. **Where nothing can be read back, each word refuses in its own way.**
//!    `setpgid` answers `ESRCH` about its first word and `EPERM` about its
//!    second; `syslog` answers `EINVAL` about a type nobody has, about a
//!    read with no buffer, and about a console level out of range, which is
//!    the only place its third word is a number and not a length.
//!
//! Fixed pids live in 48_12x and 48_13x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use kernel_hal::PAGE_SIZE;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::MMUFlags;

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
/// `POSIX_FADV_WILLNEED`, and an advice nobody has.
const POSIX_FADV_WILLNEED: usize = 3;
const AN_ADVICE_OF_NOBODY: usize = 99;
/// A length that is negative as a `loff_t`, which three of these arms refuse
/// and none of them accepts as an offset.
const A_NEGATIVE_LENGTH: usize = -1isize as usize;
/// Two of `sync_file_range`'s flags, and a bit nobody has.
const SYNC_FILE_RANGE_WAIT_AFTER: usize = 4;
const A_SYNC_FILE_RANGE_FLAG_OF_NOBODY: usize = 8;
/// `arch_prctl`'s code for reading the thread pointer back.
const ARCH_GET_FS: usize = 0x1003;
/// `get_mempolicy`'s flag that makes it answer about an address, and a
/// `maxnode` big enough that a mask may be asked for at all.
const MPOL_F_ADDR: usize = 1 << 1;
const MPOL_DEFAULT: i32 = 0;
const ENOUGH_NODES: usize = 64;
/// What a buffer is seeded with, so that "wrote nothing" cannot pass for
/// "wrote zero" and a write past the end shows up.
const A_SEED: u8 = 0xaa;
/// A page, as the memory arms count.
const PAGE: usize = PAGE_SIZE;
/// `PROT_READ | PROT_WRITE`, and read-only on its own.
const RW: usize = 0x3;
const PROT_READ: usize = 0x1;
/// `MAP_PRIVATE | MAP_FIXED`, with and without `MAP_ANONYMOUS`.
const PRIVATE_FIXED: usize = 0x12;
const ANON_FIXED: usize = 0x32;
/// What an anonymous mapping passes for a descriptor, and a descriptor that
/// is not anybody's.
const NO_FILE: usize = -1i32 as usize;
const A_DESCRIPTOR_OF_NOBODY: usize = 4242;
/// `MREMAP_MAYMOVE | MREMAP_FIXED`: move it, and to this exact address.
const MREMAP_TO_A_FIXED_ADDRESS: usize = 0x3;

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

/// `pages` anonymous pages at an offset of this test's own. Every process in
/// this binary shares one address-space base, so each test picks a band
/// nobody else uses -- and `MAP_FIXED` is what makes the choice stick.
async fn anonymous(sys: &Syscall<'_>, offset: usize, pages: usize) -> usize {
    let at = sys.zircon_process().vmar().addr() + offset;
    let got = by_number(
        sys,
        Sys::MMAP,
        [at, pages * PAGE, RW, ANON_FIXED, NO_FILE, 0],
    )
    .await;
    assert_eq!(got, at as isize, "MAP_FIXED did not map where it was told");
    at
}

/// A page touched the way a program touches it: the fault a write takes,
/// which is what commits the frame. Under libos there is no CPU to take it,
/// so the test asks the VMAR itself.
fn touch(sys: &Syscall<'_>, page: usize) {
    sys.zircon_process()
        .vmar()
        .handle_page_fault(page, MMUFlags::WRITE | MMUFlags::USER)
        .expect("a mapped, writable page takes a write fault");
}

/// And whether it takes one at all, which is what a protection change and a
/// removal are for.
fn takes_a_write(sys: &Syscall<'_>, page: usize) -> bool {
    sys.zircon_process()
        .vmar()
        .handle_page_fault(page, MMUFlags::WRITE | MMUFlags::USER)
        .is_ok()
}

/// Bytes a program leaves in a page it has touched.
fn write_bytes(at: usize, bytes: &[u8]) {
    let mut out: UserOutPtr<u8> = UserOutPtr::from(at);
    out.write_array(bytes).expect("a touched page is writable");
}

/// And what it reads back out of one.
fn read_bytes(at: usize, len: usize) -> Vec<u8> {
    UserInPtr::<u8>::from(at)
        .read_array(len)
        .expect("a touched page is readable")
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

#[test]
fn the_three_group_ids_are_three_different_numbers() {
    // The group half of the table. `dispatch_tests` has the user half, where
    // the ids are read back by name; here they come back through
    // `getresgid`, whose three words are three pointers that look alike, so
    // one call checks the order both arms put them in. Every value is
    // different from every other on purpose: with `rgid == egid` a swap is
    // the same answer.
    let proc = a_process(48_130, "args-gids", "args_gids");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let (mut r, mut e, mut s) = (0u32, 0u32, 0u32);
            let out = [
                &mut r as *mut u32 as usize,
                &mut e as *mut u32 as usize,
                &mut s as *mut u32 as usize,
            ];
            assert_eq!(
                by_number(sys, Sys::SETRESGID, [31, 37, 41, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                by_number(sys, Sys::GETRESGID, [out[0], out[1], out[2], 0, 0, 0]).await,
                0
            );
            assert_eq!(
                (r, e, s),
                (31, 37, 41),
                "the three group ids came back in another order"
            );

            // `setregid` has two words where `setresgid` has three.
            assert_eq!(by_number(sys, Sys::SETREGID, [43, 47, 0, 0, 0, 0]).await, 0);
            assert_eq!(
                by_number(sys, Sys::GETRESGID, [out[0], out[1], out[2], 0, 0, 0]).await,
                0
            );
            assert_eq!(
                (r, e),
                (43, 47),
                "setregid put the real id where the effective one goes"
            );

            // `setgid` from a privileged process moves all three.
            assert_eq!(by_number(sys, Sys::SETGID, [53, 0, 0, 0, 0, 0]).await, 0);
            assert_eq!(
                by_number(sys, Sys::GETRESGID, [out[0], out[1], out[2], 0, 0, 0]).await,
                0
            );
            assert_eq!((r, e, s), (53, 53, 53));

            // `setfsgid` answers the id it is REPLACING, which is the only
            // thing that says its word was read at all.
            assert_eq!(
                by_number(sys, Sys::SETFSGID, [59, 0, 0, 0, 0, 0]).await,
                53,
                "setfsgid did not answer the filesystem group it replaced"
            );
            // Asked again, so that the 59 above is one it KEPT and not just
            // one it read: the answer to the second call is the first call's
            // argument.
            assert_eq!(
                by_number(sys, Sys::SETFSGID, [61, 0, 0, 0, 0, 0]).await,
                59,
                "the filesystem group the call before set is not the one it replaced"
            );
        })
    });
}

#[test]
fn setgroups_takes_the_count_before_the_list() {
    // `setgroups(size, list)`: two words, one a count and one a pointer, and
    // nothing in the answer tells them apart. Read back by name, because
    // `getgroups` is already called by number elsewhere.
    let proc = a_process(48_131, "args-groups", "args_groups");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let wanted = [61u32, 67, 71];
            assert_eq!(
                by_number(
                    sys,
                    Sys::SETGROUPS,
                    [3, wanted.as_ptr() as usize, 0, 0, 0, 0]
                )
                .await,
                0
            );
            let mut got = [0u32; 8];
            assert_eq!(
                sys.sys_getgroups(8, UserOutPtr::from(got.as_mut_ptr() as usize)),
                Ok(3)
            );
            assert_eq!(
                &got[..3],
                &wanted[..],
                "the supplementary groups are not the ones setgroups was given"
            );
        })
    });
}

#[test]
fn the_process_group_of_a_pid_nobody_asked_about() {
    // `getpgrp` takes no arguments, and the arm says so by passing a zero of
    // its own rather than `a0`: a program that leaves rubbish in `rdi` --
    // which it may, the call has no arguments -- must still get its own
    // process group. The reference is the same handler called by name with
    // the zero the arm should have used.
    let proc = a_process(48_132, "args-pgrp", "args_pgrp");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mine = sys.sys_getpgid(0).expect("a process has a group");
            assert_eq!(
                by_number(sys, Sys::GETPGRP, [0x5eed_5eed, 7, 0, 0, 0, 0]).await,
                mine as isize,
                "getpgrp read a word that is not its own"
            );

            // `setpgid(pid, pgid)`: a process that is not there and a group
            // that is not there are DIFFERENT refusals -- `ESRCH` for the
            // first word and `EPERM` for the second, since joining a group
            // outside the caller's session is not allowed -- so the pair
            // pins the order with nothing else set up.
            const NOBODY: usize = 999_999;
            assert_eq!(
                by_number(sys, Sys::SETPGID, [NOBODY, 0, 0, 0, 0, 0]).await,
                errno(LxError::ESRCH),
                "setpgid looked for the process in the wrong word"
            );
            assert_eq!(
                by_number(sys, Sys::SETPGID, [0, NOBODY, 0, 0, 0, 0]).await,
                errno(LxError::EPERM),
                "setpgid took the group id from the wrong word"
            );

            // And a process group leader may not open a session: POSIX, and
            // the reason the daemonize idiom forks first.
            assert_eq!(
                by_number(sys, Sys::SETSID, [0, 0, 0, 0, 0, 0]).await,
                errno(LxError::EPERM),
                "a group leader was given a session of its own"
            );
        })
    });
}

#[test]
fn the_robust_list_comes_back_as_it_went_in() {
    // `set_robust_list(head, len)` and `get_robust_list(pid, head, len)`
    // are a round trip, which is the strongest thing a pair of arms can be
    // asked for: the address and the length have to come back where they
    // went in. The length is checked against the size of the structure, so a
    // swap is an error and not a wrong answer.
    let proc = a_process(48_133, "args-robust", "args_robust");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let size = core::mem::size_of::<linux_object::thread::RobustList>();
            let head = [0usize; 3];
            let at_head = head.as_ptr() as usize;

            assert_eq!(
                by_number(sys, Sys::SET_ROBUST_LIST, [at_head, 8, 0, 0, 0, 0]).await,
                errno(LxError::EINVAL),
                "set_robust_list took a length that is not the structure's"
            );
            assert_eq!(
                by_number(sys, Sys::SET_ROBUST_LIST, [at_head, size, 0, 0, 0, 0]).await,
                0
            );

            let mut back = 0usize;
            let mut len = 0usize;
            assert_eq!(
                by_number(
                    sys,
                    Sys::GET_ROBUST_LIST,
                    [
                        0,
                        &mut back as *mut usize as usize,
                        &mut len as *mut usize as usize,
                        0,
                        0,
                        0
                    ]
                )
                .await,
                0
            );
            assert_eq!(back, at_head, "the robust list came back somewhere else");
            assert_eq!(len, size, "the robust list came back a different size");

            // `set_tid_address` answers the caller's thread id, which is the
            // only way to see that its one word was taken as a pointer and
            // not as something else.
            let mut slot = 0i32;
            assert_eq!(
                by_number(
                    sys,
                    Sys::SET_TID_ADDRESS,
                    [&mut slot as *mut i32 as usize, 0, 0, 0, 0, 0]
                )
                .await,
                sys.thread.id() as isize
            );
        })
    });
}

#[test]
fn each_setting_answers_what_it_was_set_to() {
    // Four arms whose words are all small integers, and whose answer is the
    // state they left behind: `personality` answers the persona it is
    // REPLACING, `setpriority` is read back by name, `sched_setscheduler`
    // takes a pointer where its neighbours take a number, and `membarrier`
    // has a flag word that must be zero.
    let proc = a_process(48_134, "args-set", "args_set");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // `ADDR_NO_RANDOMIZE`. The first call answers the persona before
            // it, the query answers the one after: one word, two readings.
            assert_eq!(
                by_number(sys, Sys::PERSONALITY, [8, 0, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                by_number(sys, Sys::PERSONALITY, [0xffff_ffff, 0, 0, 0, 0, 0]).await,
                8,
                "personality did not keep the persona it was given"
            );

            // `setpriority(which, who, prio)`: `PRIO_PROCESS` and the caller,
            // so the only word with anything in it is the third.
            assert_eq!(
                by_number(sys, Sys::SETPRIORITY, [0, 0, 7, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                sys.sys_getpriority(0, 0),
                Ok(13),
                "the nice value is not the one setpriority was given (20 - 7)"
            );

            // `sched_setscheduler(pid, policy, param)`: the parameter is a
            // pointer, and a policy where the pointer goes is a number too
            // big to be a policy.
            let priority = 10i32;
            assert_eq!(
                by_number(
                    sys,
                    Sys::SCHED_SETSCHEDULER,
                    [0, 1, &priority as *const i32 as usize, 0, 0, 0]
                )
                .await,
                0
            );
            assert_eq!(
                sys.sys_sched_getscheduler(0),
                Ok(1),
                "the policy is not the one sched_setscheduler was given"
            );

            // `membarrier(cmd, flags, cpu)`: the query answers a mask of the
            // commands it has, and a flag word that is not zero is EINVAL.
            assert_eq!(
                by_number(sys, Sys::MEMBARRIER, [0, 1, 0, 0, 0, 0]).await,
                errno(LxError::EINVAL),
                "membarrier read its flag word somewhere else"
            );
            assert!(
                by_number(sys, Sys::MEMBARRIER, [0, 0, 0, 0, 0, 0]).await > 0,
                "the membarrier query answered no commands at all"
            );

            // And the arm that is an answer and nothing else.
            assert_eq!(
                by_number(sys, Sys::UNSHARE, [0, 0, 0, 0, 0, 0]).await,
                errno(LxError::ENOSYS)
            );
        })
    });
}

#[test]
fn the_name_a_process_gives_itself_and_the_domain_it_lives_in() {
    // `prctl` is the widest arm in the table -- an option and four words --
    // and a round trip through `PR_SET_NAME` and `PR_GET_NAME` says that the
    // option is the first of them and the argument the second. `uname` then
    // shows the domain name `setdomainname` was given, which is the only
    // thing that proves its length word was read.
    let proc = a_process(48_135, "args-name", "args_name");
    let was_domain = linux_object::uname::domainname();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            const PR_SET_NAME: usize = 15;
            const PR_GET_NAME: usize = 16;
            let name = b"a-thread-name\0";
            assert_eq!(
                by_number(
                    sys,
                    Sys::PRCTL,
                    [PR_SET_NAME, name.as_ptr() as usize, 0, 0, 0, 0]
                )
                .await,
                0
            );
            let mut got = [0u8; 16];
            assert_eq!(
                by_number(
                    sys,
                    Sys::PRCTL,
                    [PR_GET_NAME, got.as_mut_ptr() as usize, 0, 0, 0, 0]
                )
                .await,
                0
            );
            assert_eq!(
                &got[..13],
                b"a-thread-name",
                "the name prctl gave back is not the one it was given"
            );

            // The domain name is a global every test in this binary shares,
            // so it goes back where it was at the end.
            let domain = b"args.example";
            assert_eq!(
                by_number(
                    sys,
                    Sys::SETDOMAINNAME,
                    [domain.as_ptr() as usize, domain.len(), 0, 0, 0, 0]
                )
                .await,
                0
            );
            let mut uts = [0u8; 512];
            let mut uts_by_name = [0u8; 512];
            assert_eq!(
                by_number(sys, Sys::UNAME, [uts.as_mut_ptr() as usize, 0, 0, 0, 0, 0]).await,
                0
            );
            sys.sys_uname(UserOutPtr::from(uts_by_name.as_mut_ptr() as usize))
                .expect("uname by name");
            assert_eq!(uts[..], uts_by_name[..], "uname filled another struct");
            assert!(
                uts.windows(domain.len()).any(|w| w == &domain[..]),
                "uname does not show the domain name setdomainname was given"
            );

            // `getrandom(buf, len, flags)` answers the count it filled, and
            // sixteen zero bytes out of a random source is not a thing that
            // happens.
            let mut bytes = [0u8; 16];
            assert_eq!(
                by_number(
                    sys,
                    Sys::GETRANDOM,
                    [bytes.as_mut_ptr() as usize, 16, 0, 0, 0, 0]
                )
                .await,
                16
            );
            assert!(
                bytes.iter().any(|b| *b != 0),
                "getrandom left the buffer as it found it"
            );

            // `syslog(type, buf, len)`: each of the three words refuses in
            // its own way. A type nobody has is EINVAL; a read with no
            // buffer is EINVAL; and the console level wants its level in
            // 1..=8, which is the only place the third word is a NUMBER and
            // not a length -- read as a signed one, so a -1 that a wider
            // cast would turn into something huge and positive is EINVAL
            // here.
            assert_eq!(
                by_number(sys, Sys::SYSLOG, [99, 0, 0, 0, 0, 0]).await,
                errno(LxError::EINVAL),
                "syslog took the type from another word"
            );
            assert_eq!(
                by_number(sys, Sys::SYSLOG, [2, 0, 16, 0, 0, 0]).await,
                errno(LxError::EINVAL),
                "syslog read into a buffer it was not given"
            );
            assert_eq!(by_number(sys, Sys::SYSLOG, [8, 0, 5, 0, 0, 0]).await, 0);
            assert_eq!(
                by_number(sys, Sys::SYSLOG, [8, 0, 9, 0, 0, 0]).await,
                errno(LxError::EINVAL),
                "syslog took a console level nobody has"
            );
            let mut log = [0u8; 32];
            assert_eq!(
                by_number(
                    sys,
                    Sys::SYSLOG,
                    [2, log.as_mut_ptr() as usize, -1isize as usize, 0, 0, 0]
                )
                .await,
                errno(LxError::EINVAL),
                "syslog read its length as an unsigned number"
            );
        })
    });
    linux_object::uname::set_domainname(&was_domain);
}

#[test]
fn the_file_and_the_offset_are_the_last_two_words_of_the_only_six_word_arm() {
    // `mmap` is the one arm of the table that hands over all six words, and
    // the last two are the ones that cannot be told apart by a return value:
    // whatever it maps, it answers with an address. What separates them is
    // the BYTES that turn up at that address, so the file here holds a
    // different letter in each of its three pages.
    let proc = a_process(48_140, "args-mmap", "args_mmap");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut three: Vec<u8> = Vec::new();
            for letter in [b'A', b'B', b'C'] {
                three.resize(three.len() + PAGE, letter);
            }
            a_file_named(sys.linux_process(), "/args_mmap", "pages", &three);
            let path = c("/args_mmap/pages");
            let fd = opened(sys, &path, O_RDWR);

            // One page of that file, from its THIRD page. A length and an
            // offset the wrong way round would map two pages from the second
            // one, land at the same address, and answer the same number: the
            // letter is the only thing that tells them apart.
            let at = sys.zircon_process().vmar().addr() + 0x4800_0000;
            assert_eq!(
                by_number(sys, Sys::MMAP, [at, PAGE, RW, PRIVATE_FIXED, fd, 2 * PAGE]).await,
                at as isize,
                "MAP_FIXED did not map where it was told to"
            );
            touch(sys, at);
            assert_eq!(
                read_bytes(at, 4),
                b"CCCC",
                "the page that turned up is not the one the sixth word named"
            );

            // The fifth word is a descriptor and the fourth a flag word.
            // Swapped, the flag word would be a small number and the
            // descriptor an impossible one.
            let other = sys.zircon_process().vmar().addr() + 0x4810_0000;
            assert_eq!(
                by_number(
                    sys,
                    Sys::MMAP,
                    [other, PAGE, RW, PRIVATE_FIXED, A_DESCRIPTOR_OF_NOBODY, 0]
                )
                .await,
                errno(LxError::EBADF),
                "mmap mapped a descriptor nobody has"
            );

            // The second word is a length, and a length of zero is the one
            // thing `mmap` refuses outright. With the address in its place
            // this would be an enormous mapping and not a refusal.
            assert_eq!(
                by_number(sys, Sys::MMAP, [other, 0, RW, ANON_FIXED, NO_FILE, 0]).await,
                errno(LxError::EINVAL),
                "mmap took its first word as the length"
            );

            // And the sixth word is an offset into the file, which has to be
            // a whole number of pages. One byte is not, and the second word
            // in its place would be.
            assert_eq!(
                by_number(sys, Sys::MMAP, [other, PAGE, RW, PRIVATE_FIXED, fd, 1]).await,
                errno(LxError::EINVAL),
                "mmap took an offset that is not a whole number of pages"
            );

            // The third word is a protection, and nothing it can answer says
            // which one it got: what a protection does is only visible in
            // what the page will take. A read-only page refuses a write --
            // and `mmap` keeps only the three bits it knows, so the flag word
            // in its place would be truncated to `PROT_WRITE` and take one.
            let only_read = sys.zircon_process().vmar().addr() + 0x4820_0000;
            assert_eq!(
                by_number(
                    sys,
                    Sys::MMAP,
                    [only_read, PAGE, PROT_READ, ANON_FIXED, NO_FILE, 0]
                )
                .await,
                only_read as isize,
                "a read-only anonymous page maps"
            );
            assert!(
                !takes_a_write(sys, only_read),
                "the page mmap mapped read-only took a write"
            );
        })
    });
}

#[test]
fn a_mapping_is_protected_and_removed_a_page_at_a_time() {
    // `mprotect(addr, len, prot)` and `munmap(addr, len)`: three words and
    // two, all of them plain integers, and both arms answer zero. What they
    // did is only visible in which PAGE still takes a write fault, so each
    // call here is given a two-page mapping and asked about the first page
    // alone -- a length that reached the second page, or an address that
    // started there, shows up as the wrong page changing.
    let proc = a_process(48_141, "args-prot", "args_prot");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = anonymous(sys, 0x4900_0000, 2).await;
            // Read-only, and only the first page of the two. Done before
            // either page is touched, so what is being asked is the mapping
            // and not a stale entry.
            assert_eq!(
                by_number(sys, Sys::MPROTECT, [at, PAGE, PROT_READ, 0, 0, 0]).await,
                0,
                "mprotect over one mapped page"
            );
            assert!(
                !takes_a_write(sys, at),
                "the page the first word named is still writable"
            );
            assert!(
                takes_a_write(sys, at + PAGE),
                "the length reached a page mprotect was not given"
            );

            // The third word is a protection and the second a length. In
            // each other's places the protection would be a bit nobody has.
            assert_eq!(
                by_number(sys, Sys::MPROTECT, [at, PROT_READ, PAGE, 0, 0, 0]).await,
                errno(LxError::EINVAL),
                "mprotect took a page size as a set of protection bits"
            );

            // The same two words again, this time removing the mapping --
            // and the SECOND page of the two, not the first. Swapping an
            // address and a length leaves their sum alone, so a range asked
            // only about where it ENDS cannot tell the two apart: what
            // separates them is a page below the start surviving.
            let gone = anonymous(sys, 0x4a00_0000, 2).await;
            assert_eq!(
                by_number(sys, Sys::MUNMAP, [gone + PAGE, PAGE, 0, 0, 0, 0]).await,
                0,
                "munmap over one mapped page"
            );
            assert!(
                !takes_a_write(sys, gone + PAGE),
                "the page the first word named is still there"
            );
            assert!(
                takes_a_write(sys, gone),
                "munmap reached back past the address it was given"
            );
        })
    });
}

#[test]
fn the_break_and_a_moved_mapping_both_answer_with_an_address() {
    // `brk(addr)` is a one-word arm and `mremap(old, old_len, new_len,
    // flags, new_addr)` a five-word one, and both answer an address. For
    // `brk` the second word in the first one's place is a zero, which is the
    // query form and answers the break that is already there; for `mremap`
    // the words are told apart by the mapping landing where the fifth word
    // says, two pages long where it was one, with the bytes it had.
    let proc = a_process(48_142, "args-brk", "args_brk");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let base = linux_object::loader::heap_base(&sys.zircon_process().vmar());
            sys.linux_process().set_brk(base);
            assert_eq!(
                by_number(sys, Sys::BRK, [base + 3 * PAGE, 0, 0, 0, 0, 0]).await,
                (base + 3 * PAGE) as isize,
                "brk did not move the break to the address it was given"
            );
            // And asked again with nothing, it answers the break it now has
            // rather than the one it started with: one call could not tell
            // the stored break from the word just passed in.
            assert_eq!(
                by_number(sys, Sys::BRK, [0, base, 0, 0, 0, 0]).await,
                (base + 3 * PAGE) as isize,
                "brk read its break out of the word next to it"
            );

            let old = anonymous(sys, 0x4b00_0000, 1).await;
            touch(sys, old);
            write_bytes(old, b"moved");
            let to = sys.zircon_process().vmar().addr() + 0x4b10_0000;
            assert_eq!(
                by_number(
                    sys,
                    Sys::MREMAP,
                    [old, PAGE, 2 * PAGE, MREMAP_TO_A_FIXED_ADDRESS, to, 0]
                )
                .await,
                to as isize,
                "mremap did not put the mapping where its fifth word said"
            );
            touch(sys, to);
            assert_eq!(
                read_bytes(to, 5),
                b"moved",
                "the mapping that moved is not the one that was there"
            );
            // Two pages long now, so the second one is there to be written:
            // with the old and the new length swapped this would have shrunk
            // instead, and nothing would be mapped here.
            assert!(
                takes_a_write(sys, to + PAGE),
                "mremap grew the mapping by the wrong length"
            );
            // The fourth word is a flag word and the fifth an address. In
            // each other's places the flags would be bits nobody has.
            assert_eq!(
                by_number(sys, Sys::MREMAP, [to, PAGE, PAGE, to, 2, 0]).await,
                errno(LxError::EINVAL),
                "mremap took an address as a set of flags"
            );
        })
    });
}

#[test]
fn the_words_of_the_hints_and_the_syncs_about_a_file() {
    // Seven arms that answer zero and nothing else: `fadvise64`,
    // `readahead`, `sync_file_range`, `fsync`, `fdatasync`, `syncfs` and
    // `sync`. Nothing comes back out of any of them, so what tells their
    // words apart is WHICH of them refuses: the word next to the descriptor
    // is a number no descriptor has, so a call that read it instead would be
    // about a file nobody opened.
    let proc = a_process(48_150, "args-hint", "args_hint");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/args_hint", "f", b"0123456789");
            let path = c("/args_hint/f");
            let fd = opened(sys, &path, O_RDWR);

            assert_eq!(
                by_number(
                    sys,
                    Sys::FADVISE64,
                    [fd, A_DESCRIPTOR_OF_NOBODY, 4, POSIX_FADV_WILLNEED, 0, 0]
                )
                .await,
                0,
                "fadvise64 took its second word as the descriptor"
            );
            // The fourth word is an advice and the third a length. An advice
            // nobody has is EINVAL; with the two in each other's places the
            // advice would be 4, which this kernel knows.
            assert_eq!(
                by_number(sys, Sys::FADVISE64, [fd, 0, 4, AN_ADVICE_OF_NOBODY, 0, 0]).await,
                errno(LxError::EINVAL),
                "fadvise64 took a length as its advice"
            );
            // And the second word is only an offset, which this call does not
            // judge, while the third is a length read as a SIGNED number: a
            // negative offset is fine and a negative length is not.
            assert_eq!(
                by_number(
                    sys,
                    Sys::FADVISE64,
                    [fd, A_NEGATIVE_LENGTH, 4, POSIX_FADV_WILLNEED, 0, 0]
                )
                .await,
                0,
                "fadvise64 judged an offset it has no business judging"
            );
            assert_eq!(
                by_number(
                    sys,
                    Sys::FADVISE64,
                    [fd, 0, A_NEGATIVE_LENGTH, POSIX_FADV_WILLNEED, 0, 0]
                )
                .await,
                errno(LxError::EINVAL),
                "fadvise64 read its length as an unsigned number"
            );

            // `readahead`'s three words, told apart the same two ways.
            assert_eq!(
                by_number(
                    sys,
                    Sys::READAHEAD,
                    [fd, A_DESCRIPTOR_OF_NOBODY, 4, 0, 0, 0]
                )
                .await,
                0,
                "readahead took its second word as the descriptor"
            );
            assert_eq!(
                by_number(sys, Sys::READAHEAD, [fd, 0, A_NEGATIVE_LENGTH, 0, 0, 0]).await,
                errno(LxError::EINVAL),
                "readahead read its count as an unsigned number"
            );

            // `sync_file_range` judges its FLAGS first and its descriptor
            // last, so a flag nobody has is EINVAL even about a file nobody
            // opened -- and that is also what tells its fourth word from its
            // third, because the third in that place would be a flag it has.
            assert_eq!(
                by_number(
                    sys,
                    Sys::SYNC_FILE_RANGE,
                    [
                        A_DESCRIPTOR_OF_NOBODY,
                        0,
                        SYNC_FILE_RANGE_WAIT_AFTER,
                        A_SYNC_FILE_RANGE_FLAG_OF_NOBODY,
                        0,
                        0
                    ]
                )
                .await,
                errno(LxError::EINVAL),
                "sync_file_range took a length as its flags"
            );
            assert_eq!(
                by_number(
                    sys,
                    Sys::SYNC_FILE_RANGE,
                    [
                        fd,
                        A_DESCRIPTOR_OF_NOBODY,
                        4,
                        SYNC_FILE_RANGE_WAIT_AFTER,
                        0,
                        0
                    ]
                )
                .await,
                0,
                "sync_file_range took its second word as the descriptor"
            );

            // Three arms of one word each.
            assert_eq!(
                by_number(sys, Sys::FSYNC, [fd, A_DESCRIPTOR_OF_NOBODY, 0, 0, 0, 0]).await,
                0,
                "fsync took its second word as the descriptor"
            );
            assert_eq!(
                by_number(
                    sys,
                    Sys::FDATASYNC,
                    [fd, A_DESCRIPTOR_OF_NOBODY, 0, 0, 0, 0]
                )
                .await,
                0,
                "fdatasync took its second word as the descriptor"
            );
            assert_eq!(
                by_number(sys, Sys::SYNCFS, [fd, A_DESCRIPTOR_OF_NOBODY, 0, 0, 0, 0]).await,
                0,
                "syncfs took its second word as the descriptor"
            );
            // And one of none at all: the table hands `sync` nothing, so with
            // garbage in all six words it answers the same.
            assert_eq!(
                by_number(sys, Sys::SYNC, [A_DESCRIPTOR_OF_NOBODY; 6]).await,
                0,
                "sync looked at a word it does not have"
            );
        })
    });
}

#[test]
fn the_descriptors_these_arms_hand_out_and_the_ones_they_ask_for() {
    // `pidfd_open`, `pidfd_getfd`, `inotify_init`, `epoll_create` and
    // `name_to_handle_at`. Four of them answer a descriptor, which says
    // nothing about where their words came from -- but `pidfd_getfd` hands
    // back a descriptor on a file this process already has open, and THAT can
    // be read from. Two of the five pass a word of their own rather than one
    // of the caller's, and those are told apart by putting garbage where the
    // word they do not read would be.
    let proc = a_process(48_151, "args-pidfd", "args_pidfd");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            a_file_named(sys.linux_process(), "/args_pidfd", "f", b"0123456789");
            let path = c("/args_pidfd/f");
            // Twice, so the descriptor asked for below is not zero: a zero
            // would be indistinguishable from the flag word beside it.
            let _first = opened(sys, &path, O_RDWR);
            let second = opened(sys, &path, O_RDWR);
            assert_ne!(second, 0, "the second open did not get its own descriptor");

            // The first word is a pid and the second a flag word. A pid of
            // zero is the one pid `pidfd_open` refuses outright, so the flag
            // word in its place is a refusal and not a descriptor.
            let own = sys.zircon_process().id() as usize;
            let pidfd = by_number(sys, Sys::PIDFD_OPEN, [own, 0, 0, 0, 0, 0]).await;
            assert!(
                pidfd >= 0,
                "pidfd_open took its second word as the pid: {}",
                pidfd
            );

            // `pidfd_getfd(pidfd, targetfd, flags)`: the flags have to be
            // zero, so the descriptor in their place is a refusal; and the
            // pidfd is the first word, so a regular file in its place is one
            // too. What proves it worked is reading the file's own bytes out
            // of the descriptor that came back.
            let taken =
                by_number(sys, Sys::PIDFD_GETFD, [pidfd as usize, second, 0, 0, 0, 0]).await;
            assert!(
                taken >= 0,
                "pidfd_getfd did not hand back a descriptor: {}",
                taken
            );
            let mut buf = [0u8; 4];
            assert_eq!(
                by_number(
                    sys,
                    Sys::PREAD64,
                    [taken as usize, buf.as_mut_ptr() as usize, 4, 0, 0, 0]
                )
                .await,
                4
            );
            assert_eq!(
                &buf, b"0123",
                "the descriptor that came back does not name the file asked for"
            );

            // `inotify_init` has no words, and the table hands it a zero of
            // its own rather than `a0`: with garbage in that place it still
            // answers a descriptor, because garbage is not a flag word it
            // would accept.
            let watch = by_number(sys, Sys::INOTIFY_INIT, [A_DESCRIPTOR_OF_NOBODY; 6]).await;
            assert!(
                watch >= 0,
                "inotify_init believed the garbage in its first word: {}",
                watch
            );

            // `epoll_create(size)` takes one word, and the size has to be
            // positive: the zero beside it is a refusal.
            let epoll = by_number(sys, Sys::EPOLL_CREATE, [1, 0, 0, 0, 0, 0]).await;
            assert!(
                epoll >= 0,
                "epoll_create took its second word as the size: {}",
                epoll
            );

            // And one arm that reads nothing and refuses everything.
            assert_eq!(
                by_number(sys, Sys::NAME_TO_HANDLE_AT, [A_DESCRIPTOR_OF_NOBODY; 6]).await,
                errno(LxError::ENOSYS),
                "name_to_handle_at answered something other than ENOSYS"
            );
        })
    });
}

#[test]
fn the_two_buffers_get_mempolicy_fills_and_the_one_arch_prctl_writes() {
    // `get_mempolicy(policy, nmask, maxnode, addr, flags)` is a five-word arm
    // whose first TWO words are output pointers of different widths, so a
    // swap writes four bytes where eight belong and eight where four do: the
    // only thing that sees it is reading both buffers byte for byte, with a
    // guard past the end of each. Its third and fourth words are told apart
    // by a `maxnode` too small for a mask, and its fourth and fifth by an
    // address that has to be mapped.
    let proc = a_process(48_152, "args-mempol", "args_mempol");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // Four slots' worth of room for a word, so that a swap writing
            // eight bytes here cannot run off anything.
            let mut policy = [MPOL_DEFAULT - 7; 4];
            let mut mask = [A_SEED; 16];
            assert_eq!(
                by_number(
                    sys,
                    Sys::GET_MEMPOLICY,
                    [
                        policy.as_mut_ptr() as usize,
                        mask.as_mut_ptr() as usize,
                        ENOUGH_NODES,
                        0,
                        0,
                        0
                    ]
                )
                .await,
                0,
                "get_mempolicy over a buffer with room"
            );
            assert_eq!(
                policy[0], MPOL_DEFAULT,
                "the mode did not land in its own word"
            );
            assert_eq!(
                mask[..8],
                [0u8; 8],
                "the mask the second word names is not the one that was filled"
            );
            assert_eq!(
                mask[8..],
                [A_SEED; 8],
                "the mask ran past the eight bytes this many nodes take"
            );

            // The third word is a node count and the fourth an address. A
            // count of zero is too small for any mask at all, so the address
            // in its place is a refusal and not a zeroed mask.
            assert_eq!(
                by_number(
                    sys,
                    Sys::GET_MEMPOLICY,
                    [
                        policy.as_mut_ptr() as usize,
                        mask.as_mut_ptr() as usize,
                        0,
                        0,
                        0,
                        0
                    ]
                )
                .await,
                errno(LxError::EINVAL),
                "get_mempolicy accepted a node count with no room for a mask"
            );

            // And the fifth word is a flag word that makes the fourth an
            // address this process has to have mapped. In each other's
            // places the address would be 2 and the flags a number whose
            // bits nobody defined.
            let here = anonymous(sys, 0x4c00_0000, 1).await;
            assert_eq!(
                by_number(
                    sys,
                    Sys::GET_MEMPOLICY,
                    [policy.as_mut_ptr() as usize, 0, 0, here, MPOL_F_ADDR, 0]
                )
                .await,
                0,
                "get_mempolicy did not take its fourth word as the address"
            );

            // `arch_prctl(code, addr)`: the code names what to do and the
            // address where to put it, and the only thing that proves which
            // is which is the word landing there. A code nobody has is
            // EINVAL, so the address in its place is a refusal.
            let mut tp = [0usize; 1];
            assert_eq!(
                by_number(
                    sys,
                    Sys::ARCH_PRCTL,
                    [ARCH_GET_FS, tp.as_mut_ptr() as usize, 0, 0, 0, 0]
                )
                .await,
                0,
                "arch_prctl took its second word as the code"
            );
        })
    });
}
