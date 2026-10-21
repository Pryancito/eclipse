//! What `read` and `write` do past the first chunk, and when the file says no.
//!
//! Both calls are loops: the request is cut into chunks of at most
//! `SYSCALL_IO_MAX` (64 KiB) and the loop keeps going until the request is
//! spent. Every test in the crate asked them for less than one chunk, so the
//! loop never went round twice and none of the decisions it makes on the way
//! was ever taken:
//!
//! - **`is_seekable`**, which decides whether a read refills the buffer past
//!   the first chunk. It is computed only when `len > chunk_size`, from the
//!   file's TYPE -- a regular file or a block device yes, anything else no --
//!   so a `read(fd, buf, 70000)` on a file either comes back with 70000 bytes
//!   or with 65536, and nothing else in the kernel can tell you which.
//! - **The POSIX partial-write rule**: a chunk that fails AFTER earlier
//!   chunks went in must report the bytes already consumed and not the error,
//!   because a caller told "error" resends data the file already took. The
//!   same errno, on the same file, means two different things depending on
//!   how far the loop got, and nothing asked for either.
//! - **The `SIGPIPE` that goes with `EPIPE`**, and only from a pipe or a
//!   socket. ALSA uses `-EPIPE` to mean an underrun and no signal comes with
//!   it, so the signal is keyed on the fd being a stream, not on the errno.
//!   Without it, `yes | head -1` ran `yes` for ever.
//!
//! The inodes here are written to answer in ways no filesystem in the tree
//! answers: one serves 70000 bytes so the loop has somewhere to go, one takes
//! a first chunk and then refuses, and one reports a broken pipe without
//! being one. That is the only way to put a number on each side of these
//! decisions -- ramfs takes everything it is given and never short-writes,
//! which is why a suite built on it cannot see any of this.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use core::any::Any;
use core::sync::atomic::{AtomicUsize, Ordering};
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use linux_object::signal::Signal;
use rcore_fs::vfs::{FileSystem, FileType, FsError, Metadata, PollStatus, Timespec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `SYSCALL_IO_MAX`, the chunk these loops cut a request into. Written as the
/// number rather than taken from the crate, because what this module is about
/// is the second time round the loop and that is what the number decides.
const ONE_CHUNK: usize = 64 * 1024;

/// A request that does not fit in one chunk, and the bytes left over for the
/// second: 70000 = 65536 + 4464.
const PAST_ONE_CHUNK: usize = 70_000;
const THE_SECOND_CHUNK: usize = PAST_ONE_CHUNK - ONE_CHUNK;

/// The stack buffer `read` keeps for line-oriented callers, and one byte
/// either side of it: below it the bytes come off the stack, above it off the
/// heap, and both have to deliver the same bytes.
const THE_STACK_BUFFER: usize = 512;

/// A descriptor this harness never hands out.
const A_CLOSED_FD: usize = 100;

/// What the byte at `offset` is, so any offset can be checked against the
/// answer without carrying 70000 bytes of expectation around. 251 is prime
/// and not a power of two, so no chunk boundary lands on a repeat.
fn byte_at(offset: usize) -> u8 {
    (offset % 251) as u8
}

/// A file of `size` bytes that serves [`byte_at`] and reports whatever file
/// type it was built with -- which is the whole input to `is_seekable`.
struct Deep {
    kind: FileType,
    size: usize,
}

/// A file that takes `takes` bytes and then refuses with `Again` (`EAGAIN`),
/// or that answers every write with `Ok(0)`.
struct Fickle {
    takes: usize,
    taken: AtomicUsize,
    /// Answer `Ok(0)` to every write instead of refusing: the short write
    /// that would spin the loop for ever without its own guard.
    always_zero: bool,
    /// Answer `Broken` (`EPIPE`) rather than `Again`, to ask whether the
    /// signal is keyed on the errno or on the kind of descriptor.
    broken: bool,
}

fn metadata_of(kind: FileType, size: usize) -> Metadata {
    Metadata {
        dev: 0,
        inode: 1,
        size,
        blk_size: 4096,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: kind,
        mode: 0o644,
        nlinks: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
    }
}

impl rcore_fs::vfs::INode for Deep {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        let left = self.size.saturating_sub(offset);
        let n = buf.len().min(left);
        for (i, slot) in buf[..n].iter_mut().enumerate() {
            *slot = byte_at(offset + i);
        }
        Ok(n)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(buf.len())
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
        Ok(metadata_of(self.kind, self.size))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        RamFS::new()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

impl rcore_fs::vfs::INode for Fickle {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        if self.always_zero {
            return Ok(0);
        }
        let already = self.taken.load(Ordering::SeqCst);
        if already >= self.takes {
            return Err(if self.broken {
                FsError::Broken
            } else {
                FsError::Again
            });
        }
        let n = buf.len().min(self.takes - already);
        self.taken.store(already + n, Ordering::SeqCst);
        Ok(n)
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
        Ok(metadata_of(FileType::File, 0))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        RamFS::new()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A process with a filesystem of its own.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A fresh pointer at `bytes`, because `UserInPtr` is not `Copy` and these
/// tests hand the same buffer to one call after another.
fn reading(bytes: &[u8]) -> UserInPtr<u8> {
    UserInPtr::from(bytes.as_ptr() as usize)
}

/// A descriptor on `inode`, opened as `flags` says.
fn opened(lp: &LinuxProcess, inode: Arc<dyn rcore_fs::vfs::INode>, flags: OpenFlags) -> FileDesc {
    let fd: i32 = lp
        .add_file(File::new(inode, flags, String::from("/made-up")))
        .unwrap()
        .into();
    FileDesc::from(fd)
}

#[test]
fn a_read_past_one_chunk_goes_round_again_only_for_a_file_it_can_seek() {
    // `is_seekable` is read once, at the bottom of the loop, and it is the
    // only thing that decides whether a 70000-byte request comes back with
    // 70000 bytes or with 65536. It is computed from the file's TYPE, so the
    // four types here are the four answers: a regular file and a block
    // device go round again, a char device and a fifo do not.
    //
    // The bytes are checked at the far end as well as the near one, because
    // the second chunk is written at `base.add(read_len)` and a chunk that
    // landed back at the start of the buffer would still count 70000.
    let proc = a_process(48_230, "deepread");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let mut buf = vec![0u8; PAST_ONE_CHUNK];

            for kind in [FileType::File, FileType::BlockDevice] {
                let fd = opened(
                    &lp,
                    Arc::new(Deep {
                        kind,
                        size: PAST_ONE_CHUNK,
                    }),
                    OpenFlags::RDWR,
                );
                buf.iter_mut().for_each(|b| *b = 0);
                assert_eq!(
                    sys.sys_read(
                        fd,
                        UserOutPtr::from(buf.as_mut_ptr() as usize),
                        PAST_ONE_CHUNK
                    )
                    .await,
                    Ok(PAST_ONE_CHUNK),
                    "{:?} stopped after one chunk",
                    kind
                );
                assert_eq!(
                    (
                        buf[0],
                        buf[ONE_CHUNK - 1],
                        buf[ONE_CHUNK],
                        buf[PAST_ONE_CHUNK - 1]
                    ),
                    (
                        byte_at(0),
                        byte_at(ONE_CHUNK - 1),
                        byte_at(ONE_CHUNK),
                        byte_at(PAST_ONE_CHUNK - 1)
                    ),
                    "the second chunk did not land after the first ({:?})",
                    kind
                );
            }

            for kind in [FileType::CharDevice, FileType::NamedPipe] {
                let fd = opened(
                    &lp,
                    Arc::new(Deep {
                        kind,
                        size: PAST_ONE_CHUNK,
                    }),
                    OpenFlags::RDWR,
                );
                buf.iter_mut().for_each(|b| *b = 0);
                assert_eq!(
                    sys.sys_read(
                        fd,
                        UserOutPtr::from(buf.as_mut_ptr() as usize),
                        PAST_ONE_CHUNK
                    )
                    .await,
                    Ok(ONE_CHUNK),
                    "{:?} kept reading past one chunk",
                    kind
                );
                assert_eq!(
                    (buf[ONE_CHUNK - 1], buf[ONE_CHUNK]),
                    (byte_at(ONE_CHUNK - 1), 0),
                    "one chunk was not exactly one chunk ({:?})",
                    kind
                );
            }
        })
    });
}

