//! What `fallocate(2)` does to the file, asked through the syscall.
//!
//! `fallocate_op` decodes the mode word and has `fallocate_mode_tests`
//! beside this file. What nothing could see from there is the **wiring**,
//! and here it carries a promise rather than just a permission: two of the
//! modes tell the caller that the range **reads back as zeros afterwards**,
//! and a no-op that reports success is how a program ends up reading the
//! old bytes out of a hole it believes it punched.
//!
//! The order matters for the same reason it did in `mount`, and the
//! comment in production already names it: a caller probing for a mode on
//! a read-only descriptor is told about the mode, not about the
//! descriptor. So `fallocate(ro_fd, SOME_NEW_MODE, ..)` has to answer
//! about the mode -- that is how a program finds out whether this kernel
//! implements it at all, and answering `EBADF` instead makes the probe
//! come back "your descriptor is wrong" for a mode that does not exist.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::{LinuxProcess, RLimit, RLIMIT_FSIZE};
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// A descriptor number this harness never hands out.
const A_CLOSED_FD: usize = 100;

/// An offset or length userspace wrote as `-1`.
const A_NEGATIVE: usize = -1_i64 as usize;

/// Ten bytes whose every position is its own digit, so a range that was
/// zeroed and a range that was not can be told apart at a glance.
const TEN_DIGITS: &[u8; 10] = b"0123456789";

/// A mode word with a bit this kernel has never heard of.
const AN_UNKNOWN_MODE: usize = 0x80;

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A node of `dir_path` called `name`, holding `content` when it is a
/// regular file, and the descriptor it got.
fn a_node(
    lp: &LinuxProcess,
    dir_path: &str,
    name: &str,
    type_: FileType,
    content: &[u8],
    flags: OpenFlags,
) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, type_, 0o644).unwrap();
    if !content.is_empty() {
        inode.resize(content.len()).unwrap();
        inode.write_at(0, content).unwrap();
    }
    let path = alloc::format!("{}/{}", dir_path, name);
    let fd: i32 = lp.add_file(File::new(inode, flags, path)).unwrap().into();
    fd as usize
}

/// The length the file really has, asked of the filesystem and not of the
/// syscall under test.
fn length_of(lp: &LinuxProcess, dir_path: &str, name: &str) -> usize {
    lp.lookup_inode(&alloc::format!("{}/{}", dir_path, name))
        .unwrap()
        .metadata()
        .unwrap()
        .size
}

/// What the file really holds, same source.
fn bytes_of(lp: &LinuxProcess, dir_path: &str, name: &str, n: usize) -> alloc::vec::Vec<u8> {
    let inode = lp
        .lookup_inode(&alloc::format!("{}/{}", dir_path, name))
        .unwrap();
    let mut buf = alloc::vec![0xffu8; n];
    let read = inode.read_at(0, &mut buf).unwrap();
    buf.truncate(read);
    buf
}

#[test]
fn allocating_grows_the_file_unless_keep_size_says_not_to() {
    // The plain job: `fallocate(fd, 0, offset, len)` makes sure the range
    // is there, which on a filesystem with no extent map means the file
    // has to be long enough to hold it. With `FALLOC_FL_KEEP_SIZE` the
    // length must not move -- that is the whole difference between the
    // two, and it is what `posix_fallocate` versus a database's
    // preallocation turns on.
    let proc = a_process(46_000, "falloc-grow", "falloc_grow");
    let dir_path = String::from("/falloc_grow");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_node(
                &lp,
                &dir_path,
                "f",
                FileType::File,
                TEN_DIGITS,
                OpenFlags::RDWR,
            );
            let fd = FileDesc::from(fd);

            assert_eq!(
                sys.sys_fallocate(fd, 0, 8, 8),
                Ok(0),
                "a plain fallocate was refused"
            );
            assert_eq!(
                length_of(&lp, &dir_path, "f"),
                16,
                "fallocate did not make room for the range it was given"
            );

            // And a range that is already inside the file does not shrink
            // it back.
            assert_eq!(sys.sys_fallocate(fd, 0, 0, 4), Ok(0));
            assert_eq!(length_of(&lp, &dir_path, "f"), 16);

            assert_eq!(
                sys.sys_fallocate(fd, FALLOC_FL_KEEP_SIZE, 100, 8),
                Ok(0),
                "FALLOC_FL_KEEP_SIZE was refused"
            );
            assert_eq!(
                length_of(&lp, &dir_path, "f"),
                16,
                "FALLOC_FL_KEEP_SIZE changed the length anyway"
            );
        })
    });
}

