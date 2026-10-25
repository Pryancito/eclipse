//! `lseek` argument checks, `SEEK_DATA`/`SEEK_HOLE` and `access` mode checks.
//!
//! The two pure helpers at the top have had tests since the day they were
//! written, and `seek_data_hole_generic` is asked for every one of its
//! answers below. What had never been asked for even once is the branch of
//! `sys_lseek` that CALLS it: eight lines, all of them at zero, which is the
//! whole of `lseek(fd, off, SEEK_DATA)` and `SEEK_HOLE` as a syscall.
//!
//! That gap matters more than eight lines sound, because the wiring does one
//! thing the helper knows nothing about: **it moves the file offset**. The
//! helper is a function from `(size, whence, offset)` to a position; the
//! syscall has to seek the descriptor there and answer with it. A kernel
//! that computed the position correctly and forgot the seek answers every
//! `lseek(fd, off, SEEK_DATA)` with the right number and leaves the
//! descriptor where it was, so the caller's next `read` returns the wrong
//! bytes with no error anywhere -- and the pure tests still pass.
//!
//! The same branch also decides two refusals of its own: a descriptor that
//! is not a `File` (a pipe) and one that is not open at all.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags, Pipe};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `SeekFrom` does not implement `PartialEq`, so compare by shape.
fn seek(whence: usize, offset: i64) -> Result<SeekFrom, LxError> {
    seek_from(whence, offset)
}

#[test]
fn the_three_whences_translate() {
    assert!(matches!(seek(0, 42), Ok(SeekFrom::Start(42))));
    assert!(matches!(seek(1, -42), Ok(SeekFrom::Current(-42))));
    assert!(matches!(seek(2, -42), Ok(SeekFrom::End(-42))));
}

#[test]
fn a_relative_seek_may_be_negative() {
    // SEEK_CUR and SEEK_END are relative, so Linux checks the *resulting*
    // position, not the argument. Rejecting these would break every
    // `lseek(fd, -n, SEEK_END)`, which is how a reader finds a trailer.
    assert!(matches!(seek(1, i64::MIN), Ok(SeekFrom::Current(i64::MIN))));
    assert!(matches!(seek(2, -1), Ok(SeekFrom::End(-1))));
}

#[test]
fn an_absolute_seek_may_not_be_negative() {
    // `vfs_setpos()` answers EINVAL. Read as `offset as u64` it became a
    // position near 2^64, which leaves the syscall looking like an errno.
    assert_eq!(seek(0, -1).err(), Some(LxError::EINVAL));
    assert_eq!(seek(0, i64::MIN).err(), Some(LxError::EINVAL));
    assert!(matches!(seek(0, 0), Ok(SeekFrom::Start(0))));
}

#[test]
fn an_unknown_whence_is_rejected_instead_of_truncated() {
    // 0x100 has low byte 0, so it used to mean SEEK_SET and the file
    // position quietly moved to `offset`.
    assert_eq!(seek(0x100, 42).err(), Some(LxError::EINVAL));
    assert_eq!(seek(0x102, 42).err(), Some(LxError::EINVAL));
    // SEEK_DATA and SEEK_HOLE never land on one of the three above
    // (`sys_lseek` answers them before it gets here).
    assert_eq!(seek(3, 0).err(), Some(LxError::EINVAL));
    assert_eq!(seek(4, 0).err(), Some(LxError::EINVAL));
}

#[test]
fn seek_data_and_hole_on_a_filesystem_without_holes() {
    const DATA: usize = 3;
    const HOLE: usize = 4;
    assert_eq!(seek_data_hole_generic(100, DATA, 0), Ok(0));
    assert_eq!(seek_data_hole_generic(100, DATA, 42), Ok(42));
    assert_eq!(seek_data_hole_generic(100, HOLE, 42), Ok(100));
    assert_eq!(seek_data_hole_generic(100, DATA, 100), Err(LxError::ENXIO));
    assert_eq!(seek_data_hole_generic(100, HOLE, 100), Err(LxError::ENXIO));
    assert_eq!(seek_data_hole_generic(100, DATA, -1), Err(LxError::ENXIO));
    assert_eq!(seek_data_hole_generic(0, HOLE, 0), Err(LxError::ENXIO));
    assert_eq!(seek(usize::MAX, 0).err(), Some(LxError::EINVAL));
}

#[test]
fn the_access_modes_pass_through() {
    // R_OK | W_OK | X_OK and every combination below them, F_OK included.
    for mode in 0..=0o7usize {
        assert_eq!(
            Syscall::access_mode(mode),
            Ok(mode as u16),
            "mode {:o}",
            mode
        );
    }
}

