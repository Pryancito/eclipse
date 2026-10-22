//! What the positional calls answer when the file takes less than it was
//! given, which nothing in this tree does.
//!
//! `pwrite` and `pwritev` both carry the POSIX short-write rule -- a file
//! that takes a prefix is reported as having taken that prefix, and a
//! failure after some bytes are in is reported as the count and not as the
//! error -- and neither rule had ever run. The reason is the same one the
//! waiting arms of `write` had: **nothing a test could put behind a
//! descriptor takes less than it is handed**. ramfs writes every byte of
//! every buffer; a pipe refuses a positional write outright with `ESPIPE`;
//! a terminal is not seekable either. So the three arms that decide what a
//! short take means, and the one that decides a refusal with nothing taken,
//! were at zero in both calls.
//!
//! A real filesystem does take less: a disk that fills mid-write, a device
//! whose last block fails, a network filesystem whose server goes away. The
//! difference matters to every caller that writes a buffer in a loop:
//! handed a count it writes the rest from there, handed an error it believes
//! NOTHING went in, and writes bytes the file already holds a second time.
//!
//! The inode here is a double for exactly that, in four shapes. It also
//! reaches the one line of `fallocate` that says what a filesystem claiming
//! to have zeroed nothing means: `EIO`, and not a loop that asks it again
//! for ever.
//!
//! Two of the lines are about a size instead: `pread` keeps a 512-byte
//! buffer on the stack and only reaches for the heap past that, and it stops
//! early when the file answers with less than the chunk it asked for. Every
//! positional read in the crate was short enough to stay on the stack.
//!
//! The short read is the same rule the other way round, and it is the one
//! the loop actually decides: a read that comes back with less than it
//! asked for ends the call. A file that answers short and still has more to
//! give is the only thing that tells the two apart, and ramfs is not it --
//! past its end it answers zero, which the arm above already stops on.
//!
//! Fixed pids live in 49_370-49_380.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use core::any::Any;
use core::sync::atomic::{AtomicUsize, Ordering};
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileSystem, FileType, FsError, Metadata, PollStatus, Timespec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// How much the half-taking double accepts, and how much is offered to it:
/// a count that is neither the whole buffer nor zero, so an answer of
/// either would show.
const IT_TAKES: usize = 100;
const IT_IS_OFFERED: usize = 200;

/// `SYSCALL_IO_MAX`, the chunk a positional write is staged through, and
/// twice it, which is what it takes to have a whole chunk accepted before
/// the next one can fail. Written out rather than read from the constant,
/// which is part of what is under test.
const ONE_WHOLE_CHUNK: usize = 64 * 1024;
const TWO_WHOLE_CHUNKS: usize = 2 * ONE_WHOLE_CHUNK;

/// The size the double reports, big enough that a zeroing `fallocate` of
/// `IT_TAKES` bytes is entirely inside the file and so does not grow it.
const THE_DOUBLE_IS_THIS_BIG: usize = 4096;

/// `FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE`, as userspace writes it:
/// make this range read as zeros and do not change the length.
const ZERO_RANGE_KEEPING_THE_SIZE: usize = 0x10 | 0x01;

/// More than the 512 bytes `pread` keeps on the stack, and more than the
/// file holds, so one call takes both the heap path and the early stop.
const PAST_THE_STACK_BUFFER: usize = 1000;

/// What the file holds for the read test. Ten bytes, so the digit at each
/// offset names it.
const TEN_DIGITS: &[u8; 10] = b"0123456789";

/// How much the double answers a read with, however much is asked of it:
/// neither nothing nor the whole buffer, which is the one shape that tells
/// "stop at the short count" apart from "stop at the end of the file".
const IT_ANSWERS: usize = 7;

/// How the double answers a write.
#[derive(Clone, Copy)]
enum Takes {
    /// A prefix of what it is handed, never more than this many bytes.
    AtMost(usize),
    /// Nothing at all, and says so as a success -- which is what a
    /// filesystem that claims to have zeroed no bytes looks like.
    Nothing,
    /// Refuses every write with a device error.
    Never,
    /// Takes the first write whole and refuses every one after it.
    OnlyTheFirst,
}

/// An inode that takes less than it is given, and counts the writes it was
/// asked for.
struct Picky {
    takes: Takes,
    writes: AtomicUsize,
}