#[test]
fn a_punched_hole_really_reads_back_as_zeros() {
    // The promise, and the reason a successful no-op here is worse than an
    // error: a caller that punched a hole believes those bytes are gone.
    // Asked of the two modes that carry it, with the digits around the
    // hole left untouched so a zeroing that ran too wide is visible too.
    let proc = a_process(46_001, "falloc-hole", "falloc_hole");
    let dir_path = String::from("/falloc_hole");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let hole = a_node(
                &lp,
                &dir_path,
                "hole",
                FileType::File,
                TEN_DIGITS,
                OpenFlags::RDWR,
            );
            let zero = a_node(
                &lp,
                &dir_path,
                "zero",
                FileType::File,
                TEN_DIGITS,
                OpenFlags::RDWR,
            );

            // A punch must carry KEEP_SIZE, so the length cannot move.
            assert_eq!(
                sys.sys_fallocate(
                    FileDesc::from(hole),
                    FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                    3,
                    4,
                ),
                Ok(0)
            );
            // The digits with four of them replaced by zeros, built rather
            // than spelled: an escape next to a digit is too easy to get
            // wrong, and the point of this assertion is the zeros.
            let mut with_a_hole = TEN_DIGITS.to_vec();
            with_a_hole[3..7].fill(0);
            assert_eq!(
                bytes_of(&lp, &dir_path, "hole", 10),
                with_a_hole,
                "the punched range did not read back as zeros, or the zeroing ran wide"
            );
            assert_eq!(length_of(&lp, &dir_path, "hole"), 10);

            // ZERO_RANGE is the one that may grow the file.
            assert_eq!(
                sys.sys_fallocate(FileDesc::from(zero), FALLOC_FL_ZERO_RANGE, 8, 4),
                Ok(0)
            );
            assert_eq!(length_of(&lp, &dir_path, "zero"), 12);
            assert_eq!(
                bytes_of(&lp, &dir_path, "zero", 12),
                b"01234567\0\0\0\0".to_vec(),
                "ZERO_RANGE left the old bytes inside the file"
            );
        })
    });
}

#[test]
fn a_mode_this_kernel_does_not_implement_is_answered_before_the_descriptor_is_judged() {
    // The order the production comment names, and the one place where two
    // of the arguments can both be wrong: a program probing for a mode
    // must be told about the mode. The same read-only descriptor gives
    // both answers here, so nothing but the mode can account for the
    // difference.
    let proc = a_process(46_002, "falloc-order", "falloc_order");
    let dir_path = String::from("/falloc_order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let read_only = a_node(
                &lp,
                &dir_path,
                "ro",
                FileType::File,
                TEN_DIGITS,
                OpenFlags::RDONLY,
            );
            let read_only = FileDesc::from(read_only);

            assert_eq!(
                sys.sys_fallocate(read_only, AN_UNKNOWN_MODE, 0, 4),
                Err(LxError::EOPNOTSUPP),
                "the mode was judged after the descriptor's access mode"
            );
            assert_eq!(
                sys.sys_fallocate(read_only, 0, 0, 4),
                Err(LxError::EBADF),
                "fallocate wrote through a descriptor open for reading only"
            );
            assert_eq!(
                length_of(&lp, &dir_path, "ro"),
                10,
                "a refused fallocate changed the length anyway"
            );

            // And the descriptor itself comes first of all: a closed one
            // is EBADF even with a mode that would have been refused.
            assert_eq!(
                sys.sys_fallocate(FileDesc::from(A_CLOSED_FD), AN_UNKNOWN_MODE, 0, 4),
                Err(LxError::EBADF)
            );
        })
    });
}

#[test]
fn a_length_that_is_not_positive_or_an_offset_that_is_negative_is_einval() {
    // `fallocate(2)`: EINVAL for a negative offset and for a length that
    // is not positive. Zero is the one that is easy to let through -- it
    // is not negative, and a loop that allocates nothing looks like a
    // success -- so it is asked for on its own.
    let proc = a_process(46_003, "falloc-len", "falloc_len");
    let dir_path = String::from("/falloc_len");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_node(
                &lp,
                &dir_path,
                "f",
                FileType::File,
                TEN_DIGITS,
                OpenFlags::RDWR,
            );
            let fd = FileDesc::from(fd);

            assert_eq!(
                sys.sys_fallocate(fd, 0, 0, 0),
                Err(LxError::EINVAL),
                "a length of zero was accepted"
            );
            assert_eq!(
                sys.sys_fallocate(fd, 0, 0, A_NEGATIVE),
                Err(LxError::EINVAL),
                "a negative length was accepted"
            );
            assert_eq!(
                sys.sys_fallocate(fd, 0, A_NEGATIVE, 4),
                Err(LxError::EINVAL),
                "a negative offset was accepted"
            );
            assert_eq!(length_of(&lp, &dir_path, "f"), 10);
        })
    });
}