#[test]
fn a_mode_outside_rwx_is_rejected_instead_of_masked() {
    // `do_faccessat()`: `if (mode & ~S_IRWXO) return -EINVAL;`. Masked
    // instead, `access(path, 8)` asked F_OK and answered "yes, you may"
    // for any file that merely exists.
    assert_eq!(Syscall::access_mode(0o10), Err(LxError::EINVAL));
    assert_eq!(Syscall::access_mode(0o17), Err(LxError::EINVAL));
    assert_eq!(Syscall::access_mode(usize::MAX), Err(LxError::EINVAL));
}

/// All five `whence` values as userspace writes them. Spelled out rather
/// than taken from the constants inside `sys_lseek`, which are private to it
/// and are half of what is under test.
const SEEK_SET: usize = 0;
const SEEK_CUR: usize = 1;
const SEEK_END: usize = 2;
const SEEK_DATA: usize = 3;
const SEEK_HOLE: usize = 4;

/// The file every test here seeks in, and a position inside it. 100 is not a
/// power of two, so a position confused with a block count would show.
const THE_SIZE: usize = 100;
const INSIDE_IT: i64 = 42;

/// A descriptor no process here holds, so it is closed by construction.
const A_CLOSED_FD: i32 = 100;

/// A process holding one `THE_SIZE`-byte file of its own, and its descriptor.
fn a_process_with_a_file(pid: KoID, name: &str) -> (Arc<Process>, i32) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let inode = lp
        .root_inode()
        .create("seekable", FileType::File, 0o644)
        .unwrap();
    inode.resize(THE_SIZE).unwrap();
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, String::from("/seekable")))
        .unwrap()
        .into();
    (
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap(),
        fd,
    )
}

/// A process holding the read end of a pipe, which is a `FileLike` and not a
/// `File`.
fn a_process_with_a_pipe(pid: KoID, name: &str) -> (Arc<Process>, i32) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let (read, write) = Pipe::create_pair();
    // The write end stays in the table too: a pipe with no writer hangs up,
    // and nothing here wants that.
    let fd: i32 = lp
        .add_file(File::new(
            Arc::new(read),
            OpenFlags::RDONLY,
            String::from("pipe_r:[]"),
        ))
        .unwrap()
        .into();
    lp.add_file(File::new(
        Arc::new(write),
        OpenFlags::WRONLY,
        String::from("pipe_w:[]"),
    ))
    .unwrap();
    (
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap(),
        fd,
    )
}

/// Where the descriptor is now, asked the only way a caller can: a relative
/// seek of zero.
fn where_it_is(sys: &Syscall<'_>, fd: i32) -> SysResult {
    sys.sys_lseek(FileDesc::from(fd), 0, SEEK_CUR)
}

#[test]
fn a_seek_to_data_through_the_syscall_moves_the_file_offset_there() {
    // The one thing the pure helper cannot be asked: the branch has to SEEK
    // the descriptor to the position it computed, not just answer with it.
    let (proc, fd) = a_process_with_a_file(49_340, "seek-data");
    let (landed, now) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let landed = sys.sys_lseek(FileDesc::from(fd), INSIDE_IT, SEEK_DATA);
            (landed, where_it_is(sys, fd))
        })
    });
    assert_eq!(
        landed,
        Ok(INSIDE_IT as usize),
        "a filesystem without holes reports data wherever it was asked"
    );
    assert_eq!(
        now,
        Ok(INSIDE_IT as usize),
        "the answer was right and the descriptor never moved"
    );
}

#[test]
fn a_seek_to_the_next_hole_lands_at_the_end_of_the_file() {
    // A filesystem with no holes has exactly one, at EOF, which is what
    // Linux reports for a fully allocated file. The offset follows.
    let (proc, fd) = a_process_with_a_file(49_341, "seek-hole");
    let (landed, now) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let landed = sys.sys_lseek(FileDesc::from(fd), INSIDE_IT, SEEK_HOLE);
            (landed, where_it_is(sys, fd))
        })
    });
    assert_eq!(landed, Ok(THE_SIZE), "the only hole is the end of the file");
    assert_eq!(
        now,
        Ok(THE_SIZE),
        "the descriptor did not follow the answer"
    );
}