#[test]
fn the_stack_buffer_and_the_heap_one_deliver_the_same_bytes() {
    // A request of 512 bytes or fewer is served from a buffer on the stack
    // and anything larger from the heap, which is invisible from outside --
    // until the boundary is wrong, and then it is a slice out of range on
    // one side or a zero-length buffer on the other. Three lengths, one on
    // each side of 512 and one exactly on it.
    let proc = a_process(48_231, "bufsize");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = opened(
                &lp,
                Arc::new(Deep {
                    kind: FileType::File,
                    size: PAST_ONE_CHUNK,
                }),
                OpenFlags::RDWR,
            );

            for len in [THE_STACK_BUFFER - 1, THE_STACK_BUFFER, THE_STACK_BUFFER + 1] {
                let mut buf = vec![0u8; len + 1];
                assert_eq!(sys.sys_lseek(fd, 0, 0), Ok(0));
                assert_eq!(
                    sys.sys_read(fd, UserOutPtr::from(buf.as_mut_ptr() as usize), len)
                        .await,
                    Ok(len),
                    "a read of {} bytes came back short",
                    len
                );
                assert_eq!(
                    (buf[0], buf[len - 1], buf[len]),
                    (byte_at(0), byte_at(len - 1), 0),
                    "a read of {} bytes delivered the wrong bytes, or one too many",
                    len
                );
            }
        })
    });
}