impl Picky {
    fn new(takes: Takes) -> Arc<Self> {
        Arc::new(Picky {
            takes,
            writes: AtomicUsize::new(0),
        })
    }
}

impl rcore_fs::vfs::INode for Picky {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(buf.len().min(IT_ANSWERS))
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        let nth = self.writes.fetch_add(1, Ordering::SeqCst);
        match self.takes {
            Takes::AtMost(most) => Ok(buf.len().min(most)),
            Takes::Nothing => Ok(0),
            Takes::Never => Err(FsError::DeviceError),
            Takes::OnlyTheFirst => {
                if nth == 0 {
                    Ok(buf.len())
                } else {
                    Err(FsError::DeviceError)
                }
            }
        }
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 1,
            size: THE_DOUBLE_IS_THIS_BIG,
            blk_size: 4096,
            blocks: THE_DOUBLE_IS_THIS_BIG / 4096,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        RamFS::new()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A process whose one descriptor is open on the double, with `how` as its
/// access mode.
fn a_process_with(pid: KoID, name: &str, takes: Takes, how: OpenFlags) -> (Arc<Process>, i32) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let fd: i32 = lp
        .add_file(File::new(Picky::new(takes), how, String::from("/picky")))
        .unwrap()
        .into();
    (
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap(),
        fd,
    )
}

/// The same, writable, which is what every write below asks for.
fn a_process_writing_to(pid: KoID, name: &str, takes: Takes) -> (Arc<Process>, i32) {
    a_process_with(pid, name, takes, OpenFlags::WRONLY)
}

/// `struct iovec` as userspace lays it out, the same two words
/// `positional_io_tests` builds and holds against the kernel's layout.
#[repr(C)]
struct AnIoVec {
    base: usize,
    len: usize,
}

fn an_iovec(buf: &[u8]) -> AnIoVec {
    AnIoVec {
        base: buf.as_ptr() as usize,
        len: buf.len(),
    }
}

#[test]
fn a_positional_write_of_nothing_still_reaches_the_file_so_it_can_be_refused() {
    // `pwrite(fd, buf, 0)` has its own path, and the comment on it says why:
    // zero bytes still has to reach the file, because that is where a
    // descriptor not open for writing answers EBADF. Short-circuiting it to
    // `Ok(0)` would make a write probe on a read-only descriptor succeed.
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let dir = lp
        .root_inode()
        .create("shortwzero", FileType::Dir, 0o755)
        .unwrap();
    let inode = dir.create("f", FileType::File, 0o644).unwrap();
    let readable: i32 = lp
        .add_file(File::new(
            inode.clone(),
            OpenFlags::RDONLY,
            String::from("/shortwzero/f"),
        ))
        .unwrap()
        .into();
    let writable: i32 = lp
        .add_file(File::new(
            inode,
            OpenFlags::WRONLY,
            String::from("/shortwzero/f"),
        ))
        .unwrap()
        .into();
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, 49_370, "pwrite-zero", lp).unwrap();
    let (refused, allowed) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nothing: [u8; 0] = [];
            let at = nothing.as_ptr() as usize;
            let refused = sys.sys_pwrite(FileDesc::from(readable), UserInPtr::from(at), 0, 0);
            let allowed = sys.sys_pwrite(FileDesc::from(writable), UserInPtr::from(at), 0, 0);
            (refused, allowed)
        })
    });
    assert_eq!(
        refused,
        Err(LxError::EBADF),
        "a write of nothing through a read-only descriptor was allowed"
    );
    assert_eq!(
        allowed,
        Ok(0),
        "a write of nothing through a writable descriptor was refused"
    );
}

#[test]
fn a_positional_write_the_file_only_partly_takes_reports_what_went_in() {
    // POSIX's short write: the count, not the whole buffer and not an error.
    // A caller handed 200 for a file that took 100 writes the next 100 bytes
    // from the wrong place in its buffer.
    let (proc, fd) = a_process_writing_to(49_371, "pwrite-partial", Takes::AtMost(IT_TAKES));
    let written = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'p'; IT_IS_OFFERED];
            sys.sys_pwrite(
                FileDesc::from(fd),
                UserInPtr::from(bytes.as_ptr() as usize),
                IT_IS_OFFERED,
                0,
            )
        })
    });
    assert_eq!(
        written,
        Ok(IT_TAKES),
        "a positional write did not report the prefix the file took"
    );
}