#[test]
fn a_seek_past_the_end_is_refused_and_leaves_the_offset_where_it_was() {
    // `ENXIO`, and the descriptor must not move: a caller that loops
    // `SEEK_DATA` to walk a sparse file reads from wherever it was left, so
    // an offset moved by a refusal is a silently wrong read.
    let (proc, fd) = a_process_with_a_file(49_342, "seek-past");
    let (parked, refused, negative, now) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let parked = sys.sys_lseek(FileDesc::from(fd), INSIDE_IT, SEEK_DATA);
            let refused = sys.sys_lseek(FileDesc::from(fd), THE_SIZE as i64, SEEK_DATA);
            let negative = sys.sys_lseek(FileDesc::from(fd), -1, SEEK_HOLE);
            (parked, refused, negative, where_it_is(sys, fd))
        })
    });
    assert_eq!(parked, Ok(INSIDE_IT as usize));
    assert_eq!(
        refused,
        Err(LxError::ENXIO),
        "a position at or past the end has no data"
    );
    assert_eq!(negative, Err(LxError::ENXIO), "nor does a negative one");
    assert_eq!(
        now,
        Ok(INSIDE_IT as usize),
        "a refused seek moved the descriptor anyway"
    );
}

#[test]
fn a_seek_to_data_on_something_that_is_not_a_file_is_refused() {
    // The plain whences go through `FileLike::seek`, which works on a
    // descriptor that is not a `File` -- notably the DRM PRIME dma-buf fd
    // that Mesa sizes with `SEEK_END`. This branch cannot: it needs an
    // inode's size, so a descriptor that has no inode is EINVAL. An epoll is
    // the one to hand.
    let (proc, fd) = a_process_with_a_file(49_343, "seek-epoll");
    let (data, hole, a_real_file) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let epfd = sys.sys_epoll_create1(0).unwrap() as i32;
            let data = sys.sys_lseek(FileDesc::from(epfd), 0, SEEK_DATA);
            let hole = sys.sys_lseek(FileDesc::from(epfd), 0, SEEK_HOLE);
            // The control, in the same process and the same call: the branch
            // itself works, so the refusal above is about the descriptor.
            let a_real_file = sys.sys_lseek(FileDesc::from(fd), 0, SEEK_DATA);
            (data, hole, a_real_file)
        })
    });
    assert_eq!(data, Err(LxError::EINVAL), "SEEK_DATA needs an inode");
    assert_eq!(hole, Err(LxError::EINVAL), "and so does SEEK_HOLE");
    assert_eq!(
        a_real_file,
        Ok(0),
        "the branch works on a file in the same process"
    );
}

#[test]
fn a_seek_to_data_on_a_pipe_is_espipe_like_every_other_whence() {
    // A pipe IS wrapped in a `File`, so it gets past the EINVAL above and
    // used to reach the generic answer -- computed from an inode size of
    // zero, which puts every position at or past the end, so these two
    // whences answered ENXIO while SEEK_SET, SEEK_CUR and SEEK_END on the
    // same descriptor answered ESPIPE.
    //
    // Linux answers ESPIPE to all five, and the difference matters to a
    // caller that tells the two apart: ESPIPE means "this is a stream, read
    // it as one", and ENXIO means "this is a seekable file and there is no
    // data from here on". A program handed ENXIO for a pipe concludes the
    // pipe is a fully sparse file and stops reading it.
    //
    // All five are asked here, so the answer cannot drift apart again.
    let (proc, fd) = a_process_with_a_pipe(49_345, "seek-pipe");
    let answers = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            [
                sys.sys_lseek(FileDesc::from(fd), 0, SEEK_SET),
                sys.sys_lseek(FileDesc::from(fd), 0, SEEK_CUR),
                sys.sys_lseek(FileDesc::from(fd), 0, SEEK_END),
                sys.sys_lseek(FileDesc::from(fd), 0, SEEK_DATA),
                sys.sys_lseek(FileDesc::from(fd), 0, SEEK_HOLE),
            ]
        })
    });
    for (whence, answer) in [SEEK_SET, SEEK_CUR, SEEK_END, SEEK_DATA, SEEK_HOLE]
        .iter()
        .zip(answers.iter())
    {
        assert_eq!(
            *answer,
            Err(LxError::ESPIPE),
            "a pipe is not seekable, whichever whence asks: {:?}",
            whence
        );
    }
}

#[test]
fn a_seek_to_data_on_a_closed_descriptor_is_ebadf() {
    // The lookup this branch does for itself, before anything else it
    // decides. EBADF and not EINVAL: the descriptor is the problem.
    let (proc, _fd) = a_process_with_a_file(49_344, "seek-closed");
    let (data, hole) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            (
                sys.sys_lseek(FileDesc::from(A_CLOSED_FD), 0, SEEK_DATA),
                sys.sys_lseek(FileDesc::from(A_CLOSED_FD), 0, SEEK_HOLE),
            )
        })
    });
    assert_eq!(data, Err(LxError::EBADF));
    assert_eq!(hole, Err(LxError::EBADF));
}