#[test]
fn a_chunk_refused_after_earlier_ones_went_in_reports_the_bytes_and_not_the_error() {
    // The POSIX partial-write rule, and the whole point of it: the SAME
    // errno from the SAME file means "nothing happened, try again" at offset
    // zero and "this much went in" once something has. A caller told
    // "error" after a partial write resends bytes the file already took.
    let proc = a_process(48_232, "partial");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let bytes = vec![b'z'; PAST_ONE_CHUNK];

            // Takes the first chunk, refuses the second.
            let partial = opened(
                &lp,
                Arc::new(Fickle {
                    takes: ONE_CHUNK,
                    taken: AtomicUsize::new(0),
                    always_zero: false,
                    broken: false,
                }),
                OpenFlags::RDWR,
            );
            assert_eq!(
                sys.sys_write(partial, reading(&bytes), PAST_ONE_CHUNK)
                    .await,
                Ok(ONE_CHUNK),
                "a refusal after the first chunk was reported as an error"
            );

            // Refuses the first, so there is nothing to report but the error.
            let refuses = opened(
                &lp,
                Arc::new(Fickle {
                    takes: 0,
                    taken: AtomicUsize::new(0),
                    always_zero: false,
                    broken: false,
                }),
                OpenFlags::RDWR,
            );
            assert_eq!(
                sys.sys_write(refuses, reading(&bytes), PAST_ONE_CHUNK)
                    .await,
                Err(LxError::EAGAIN),
                "a refusal with nothing written was reported as a short write"
            );

            // It is the bytes written that decide and not the length asked
            // for: the same file, asked for less than one chunk, refuses.
            let again = opened(
                &lp,
                Arc::new(Fickle {
                    takes: 0,
                    taken: AtomicUsize::new(0),
                    always_zero: false,
                    broken: false,
                }),
                OpenFlags::RDWR,
            );
            assert_eq!(
                sys.sys_write(again, reading(&bytes), 10).await,
                Err(LxError::EAGAIN)
            );

            // A file that keeps answering `Ok(0)` would spin the loop for
            // ever; the loop stops and reports what it has.
            let zero = opened(
                &lp,
                Arc::new(Fickle {
                    takes: 0,
                    taken: AtomicUsize::new(0),
                    always_zero: true,
                    broken: false,
                }),
                OpenFlags::RDWR,
            );
            assert_eq!(
                sys.sys_write(zero, reading(&bytes), PAST_ONE_CHUNK).await,
                Ok(0),
                "a file that takes nothing was not reported as a short write"
            );

            // And the chunk really is 64 KiB: a second chunk was asked for
            // above, which is what the first assertion stands on.
            assert_eq!(THE_SECOND_CHUNK, 4464);
        })
    });
}