#[test]
fn a_positional_write_the_file_refuses_outright_is_the_error_and_not_a_count() {
    // The other side of the same decision: with nothing taken there is no
    // count to report, so the file's own errno is the answer. Reporting
    // `Ok(0)` instead would look like a full disk to a caller that then
    // retries for ever.
    let (proc, fd) = a_process_writing_to(49_372, "pwrite-refused", Takes::Never);
    let refused = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'r'; IT_IS_OFFERED];
            sys.sys_pwrite(
                FileDesc::from(fd),
                UserInPtr::from(bytes.as_ptr() as usize),
                IT_IS_OFFERED,
                0,
            )
        })
    });
    assert_eq!(
        refused,
        Err(LxError::EIO),
        "a positional write that took nothing hid the file's error"
    );
}

#[test]
fn a_positional_write_that_fails_after_a_whole_chunk_reports_the_chunk() {
    // Two trips through the staging buffer, with the second refused. The
    // bytes of the first are in the file, so the answer is their count: the
    // error will come back on the caller's next write, and reporting it now
    // would have the caller resend what the file already has.
    let (proc, fd) = a_process_writing_to(49_373, "pwrite-mid", Takes::OnlyTheFirst);
    let written = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'm'; TWO_WHOLE_CHUNKS];
            sys.sys_pwrite(
                FileDesc::from(fd),
                UserInPtr::from(bytes.as_ptr() as usize),
                TWO_WHOLE_CHUNKS,
                0,
            )
        })
    });
    assert_eq!(
        written,
        Ok(ONE_WHOLE_CHUNK),
        "a positional write that failed mid-stream did not report the bytes already taken"
    );
}

#[test]
fn a_gather_positional_write_the_file_only_partly_takes_reports_what_went_in() {
    // The same three decisions again in `pwritev`, which carries its own
    // copy of them. Nothing shared decides this, so each call has to be
    // asked.
    let (proc, fd) = a_process_writing_to(49_374, "pwritev-partial", Takes::AtMost(IT_TAKES));
    let written = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'g'; IT_IS_OFFERED];
            let set = [an_iovec(&bytes)];
            sys.sys_pwritev(
                FileDesc::from(fd),
                UserInPtr::from(set.as_ptr() as usize),
                1,
                0,
            )
        })
    });
    assert_eq!(
        written,
        Ok(IT_TAKES),
        "a gather positional write did not report the prefix the file took"
    );
}

#[test]
fn a_gather_positional_write_that_refuses_outright_is_the_error_and_not_a_count() {
    let (proc, fd) = a_process_writing_to(49_375, "pwritev-refused", Takes::Never);
    let refused = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'g'; IT_IS_OFFERED];
            let set = [an_iovec(&bytes)];
            sys.sys_pwritev(
                FileDesc::from(fd),
                UserInPtr::from(set.as_ptr() as usize),
                1,
                0,
            )
        })
    });
    assert_eq!(
        refused,
        Err(LxError::EIO),
        "a gather positional write that took nothing hid the file's error"
    );
}

#[test]
fn a_gather_positional_write_that_fails_after_a_whole_chunk_reports_the_chunk() {
    let (proc, fd) = a_process_writing_to(49_376, "pwritev-mid", Takes::OnlyTheFirst);
    let written = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let first = vec![b'1'; ONE_WHOLE_CHUNK];
            let second = vec![b'2'; ONE_WHOLE_CHUNK];
            let set = [an_iovec(&first), an_iovec(&second)];
            sys.sys_pwritev(
                FileDesc::from(fd),
                UserInPtr::from(set.as_ptr() as usize),
                2,
                0,
            )
        })
    });
    assert_eq!(
        written,
        Ok(ONE_WHOLE_CHUNK),
        "a gather positional write that failed mid-stream did not report the bytes already taken"
    );
}