#[test]
fn each_kind_of_node_gets_the_errno_its_kind_calls_for() {
    // Three different errnos for three kinds of file, and they are not
    // interchangeable: a program that gets ESPIPE knows it is talking to a
    // pipe and can fall back, while EISDIR and ENODEV each say something
    // else. All three are reached through the same call on the same line,
    // so only a test that asks for each kind separately sees them.
    let proc = a_process(46_004, "falloc-kind", "falloc_kind");
    let dir_path = String::from("/falloc_kind");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let dir = a_node(&lp, &dir_path, "d", FileType::Dir, &[], OpenFlags::RDWR);
            let fifo = a_node(
                &lp,
                &dir_path,
                "fifo",
                FileType::NamedPipe,
                &[],
                OpenFlags::RDWR,
            );
            let chr = a_node(
                &lp,
                &dir_path,
                "chr",
                FileType::CharDevice,
                &[],
                OpenFlags::RDWR,
            );

            assert_eq!(
                sys.sys_fallocate(FileDesc::from(dir), 0, 0, 4),
                Err(LxError::EISDIR)
            );
            assert_eq!(
                sys.sys_fallocate(FileDesc::from(fifo), 0, 0, 4),
                Err(LxError::ESPIPE)
            );
            assert_eq!(
                sys.sys_fallocate(FileDesc::from(chr), 0, 0, 4),
                Err(LxError::ENODEV)
            );
        })
    });
}

#[test]
fn only_a_fallocate_that_grows_the_file_answers_to_rlimit_fsize() {
    // `inode_newsize_ok` is asked when the new end is past the file's
    // length, and only then: a `KEEP_SIZE` call names a range past the
    // limit and allocates nothing, so there is nothing for the limit to
    // refuse. Both halves, because a check that is asked unconditionally
    // starts refusing the second one.
    let proc = a_process(46_005, "falloc-fsize", "falloc_fsize");
    let dir_path = String::from("/falloc_fsize");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_node(
                &lp,
                &dir_path,
                "f",
                FileType::File,
                TEN_DIGITS,
                OpenFlags::RDWR,
            );
            let fd = FileDesc::from(fd);
            let max = lp.rlimit(RLIMIT_FSIZE, None, false).unwrap().max;
            lp.rlimit(RLIMIT_FSIZE, Some(RLimit { cur: 16, max }), false)
                .unwrap();

            assert_eq!(
                sys.sys_fallocate(fd, 0, 0, 64),
                Err(LxError::EFBIG),
                "fallocate grew a file past RLIMIT_FSIZE"
            );
            assert_eq!(
                length_of(&lp, &dir_path, "f"),
                10,
                "a refused fallocate allocated something anyway"
            );

            assert_eq!(
                sys.sys_fallocate(fd, FALLOC_FL_KEEP_SIZE, 0, 64),
                Ok(0),
                "a KEEP_SIZE call that cannot grow the file was refused by the limit"
            );
            assert_eq!(length_of(&lp, &dir_path, "f"), 10);
        })
    });
}

#[test]
fn an_append_only_descriptor_may_allocate_but_may_not_punch() {
    // "It is not possible to punch hole on an append-only file": the point
    // of `O_APPEND` on a log is that what is already in it cannot be
    // rewritten, and zeroing a range inside it is rewriting it. Allocating
    // more room is not, so that one still goes through -- which is what
    // makes this a test of the condition and not of the flag.
    let proc = a_process(46_006, "falloc-append", "falloc_append");
    let dir_path = String::from("/falloc_append");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_node(
                &lp,
                &dir_path,
                "log",
                FileType::File,
                TEN_DIGITS,
                OpenFlags::RDWR | OpenFlags::APPEND,
            );
            let fd = FileDesc::from(fd);

            assert_eq!(
                sys.sys_fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, 4),
                Err(LxError::EPERM),
                "a hole was punched in an append-only file"
            );
            assert_eq!(
                bytes_of(&lp, &dir_path, "log", 10),
                TEN_DIGITS.to_vec(),
                "a refused punch rewrote the file anyway"
            );

            assert_eq!(
                sys.sys_fallocate(fd, 0, 10, 6),
                Ok(0),
                "an append-only file was refused room it was only growing into"
            );
            assert_eq!(length_of(&lp, &dir_path, "log"), 16);
        })
    });
}