#[test]
fn the_signal_that_goes_with_a_broken_pipe_goes_only_with_a_stream() {
    // `SIGPIPE` is keyed on the fd being a pipe or a socket, not on the
    // errno: ALSA answers `-EPIPE` to mean an underrun and no signal comes
    // with that. So the two halves here are the same errno from two kinds of
    // descriptor, and they are asked in this order because a signal, once
    // pending, stays pending -- the quiet one has to go first.
    let proc = a_process(48_233, "sigpipe");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let byte = [b'x'; 1];

            assert!(
                !sys.thread.lock_linux().signals.contains(Signal::SIGPIPE),
                "this thread came with SIGPIPE already pending"
            );

            // An ordinary file reporting a broken pipe: the errno goes
            // through and the signal does not.
            let device = opened(
                &lp,
                Arc::new(Fickle {
                    takes: 0,
                    taken: AtomicUsize::new(0),
                    always_zero: false,
                    broken: true,
                }),
                OpenFlags::RDWR,
            );
            assert_eq!(
                sys.sys_write(device, reading(&byte), 1).await,
                Err(LxError::EPIPE)
            );
            assert!(
                !sys.thread.lock_linux().signals.contains(Signal::SIGPIPE),
                "a file that is not a stream raised SIGPIPE with its EPIPE"
            );

            // A pipe whose reader has gone: both.
            let mut fds = [0_i32; 2];
            assert_eq!(
                sys.sys_pipe2(UserOutPtr::from(fds.as_mut_ptr() as usize), 0),
                Ok(0)
            );
            let (r, w) = (FileDesc::from(fds[0]), FileDesc::from(fds[1]));
            assert_eq!(sys.sys_close(r), Ok(0));
            assert_eq!(
                sys.sys_write(w, reading(&byte), 1).await,
                Err(LxError::EPIPE),
                "a write to a pipe with no reader was accepted"
            );
            assert!(
                sys.thread.lock_linux().signals.contains(Signal::SIGPIPE),
                "a writer whose reader has gone was not sent SIGPIPE, so \
                 `yes | head -1` runs for ever"
            );
        })
    });
}

#[test]
fn the_two_reasons_a_write_answers_ebadf_and_the_pointer_it_needs() {
    // `write(2)` answers `EBADF` for two unrelated reasons -- the descriptor
    // is not in the table, or it is there and not open for writing -- and
    // the kernel's own diagnostics exist to tell them apart. Both come back
    // as the same errno, which is why both are asked for here: a check that
    // covered only one of them would miss the other entirely.
    let proc = a_process(48_234, "ebadfwrite");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let byte = [b'x'; 4];
            let readable = opened(
                &lp,
                Arc::new(Deep {
                    kind: FileType::File,
                    size: PAST_ONE_CHUNK,
                }),
                OpenFlags::RDONLY,
            );
            let writable = opened(
                &lp,
                Arc::new(Deep {
                    kind: FileType::File,
                    size: PAST_ONE_CHUNK,
                }),
                OpenFlags::WRONLY,
            );

            assert_eq!(
                sys.sys_write(
                    FileDesc::from(A_CLOSED_FD as i32),
                    UserInPtr::from(byte.as_ptr() as usize),
                    4
                )
                .await,
                Err(LxError::EBADF),
                "a descriptor nobody has was written to"
            );
            assert_eq!(
                sys.sys_write(readable, UserInPtr::from(byte.as_ptr() as usize), 4)
                    .await,
                Err(LxError::EBADF),
                "a descriptor opened read-only took a write"
            );
            let mut four = [0u8; 4];
            assert_eq!(
                sys.sys_read(writable, UserOutPtr::from(four.as_mut_ptr() as usize), 4)
                    .await,
                Err(LxError::EBADF),
                "a descriptor opened write-only was read"
            );

            // The bytes have to come from somewhere and go somewhere: a null
            // pointer is refused rather than taken for an empty buffer.
            assert_eq!(
                sys.sys_write(writable, UserInPtr::from(0usize), 4).await,
                Err(LxError::EFAULT),
                "a write read its bytes from nowhere"
            );
            assert_eq!(
                sys.sys_read(readable, UserOutPtr::from(0usize), 4).await,
                Err(LxError::EFAULT),
                "a read put its bytes nowhere"
            );

            // Asking for nothing touches no pointer at all, so a null one is
            // not an error: the loop never goes round.
            assert_eq!(
                sys.sys_read(readable, UserOutPtr::from(0usize), 0).await,
                Ok(0),
                "a zero-length read looked at the pointer it was not given"
            );
            assert_eq!(
                sys.sys_write(writable, UserInPtr::from(0usize), 0).await,
                Ok(0),
                "a zero-length write looked at the pointer it was not given"
            );
        })
    });
}