#[test]
fn a_fallocate_whose_filesystem_zeroes_nothing_is_an_io_error_and_not_a_spin() {
    // `FALLOC_FL_ZERO_RANGE` writes the zeros itself where the filesystem
    // has no extent map to punch. The loop advances by what each write
    // takes, so a filesystem that claims to have taken none would spin
    // under the caller for ever: the line under test is the one that calls
    // that an I/O error instead.
    let (proc, fd) = a_process_writing_to(49_377, "fallocate-nothing", Takes::Nothing);
    let answered = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            sys.sys_fallocate(FileDesc::from(fd), ZERO_RANGE_KEEPING_THE_SIZE, 0, IT_TAKES)
        })
    });
    assert_eq!(
        answered,
        Err(LxError::EIO),
        "a fallocate whose zeroing wrote nothing did not answer EIO"
    );
}

#[test]
fn a_positional_read_past_the_stack_buffer_stops_at_the_end_of_the_file() {
    // Two lines in one call: a read of more than 512 bytes takes the heap
    // buffer instead of the one on the stack, and a file that answers with
    // less than the chunk asked for ends the loop instead of being asked
    // again at a position past its end. Every positional read in the crate
    // until now was four bytes.
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let dir = lp
        .root_inode()
        .create("shortwread", FileType::Dir, 0o755)
        .unwrap();
    let inode = dir.create("f", FileType::File, 0o644).unwrap();
    let fd: i32 = lp
        .add_file(File::new(
            inode,
            OpenFlags::RDWR,
            String::from("/shortwread/f"),
        ))
        .unwrap()
        .into();
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, 49_378, "pread-short", lp).unwrap();
    let (put, got, what) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(fd);
            let put = sys.sys_pwrite(
                fd,
                UserInPtr::from(TEN_DIGITS.as_ptr() as usize),
                TEN_DIGITS.len(),
                0,
            );
            let mut out = vec![0u8; PAST_THE_STACK_BUFFER];
            let got = sys
                .sys_pread(
                    fd,
                    UserOutPtr::from(out.as_mut_ptr() as usize),
                    PAST_THE_STACK_BUFFER,
                    0,
                )
                .await;
            (put, got, out)
        })
    });
    assert_eq!(put, Ok(TEN_DIGITS.len()), "the ten digits did not go in");
    assert_eq!(
        got,
        Ok(TEN_DIGITS.len()),
        "a long read of a short file did not stop at its end"
    );
    assert_eq!(
        &what[..TEN_DIGITS.len()],
        &TEN_DIGITS[..],
        "the heap path copied out the wrong bytes"
    );
    assert!(
        what[TEN_DIGITS.len()..].iter().all(|b| *b == 0),
        "the read wrote past the end of the file into the caller's buffer"
    );
}

#[test]
fn a_positional_write_the_file_takes_none_of_is_a_count_of_nothing_and_not_a_spin() {
    // The third answer a file can give: no error, and no bytes either. The
    // loop advances by what each write takes, so the arm that stops here is
    // what keeps a `pwrite` from asking the same question for ever. Zero is
    // the honest answer, and it is the caller's own loop that ends on it.
    let (proc, fd) = a_process_writing_to(49_379, "pwrite-none", Takes::Nothing);
    let written = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let bytes = vec![b'n'; IT_IS_OFFERED];
            sys.sys_pwrite(
                FileDesc::from(fd),
                UserInPtr::from(bytes.as_ptr() as usize),
                IT_IS_OFFERED,
                0,
            )
        })
    });
    assert_eq!(
        written,
        Ok(0),
        "a positional write the file took none of did not answer a count of nothing"
    );
}

#[test]
fn a_positional_read_the_file_answers_short_stops_instead_of_asking_again() {
    // A read that comes back with less than it asked for has had everything
    // the file has to give now, so the call ends there. Asking again
    // instead turns one `pread` of a device or of a network file into as
    // many as it takes to fill the buffer, which is the very thing the
    // caller picked the length to bound. The double answers seven bytes
    // however much is asked, so the two behaviours are 7 and 1000.
    let (proc, fd) = a_process_with(
        49_380,
        "pread-short-answer",
        Takes::Nothing,
        OpenFlags::RDONLY,
    );
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut out = vec![0u8; PAST_THE_STACK_BUFFER];
            sys.sys_pread(
                FileDesc::from(fd),
                UserOutPtr::from(out.as_mut_ptr() as usize),
                PAST_THE_STACK_BUFFER,
                0,
            )
            .await
        })
    });
    assert_eq!(
        got,
        Ok(IT_ANSWERS),
        "a positional read did not stop at the short count the file gave"
    );
}
