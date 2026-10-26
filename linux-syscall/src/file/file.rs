//! File operations
//!
//! - read, pread, readv
//! - write, pwrite, writev
//! - lseek
//! - truncate, ftruncate
//! - sendfile, copy_file_range
//! - sync, fsync, fdatasync
//! - ioctl, fcntl
//! - access, faccessat

use super::*;
use linux_object::error::LxResult;
use linux_object::fs::ioctl::{
    FIOASYNC, FIOCLEX, FIONBIO, FIONCLEX, FIONREAD, FIOQSIZE, TCGETS2, TCSETS2, TCSETSF2, TCSETSW2,
    TIOCGPTPEER, TIOCGWINSZ, TIOCSCTTY,
};
use linux_object::{process::FsInfo, thread::ThreadExt, time::TimeSpec};
use rcore_fs::vfs::INode;

/// POSIX record locks, classic and open-file-description flavours, as
/// `fcntl(2)` numbers them.
///
/// They are at module level, and not inside `sys_fcntl` where they were
/// declared, because this is where it is decided whether a lock command
/// waits -- `F_SETLKW` and `F_OFD_SETLKW` do -- and `restart` has to report
/// on exactly that set. A constant in a method body cannot be named from
/// anywhere, so `restart` had its own copy of 38, free to be renumbered on
/// its own while this one went on deciding: the call that really waits would
/// then come back `EINTR` with `SA_RESTART` installed, which is the one
/// thing that file exists to prevent.
pub(crate) const F_GETLK: usize = 5;
/// `F_SETLK`: take a classic lock or fail; it answers `EAGAIN` and never
/// waits.
pub(crate) const F_SETLK: usize = 6;
/// `F_SETLKW`: take a classic lock, waiting for it. Cut short, it is
/// `-ERESTARTSYS`.
pub(crate) const F_SETLKW: usize = 7;
/// `F_OFD_GETLK`: ask about an open-file-description lock.
pub(crate) const F_OFD_GETLK: usize = 36;
/// `F_OFD_SETLK`: take one or fail, as `F_SETLK` does.
pub(crate) const F_OFD_SETLK: usize = 37;
/// `F_OFD_SETLKW`: take one, waiting for it, as `F_SETLKW` does.
pub(crate) const F_OFD_SETLKW: usize = 38;

/// Land one entry in the calling process's DRM ioctl trail for a call this
/// layer answered itself.
///
/// The three fd-table families (PRIME, syncobj export/import, eventfd) return
/// straight from `sys_ioctl` and never reach `drm_ioctl`, which is where every
/// other DRM call is recorded. Without this they would be a hole in the trail
/// precisely where a Vulkan driver gets the objects it later dereferences --
/// see `linux_object::fs::devfs::drm_trail`.
fn trail_drm(cmd: u32, arg1: usize, ret: i64) {
    linux_object::fs::devfs::drm_scheme::trail_record(cmd, arg1, ret);
}

/// Largest staging chunk that is served from the stack; anything bigger goes
/// through the heap. Same ceiling the read path uses.
const STAGING_STACK_BUF: usize = 512;

/// A kernel-side staging buffer of exactly `chunk` bytes, on the stack while it
/// is small and on the heap otherwise.
///
/// EVERY byte a write syscall hands to a file passes through one of these. The
/// bytes MUST be copied out of user memory before they reach `write`/
/// `write_at`, because a filesystem takes its own lock around that call and
/// then *reads the buffer under it*. If the buffer still points into user
/// memory, that read can touch a page that is not resident yet, and the page
/// fault is served by `FileFrameFiller`, which calls `INode::metadata()` and
/// `INode::read_at()` — back into the same filesystem, for a lock THIS cpu is
/// already holding. A ticket mutex is not re-entrant, so the cpu spins on
/// itself for ever and every other cpu queues behind it.
///
/// That is the btrfs `KERNEL STOP` seen on real hardware: `HOLDER cpu=3 at
/// btrfs_mount.rs:504` (the `inner` lock taken by `write_at`) with the same
/// `cpu=3` spinning at `btrfs_mount.rs:576` (`metadata()`, reached from the
/// fault) and six more cpus piled up behind it. Not an AB-BA cycle: one lock,
/// one cpu, taken twice.
///
/// `writev`/`pwritev` always staged their bytes this way; `write`/`pwrite` were
/// the two that handed the raw user slice straight through.
struct Staging {
    stack: [u8; STAGING_STACK_BUF],
    heap: alloc::vec::Vec<u8>,
    len: usize,
}

impl Staging {
    /// A buffer for chunks of `chunk` bytes. `ENOMEM` rather than a kernel
    /// abort when the heap cannot hold it (`try_zeroed_buf`).
    fn new(chunk: usize) -> LxResult<Self> {
        Ok(Staging {
            stack: [0u8; STAGING_STACK_BUF],
            heap: if chunk > STAGING_STACK_BUF {
                crate::try_zeroed_buf(chunk)?
            } else {
                alloc::vec::Vec::new()
            },
            len: chunk,
        })
    }

    /// The whole buffer, `chunk` bytes long.
    fn buf(&mut self) -> &mut [u8] {
        if self.len > STAGING_STACK_BUF {
            &mut self.heap[..]
        } else {
            &mut self.stack[..self.len]
        }
    }
}

/// SEEK_DATA (3) and SEEK_HOLE (4) on a filesystem that cannot tell holes
/// from data, as `generic_file_llseek_size` answers them: the whole file
/// is data and the end of file is the only hole.
fn seek_data_hole_generic(
    size: u64,
    whence: usize,
    offset: i64,
) -> linux_object::error::LxResult<u64> {
    const SEEK_DATA: usize = 3;
    if offset < 0 || offset as u64 >= size {
        return Err(LxError::ENXIO);
    }
    Ok(if whence == SEEK_DATA {
        offset as u64
    } else {
        size
    })
}

/// `lseek(2)`'s `whence`, which the syscall declares `int` and this tree read
/// as a `u8`.
///
/// The low byte of 0x100 is 0, so `lseek(fd, off, 0x100)` seeked to `off` from
/// the start of the file instead of answering EINVAL, and the caller then read
/// the wrong bytes with no error anywhere. The same truncation made
/// SEEK_DATA (3) and SEEK_HOLE (4) indistinguishable from nothing.
fn seek_from(whence: usize, offset: i64) -> linux_object::error::LxResult<SeekFrom> {
    const SEEK_SET: usize = 0;
    const SEEK_CUR: usize = 1;
    const SEEK_END: usize = 2;
    match whence {
        SEEK_SET => {
            // `vfs_setpos()`: a negative absolute position is EINVAL. Read as
            // `offset as u64` it became a position near 2^64 instead, which
            // comes back out of the syscall looking like an errno.
            if offset < 0 {
                return Err(LxError::EINVAL);
            }
            Ok(SeekFrom::Start(offset as u64))
        }
        // Negative is legal for both of these: they are relative. Linux checks
        // the *resulting* position, not the argument.
        SEEK_CUR => Ok(SeekFrom::Current(offset)),
        SEEK_END => Ok(SeekFrom::End(offset)),
        _ => Err(LxError::EINVAL),
    }
}

/// `sync_file_range(2)`'s flags (`<linux/fs.h>`).
const SYNC_FILE_RANGE_WAIT_BEFORE: usize = 1;
/// See [`SYNC_FILE_RANGE_WAIT_BEFORE`].
const SYNC_FILE_RANGE_WRITE: usize = 2;
/// See [`SYNC_FILE_RANGE_WAIT_BEFORE`].
const SYNC_FILE_RANGE_WAIT_AFTER: usize = 4;

/// Whether `sync_file_range` accepts this flag word, and if so whether it
/// asks for any write-back at all.
///
/// A bit the call does not have has to be `EINVAL`: that is how a caller finds
/// out this kernel is older than the flag it wanted, and a flag word taken
/// wholesale reports success for work nobody asked for in a way nobody can
/// see. An empty flag word is legal and means "do nothing", which is why the
/// answer is a bool and not just `Ok(())`.
fn sync_file_range_writes_back(flags: usize) -> LxResult<bool> {
    if flags & !(SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER)
        != 0
    {
        return Err(LxError::EINVAL);
    }
    Ok(flags != 0)
}

/// What an EclipseFS ioctl needs from the calling process: its descriptors,
/// its view of the tree and its credentials.
struct FsIoctlCtx<'a> {
    proc: &'a LinuxProcess,
}

impl linux_object::fs::FsIoctlContext for FsIoctlCtx<'_> {
    fn file(&self, fd: i32) -> LxResult<Arc<File>> {
        self.proc.get_file(FileDesc::from(fd))
    }

    fn lookup(&self, path: &str) -> LxResult<Arc<dyn INode>> {
        self.proc.lookup_inode(path)
    }

    fn capable(&self, cap: u32) -> bool {
        self.proc.capable(cap)
    }

    fn euid(&self) -> u32 {
        self.proc.euid()
    }
}

#[cfg(test)]
mod ioctl_number_tests;

#[cfg(test)]
mod permission_tests;

#[cfg(test)]
mod positional_io_tests;

#[cfg(test)]
mod sync_file_range_tests;

#[cfg(test)]
mod wire_layout_tests;

#[cfg(test)]
mod drm_ioctl_layout_tests;

#[cfg(test)]
mod setlkw_tests;

/// Whether an `EINVAL` from the file's own ioctl handler should be reported
/// as `ENOTTY`: the request is in the `0x54xx` terminal block and is not one
/// of the file ioctls that block also carries.
///
/// The block runs from `TCGETS` (0x5401) to [`FIOQSIZE`] (0x5460), which is
/// its last number, so the range is **inclusive**. An exclusive `..` here
/// leaves `FIOQSIZE` out by one, and then the exclusion below has nothing
/// left to do for it.
///
/// The five excluded requests are legal on every kind of fd -- pipes,
/// sockets, regular files -- and are answered at the VFS layer above, so an
/// `EINVAL` from one of them is the file's real answer and not "this is not
/// a terminal".
///
/// Two requests that look like they belong in that list do not.
/// `TIOCOUTQ` (0x5411) is a question about a terminal's output queue, so it
/// stays in the block. And [`FIOQSIZE`] is a VFS file ioctl, but not one
/// that applies to every fd: Linux answers it only for a directory, a
/// non-anonymous regular file or a symlink, and for anything else --- a
/// pipe, a socket --- it returns `ENOTTY` itself (`fs/ioctl.c`,
/// `do_vfs_ioctl`). So the block has to cover it, which is why the range
/// ends `..=FIOQSIZE` and not `..`: the number is the last one in the
/// block, and an exclusive bound would leave a pipe's `FIOQSIZE` answering
/// `EINVAL` where Linux answers `ENOTTY`.
///
/// Every number here is named rather than written out. This used to be a
/// bare `matches!(request, 0x541B | 0x5421 | ...)` whose last arm was
/// `0x545E` -- a number `<asm-generic/ioctls.h>` has never assigned -- while
/// the comment beside it called that arm `FIOQSIZE`, which is 0x5460.
pub(crate) fn tty_block_wants_enotty(request: usize) -> bool {
    ((0x5400..=FIOQSIZE).contains(&request)
        || matches!(request, TCGETS2 | TCSETS2 | TCSETSW2 | TCSETSF2))
        && !matches!(request, FIONREAD | FIONBIO | FIONCLEX | FIOCLEX | FIOASYNC)
}

impl Syscall<'_> {
    /// Reads from a specified file using a file descriptor. Before using this call,
    /// you must first obtain a file descriptor using the opensyscall. Returns bytes read successfully.
    /// - fd – file descriptor
    /// - base – pointer to the buffer to fill with read contents
    /// - len – number of bytes to read
    pub async fn sys_read(&self, fd: FileDesc, base: UserOutPtr<u8>, len: usize) -> SysResult {
        info!("read: fd={:?}, base={:?}, len={:#x}", fd, base, len);
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;

        let chunk_size = len.min(super::SYSCALL_IO_MAX);
        // `is_seekable` only changes the outcome for a multi-chunk request: it
        // decides whether to keep refilling the buffer past one `chunk_size`
        // slice. For a single-chunk read (`len <= chunk_size`, the overwhelming
        // majority) the loop terminates the same way either way, so skip the
        // `metadata()` probe — it would otherwise re-read the inode on every
        // read syscall.
        let is_seekable = if len > chunk_size {
            if let Ok(file) = file_like.clone().downcast_arc::<linux_object::fs::File>() {
                file.metadata()
                    .map(|meta| {
                        meta.type_ == linux_object::fs::vfs::FileType::File
                            || meta.type_ == linux_object::fs::vfs::FileType::BlockDevice
                    })
                    .unwrap_or(false)
            } else {
                false
            }
        } else {
            false
        };
        // Hybrid stack/heap buffer: line-oriented apps (busybox shell, getline,
        // fgetc) drive a stream of small reads — keep those alloc-free. The
        // ~64 KiB ceiling case still goes via the buddy allocator.
        const STACK_BUF: usize = 512;
        let mut stack_buf = [0u8; STACK_BUF];
        let mut heap_buf: alloc::vec::Vec<u8> = if chunk_size > STACK_BUF {
            crate::try_zeroed_buf(chunk_size)?
        } else {
            alloc::vec::Vec::new()
        };
        let buf: &mut [u8] = if chunk_size > STACK_BUF {
            &mut heap_buf[..]
        } else {
            &mut stack_buf[..chunk_size]
        };
        let mut read_len = 0;

        while read_len < len {
            let current_len = (len - read_len).min(chunk_size);
            // [diag dd-efault] name the failing layer: is the EFAULT for a
            // >=4 KiB read produced by the file's read (inode/driver) or by
            // the copy-out below?
            let n = file_like
                .read(&mut buf[..current_len])
                .await
                .inspect_err(|e| {
                    // ONLY the EFAULT this diag hunts. It used to fire on ANY error,
                    // so every non-blocking read that returns EAGAIN -- what every
                    // event loop (labwc/Xwayland/wayland clients, and the oopslog
                    // poller) does thousands of times a second -- wrote a line
                    // through the UNCONDITIONAL klog. That flooded the slow UART
                    // console, stalled each read on the serial write, and starved
                    // the desktop until the session fell over a few seconds in.
                    if matches!(e, LxError::EFAULT) {
                        kernel_hal::klog_info!(
                            "[read-efault] fd={:?} len={:#x} current={:#x}: file_like.read -> {:?}",
                            fd,
                            len,
                            current_len,
                            e
                        );
                    }
                })?;
            if n == 0 {
                break;
            }
            // NOTE: an ETX (0x03) -> SIGINT conversion used to live HERE, keyed
            // only on `fd == 0`. That misfired catastrophically for any program
            // whose stdin is NOT a terminal: piping a binary through a pipeline
            // (`cat /bin/busybox | wc -c`, tar/gzip pipelines, X startup
            // scripts) killed the reader with a spurious SIGINT whenever a read
            // chunk happened to start with byte 0x03. Terminal interrupt
            // handling belongs to the line discipline, and both the VT `Stdin`
            // (termios ISIG, stdio.rs) and the PTY slaves (pty.rs, devfs/pty.rs)
            // already convert VINTR into SIGINT for the foreground pgrp
            // themselves — so the syscall-level check was pure downside and was
            // removed.
            base.add(read_len).write_array(&buf[..n]).inspect_err(|e| {
                // Unguarded is fine here: write_array only fails on a bad user
                // address (the EFAULT this diag hunts), never on a benign/hot
                // EAGAIN, so it cannot flood the way the read path above did.
                kernel_hal::klog_info!(
                    "[read-efault] fd={:?} base={:#x} n={:#x}: write_array -> {:?}",
                    fd,
                    base.as_addr() + read_len,
                    n,
                    e
                );
            })?;
            read_len += n;
            if n < current_len || !is_seekable {
                break;
            }
        }
        Ok(read_len)
    }

    /// Writes to a specified file using a file descriptor. Before using this call,
    /// you must first obtain a file descriptor using the open syscall. Returns bytes written successfully.
    /// - fd – file descriptor
    /// - base – pointer to the buffer write
    /// - len – number of bytes to write
    pub async fn sys_write(&self, fd: FileDesc, base: UserInPtr<u8>, len: usize) -> SysResult {
        info!("write: fd={:?}, base={:?}, len={:#x}", fd, base, len);
        // Diagnostic: surface X-server log/error lines into the dmesg ring so the
        // reason a graphics server aborts is visible even without its logfile.
        // Only stdout/stderr (where Xorg and the dynamic linker print — services
        // dup2 them onto their log files, keeping fd 1/2): the needle
        // substring scan used to run on EVERY write of every fd, taxing the
        // hottest syscall in the system for pipes, sockets and data files that
        // can never carry these markers.
        if <FileDesc as Into<i32>>::into(fd) <= 2 {
            if let Ok(peek) = base.as_slice(len.min(512)) {
                tee_x_diag(peek);
            }
        }
        let proc = self.linux_process();
        // [ebadf-write] dbus-daemon dies with "Writing to pipe: Bad file
        // descriptor" on the fd dbus-launch hands it via --print-address, and
        // the execve CLOEXEC sweep is NOT what closes it. write(2) can answer
        // EBADF for two distinct reasons here, so name which one: the fd is
        // absent from the table, or it is present but its access mode is not
        // writable (File::write_at). Error path only — the fast path is
        // untouched.
        let file_like = proc.get_file_like(fd).inspect_err(|_| {
            kernel_hal::klog_info!(
                "[ebadf-write] fd={:?} ABSENT from the fd table (proc={:?}, live fds={:?})",
                fd,
                proc.execute_path(),
                proc.get_files().map(|f| {
                    let mut v: alloc::vec::Vec<i32> = f.keys().map(|k| (*k).into()).collect();
                    v.sort_unstable();
                    v
                })
            );
        })?;
        let chunk_size = len.min(super::SYSCALL_IO_MAX);
        let waits = self.waits_for_room(&file_like);
        // The bytes are copied into kernel memory before the file sees them;
        // see `Staging` for the deadlock that a raw user slice caused here.
        let mut staging = Staging::new(chunk_size)?;
        let mut written = 0usize;
        while written < len {
            let n = (len - written).min(chunk_size);
            let chunk: &[u8] = {
                let dst = &mut staging.buf()[..n];
                dst.copy_from_slice(base.add(written).as_slice(n)?);
                dst
            };
            let res = file_like.write(chunk).inspect_err(|&e| {
                if e == LxError::EBADF {
                    let path = file_like
                        .downcast_ref::<linux_object::fs::File>()
                        .map(|f| f.path().clone())
                        .unwrap_or_default();
                    kernel_hal::klog_info!(
                        "[ebadf-write] fd={:?} PRESENT but write refused: path={:?} \
                             flags={:?} (proc={:?})",
                        fd,
                        path,
                        file_like.flags(),
                        proc.execute_path()
                    );
                }
            });
            let w = match res {
                Ok(w) => w,
                Err(e) => match after_write_error(e, waits, written) {
                    // A blocking pipe writer waits for room and carries on:
                    // `pipe_write` returns only once every byte is in (or a
                    // signal cuts it short), never `EAGAIN`.
                    AfterWriteError::Wait => {
                        file_like.async_poll(PollEvents::OUT).await?;
                        continue;
                    }
                    // A later chunk failing (e.g. EAGAIN once the socket
                    // buffer filled) must report the bytes already consumed,
                    // not the error — POSIX partial write. Erroring would
                    // make the caller resend data the file already took.
                    AfterWriteError::Partial => break,
                    AfterWriteError::Fail => {
                        self.raise_sigpipe_if_due(e, self.is_stream(&file_like));
                        return Err(e);
                    }
                },
            };
            // A write of 0 would otherwise spin forever; stop and report the
            // bytes written so far (short write).
            if w == 0 {
                break;
            }
            written += w;
        }
        Ok(written)
    }

    /// True for a pipe fd (either end).
    fn is_pipe(&self, file_like: &Arc<dyn FileLike>) -> bool {
        super::splice::pipe_inode(file_like).is_some()
    }

    /// True for a socket whose `write` queues into a bounded buffer, like a
    /// pipe, and answers `EAGAIN` when that is full (unix, UDP).
    fn is_bounded_socket(&self, file_like: &Arc<dyn FileLike>) -> bool {
        crate::net::queue_is_bounded(file_like)
    }

    /// A pipe or a socket: the fds whose `EPIPE` means "the other side is
    /// gone" and comes with `SIGPIPE` (`pipe_write`, `unix_stream_sendmsg`,
    /// `sk_stream_error`), as opposed to a device reusing the errno.
    fn is_stream(&self, file_like: &Arc<dyn FileLike>) -> bool {
        self.is_pipe(file_like) || file_like.as_socket().is_ok()
    }

    /// True for a pipe, unix/UDP socket, or eventfd without `O_NONBLOCK`: the
    /// fds whose `write` this syscall waits on, as `pipe_write`,
    /// `unix_stream_sendmsg`, `sock_alloc_send_skb` and `eventfd_write` do,
    /// instead of handing the caller the `EAGAIN` that only a non-blocking fd
    /// may see. (A TCP socket's `write` waits for window on its own,
    /// synchronously.)
    fn waits_for_room(&self, file_like: &Arc<dyn FileLike>) -> bool {
        (self.is_pipe(file_like)
            || self.is_bounded_socket(file_like)
            || file_like
                .downcast_ref::<linux_object::fs::EventFd>()
                .is_some())
            && !file_like.flags().non_block()
    }

    /// `pipe_write` pairs its `-EPIPE` with `send_sig(SIGPIPE, current, 0)`,
    /// and every socket family does the same unless `MSG_NOSIGNAL` says
    /// otherwise (`write(2)` cannot say it): a writer whose reader has gone
    /// is killed unless it asked to be told instead. Without the signal,
    /// `yes | head -1` ran `yes` for ever.
    /// `send_sig(SIGXFSZ, current, 0)`: what a call that would make a file
    /// bigger than `RLIMIT_FSIZE` sends along with its `EFBIG`. The default
    /// action ends the process, the way `SIGPIPE`'s does for a writer to a
    /// closed pipe.
    pub(crate) fn raise_sigxfsz(&self) {
        self.thread
            .lock_linux()
            .signals
            .insert(linux_object::signal::Signal::SIGXFSZ);
    }

    pub(super) fn raise_sigpipe_if_due(&self, e: LxError, stream: bool) {
        if sigpipe_due(e, stream) {
            self.thread
                .lock_linux()
                .signals
                .insert(linux_object::signal::Signal::SIGPIPE);
        }
    }

    /// read from or write to a file descriptor at a given offset
    /// reads up to count bytes from file descriptor fd at offset offset
    /// (from the start of the file) into the buffer starting at buf. The file offset is not changed.
    pub async fn sys_pread(
        &self,
        fd: FileDesc,
        base: UserOutPtr<u8>,
        len: usize,
        offset: u64,
    ) -> SysResult {
        info!(
            "pread: fd={:?}, base={:?}, len={}, offset={}",
            fd, base, len, offset
        );
        // `off_t` is signed: a negative offset is EINVAL, not an offset of
        // 2^64 - 1 handed to the filesystem.
        let offset = linux_object::fs::user_offset(offset)?;
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;

        let chunk_size = len.min(super::SYSCALL_IO_MAX);
        // Same hybrid buffer as sys_read — short positional reads (e.g. ELF
        // header probes during dlopen, libc pread of small struct slots) hit
        // the stack path.
        const STACK_BUF: usize = 512;
        let mut stack_buf = [0u8; STACK_BUF];
        let mut heap_buf: alloc::vec::Vec<u8> = if chunk_size > STACK_BUF {
            crate::try_zeroed_buf(chunk_size)?
        } else {
            alloc::vec::Vec::new()
        };
        let buf: &mut [u8] = if chunk_size > STACK_BUF {
            &mut heap_buf[..]
        } else {
            &mut stack_buf[..chunk_size]
        };
        let mut read_len = 0;

        while read_len < len {
            let current_len = (len - read_len).min(chunk_size);
            let n = file_like
                .read_at(offset + read_len as u64, &mut buf[..current_len])
                .await?;
            if n == 0 {
                break;
            }
            base.add(read_len).write_array(&buf[..n])?;
            read_len += n;
            if n < current_len {
                break;
            }
        }
        Ok(read_len)
    }

    /// writes up to count bytes from the buffer
    /// starting at buf to the file descriptor fd at offset offset. The file offset is not changed.
    pub fn sys_pwrite(
        &self,
        fd: FileDesc,
        base: UserInPtr<u8>,
        len: usize,
        offset: u64,
    ) -> SysResult {
        info!(
            "pwrite: fd={:?}, base={:?}, len={}, offset={}",
            fd, base, len, offset
        );
        let offset = linux_object::fs::user_offset(offset)?;
        let file_like = self.linux_process().get_file_like(fd)?;
        // Zero bytes still has to reach the file: that is where a descriptor
        // not open for writing answers EBADF.
        if len == 0 {
            return file_like.write_at(offset, &[]);
        }
        // Staged through kernel memory, chunked like `pwritev`: a raw user
        // slice here is the btrfs KERNEL STOP described on `Staging`.
        let chunk_size = len.min(super::SYSCALL_IO_MAX);
        let mut staging = Staging::new(chunk_size)?;
        let mut written = 0usize;
        while written < len {
            let n = (len - written).min(chunk_size);
            let chunk: &[u8] = {
                let dst = &mut staging.buf()[..n];
                dst.copy_from_slice(base.add(written).as_slice(n)?);
                dst
            };
            match file_like.write_at(offset + written as u64, chunk) {
                // A write of nothing would spin the loop for ever.
                Ok(0) => break,
                Ok(w) => {
                    written += w;
                    if w < n {
                        break;
                    }
                }
                // POSIX partial write: bytes already taken are the answer.
                Err(_) if written > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(written)
    }

    /// works just like read except that multiple buffers are filled.
    /// reads iov_count buffers from the file
    /// associated with the file descriptor fd into the buffers described by iov ("scatter input")
    pub async fn sys_readv(
        &self,
        fd: FileDesc,
        iov_ptr: UserInPtr<IoVecOut>,
        iov_count: usize,
    ) -> SysResult {
        info!("readv: fd={:?}, iov={:?}, count={}", fd, iov_ptr, iov_count);
        let mut iovs = iov_ptr.read_iovecs(iov_count)?;
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        let total_len = iovs.total_len().min(super::SYSCALL_IO_MAX);
        // Mirror the sys_read hybrid buffer: many readv callers (e.g. socket
        // headers + payload split into two small iovecs) request totals well
        // under 512 B, so keep those alloc-free.
        const STACK_BUF: usize = 512;
        let mut stack_buf = [0u8; STACK_BUF];
        let mut heap_buf: alloc::vec::Vec<u8> = if total_len > STACK_BUF {
            crate::try_zeroed_buf(total_len)?
        } else {
            alloc::vec::Vec::new()
        };
        let buf: &mut [u8] = if total_len > STACK_BUF {
            &mut heap_buf[..]
        } else {
            &mut stack_buf[..total_len]
        };
        let len = file_like.read(buf).await?;
        iovs.write_from_buf(&buf[..len])?;
        Ok(len)
    }

    /// works just like write except that multiple buffers are written out.
    /// writes iov_count buffers of data described
    /// by iov to the file associated with the file descriptor fd ("gather output").
    ///
    /// There is deliberately NO total-length limit here. Linux caps a writev
    /// only at `MAX_RW_COUNT` (~2 GiB); rejecting large totals with `EINVAL`
    /// instead is what killed every GLX client against the now-working
    /// Xwayland: xcb flushes big requests as `writev(fd, [queue, header,
    /// payload], 3)`, and an uncompressed `PutImage` of a 300×300 window is
    /// ~352 KiB — the old `> SYSCALL_IO_MAX -> EINVAL` answer made xcb mark
    /// the connection dead and the client exit with "XIO: fatal IO error 22
    /// (Invalid argument)" right after its window appeared. Instead, gather
    /// through a bounded kernel buffer in `SYSCALL_IO_MAX` chunks — the same
    /// bounded-copy iteration Linux does — and report a partial count when a
    /// later chunk cannot proceed (POSIX short write; xcb and stdio both
    /// resume from it).
    pub async fn sys_writev(
        &self,
        fd: FileDesc,
        iov_ptr: UserInPtr<IoVecIn>,
        iov_count: usize,
    ) -> SysResult {
        info!(
            "writev: fd={:?}, iov={:?}, count={}",
            fd, iov_ptr, iov_count
        );
        let iovs = iov_ptr.read_iovecs(iov_count)?;
        let total = iovs.total_len();
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        let waits = self.waits_for_room(&file_like);
        let mut buf = crate::try_zeroed_buf(total.min(super::SYSCALL_IO_MAX))?;
        let mut written = 0usize;
        while written < total {
            let n = iovs.read_bytes_at(written, &mut buf)?;
            if n == 0 {
                break;
            }
            // stdout/stderr only — see sys_write.
            if <FileDesc as Into<i32>>::into(fd) <= 2 {
                tee_x_diag(&buf[..n]);
            }
            match file_like.write(&buf[..n]) {
                Ok(w) => {
                    written += w;
                    // A pipe that took part of a chunk has no room for the
                    // rest yet; a blocking writer waits for it (below, on
                    // the next `Again`) rather than stopping short.
                    if w < n && !waits {
                        break;
                    }
                }
                Err(e) => match after_write_error(e, waits, written) {
                    AfterWriteError::Wait => {
                        file_like.async_poll(PollEvents::OUT).await?;
                    }
                    // Progress already made: report the partial count; the
                    // error will surface on the caller's next write. Erroring
                    // here instead would make the caller believe NOTHING was
                    // written and resend bytes the file already consumed.
                    AfterWriteError::Partial => break,
                    AfterWriteError::Fail => {
                        self.raise_sigpipe_if_due(e, self.is_stream(&file_like));
                        return Err(e);
                    }
                },
            }
        }
        Ok(written)
    }

    /// read multiple buffers from a file descriptor at a given offset
    /// (see [linux man preadv(2)](https://www.man7.org/linux/man-pages/man2/preadv.2.html)).
    ///
    /// Combines `readv` scatter semantics with `pread`'s stateless offset: the
    /// file position is left untouched, which is what makes it safe under
    /// concurrent readers (glibc's stdio unlocked paths and various databases
    /// pick it over `lseek`+`readv` precisely for that atomicity).
    pub async fn sys_preadv(
        &self,
        fd: FileDesc,
        iov_ptr: UserInPtr<IoVecOut>,
        iov_count: usize,
        offset: u64,
    ) -> SysResult {
        info!(
            "preadv: fd={:?}, iov={:?}, count={}, offset={}",
            fd, iov_ptr, iov_count, offset
        );
        let offset = linux_object::fs::user_offset(offset)?;
        let mut iovs = iov_ptr.read_iovecs(iov_count)?;
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        let total_len = iovs.total_len().min(super::SYSCALL_IO_MAX);
        let mut buf = crate::try_zeroed_buf(total_len)?;
        let len = file_like.read_at(offset, &mut buf).await?;
        iovs.write_from_buf(&buf[..len])?;
        Ok(len)
    }

    /// write multiple buffers to a file descriptor at a given offset
    /// (see [linux man pwritev(2)](https://www.man7.org/linux/man-pages/man2/pwritev.2.html)).
    ///
    /// Gather-output twin of [`sys_preadv`](Self::sys_preadv); the file position
    /// is not changed.
    pub fn sys_pwritev(
        &self,
        fd: FileDesc,
        iov_ptr: UserInPtr<IoVecIn>,
        iov_count: usize,
        offset: u64,
    ) -> SysResult {
        info!(
            "pwritev: fd={:?}, iov={:?}, count={}, offset={}",
            fd, iov_ptr, iov_count, offset
        );
        let offset = linux_object::fs::user_offset(offset)?;
        let iovs = iov_ptr.read_iovecs(iov_count)?;
        // Same chunked gather as sys_writev — no artificial total cap, short
        // write on a mid-stream failure — with the position carried in the
        // explicit offset instead of the file cursor.
        let total = iovs.total_len();
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        let mut buf = crate::try_zeroed_buf(total.min(super::SYSCALL_IO_MAX))?;
        let mut written = 0usize;
        while written < total {
            let n = iovs.read_bytes_at(written, &mut buf)?;
            if n == 0 {
                break;
            }
            match file_like.write_at(offset + written as u64, &buf[..n]) {
                Ok(w) => {
                    written += w;
                    if w < n {
                        break;
                    }
                }
                Err(_) if written > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(written)
    }

    /// `preadv2`: [`sys_preadv`](Self::sys_preadv) plus per-call flags
    /// (see [linux man preadv2(2)](https://www.man7.org/linux/man-pages/man2/readv.2.html)).
    ///
    /// An offset of -1 means "use and update the current file position", i.e.
    /// plain `readv`. The RWF_* flags request behaviours this kernel does not
    /// implement (NOWAIT, HIPRI, DSYNC, ...), so any non-zero flag answers
    /// `EOPNOTSUPP` — the documented reply for unsupported flags, which callers
    /// treat as "fall back to preadv". Silently accepting RWF_NOWAIT and then
    /// blocking would be worse than refusing it.
    pub async fn sys_preadv2(
        &self,
        fd: FileDesc,
        iov_ptr: UserInPtr<IoVecOut>,
        iov_count: usize,
        offset: i64,
        flags: usize,
    ) -> SysResult {
        if flags != 0 {
            return Err(LxError::EOPNOTSUPP);
        }
        if offset == -1 {
            return self.sys_readv(fd, iov_ptr, iov_count).await;
        }
        self.sys_preadv(fd, iov_ptr, iov_count, offset as u64).await
    }

    /// `pwritev2`: [`sys_pwritev`](Self::sys_pwritev) plus per-call flags
    /// (see [linux man pwritev2(2)](https://www.man7.org/linux/man-pages/man2/readv.2.html)).
    ///
    /// Offset -1 falls back to `writev` semantics. `RWF_DSYNC`/`RWF_SYNC`
    /// make the file durable after the write (`fsync`, which on EclipseFS
    /// is one fsync log record). `RWF_ATOMIC` writes all or nothing, untorn
    /// by a crash, where `statx(STATX_WRITE_ATOMIC)` says the file can: one
    /// segment whose length is a power of two within the file's atomic unit,
    /// at an offset aligned to it, else `EINVAL`; `EOPNOTSUPP` on other
    /// filesystems. The other flags answer `EOPNOTSUPP` for the same reason
    /// as [`sys_preadv2`](Self::sys_preadv2).
    pub async fn sys_pwritev2(
        &self,
        fd: FileDesc,
        iov_ptr: UserInPtr<IoVecIn>,
        iov_count: usize,
        offset: i64,
        flags: usize,
    ) -> SysResult {
        if flags & !(RWF_DSYNC | RWF_SYNC | RWF_ATOMIC) != 0 {
            return Err(LxError::EOPNOTSUPP);
        }
        let written = if flags & RWF_ATOMIC != 0 {
            self.pwrite_atomic(fd, iov_ptr, iov_count, offset)?
        } else if offset == -1 {
            self.sys_writev(fd, iov_ptr, iov_count).await?
        } else {
            self.sys_pwritev(fd, iov_ptr, iov_count, offset as u64)?
        };
        if flags & (RWF_DSYNC | RWF_SYNC) != 0 && written > 0 {
            let file = self.linux_process().get_file(fd)?;
            file.inode().sync_all()?;
        }
        Ok(written)
    }

    /// The `RWF_ATOMIC` write of [`sys_pwritev2`](Self::sys_pwritev2).
    fn pwrite_atomic(
        &self,
        fd: FileDesc,
        iov_ptr: UserInPtr<IoVecIn>,
        iov_count: usize,
        offset: i64,
    ) -> SysResult {
        let file = self.linux_process().get_file(fd)?;
        let (min, max) = linux_object::fs::eclipsefs_atomic_write_unit(&file.inode())
            .ok_or(LxError::EOPNOTSUPP)?;
        // One segment (`stx_atomic_write_segments_max`).
        if iov_count != 1 {
            return Err(LxError::EINVAL);
        }
        let iovs = iov_ptr.read_iovecs(iov_count)?;
        let len = iovs.total_len();
        let pos = if offset == -1 {
            file.seek(SeekFrom::Current(0))?
        } else {
            linux_object::fs::user_offset(offset as u64)?
        };
        if !atomic_write_valid(len, pos, min as usize, max as usize) {
            return Err(LxError::EINVAL);
        }
        let mut buf = crate::try_zeroed_buf(len)?;
        if iovs.read_bytes_at(0, &mut buf)? != len {
            return Err(LxError::EFAULT);
        }
        let n = file.write_atomic_at(pos, &buf)?;
        if offset == -1 {
            file.seek(SeekFrom::Start(pos + n as u64))?;
        }
        Ok(n)
    }

    /// repositions the offset of the open file associated with the file descriptor fd
    /// to the argument offset according to the directive whence
    pub fn sys_lseek(&self, fd: FileDesc, offset: i64, whence: usize) -> SysResult {
        const SEEK_DATA: usize = 3;
        const SEEK_HOLE: usize = 4;
        if whence == SEEK_DATA || whence == SEEK_HOLE {
            let proc = self.linux_process();
            let file_like = proc.get_file_like(fd)?;
            let file = file_like.downcast_ref::<File>().ok_or(LxError::EINVAL)?;
            // A pipe or a FIFO has no `llseek` at all, so `vfs_llseek` fails
            // before the `whence` is looked at and all five answer ESPIPE.
            // Asking the file where it is now is how `File::seek` already
            // answers that, and it moves nothing. Without it these two
            // whences computed a position from an inode size of zero, which
            // puts every offset at or past the end, and answered ENXIO --
            // which a caller reads as "this is a seekable file and there is
            // no data from here on", so it treats the pipe as fully sparse
            // and stops reading it.
            file.seek(SeekFrom::Current(0))?;
            let pos = match linux_object::fs::eclipsefs_seek(file, whence, offset) {
                Some(r) => r?,
                None => seek_data_hole_generic(file.metadata()?.size as u64, whence, offset)?,
            };
            file.seek(SeekFrom::Start(pos))?;
            return Ok(pos as usize);
        }
        let pos = seek_from(whence, offset)?;
        info!("lseek: fd={:?}, pos={:?}", fd, pos);

        let proc = self.linux_process();
        // Use the FileLike seek so it works on non-`File` fds too — notably the
        // DRM PRIME dma-buf fd, which Mesa's software importer sizes with
        // lseek(SEEK_END). `get_file` would reject it with EBADF.
        let file = proc.get_file_like(fd)?;
        let offset = file.seek(pos)?;
        Ok(offset as usize)
    }

    /// `faccessat(2)`'s mode word, which `do_faccessat()` validates rather than
    /// masks: `if (mode & ~S_IRWXO) return -EINVAL;`.
    ///
    /// Masking it with `& 0o7` instead turned a bad constant into F_OK, so
    /// `access(path, 8)` answered "yes, you may" for any file that merely
    /// exists -- the one answer a permission probe must never invent.
    fn access_mode(mode: usize) -> linux_object::error::LxResult<u16> {
        if mode & !0o7 != 0 {
            return Err(LxError::EINVAL);
        }
        Ok(mode as u16)
    }

    /// What `truncate(2)` says about the type of what the path names, before
    /// it asks anything about permissions (`do_sys_truncate`, `fs/open.c`):
    ///
    /// ```c
    /// error = -EISDIR;
    /// if (S_ISDIR(inode->i_mode)) goto dput_and_out;
    /// error = -EINVAL;
    /// if (!S_ISREG(inode->i_mode)) goto dput_and_out;
    /// ...
    /// error = inode_permission(idmap, inode, MAY_WRITE);
    /// ```
    ///
    /// Nothing asked this here: the write check went first, so `truncate`
    /// of a directory the caller could not write was `EACCES`, and of a
    /// device, a fifo or a socket in a ramfs it was whatever `resize` on a
    /// non-file answered (`EISDIR`, for a character device).
    fn truncate_type(type_: FileType) -> linux_object::error::LxResult {
        match type_ {
            FileType::Dir => Err(LxError::EISDIR),
            FileType::File => Ok(()),
            _ => Err(LxError::EINVAL),
        }
    }

    /// What `ftruncate(2)` says about the type of what the descriptor is open
    /// on: `EINVAL` for anything but a regular file (`do_sys_ftruncate`:
    /// `if (!S_ISREG(inode->i_mode) || !(f.file->f_mode & FMODE_WRITE))
    /// goto out_putf;` with `error = -EINVAL`). A directory is not `EISDIR`
    /// here, unlike `truncate`; the descriptor's own openness for writing is
    /// still the file's to judge.
    fn ftruncate_type(type_: FileType) -> linux_object::error::LxResult {
        if type_ == FileType::File {
            Ok(())
        } else {
            Err(LxError::EINVAL)
        }
    }

    /// `RLIMIT_FSIZE` on a call that sets a regular file's length
    /// (`inode_newsize_ok`): growing it past the limit is `EFBIG`, and a
    /// `SIGXFSZ`. A write's share of the limit is the file's to apply
    /// (`linux_object::fs::File`), and its signal the dispatcher's.
    ///
    /// Nothing here asked it at all once: the limit was stored, read back by
    /// `getrlimit` and never compared against anything, so
    /// `ftruncate(fd, 1 << 50)` from any process went straight to the
    /// filesystem. On the RAM-backed roots this kernel runs on, a `resize`
    /// commits every block it grows by, which is how one `ftruncate` to ~456
    /// MiB took the desktop out of memory (the incident this file's `warn!`
    /// below was added for). Then it was asked of every new length, so a file
    /// already past the limit could not be cut down to one that still was,
    /// and it came without the signal.
    fn check_fsize_growth(&self, size: u64, new_len: u64) -> linux_object::error::LxResult {
        let limit =
            self.linux_process()
                .rlimit(linux_object::process::RLIMIT_FSIZE, None, false)?;
        if grows_past_fsize(limit.cur, size, new_len) {
            warn!(
                "[rlimit] EFBIG: len={} > RLIMIT_FSIZE={} comm={}",
                new_len,
                limit.cur,
                self.zircon_process().name(),
            );
            self.raise_sigxfsz();
            return Err(LxError::EFBIG);
        }
        Ok(())
    }

    /// cause the regular file named by path to be truncated to a size of precisely length bytes.
    pub fn sys_truncate(&self, path: UserInPtr<u8>, len: usize) -> SysResult {
        let path = path.as_c_str()?;
        info!("truncate: path={:?}, len={}", path, len);
        let len = linux_object::fs::user_len(len)?;
        let proc = self.linux_process();
        let inode = proc.lookup_inode(path)?;
        let metadata = inode.metadata()?;
        Self::truncate_type(metadata.type_)?;
        proc.check_access(&metadata, 0o2, true)?;
        self.check_fsize_growth(metadata.size as u64, len as u64)?;
        inode
            .resize(len)
            .map_err(|e| linux_object::fs::fs_grow_error(&inode, e))?;
        linux_object::fs::cache_truncate(&inode, len);
        Ok(0)
    }

    /// cause the regular file referenced by fd to be truncated to a size of precisely length bytes.
    pub fn sys_ftruncate(&self, fd: FileDesc, len: usize) -> SysResult {
        info!("ftruncate: fd={:?}, len={}", fd, len);
        // A negative length is EINVAL in Linux, not a request to grow the
        // file to sixteen exabytes.
        let len = linux_object::fs::user_len(len)?;
        let proc = self.linux_process();
        let file = proc.get_file(fd)?;
        let metadata = file.metadata()?;
        Self::ftruncate_type(metadata.type_)?;
        self.check_fsize_growth(metadata.size as u64, len as u64)?;
        // The desktop OOM was a single ftruncate growing one RAM-backed file
        // to ~456 MiB (117k live 4 KiB ramfs blocks in one resize). Name any
        // suspicious-sized truncate loudly: which file, how big, and who.
        if len >= 16 * 1024 * 1024 {
            warn!(
                "[ftruncate] BIG: len={} MiB path={} comm={}",
                len >> 20,
                file.path(),
                self.zircon_process().name(),
            );
        }
        // File seals: a memfd sealed against shrinking or growing refuses the
        // resize with EPERM (fcntl(2), F_SEAL_SHRINK / F_SEAL_GROW).
        if !linux_object::fs::memfd_resize_allowed(&file.inode(), len as u64) {
            return Err(LxError::EPERM);
        }
        file.set_len(len as u64)?;
        // Leak telemetry: the desktop OOM is unfreed memfd content, and
        // ftruncate is the call that sizes memfds — every 64th call, log the
        // live-memfd census (count, sizes, strong refs) so the growth and its
        // holder are visible in real time without waiting for the OOM.
        {
            use core::sync::atomic::{AtomicUsize, Ordering};
            static FTRUNCATE_N: AtomicUsize = AtomicUsize::new(0);
            if FTRUNCATE_N
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(64)
            {
                linux_object::fs::memfd_dump_live(8);
            }
        }
        Ok(0)
    }

    /// Announce an intention to access file data in a specific pattern
    /// (`posix_fadvise`). Acting on the hint is optional -- `generic_fadvise`
    /// itself does nothing for NORMAL, RANDOM and SEQUENTIAL on a backing
    /// device without readahead tuning -- but *which* hints exist is not, so
    /// an advice value this kernel has never heard of is `EINVAL` rather than
    /// a silent success.
    pub fn sys_fadvise64(
        &self,
        fd: FileDesc,
        offset: usize,
        len: usize,
        advice: usize,
    ) -> SysResult {
        info!(
            "fadvise64: fd={:?}, offset={}, len={}, advice={}",
            fd, offset, len, advice
        );
        // Honour Linux's EBADF for an invalid descriptor.
        let file = self.linux_process().get_file_like(fd)?;
        // `generic_fadvise` answers ESPIPE for a FIFO before it ever looks at
        // the hint: a node from `mkfifo` or a pipe from `pipe2`, which is a
        // FIFO to `fstat` as well.
        if matches!(file.metadata(), Ok(m) if m.type_ == FileType::NamedPipe) {
            return Err(LxError::ESPIPE);
        }
        // `generic_fadvise`: `len < 0` is EINVAL, judged after the FIFO test
        // and before the advice word. `len` is a `loff_t`; as a `usize` a
        // negative one was a very long hint, taken.
        crate::intarg::loff_len(len)?;
        if !fadvise_advice_known(advice) {
            return Err(LxError::EINVAL);
        }
        Ok(0)
    }

    /// Manipulate the allocated disk space for the file referenced by `fd`
    /// (`fallocate`).
    ///
    /// The `mode` word used to be read only for the one value `0`; everything
    /// else fell through to a successful no-op. Two of those modes promise the
    /// caller that the range **reads back as zeros afterwards**, and a no-op
    /// that claims success is how a caller ends up reading the old bytes out
    /// of a hole it believes it punched. There is no hole here to punch --
    /// these filesystems have no extent map to poke a gap into -- but the
    /// observable half of the promise is a write of zeros, so that is what the
    /// call does. The modes that shift a file's contents (COLLAPSE_RANGE,
    /// INSERT_RANGE) cannot be served at all, and say so with the same
    /// `EOPNOTSUPP` a Linux filesystem without support for them gives.
    pub fn sys_fallocate(&self, fd: FileDesc, mode: usize, offset: usize, len: usize) -> SysResult {
        info!(
            "fallocate: fd={:?}, mode={:#x}, offset={}, len={}",
            fd, mode, offset, len
        );
        // The order is `ksys_fallocate` then `vfs_fallocate`: the descriptor
        // first, then the offset and length, then the mode, then the
        // descriptor's ACCESS mode, then what kind of file this is. Which
        // matters at the one step where two of them can both be wrong: a
        // caller probing for a mode on a read-only descriptor is told about
        // the mode, not about the descriptor.
        let file = self.linux_process().get_file(fd)?;
        // `fallocate(2)`: EINVAL for a negative offset or a length that is
        // not positive.
        let offset = linux_object::fs::user_len(offset)?;
        let len = linux_object::fs::user_len(len)?;
        if len == 0 {
            return Err(LxError::EINVAL);
        }
        let op = fallocate_op(mode)?;
        let flags = file.flags();
        // EBADF for a descriptor not open for writing. `set_len` and `write_at`
        // both check this, but only once they are reached: without the check
        // here, `fallocate(ro_fd, 0, 0, 1)` on a file already that long
        // reported success on a descriptor it may not write a byte through.
        if !flags.writable() {
            return Err(LxError::EBADF);
        }
        let zeroing = matches!(op, FallocateOp::ZeroRange { .. });
        // "It is not possible to punch hole on an append-only file."
        if zeroing && flags.is_append() {
            return Err(LxError::EPERM);
        }
        let meta = file.metadata()?;
        let resizable = match meta.type_ {
            FileType::File => true,
            FileType::Dir => return Err(LxError::EISDIR),
            FileType::NamedPipe => return Err(LxError::ESPIPE),
            // `S_ISBLK` is allowed. A block device has every one of its blocks
            // already, so there is nothing to preallocate and no length to
            // change -- but an order to zero a range is still an order to zero
            // it, and answering that one with a no-op would be the same lie.
            FileType::BlockDevice => false,
            _ => return Err(LxError::ENODEV),
        };
        let end = offset.checked_add(len).ok_or(LxError::EFBIG)?;
        let keep_size = match op {
            FallocateOp::Allocate { keep_size } | FallocateOp::ZeroRange { keep_size } => keep_size,
        };
        let grows = resizable && !keep_size;
        // Only a call that grows the file answers to `RLIMIT_FSIZE`
        // (`shmem_fallocate`, `ext4_fallocate`: `inode_newsize_ok` when the
        // new end is past `i_size`), and before anything is allocated.
        if grows {
            self.check_fsize_growth(meta.size as u64, end as u64)?;
        }
        // EclipseFS has an extent map: real preallocation, and a punched
        // hole that gives its blocks back instead of a write of zeros.
        if resizable {
            if let Some(res) =
                linux_object::fs::eclipsefs_fallocate(&file, mode as u32, offset as u64, len as u64)
            {
                res?;
                return Ok(0);
            }
        }
        if grows && end > meta.size {
            file.set_len(end as u64)?;
        }
        if zeroing {
            // Where the file does not grow, only the part of the range that is
            // inside it can be made to read as zeros -- past the end it
            // already does.
            let zero_end = if grows { end } else { end.min(meta.size) };
            let mut at = offset;
            let zeros = alloc::vec![0u8; FALLOCATE_ZERO_CHUNK.min(zero_end.saturating_sub(at))];
            while at < zero_end {
                let n = zeros.len().min(zero_end - at);
                let written = file.fill_at(at as u64, &zeros[..n])?;
                if written == 0 {
                    return Err(LxError::EIO);
                }
                at += written;
            }
        }
        Ok(0)
    }

    /// copies data between one file descriptor and another.
    pub async fn sys_sendfile(
        &self,
        out_fd: FileDesc,
        in_fd: FileDesc,
        offset_ptr: UserInOutPtr<u64>,
        count: usize,
    ) -> SysResult {
        self.sys_copy_file_range(in_fd, offset_ptr, out_fd, 0.into(), count, 0)
            .await
    }

    /// copies data between one file descriptor and anothe, read from specified offset and write new offset back
    pub async fn sys_copy_file_range(
        &self,
        in_fd: FileDesc,
        mut in_offset: UserInOutPtr<u64>,
        out_fd: FileDesc,
        mut out_offset: UserInOutPtr<u64>,
        count: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "copy_file_range: in={:?}, out={:?}, in_offset={:?}, out_offset={:?}, count={}, flags={}",
            in_fd, out_fd, in_offset, out_offset, count, flags
        );
        // `copy_file_range(2)` defines no flags yet and Linux reserves the
        // word, so that the day one means something a program can tell a
        // kernel that honoured it from a kernel that dropped it.
        copy_file_range_flags(flags)?;
        let proc = self.linux_process();
        // Any file-like will do on either side: `sendfile(2)`'s reason to
        // exist is `out_fd` being a socket, and a socket is not a `File`, so
        // `get_file` used to answer EBADF to every web server's fast path.
        let in_file = proc.get_file_like(in_fd)?;
        let out_file = proc.get_file_like(out_fd)?;

        // for in_offset and out_offset
        // null means update file offset
        // non-null means update {in,out}_offset instead

        // Both offsets are `off_t` read from a user pointer, and both are
        // *walked* by the copy below, so each needs the window checked and not
        // just the starting position -- see `user_offset_end`. Unvalidated, a
        // negative one arrived here as a number near 2^64 and went straight
        // into `read_at`/`seek`, and the `+=` further down wrapped on top.
        let read_offset = if !in_offset.is_null() {
            let offset = in_offset.read()?;
            linux_object::fs::user_offset_end(offset, count)?;
            Some(offset)
        } else {
            None
        };
        let write_offset = if !out_offset.is_null() {
            let offset = out_offset.read()?;
            linux_object::fs::user_offset_end(offset, count)?;
            Some(offset)
        } else {
            None
        };

        // Two files of one EclipseFS mount at block-aligned offsets: share
        // the blocks (a reflink, as btrfs does here) instead of copying them.
        if let (Some(src), Some(dst)) = (
            in_file.downcast_ref::<File>(),
            out_file.downcast_ref::<File>(),
        ) {
            if src.flags().readable() && dst.flags().writable() {
                let src_pos = match read_offset {
                    Some(off) => off,
                    None => src.seek(SeekFrom::Current(0))?,
                };
                let dst_pos = match write_offset {
                    Some(off) => off,
                    None => dst.seek(SeekFrom::Current(0))?,
                };
                if let Some(res) = linux_object::fs::eclipsefs_copy_reflink(
                    src,
                    src_pos,
                    dst,
                    dst_pos,
                    count as u64,
                ) {
                    let n = res?;
                    match read_offset {
                        Some(_) => in_offset.write(src_pos + n)?,
                        None => {
                            src.seek(SeekFrom::Start(src_pos + n))?;
                        }
                    }
                    match write_offset {
                        Some(_) => out_offset.write(dst_pos + n)?,
                        None => {
                            dst.seek(SeekFrom::Start(dst_pos + n))?;
                        }
                    }
                    return Ok(n as usize);
                }
            }
        }
        let waits = self.waits_for_room(&out_file);
        let copied = copy_range(&in_file, read_offset, &out_file, write_offset, count, waits)
            .await
            .inspect_err(|&e| self.raise_sigpipe_if_due(e, self.is_stream(&out_file)))?;
        if let Some(off) = copied.in_offset {
            in_offset.write(off)?;
        }
        if let Some(off) = copied.out_offset {
            out_offset.write(off)?;
        }
        Ok(copied.moved)
    }
}

/// What `copy_range` moved and where the explicit offsets now stand: each
/// advanced by the bytes that reached the output, `None` where the caller gave
/// none (that side used and moved its own position).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Copied {
    pub moved: usize,
    pub in_offset: Option<u64>,
    pub out_offset: Option<u64>,
}

/// Copy up to `count` bytes from `in_file` to `out_file`, the way
/// `do_splice_direct` does for `sendfile(2)` and `copy_file_range(2)`: **what
/// the input hands over is what the output took**, and a call that moved
/// anything reports that count rather than an error.
///
/// The old loop read a chunk, then wrote it with `?` on every write: a full
/// socket or pipe answered `EAGAIN` after some chunks were already out, the
/// error came back with no count, and the caller -- a web server sending a
/// file -- resent from the same offset, so the peer received the head of the
/// file twice. A blocking socket got that `EAGAIN` too, where `write(2)` on
/// the same fd waits. And a write of 0 bytes was reported as `EBADF`.
///
/// Now a write error goes through `after_write_error`, exactly as `write(2)`
/// does: wait for room when the fd is a blocking pipe or socket, report the
/// bytes already moved when there are any, and only fail when nothing moved.
/// The input position (`*off_in` or the fd's own) ends after the last byte
/// delivered, the output fd's own position is never touched when `off_out`
/// is given, and a chunk is `SYSCALL_IO_MAX`, not 1 KiB.
pub(crate) async fn copy_range(
    in_file: &Arc<dyn FileLike>,
    in_offset: Option<u64>,
    out_file: &Arc<dyn FileLike>,
    out_offset: Option<u64>,
    count: usize,
    waits: bool,
) -> linux_object::error::LxResult<Copied> {
    // EBADF for descriptors opened the wrong way round. `do_sendfile`
    // makes the same two checks, so they belong here and not at either
    // caller.
    if !in_file.flags().readable() || !out_file.flags().writable() {
        return Err(LxError::EBADF);
    }
    let read_start = match in_offset {
        Some(off) => off,
        None => in_file.seek(SeekFrom::Current(0))?,
    };
    let mut buffer = crate::try_zeroed_buf(count.min(super::SYSCALL_IO_MAX))?;
    let mut moved = 0usize;
    let mut error = None;
    'copy: while moved < count {
        let len = buffer.len().min(count - moved);
        // `user_offset_end` bounds the whole window for an explicit offset;
        // `checked_add` keeps the fd's own position honest too.
        let read_at = read_start
            .checked_add(moved as u64)
            .ok_or(LxError::EOVERFLOW)?;
        let read_len = match in_file.read_at(read_at, &mut buffer[..len]).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        let mut written = 0usize;
        while written < read_len {
            let chunk = &buffer[written..read_len];
            let res = match out_offset {
                Some(off) => out_file.write_at(off + (moved + written) as u64, chunk),
                None => out_file.write(chunk),
            };
            match res {
                Ok(0) => {
                    moved += written;
                    break 'copy;
                }
                Ok(n) => written += n,
                Err(e) => match after_write_error(e, waits, moved + written) {
                    AfterWriteError::Wait => {
                        out_file.async_poll(PollEvents::OUT).await?;
                    }
                    AfterWriteError::Partial => {
                        moved += written;
                        break 'copy;
                    }
                    AfterWriteError::Fail => {
                        error = Some(e);
                        break 'copy;
                    }
                },
            }
        }
        moved += written;
    }
    if moved == 0 {
        if let Some(e) = error {
            return Err(e);
        }
    }
    let in_offset = match in_offset {
        Some(_) => Some(read_start + moved as u64),
        None => {
            in_file.seek(SeekFrom::Current(moved as i64))?;
            None
        }
    };
    Ok(Copied {
        moved,
        in_offset,
        out_offset: out_offset.map(|off| off + moved as u64),
    })
}

impl Syscall<'_> {
    /// causes all buffered modifications to file metadata and data to be written to the underlying file systems.
    pub fn sys_sync(&self) -> SysResult {
        info!("sync:");
        let proc = self.linux_process();
        proc.root_inode().fs().sync()?;
        Ok(0)
    }

    /// transfers ("flushes") all modified in-core data of (i.e., modified buffer cache pages for) the file
    /// referred to by the file descriptor fd to the disk device
    pub fn sys_fsync(&self, fd: FileDesc) -> SysResult {
        info!("fsync: fd={:?}", fd);
        let proc = self.linux_process();
        proc.get_file(fd)?.sync_all()?;
        Ok(0)
    }

    /// is similar to fsync(), but does not flush modified metadata unless that metadata is needed
    pub fn sys_fdatasync(&self, fd: FileDesc) -> SysResult {
        info!("fdatasync: fd={:?}", fd);
        let proc = self.linux_process();
        proc.get_file(fd)?.sync_data()?;
        Ok(0)
    }

    /// Commit filesystem caches of the filesystem containing `fd` to disk
    /// (see syncfs(2)). Unlike `sync`, only that one filesystem is flushed.
    pub fn sys_syncfs(&self, fd: FileDesc) -> SysResult {
        info!("syncfs: fd={:?}", fd);
        let proc = self.linux_process();
        proc.get_file(fd)?.inode().fs().sync()?;
        Ok(0)
    }

    /// initiate file readahead into the page cache
    /// (see [linux man readahead(2)](https://www.man7.org/linux/man-pages/man2/readahead.2.html)).
    ///
    /// Purely an optimisation hint on Linux. File reads here are synchronous
    /// (no background page-cache fill to kick off), so after validating that
    /// `fd` names a real seekable file — the errors the man page requires —
    /// there is nothing useful left to start, and success is the honest reply.
    pub fn sys_readahead(&self, fd: FileDesc, offset: u64, count: usize) -> SysResult {
        info!("readahead: fd={:?}, offset={}, count={}", fd, offset, count);
        let proc = self.linux_process();
        let file = proc.get_file(fd)?;
        // `ksys_readahead`: EBADF unless the descriptor is open for reading,
        // then EINVAL unless it names something with a page cache to fill.
        if !file.flags().readable() {
            return Err(LxError::EBADF);
        }
        if !matches!(
            file.metadata().map(|m| m.type_),
            Ok(FileType::File) | Ok(FileType::BlockDevice)
        ) {
            return Err(LxError::EINVAL);
        }
        // Then `vfs_fadvise(POSIX_FADV_WILLNEED)`, which reads the count as
        // a `loff_t` and refuses a negative one.
        crate::intarg::loff_len(count)?;
        Ok(0)
    }

    /// sync a file segment with disk
    /// (see [linux man sync_file_range(2)](https://www.man7.org/linux/man-pages/man2/sync_file_range.2.html)).
    ///
    /// Flag validation per the man page; the actual sync is delegated to the
    /// file's `sync_data` when a write-back is requested — syncing more than
    /// the asked-for range is explicitly permitted behaviour, and it is the
    /// only granularity the filesystems here offer.
    pub fn sys_sync_file_range(
        &self,
        fd: FileDesc,
        offset: usize,
        nbytes: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "sync_file_range: fd={:?}, offset={}, nbytes={}, flags={:#x}",
            fd, offset, nbytes, flags
        );
        let writes_back = sync_file_range_writes_back(flags)?;
        // `ksys_sync_file_range`: the range is judged after the flags and
        // before the descriptor. Both are `loff_t`, so a negative offset or
        // length, or an end past `LLONG_MAX`, is EINVAL; read as `u64` they
        // were accepted and the whole file synced.
        crate::intarg::loff_range(offset, nbytes)?;
        let proc = self.linux_process();
        let file = proc.get_file(fd)?;
        if writes_back {
            file.sync_data()?;
        }
        Ok(0)
    }

    /// DRM ioctls that must install / look up a process fd (PRIME dma-buf
    /// export-import, and CREATE_LEASE) — the inode-level DRM `io_control`
    /// cannot touch the fd table. Returns `Ok(Some(0))` if handled, `Ok(None)`
    /// if `fd` is not a DRM device (fall through to the normal ioctl path).
    fn sys_drm_prime(
        &self,
        file_like: &alloc::sync::Arc<dyn linux_object::fs::FileLike>,
        request: usize,
        arg1: usize,
    ) -> Result<Option<usize>, LxError> {
        use linux_object::fs::devfs::drm;
        use linux_object::fs::devfs::drm_scheme::{
            nr, prime_request, DrmPrimeHandle, PrimeRequest,
        };
        use linux_object::fs::DmaBuf;

        const PRIME_HANDLE_TO_FD: u32 = nr::PRIME_HANDLE_TO_FD.0; // DRM_IOWR(0x2d, drm_prime_handle)
        const PRIME_FD_TO_HANDLE: u32 = nr::PRIME_FD_TO_HANDLE.0; // DRM_IOWR(0x2e, drm_prime_handle)
        const MODE_CREATE_LEASE: u32 = nr::MODE_CREATE_LEASE.0; // DRM_IOWR(0xc6, drm_mode_create_lease)

        // `get_unused_fd_flags(cl->flags & (O_CLOEXEC | O_NONBLOCK))`.
        const O_CLOEXEC: u32 = 0o2000000;

        // The fd really has to be a DRM node. Dispatching on the request number
        // alone meant `PRIME_HANDLE_TO_FD` on ANY fd reached the export path,
        // so a process that had never opened `/dev/dri/*` could still ask for a
        // dma-buf over a GEM handle. Linux only ever reaches
        // `drm_prime_handle_to_fd_ioctl` through a DRM file. The export path
        // now also checks who owns the handle (see `drm::export_handle`), but
        // both gates belong here: this is the one that keeps the ioctl on the
        // device it is defined for. Same downcast `WAIT_VBLANK` already uses.
        let drm = file_like.downcast_ref::<File>().and_then(|f| {
            let inode = f.inode();
            let state = inode
                .as_any_ref()
                .downcast_ref::<linux_object::fs::devfs::DrmDev>()
                .map(|dev| dev.file_state().clone())?;
            Some((inode, state))
        });
        let Some((drm_inode, drm_file)) = drm else {
            return Ok(None);
        };
        let proc = self.linux_process();
        // Match on the DRM ioctl NR only. The struct size the client encoded is
        // deliberately NOT part of the comparison -- see `is_drm_ioctl_nr`.
        match request as u32 & 0xff {
            // EXPORT (HANDLE_TO_FD, 0x2d) and IMPORT (FD_TO_HANDLE, 0x2e)
            // share one arm; `prime_request` tells them apart by the ioctl
            // NUMBER, as `drm_ioctl` does. This arm used to read the
            // operation off the struct instead (`fd < 0` = export, because
            // libdrm presets that output field to -1): the two numbers were
            // filed the wrong way round in `nr`, which is what "0xc00c642d
            // mis-routed into the import path on real hardware" was, and the
            // struct heuristic papered over it -- while breaking any exporter
            // that zeroes the struct (fd = 0 read as "import stdin").
            PRIME_HANDLE_TO_FD | PRIME_FD_TO_HANDLE => {
                let mut ptr = UserInOutPtr::<DrmPrimeHandle>::from(arg1);
                let mut h = match ptr.read() {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("[drm] PRIME read(args @ {:#x}) EFAULT: {:?}", arg1, e);
                        return Err(e.into());
                    }
                };
                let op = match prime_request(request as u32, h) {
                    Ok(op) => op,
                    Err(e) => {
                        warn!(
                            "[drm] PRIME request={:#x} handle={} flags={:#x} fd={} refused: {:?}",
                            request, h.handle, h.flags, h.fd, e
                        );
                        return Err(e);
                    }
                };
                match op {
                    PrimeRequest::Export { handle, flags } => {
                        // handle -> new dma-buf fd. `h.fd` is an OUTPUT here:
                        // whatever the caller left in it is not read. `flags`
                        // says what the fd is open as (`DRM_RDWR`) and whether
                        // it is close-on-exec (`DRM_CLOEXEC`); `add_file` reads
                        // the second off the object, as it does for `open`.
                        let (phys, size, vmo) = match drm::export_handle(handle, drm_file.minor()) {
                            Ok(v) => v,
                            Err(refusal) => {
                                warn!(
                                    "[drm] PRIME export handle={} refused: {:?}",
                                    handle, refusal
                                );
                                return Err(refusal.into());
                            }
                        };
                        let dmabuf = DmaBuf::from_prime(handle, phys, size, vmo, flags);
                        let new_fd = match proc.add_file(dmabuf) {
                            Ok(fd) => fd,
                            Err(e) => {
                                warn!("[drm] PRIME export add_file {:?}", e);
                                return Err(e);
                            }
                        };
                        h.fd = i32::from(new_fd);
                        // debug-only: export is a HOT path. wlroots re-exports both
                        // swapchain buffers on every recreate/present cycle, so at
                        // error! this floods the graphical console (~8/s) and buries
                        // the real failure in labwc.log -- exactly the console-flood
                        // pattern to avoid. The diagnostic job (compare exported phys
                        // vs the import-side reverse lookup) is done; keep it at debug.
                        log::debug!(
                            "[drm] PRIME export handle={:#x} -> phys={:#x} size={} fd={}",
                            h.handle,
                            phys,
                            size,
                            h.fd
                        );
                        if let Err(e) = ptr.write(h) {
                            warn!("[drm] PRIME export write-back EFAULT: {:?}", e);
                            return Err(e.into());
                        }
                        Ok(Some(0))
                    }
                    PrimeRequest::Import { fd } => {
                        // fd -> new GEM handle. `h.handle` is the OUTPUT here.
                        let target = match proc.get_file_like(FileDesc::from(fd as usize)) {
                            Ok(t) => t,
                            Err(e) => {
                                warn!("[drm] PRIME import EBADF: fd={} not in fd table", fd);
                                return Err(e);
                            }
                        };
                        let dmabuf = target.downcast_ref::<DmaBuf>().ok_or(LxError::EINVAL)?;
                        // Self-import first: if this dma-buf was exported from a
                        // nouveau-uAPI GEM object, hand back the ORIGINAL nouveau
                        // handle -- real Linux PRIME semantics (importing your own
                        // export resolves to the existing GEM object). This is
                        // what the compositor does for every swapchain buffer:
                        // gbm/NVK allocates (GEM_NEW), exports the fd, and EGL
                        // imports it again on the same device. A fresh GENERIC
                        // handle over the same memory (the old behaviour) passes
                        // the generic ioctls and then fails every driver-private
                        // one -- nouveau GEM_INFO gave ENOENT, NVK's dma-buf
                        // import died, and the desktop fell over at
                        // "createImageFromDmaBufs failed" / zink "couldn't
                        // allocate memory".
                        // ...and the card behind THIS node decides, because an
                        // import is per device: `drm_gem_prime_fd_to_handle`
                        // resolves against the importing device's prime table
                        // and sends anything else to that device's own importer
                        // (`nouveau_gem_prime_import_sg_table`). The reverse
                        // lookup here is by physical address, with no card in
                        // the key, so importing a card0 buffer through `card1`
                        // used to answer card0's HANDLE: an id out of the other
                        // card's slice, handed out through this node, whose
                        // GEM_CLOSE drops a reference on the other card's
                        // object and whose GEM_INFO/VM_BIND name an object of
                        // the other card's RM. See `drm::prime_import`.
                        let (handle_id, kind) = match drm::prime_import_for(
                            dmabuf.phys_addr,
                            drm_file.minor(),
                        ) {
                            drm::PrimeImport::SameCard(nouveau_handle) => {
                                // Take a PRIME reference: the importer will GEM_CLOSE
                                // this handle when done, but the exporter (wlroots)
                                // still owns the same nouveau handle. Without this
                                // bump the importer's close frees the buffer out from
                                // under the exporter, the next self-import misses, and
                                // NVK falls back to a generic handle that GEM_INFO
                                // ENOENTs -> zink "couldn't allocate memory heap=0".
                                // One reference per importing FILE, not per import:
                                // see `nouveau_gem_import_ref`.
                                let n = drm::nouveau_gem_import_ref(nouveau_handle, &drm_file);
                                // debug-only: this is the SUCCESS path and it is hot
                                // (every swapchain buffer import). Stay quiet so a
                                // working stack does not flood the console; the failure
                                // path below is the one that logs loudly.
                                log::debug!(
                                    "[drm] PRIME self-import ref++ handle={:#x} -> refcount={:?}",
                                    nouveau_handle,
                                    n
                                );
                                // ...except the first few per boot, at error! (the
                                // rig runs LOG=error): whether the COMPOSITOR ever
                                // imports a CLIENT's dma-buf is the missing half of
                                // the vkcube/eglgears present-hang picture. The pid
                                // tells the two apart -- labwc importing a phys that
                                // a client pid exported means the Wayland dmabuf
                                // dance reached the compositor at all.
                                {
                                    use core::sync::atomic::{AtomicU32, Ordering};
                                    static FIRST_IMPORTS: AtomicU32 = AtomicU32::new(0);
                                    let k = FIRST_IMPORTS.fetch_add(1, Ordering::Relaxed);
                                    if k < 8 {
                                        log::error!(
                                        "[drm] PRIME import pid={} fd={} phys={:#x} size={} -> nouveau handle={:#x} (import {}/8 this boot; informational, not an error)",
                                        self.zircon_process().id(), h.fd,
                                        dmabuf.phys_addr, dmabuf.size, nouveau_handle, k + 1
                                    );
                                    }
                                }
                                (nouveau_handle, "self-import(nouveau)")
                            }
                            // The object is another card's. A GEM handle names
                            // an object of ONE device's driver, so this node
                            // imports the pages as an object of its own --
                            // which is what wlroots' secondary card does with
                            // the composed frame under
                            // `WLR_DRM_DEVICES=/dev/dri/card0:/dev/dri/card1`.
                            // A core handle, not a driver-private one: the
                            // buffer is foreign memory with a host physical
                            // address, which is exactly what the CPU present
                            // path reads, and the importing card runs no
                            // driver-private ioctl on a frame it only scans
                            // out. Not a miss, so not the loud line below.
                            drm::PrimeImport::OtherCard(theirs) => {
                                let mine =
                                    drm::import_dmabuf(dmabuf.phys_addr, dmabuf.size, dmabuf.vmo());
                                log::info!(
                                    "[drm] PRIME cross-card import fd={} phys={:#x} size={}: \
                                     card{:?} object handle={:#x} imported on card{:?} as \
                                     handle={:#x} (a handle is one card's; the other card's id \
                                     is not handed out here)",
                                    h.fd,
                                    dmabuf.phys_addr,
                                    dmabuf.size,
                                    drm::card_index_holding_gem(theirs),
                                    theirs,
                                    drm::card_index_of_minor(drm_file.minor()),
                                    mine,
                                );
                                (mine, "cross-card")
                            }
                            drm::PrimeImport::Untracked => (
                                drm::import_dmabuf(dmabuf.phys_addr, dmabuf.size, dmabuf.vmo()),
                                "generic",
                            ),
                        };
                        // Loud ONLY on the failure path: "generic" means the
                        // self-import reverse lookup MISSED (the dma-buf's phys is not
                        // registered as a nouveau GEM object), so NVK's driver-private
                        // GEM_INFO will ENOENT the fresh handle -> zink "couldn't
                        // allocate memory heap=0" / createImageFromDmaBufs failed. The
                        // success path ("self-import(nouveau)") is hot and stays at
                        // debug so a working swapchain loop doesn't flood the console.
                        if kind == "generic" {
                            log::info!(
                            "[drm] PRIME import fd={} phys={:#x} size={} -> generic handle={:#x} (SELF-IMPORT MISS -> GEM_INFO will ENOENT)",
                            h.fd, dmabuf.phys_addr, dmabuf.size, handle_id
                        );
                        } else {
                            log::debug!(
                                "[drm] PRIME import fd={} phys={:#x} size={} -> {} handle={:#x}",
                                h.fd,
                                dmabuf.phys_addr,
                                dmabuf.size,
                                kind,
                                handle_id
                            );
                        }
                        h.handle = handle_id;
                        ptr.write(h)?;
                        Ok(Some(0))
                    }
                }
            }
            MODE_CREATE_LEASE => {
                // `drm_mode_create_lease_ioctl` in two halves. The DRM file
                // does everything up to the fd (`DRM_IOCTL_MODE_CREATE_LEASE`
                // in `drm_scheme.rs`: the master check, the flags, the object
                // list and the lessee id); what only this layer can do is
                // install the fd. Linux clones the lessor's file into a new
                // one that holds the lease; here the lease fd is a second
                // descriptor of this same open file description, the way
                // `dup` makes one -- so closing the lease does not run the
                // DRM file's teardown on state the lessor is still using, and
                // so the lessee keeps the whole device (see README-drm). Our
                // GEM table is global, so the per-fd handle ref-counting that
                // makes wlroots' dumb allocator lease is not needed either.
                drm_inode.io_control(request as u32, arg1)?;
                let mut ptr = UserInOutPtr::<DrmModeCreateLease>::from(arg1);
                let mut l = ptr.read()?;
                let lease = file_like.clone();
                let new_fd = match proc.add_file_cloexec(lease, l.flags & O_CLOEXEC != 0) {
                    Ok(fd) => fd,
                    Err(e) => {
                        // `get_unused_fd_flags` failing: no lease is made.
                        drm::lease_drop(drm_file.minor(), l.lessee_id);
                        return Err(e);
                    }
                };
                l.fd = i32::from(new_fd);
                ptr.write(l)?;
                Ok(Some(0))
            }
            _ => Ok(None),
        }
    }

    /// `SYNCOBJ_HANDLE_TO_FD` / `SYNCOBJ_FD_TO_HANDLE` (`drm.h`, core DRM,
    /// not driver-private) -- like PRIME dma-buf export/import above, these
    /// need process fd table access the DRM inode's `io_control` doesn't
    /// have. See [`linux_object::fs::SyncobjHandle`]'s module doc for what
    /// "export" means here: the syncobj table is a single global handle
    /// space (not per-process), so this just carries the already-globally-
    /// valid handle number across the fd boundary -- it doesn't move or
    /// copy any state, and closing the fd does NOT destroy the syncobj.
    /// Gated on the same `nvidia.nouveau_uapi` opt-in as the rest of the
    /// syncobj ioctls (`drm_scheme.rs`).
    fn sys_drm_syncobj_fd(&self, request: usize, arg1: usize) -> Result<Option<usize>, LxError> {
        // Match by ioctl NR + type only, IGNORING the encoded struct size.
        // `struct drm_syncobj_handle`'s UABI is append-only: its first 16 bytes
        // (handle@0, flags@4, fd@8, pad@12) are frozen, but newer kernels grew
        // the tail, bumping the size the ioctl number encodes -- Mesa now sends
        // 0xC018_64C1 (size 24) for HANDLE_TO_FD. A size-pinned 0xC010_64C1
        // silently misses it -> falls through to nouveau dispatch -> ENOSYS ->
        // the GL client's fence export fails -> its EXEC wait times out ->
        // VK_ERROR_DEVICE_LOST. We only ever touch the frozen 16-byte prefix
        // below, so accepting any size is safe. Same bug class and fix as the
        // deadline-sized SYNCOBJ_WAIT ioctls handled in `drm_scheme.rs`.
        const SYNCOBJ_HANDLE_TO_FD_NR: usize = 0xc1; // DRM_IOWR(0xc1, drm_syncobj_handle)
        const SYNCOBJ_FD_TO_HANDLE_NR: usize = 0xc2; // DRM_IOWR(0xc2, drm_syncobj_handle)
        const DRM_IOCTL_TYPE: usize = 0x64; // 'd'

        let nr = request & 0xff;
        if (request >> 8) & 0xff != DRM_IOCTL_TYPE
            || (nr != SYNCOBJ_HANDLE_TO_FD_NR && nr != SYNCOBJ_FD_TO_HANDLE_NR)
        {
            return Ok(None);
        }
        let is_handle_to_fd = nr == SYNCOBJ_HANDLE_TO_FD_NR;
        if !kernel_hal::drivers::nouveau_uapi_enabled() {
            return Ok(None);
        }

        let mut ptr = UserInOutPtr::<DrmSyncobjHandle>::from(arg1);
        let mut h = match ptr.read() {
            Ok(h) => h,
            Err(e) => {
                warn!("[drm] SYNCOBJ read(args @ {:#x}) EFAULT: {:?}", arg1, e);
                return Err(e.into());
            }
        };
        syncobj_fd_args_check(h.flags, h.pad)?;
        drm_syncobj_fd_op(
            self.linux_process(),
            self.zircon_process().id(),
            is_handle_to_fd,
            &mut h,
        )?;
        // `DRM_IOWR`: the struct goes back whichever way round, with the new
        // fd or the new handle in it.
        if let Err(e) = ptr.write(h) {
            warn!(
                "[drm] SYNCOBJ_{} write-back EFAULT: {:?}",
                if is_handle_to_fd {
                    "HANDLE_TO_FD"
                } else {
                    "FD_TO_HANDLE"
                },
                e
            );
            return Err(e.into());
        }
        Ok(Some(0))
    }

    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (`export`) and `IMPORT_SYNC_FILE` on
    /// a dma-buf fd (`<linux/dma-buf.h>`): the fences on the buffer as a new
    /// sync_file fd, close-on-exec, or a sync_file's fence put on the buffer.
    /// The flags are read first (`sync_file_usage`), then the fd; a target
    /// that is not a dma-buf gets `ENOTTY` (the ioctl is unknown to that
    /// file), an fd that is not a sync_file, or not an fd at all, `EINVAL`
    /// (`sync_file_get_fence` finds no fence), both as in
    /// `dma_buf_export_sync_file` / `dma_buf_import_sync_file`. Neither
    /// existed: a dma-buf answered `ENOTTY` to both, so a client that fenced
    /// its buffer for an implicit-sync consumer (Xwayland's window copies,
    /// Mesa's WSI without `linux-drm-syncobj-v1`) was refused and the
    /// consumer read the buffer whenever it liked.
    fn sys_dma_buf_sync_file(
        &self,
        file: &Arc<dyn FileLike>,
        export: bool,
        arg1: usize,
    ) -> SysResult {
        use linux_object::fs::{sync_file_usage, DmaBuf, SyncobjHandle};

        // struct dma_buf_export_sync_file / dma_buf_import_sync_file
        // { __u32 flags; __s32 fd; }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct DmaBufSyncFile {
            flags: u32,
            fd: i32,
        }

        let Some(dmabuf) = file.downcast_ref::<DmaBuf>() else {
            return Err(LxError::ENOTTY);
        };
        let mut ptr = UserInOutPtr::<DmaBufSyncFile>::from(arg1);
        let mut data = ptr.read()?;
        let write = sync_file_usage(data.flags)?;
        let proc = self.linux_process();
        if export {
            // `SyncobjHandle` opens `O_CLOEXEC`, which is what
            // `get_unused_fd_flags(O_CLOEXEC)` gives the new fd.
            let sync_file = dmabuf.export_sync_file(write);
            let fd = proc.add_file(sync_file)?;
            data.fd = i32::from(fd);
            ptr.write(data)?;
            return Ok(0);
        }
        let target = proc
            .get_file_like(FileDesc::from(data.fd as usize))
            .map_err(|_| LxError::EINVAL)?;
        let fence = target
            .downcast_ref::<SyncobjHandle>()
            .ok_or(LxError::EINVAL)?;
        dmabuf.import_sync_file(write, fence)?;
        Ok(0)
    }

    /// `SYNC_IOC_MERGE` (`<linux/sync_file.h>`): a new sync_file fd that is
    /// ready once both the target fd and `fd2` are. Both must be sync_files:
    /// a target that is not one gets `ENOTTY` (the ioctl is unknown to that
    /// file), an `fd2` that is not one gets `ENOENT`, as in Linux.
    ///
    /// This is what Mesa's window-system code calls on every acquire of a
    /// swapchain image that has been presented before, to build the acquire
    /// semaphore out of the compositor's release fence and the image's last
    /// present fence. It had no implementation at all, so the acquire failed
    /// after the first `image_count` frames and zink killed the GLX
    /// swapchain; see `SyncobjHandle::merge`.
    fn sys_sync_file_merge(&self, file: &Arc<dyn FileLike>, arg1: usize) -> SysResult {
        use linux_object::fs::SyncobjHandle;

        // A syncobj fd has no ioctl at all (`drm_syncobj_file_fops`), and no
        // other file knows this one.
        let Some(first) = file.downcast_ref::<SyncobjHandle>() else {
            return Err(LxError::ENOTTY);
        };
        let mut ptr = UserInOutPtr::<SyncMergeData>::from(arg1);
        let mut data = ptr.read()?;
        sync_file_merge_op(
            self.linux_process(),
            self.zircon_process().id(),
            first,
            &mut data,
        )?;
        ptr.write(data)?;
        Ok(0)
    }

    /// `SYNCOBJ_EVENTFD` (`drm.h`, core DRM): signal an eventfd when syncobj
    /// `handle` reaches `point`. Needs the eventfd from the process fd table
    /// (like the FD ioctls above), so it is dispatched here, not in the DRM
    /// inode's `io_control`; the waiter table and delivery live in
    /// [`linux_object::fs::register_syncobj_eventfd`]. This is the wait side of
    /// wlroots' `linux-drm-syncobj-v1` explicit sync. Gated on the same
    /// `nvidia.nouveau_uapi` opt-in as the rest of the syncobj ioctls.
    fn sys_drm_syncobj_eventfd(
        &self,
        request: usize,
        arg1: usize,
    ) -> Result<Option<usize>, LxError> {
        // NR + type only; the encoded struct size is not part of the match.
        // Both halves of the pair come from the dispatch table `sys_ioctl`
        // already decided with: a second copy written out here would refuse
        // in silence -- `Ok(None)`, which falls through to nouveau dispatch --
        // anything the table had let in. The byte count in that table is the
        // size of `DrmSyncobjEventfd`, which is what this method parses;
        // `drm_ioctl_layout_tests` holds the two together.
        let (eventfd_nr, eventfd_min) = linux_object::fs::devfs::drm_scheme::nr::SYNCOBJ_EVENTFD;
        if !linux_object::fs::devfs::drm_scheme::is_drm_ioctl_nr(
            request as u32,
            eventfd_nr,
            eventfd_min,
        ) {
            return Ok(None);
        }
        if !kernel_hal::drivers::nouveau_uapi_enabled() {
            return Ok(None);
        }

        let ptr = UserInPtr::<DrmSyncobjEventfd>::from(arg1);
        let req = match ptr.read() {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "[drm] SYNCOBJ_EVENTFD read(args @ {:#x}) EFAULT: {:?}",
                    arg1, e
                );
                return Err(e.into());
            }
        };
        // WAIT_AVAILABLE: fire when EXEC has attached a fence covering `point`,
        // even if the GPU has not written the landing zone yet. Same bit as
        // SYNCOBJ_WAIT; NVK's WAIT_PENDING / explicit-sync path uses it.
        const WAIT_AVAILABLE: u32 = 1 << 2;
        let wait_available = req.flags & WAIT_AVAILABLE != 0;
        // Argument validation first, exactly where `drm_syncobj_eventfd_ioctl`
        // does it: an unknown flag bit or a non-zero pad is EINVAL before the
        // handle is ever looked up. WAIT_AVAILABLE is the only flag core DRM
        // defines for this ioctl.
        if req.flags & !WAIT_AVAILABLE != 0 || req.pad != 0 {
            return Err(LxError::EINVAL);
        }
        // Resolve both preconditions up front so the trace below can report the
        // EXACT reason, then apply them in order.
        // ...and it must be the caller's own (`drm_syncobj_find` in the
        // caller's file): another process's handle is ENOENT.
        let live =
            kernel_hal::drivers::scheme::syncobj::usable_by(self.zircon_process().id(), req.handle);
        // The target must be an eventfd: delivery is a `write` of 1, which for
        // any other FileLike would mean something else entirely.
        let proc = self.linux_process();
        let ev = proc.get_file_like(FileDesc::from(req.fd as usize));
        let is_eventfd = ev
            .as_ref()
            .ok()
            .map(|e| e.downcast_ref::<linux_object::fs::EventFd>().is_some())
            .unwrap_or(false);
        let outcome = if !live {
            "ENOENT: handle not a live syncobj"
        } else if ev.is_err() {
            "EBADF: fd not in the process fd table"
        } else if !is_eventfd {
            "EINVAL: fd is not an eventfd"
        } else {
            "OK: waiter armed"
        };
        // Bounded, ERROR-level (always console-visible, same as `einval-hunt`)
        // trace of the first 32 arm attempts. klog_info did not surface on the
        // rig's console; the `einval-hunt` error! line does, so match it.
        // The `einval-hunt` sees SYNCOBJ_EVENTFD fail but not WHY, and this is
        // the compositor's explicit-sync WAIT: wlroots arms one per
        // GPU-client frame to learn when the client's render fence has landed.
        // If it fails, the compositor never waits on that fence and may sample
        // the client's dma-buf MID-RENDER -- block-structured garbage on a GPU
        // client (vkcube) while the compositor's own CPU-blit surfaces stay
        // clean. Bounded because a per-frame console storm has wedged spinlocks
        // before (see the EXEC-failure dedup in nouveau_uapi.rs).
        {
            static ARM_TRACE_BUDGET: core::sync::atomic::AtomicU32 =
                core::sync::atomic::AtomicU32::new(0);
            if ARM_TRACE_BUDGET.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 32 {
                error!(
                    "[drm] SYNCOBJ_EVENTFD pid={} handle={} point={} fd={} live_syncobj={} is_eventfd={} -> {}",
                    self.zircon_process().id(),
                    req.handle,
                    req.point,
                    req.fd,
                    live,
                    is_eventfd,
                    outcome,
                );
            }
        }
        if !live {
            // ENOENT, not EINVAL. `drm_syncobj_eventfd_ioctl` returns `-ENOENT`
            // when `drm_syncobj_find` misses, and wlroots does not just tolerate
            // that errno -- it DEPENDS on it. Its support probe calls this ioctl
            // with a deliberately impossible request, `{handle = 0, flags = 0,
            // point = 0, fd = -1}`, and concludes the kernel implements
            // SYNCOBJ_EVENTFD only if the failure is exactly ENOENT (a kernel
            // without the ioctl answers EINVAL/ENOTTY). Answering EINVAL made
            // us look like that older kernel, so wlroots switched
            // `linux-drm-syncobj-v1` off and every GPU client fell back to
            // implicit sync. The probe is visible in the boot log as the
            // `handle=0 point=0 fd=-1` arm attempt, which is why it is worth
            // saying out loud that a handle of 0 here is not a bug in the
            // caller -- it is the caller asking us a question.
            return Err(LxError::ENOENT);
        }
        let ev = ev?;
        if !is_eventfd {
            return Err(LxError::EINVAL);
        }
        // The waiter holds this Arc, so the eventfd stays alive until delivered.
        linux_object::fs::register_syncobj_eventfd(req.handle, req.point, ev, wait_available);
        Ok(Some(0))
    }

    /// Set parameters of device files.
    pub async fn sys_ioctl(
        &self,
        fd: FileDesc,
        request: usize,
        arg1: usize,
        arg2: usize,
        arg3: usize,
    ) -> SysResult {
        info!(
            "ioctl: fd={:?}, request={:#x}, args=[{:#x}, {:#x}, {:#x}]",
            fd, request, arg1, arg2, arg3
        );
        let proc = self.linux_process();
        let file_like = match proc.get_file_like(fd) {
            Ok(f) => f,
            Err(e) => {
                // A DRM ioctl ('d' = 0x64 family) on an fd that is NOT in the
                // fd table would otherwise fail EBADF with zero trace — make it
                // visible: this is how a stale/ghost fd in userspace shows up.
                if (request as u32 >> 8) & 0xff == 0x64 {
                    error!(
                        "[drm] pid={} ioctl {:#x}: fd {:?} NOT in fd table -> {:?}",
                        self.zircon_process().id(),
                        request as u32,
                        fd,
                        e
                    );
                }
                return Err(e);
            }
        };
        // The one ioctl in the tree that has to BLOCK. `INode::io_control` is
        // synchronous, so a wait cannot happen inside it -- and the previous
        // attempt to wait anyway (a 16.7 ms busy spin) starved every other
        // coroutine on the CPU and looked like the machine freezing. Here we
        // are in the async syscall dispatcher, so the sleep is a real one; the
        // ioctl arm below then reports the sequence that genuinely completed.
        //
        // Gated on the fd actually being a DRM device, not on the request
        // number alone: sleeping is a side effect, and it must not be possible
        // to inflict it on an unrelated fd that happens to be handed this
        // number.
        let is_wait_vblank = {
            use linux_object::fs::devfs::drm_scheme::{is_drm_ioctl_nr, nr};
            let (n, min) = nr::WAIT_VBLANK;
            is_drm_ioctl_nr(request as u32, n, min)
        };
        if is_wait_vblank {
            if let Some(file) = file_like.downcast_ref::<File>() {
                if let Some(dev) = file
                    .inode()
                    .as_any_ref()
                    .downcast_ref::<linux_object::fs::devfs::DrmDev>()
                {
                    dev.wait_vblank_sleep(arg1).await?;
                }
            }
        }
        // Same async-sleep-before-sync-io_control pattern for SYNCOBJ_WAIT /
        // TIMELINE_WAIT (classic + deadline-sized). Without this the sync arm
        // spin-polls the whole timeout and pegs a core — the same starvation
        // WAIT_VBLANK used to cause.
        {
            let syncobj_cmd = request as u32;
            if linux_object::fs::devfs::drm_scheme::is_syncobj_wait_ioctl(syncobj_cmd) {
                if let Some(file) = file_like.downcast_ref::<File>() {
                    if let Some(dev) = file
                        .inode()
                        .as_any_ref()
                        .downcast_ref::<linux_object::fs::devfs::DrmDev>()
                    {
                        dev.syncobj_wait_sleep(syncobj_cmd, arg1).await?;
                    }
                }
            }
        }
        // Same pattern again for an atomic commit carrying IN_FENCE_FD: wait
        // for the client's rendering to land before the sync arm scans that
        // buffer out. Without this the fence was accepted and ignored, so an
        // explicit-sync compositor could have a half-drawn frame presented.
        let is_mode_atomic = {
            use linux_object::fs::devfs::drm_scheme::{is_drm_ioctl_nr, nr};
            let (n, min) = nr::MODE_ATOMIC;
            is_drm_ioctl_nr(request as u32, n, min)
        };
        if is_mode_atomic {
            if let Some(file) = file_like.downcast_ref::<File>() {
                if let Some(dev) = file
                    .inode()
                    .as_any_ref()
                    .downcast_ref::<linux_object::fs::devfs::DrmDev>()
                {
                    dev.atomic_in_fence_sleep(arg1).await?;
                }
            }
        }
        // And `GEM_CPU_PREP`, the one driver-private ioctl that blocks by
        // contract: Mesa calls it whenever it recycles or reads back a
        // buffer, and the sync arm can only busy-wait for the GPU, pegging a
        // core per call. Same split as the three above.
        if linux_object::fs::devfs::drm_scheme::is_cpu_prep_ioctl(request as u32) {
            if let Some(file) = file_like.downcast_ref::<File>() {
                if let Some(dev) = file
                    .inode()
                    .as_any_ref()
                    .downcast_ref::<linux_object::fs::devfs::DrmDev>()
                {
                    dev.cpu_prep_sleep(request as u32, arg1).await?;
                }
            }
        }
        // And the implicit-sync half, for the legacy presents this kernel
        // actually runs (`drm.atomic` is opt-in, so wlroots drives SETCRTC and
        // PAGE_FLIP). Neither ioctl carries a fence, so the buffer used to be
        // scanned out the instant the ioctl arrived -- while the GPU could
        // still be drawing into it. The wait comes from the driver's own
        // per-owner ring fence; see `DrmDev::present_fence_sleep`.
        if linux_object::fs::devfs::drm_scheme::legacy_present_kind(request as u32).is_some() {
            if let Some(file) = file_like.downcast_ref::<File>() {
                if let Some(dev) = file
                    .inode()
                    .as_any_ref()
                    .downcast_ref::<linux_object::fs::devfs::DrmDev>()
                {
                    dev.present_fence_sleep(request as u32, arg1).await?;
                }
            }
        }
        // The filesystem ioctls of an EclipseFS mount (chattr, reflink,
        // snapshots, quotas, scrub, devices): some carry another descriptor
        // or a device path, which only the process can resolve.
        if let Some(file) = file_like.downcast_ref::<File>() {
            let ctx = FsIoctlCtx { proc };
            if let Some(res) = linux_object::fs::eclipsefs_ioctl(file, request as u32, arg1, &ctx) {
                return res;
            }
            // FICLONE family and FS_IOC_FIEMAP are answered at the VFS layer on Linux.
            // Without a supporting mount, falling through hits sfs's unknown-cmd
            // → EINVAL, which `cp`/`apt`/`dpkg` do not treat as "copy instead" (they want EOPNOTSUPP).
            if linux_object::fs::is_vfs_clone_ioctl(request as u32)
                || request as u32 == linux_object::fs::FS_IOC_FIEMAP
            {
                return Err(LxError::EOPNOTSUPP);
            }
        }
        // File ioctls served at the VFS layer, como en Linux (fs/ioctl.c
        // `do_vfs_ioctl`): they apply to EVERY fd kind — pipes, sockets,
        // files, device nodes — so the per-inode handlers never need to know
        // them. Rust's std is the load-bearing caller: `Command::output()`
        // sets both child report pipes nonblocking via `ioctl(FIONBIO)`
        // (sys/pal/unix/fd.rs `set_nonblocking`), and `TcpStream/UdpSocket::
        // set_nonblocking` do the same on sockets. With no VFS handling,
        // FIONBIO on a pipe fell through to the inode default -> ENOSYS ->
        // the ENOTTY normalization below -> std's `read_output` propagated
        // "Not a tty" into the `res.unwrap()` inside `Command::output()`
        // (library/std/src/sys/process/mod.rs) -> SIGABRT. That was
        // lunarbar's respawn crash loop: its volume probe runs
        // `wpctl`/`amixer` through `Command::output()` on every refresh
        // (tools/lunarbar/src/sysinfo.rs).
        //
        // Exact small values, never sign-extended (bit 31 clear), so there is
        // no collision with the DRM 0xC0xx_64xx family handled below. The
        // numbers come from `linux_object::fs::ioctl`, which is the one place
        // this kernel writes them: they used to be declared again right here,
        // and a second copy of an ABI number is a copy that can drift.
        match request {
            // FIONREAD/SIOCINQ: bytes waiting to be read. Legal on sockets and
            // pipes, and answered per object kind (see `FileLike::
            // readable_bytes`) -- an object with nothing to say falls through
            // to its own handler below.
            //
            // Unanswered, this reached `handle_net_ioctl`, which has no arm for
            // it, so a socket got ENOSYS -> the ENOTTY normalisation below ->
            // "Not a tty" for a perfectly ordinary question about a socket.
            // That is the errno Firefox's Wayland proxy died on:
            //   Warning: ProxiedConnection::TransferOrQueue() broken source
            //            socket : Not a tty
            FIONREAD => {
                // `SIOCINQ` on a listening stream is `EINVAL` (unix(7)/tcp(7)).
                // Without the check, a listener answered 0 as if empty.
                if file_like
                    .as_socket()
                    .map(|s| s.is_listening())
                    .unwrap_or(false)
                {
                    return Err(LxError::EINVAL);
                }
                if let Some(n) = file_like.readable_bytes() {
                    let mut out: UserOutPtr<i32> = arg1.into();
                    out.write(n.min(i32::MAX as usize) as i32)?;
                    return Ok(0);
                }
            }
            FIONBIO => {
                // The argument is a pointer to int: nonzero = O_NONBLOCK on.
                let on: UserInPtr<i32> = arg1.into();
                let on = on.read()? != 0;
                let mut flags = file_like.flags();
                flags.set(OpenFlags::NON_BLOCK, on);
                file_like.set_flags(flags)?;
                return Ok(0);
            }
            // Same shape as FIONBIO: nonzero = O_ASYNC / FASYNC on. The
            // ENOTTY remap below already exempted 0x5452 as a "VFS ioctl
            // answered elsewhere", but there was no arm here, so the call
            // still fell through to ENOTTY.
            FIOASYNC => {
                let on: UserInPtr<i32> = arg1.into();
                let on = on.read()? != 0;
                let mut flags = file_like.flags();
                flags.set(OpenFlags::ASYNC, on);
                file_like.set_flags(flags)?;
                return Ok(0);
            }
            // Close-on-exec lives in the per-process fd table, exactly like
            // fcntl F_SETFD — never in the shared File object.
            FIOCLEX => {
                proc.set_fd_cloexec(fd, true)?;
                return Ok(0);
            }
            FIONCLEX => {
                proc.set_fd_cloexec(fd, false)?;
                return Ok(0);
            }
            _ => {}
        }
        // DRM PRIME (dma-buf) export/import and CREATE_LEASE — need process fd
        // access, so they are handled here rather than in the DRM inode's
        // io_control.
        //
        // libdrm defines these request numbers via `_IOWR(...)`, whose value
        // has bit 31 set (direction `_IOC_READ|_IOC_WRITE` == 0xC0...) and is
        // typed `int`. Passed to the variadic `ioctl(int, unsigned long, ...)`
        // it sign-extends to 64 bits — e.g. CREATE_LEASE arrives as
        // 0xFFFF_FFFF_C018_64C6, not 0xC018_64C6. The inode-level `io_control`
        // path never saw this because it takes `cmd: u32` (truncated), but here
        // `request` is the full 64-bit value, so mask to 32 bits before
        // matching. Without this the dispatch misses and the ioctl falls
        // through to ENOTTY ("Not a tty").
        let cmd = request as u32 as usize;
        // `/dev/nvidiactl` and `/dev/nvidia<N>`: the fd table and the caller's
        // credentials are part of the escape (see `nvidia.rs`).
        #[cfg(target_arch = "x86_64")]
        if let Some(ret) = self.sys_nvidia_ioctl(&file_like, request, arg1) {
            return ret;
        }
        let is_prime_or_lease = {
            use linux_object::fs::devfs::drm_scheme::{is_drm_ioctl_nr, nr};
            let c = cmd as u32;
            [
                nr::PRIME_FD_TO_HANDLE,
                nr::PRIME_HANDLE_TO_FD,
                nr::MODE_CREATE_LEASE,
            ]
            .iter()
            .any(|&(n, min)| is_drm_ioctl_nr(c, n, min))
        };
        if is_prime_or_lease {
            // `sys_drm_prime` logs only on genuine failures; the wrapper stays
            // silent on the hot path. Ok(None) means "not a PRIME request after
            // all"; fall through to the inode `io_control`.
            match self.sys_drm_prime(&file_like, cmd, arg1) {
                Ok(Some(ret)) => {
                    trail_drm(cmd as u32, arg1, ret as i64);
                    return Ok(ret);
                }
                Ok(None) => {}
                Err(e) => {
                    trail_drm(cmd as u32, arg1, -(e as i64));
                    return Err(e);
                }
            }
        }
        // SYNC_IOC_MERGE on a sync_file fd: needs the fd table (a second fd in,
        // a new fd out), so it is dispatched here like the DRM fd ioctls.
        if is_sync_ioc_merge(request as u32) {
            return self.sys_sync_file_merge(&file_like, arg1);
        }
        // DMA_BUF_IOCTL_EXPORT_SYNC_FILE / IMPORT_SYNC_FILE on a dma-buf fd:
        // a sync_file fd out, or one in -- the fd table again.
        if let Some(export) = dma_buf_sync_file_ioctl(request as u32) {
            return self.sys_dma_buf_sync_file(&file_like, export, arg1);
        }
        // SYNCOBJ_HANDLE_TO_FD / SYNCOBJ_FD_TO_HANDLE — same fd-table-access
        // reasoning and sign-extension caveat as PRIME above. Match by ioctl NR
        // (0xc1/0xc2) + type ('d'=0x64) only, IGNORING the encoded struct size:
        // `struct drm_syncobj_handle` is UABI-frozen only in its first 16 bytes
        // and newer kernels grew it (Mesa now sends 0xC018_64C1 = size 24),
        // shifting the ioctl number a size-pinned constant would miss -> the
        // request would fall through to nouveau dispatch -> ENOSYS -> the GL
        // client's fence export fails -> VK_ERROR_DEVICE_LOST. `sys_drm_syncobj_fd`
        // re-validates NR + type before touching the (frozen) 16-byte prefix.
        if ((cmd >> 8) & 0xff) == 0x64 && matches!(cmd & 0xff, 0xc1 | 0xc2) {
            match self.sys_drm_syncobj_fd(cmd, arg1) {
                Ok(Some(ret)) => {
                    trail_drm(cmd as u32, arg1, ret as i64);
                    return Ok(ret);
                }
                Ok(None) => {}
                Err(e) => {
                    trail_drm(cmd as u32, arg1, -(e as i64));
                    return Err(e);
                }
            }
        }
        // SYNCOBJ_EVENTFD — same fd-table-access reasoning as the syncobj FD
        // ioctls above (it takes an eventfd), and the same sign-extension caveat.
        let is_syncobj_eventfd = {
            use linux_object::fs::devfs::drm_scheme::{is_drm_ioctl_nr, nr};
            let (n, min) = nr::SYNCOBJ_EVENTFD;
            is_drm_ioctl_nr(cmd as u32, n, min)
        };
        if is_syncobj_eventfd {
            match self.sys_drm_syncobj_eventfd(cmd, arg1) {
                Ok(Some(ret)) => {
                    trail_drm(cmd as u32, arg1, ret as i64);
                    return Ok(ret);
                }
                Ok(None) => {}
                Err(e) => {
                    trail_drm(cmd as u32, arg1, -(e as i64));
                    return Err(e);
                }
            }
        }
        // `TIOCSCTTY` on a pts: adopt the pty as the controlling terminal.
        // Linux sets the tty's foreground process group to the caller's pgrp
        // (`tty_jobctrl.c`), which is what makes a `login_tty()`'d shell's very
        // FIRST `tcgetpgrp()` see itself as the foreground job and go straight
        // to the prompt instead of looping in busybox ash's
        // `killpg(0, SIGTTIN)` background wait. The pty inode's own ioctl
        // handler cannot do this — it has no process context — so seed it here.
        if cmd == TIOCSCTTY {
            if let Some(file) = file_like.downcast_ref::<linux_object::fs::File>() {
                let inode = file.inode();
                if let Some(slave) = inode
                    .as_any_ref()
                    .downcast_ref::<linux_object::fs::pty::PtySlave>()
                {
                    if let Ok(pgid) =
                        linux_object::process::get_process_pgid(self.zircon_process().id())
                    {
                        slave.set_fg_pgrp(pgid as i32);
                    }
                    return Ok(0);
                }
            }
        }
        // `TIOCGPTPEER` on a pty master: open its slave end and return a NEW
        // fd (Linux 4.13+). Needs the fd table, so it lives here rather than
        // in the inode's ioctl. `arg1` carries the open flags (O_RDWR |
        // O_NOCTTY | O_CLOEXEC from rustix-openpty / glibc `openpty`).
        if cmd == TIOCGPTPEER {
            if let Some(file) = file_like.downcast_ref::<linux_object::fs::File>() {
                let inode = file.inode();
                if let Some(master) = inode
                    .as_any_ref()
                    .downcast_ref::<linux_object::fs::pty::PtyMaster>()
                {
                    let slave = master.open_peer().ok_or(LxError::ENXIO)?;
                    // Same open(2) flag word as `openat`: reject unnamed bits
                    // (O_PATH, O_TMPFILE, …) instead of truncating them into
                    // a normal peer fd.
                    let flags = super::open_flags(arg1)?;
                    let path = alloc::format!("/dev/pts/{}", master.pty_id());
                    let peer = linux_object::fs::File::new(slave, flags, path);
                    let fd = proc.add_file(peer)?;
                    return Ok(fd.into());
                }
            }
        }
        // `TIOCGWINSZ` (get terminal window size).
        let ret = match file_like.ioctl(request, arg1, arg2, arg3) {
            // Some programs insist on a valid window size and keep retrying
            // `TIOCGWINSZ` when it fails on a char device that is not a full
            // tty backend (e.g. DRM/fb). Only synthesize a size for char
            // devices that are not input nodes — pipes/sockets/regular files
            // must get `ENOTTY` so `isatty()` does not lie.
            //
            // Input device nodes (`/dev/input/mice`, `event*`) are excluded:
            // faking a window size there makes musl's `isatty()` (a TIOCGWINSZ
            // probe) report a tty, and kdrive/TinyX then treats the mouse as a
            // serial port and loops over serial mouse protocols.
            Err(LxError::ENOSYS) | Err(LxError::ENOTTY) | Err(LxError::EINVAL)
                if request == TIOCGWINSZ
                    && arg1 != 0
                    && file_like.is_char_device()
                    && !file_like.is_input_device() =>
            {
                let mut ws = kernel_hal::console::console_win_size();
                if ws.ws_col == 0 {
                    ws.ws_col = 80;
                }
                if ws.ws_row == 0 {
                    ws.ws_row = 25;
                }
                let mut ptr: UserOutPtr<kernel_hal::console::ConsoleWinSize> = arg1.into();
                ptr.write(ws)?;
                Ok(0)
            }
            // A tty-class request on something that is not a char device
            // answers ENOTTY on Linux: the file's handler simply does not
            // know the request. Some of our inode impls say EINVAL instead
            // (sfs maps every unknown cmd to `FsError::IOCTLError` ->
            // EINVAL), which musl's `isatty()` tolerates but errno-checking
            // callers do not: that was the `[einval-hunt] IOCTL a1=0x5413
            // (TIOCGWINSZ) on fd 2 -> EINVAL` hit against a service's
            // log-file stderr. Which requests the block covers, and which
            // file ioctls inside it are left alone, is
            // `tty_block_wants_enotty`.
            Err(LxError::EINVAL)
                if tty_block_wants_enotty(request) && !file_like.is_char_device() =>
            {
                Err(LxError::ENOTTY)
            }
            // An unhandled ioctl maps to `ENOSYS` ("function not implemented")
            // via the generic FsError conversion, but the POSIX/Linux convention
            // for an ioctl that does not apply to a device is `ENOTTY`
            // ("inappropriate ioctl for device"). Returning `ENOSYS` makes some
            // programs treat it as fatal or retry in a loop, so normalise it.
            Err(LxError::ENOSYS) => Err(LxError::ENOTTY),
            other => other,
        };
        // Surface a genuinely unhandled/failed ioctl on the console (and serial),
        // the same way an invalid syscall number is — but throttle a program
        // busy-looping on the same failing request so it can't flood the console
        // thousands of times a second: log the first occurrence, then only every
        // 4096th repeat. `TIOCGWINSZ` returning `ENOTTY` is the normal "not a
        // terminal" answer (musl's `isatty()` probes devices this way), so it is
        // not logged at all.
        if let Err(e) = &ret {
            // ALSA HW_REFINE EINVAL is the `*_near` search; don't treat it as
            // an unhandled ioctl (see einval_hunt in lib.rs).
            let alsa_hw_refine = {
                let cmd = request as u32;
                ((cmd >> 8) & 0xff) == b'A' as u32 && (cmd & 0xff) == 0x10
            };
            if request != TIOCGWINSZ && !alsa_hw_refine {
                use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
                static LAST_REQ: AtomicU64 = AtomicU64::new(u64::MAX);
                static REPEATS: AtomicUsize = AtomicUsize::new(0);
                let n = if LAST_REQ.swap(request as u64, Ordering::Relaxed) == request as u64 {
                    REPEATS.fetch_add(1, Ordering::Relaxed) + 1
                } else {
                    REPEATS.store(0, Ordering::Relaxed);
                    0
                };
                if n == 0 || n % 4096 == 0 {
                    // warn, not error: many programs probe optional ioctls and
                    // handle the failure themselves (EOPNOTSUPP/ENOTTY is a
                    // legitimate answer). Boot with LOG=warn to see these when
                    // chasing a missing ioctl.
                    warn!(
                        "ioctl fd={:?} request={:#x} -> ERR {:?} (unhandled/failed){}",
                        fd,
                        request,
                        e,
                        if n > 0 { " [repeating, throttled]" } else { "" }
                    );
                }
            }
        }
        ret
    }

    /// Manipulate a file descriptor.
    /// - cmd – cmd flag
    /// - arg – additional parameters based on cmd
    pub async fn sys_fcntl(&self, fd: FileDesc, cmd: usize, arg: usize) -> SysResult {
        info!("fcntl: fd={:?}, cmd={}, arg={}", fd, cmd, arg);
        // memfd file seals (`F_LINUX_SPECIFIC_BASE + 9/10`), real ones. These
        // used to be accepted as no-ops that reported "no seals set", on the
        // reasoning that there is a single trusted address space. But a seal
        // the kernel denies having is worse than one it refuses to take:
        // Firefox seals every shared-memory segment against shrinking and then
        // refuses to map any segment whose F_GET_SEALS does not report it
        // (`Platform::IsSafeToMap`), so the no-op answer failed every IPC
        // buffer and every frame.
        const F_ADD_SEALS: usize = 1033;
        const F_GET_SEALS: usize = 1034;
        const F_SETPIPE_SZ: usize = 1031;
        const F_GETPIPE_SZ: usize = 1032;
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        if cmd == F_ADD_SEALS || cmd == F_GET_SEALS {
            // Seals exist only on memfds; every other fd answers EINVAL, as
            // Linux does for a filesystem without sealing support.
            let inode = file_like
                .downcast_ref::<File>()
                .ok_or(LxError::EINVAL)?
                .inode();
            return if cmd == F_GET_SEALS {
                linux_object::fs::memfd_seals(&inode)
                    .map(|s| s as usize)
                    .ok_or(LxError::EINVAL)
            } else {
                linux_object::fs::memfd_add_seals(&inode, arg as u32).map(|_| 0)
            };
        }
        if matches!(
            cmd,
            F_GETLK | F_SETLK | F_SETLKW | F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW
        ) {
            let get = cmd == F_GETLK || cmd == F_OFD_GETLK;
            let wait = cmd == F_SETLKW || cmd == F_OFD_SETLKW;
            return self.fcntl_record_lock(&file_like, get, wait, arg).await;
        }
        // Pipe capacity (fcntl(2), F_SETPIPE_SZ/F_GETPIPE_SZ): valid only on
        // pipe fds — Linux answers EBADF for other fd kinds.
        if cmd == F_SETPIPE_SZ || cmd == F_GETPIPE_SZ {
            let file = file_like.downcast_ref::<File>().ok_or(LxError::EBADF)?;
            let inode = file.inode();
            let pipe = inode.downcast_ref::<Pipe>().ok_or(LxError::EBADF)?;
            return if cmd == F_GETPIPE_SZ {
                Ok(pipe.capacity())
            } else {
                let size = pipe_size_round(arg)?;
                // EBUSY when more than that is already queued, as
                // `pipe_set_size` answers.
                pipe.set_capacity(size)?;
                // Linux returns the actual (rounded) capacity, not 0.
                Ok(size)
            };
        }
        if let Ok(cmd) = FcntlCmd::try_from(cmd) {
            match cmd {
                // FD_CLOEXEC is per-DESCRIPTOR state, kept in the process's fd
                // table (see LinuxProcessInner::cloexec_fds) — never in the
                // File object, which fork shares between processes.
                FcntlCmd::GETFD => Ok(proc.fd_cloexec(fd)? as usize),
                FcntlCmd::SETFD => {
                    proc.set_fd_cloexec(fd, (arg & 1) != 0)?;
                    Ok(0)
                }
                FcntlCmd::GETFL => Ok(file_like.flags().status().bits()),
                FcntlCmd::SETFL => {
                    // Only the status flags change (`SETFL_MASK`); the raw
                    // argument used to go in whole, so `F_SETFL(O_NONBLOCK)`
                    // also cleared the file's `O_CLOEXEC` record, which is
                    // `F_SETFD`'s to change.
                    let requested = OpenFlags::from_bits_truncate(arg);
                    let next = OpenFlags::after_setfl(file_like.flags(), requested);
                    // Same policy as `pipe2`: no packet mode on pipes.
                    setfl_pipe_direct(&file_like, next)?;
                    file_like.set_flags(next)?;
                    Ok(0)
                }
                FcntlCmd::DUPFD | FcntlCmd::DUPFD_CLOEXEC => {
                    let cloexec = cmd == FcntlCmd::DUPFD_CLOEXEC;
                    Ok(self.sys_dupfd_cloexec(fd, arg, cloexec)?)
                }
                _ => Err(LxError::EINVAL),
            }
        } else {
            Err(LxError::EINVAL)
        }
    }

    /// The lock half of `fcntl(2)`: F_GETLK probes, F_SETLK acquires or
    /// releases in one shot, F_SETLKW retries until the range frees up. The
    /// range table itself lives in [`linux_object::fs::record_lock`].
    async fn fcntl_record_lock(
        &self,
        file_like: &Arc<dyn FileLike>,
        get: bool,
        wait: bool,
        arg: usize,
    ) -> SysResult {
        use core::time::Duration;
        use linux_object::fs::record_lock::{self, LockRequest, F_RDLCK, F_UNLCK, F_WRLCK};

        let mut ptr: UserInOutPtr<Flock> = arg.into();
        let mut fl = ptr.read()?;
        // Record locks apply to files; pipes/sockets answer EBADF like Linux.
        let file = file_like.downcast_ref::<File>().ok_or(LxError::EBADF)?;
        let meta = file.metadata()?;
        let key = (meta.dev, meta.inode);

        // Resolve l_whence + l_start (+ negative l_len) to an absolute range.
        let base = match fl.l_whence {
            0 => 0i64,                                         // SEEK_SET
            1 => file_like.seek(SeekFrom::Current(0))? as i64, // SEEK_CUR
            2 => meta.size as i64,                             // SEEK_END
            _ => return Err(LxError::EINVAL),
        };
        let (start, end) = record_lock::resolve_range(base, fl.l_start, fl.l_len)?;
        let owner = self.zircon_process().id();

        match fl.l_type {
            F_UNLCK if !get => {
                let req = LockRequest {
                    exclusive: false,
                    start,
                    end,
                    owner,
                };
                record_lock::setlk(key, &req, true);
                Ok(0)
            }
            F_RDLCK | F_WRLCK => {
                let req = LockRequest {
                    exclusive: fl.l_type == F_WRLCK,
                    start,
                    end,
                    owner,
                };
                if get {
                    match record_lock::getlk(key, &req) {
                        None => fl.l_type = F_UNLCK,
                        Some(c) => {
                            fl.l_type = c.type_;
                            fl.l_whence = 0;
                            fl.l_start = c.start as i64;
                            fl.l_len = c.len as i64;
                            fl.l_pid = c.pid as i32;
                        }
                    }
                    ptr.write(fl)?;
                    Ok(0)
                } else if wait {
                    // Contended F_SETLKW: retry on a short timer. Lock churn is
                    // rare (package-manager style whole-file locks), so a poll
                    // loop beats wiring a waiter queue through the table.
                    loop {
                        if record_lock::setlk(key, &req, false) {
                            return Ok(0);
                        }
                        // An interruptible wait in Linux: a kill did not end
                        // it, so a process killed in it was reaped and stayed
                        // for as long as the lock it waited on was held.
                        linux_object::process::check_signals()?;
                        kernel_hal::thread::sleep_until(kernel_hal::timer::deadline_after(
                            Duration::from_millis(10),
                        ))
                        .await;
                    }
                } else if record_lock::setlk(key, &req, false) {
                    Ok(0)
                } else {
                    Err(LxError::EAGAIN)
                }
            }
            _ => Err(LxError::EINVAL),
        }
    }

    /// Checks whether the calling process can access the file pathname
    pub fn sys_access(&self, path: UserInPtr<u8>, mode: usize) -> SysResult {
        self.sys_faccessat(FileDesc::CWD, path, mode, 0)
    }

    /// Check user's permissions of a file relative to a directory file descriptor
    pub fn sys_faccessat(
        &self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        mode: usize,
        flags: usize,
    ) -> SysResult {
        let path = path.as_c_str()?;
        let flags = super::dir::at_flags(flags, super::dir::FACCESSAT_FLAGS)?;
        info!(
            "faccessat: dirfd={:?}, path={:?}, mode={:#o}, flags={:?}",
            dirfd, path, mode, flags
        );
        let proc = self.linux_process();
        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        // `AT_EMPTY_PATH` names the file `dirfd` is open on (`do_faccessat`
        // takes `LOOKUP_EMPTY`); without it an empty path is `ENOENT`.
        let use_effective = flags.contains(AtFlags::EACCESS);
        // The walk too is the real user's, unless `AT_EACCESS` says otherwise.
        let walker = if use_effective {
            Walker::Caller
        } else {
            Walker::RealIds
        };
        let inode = if flags.contains(AtFlags::EMPTY_PATH) && path.is_empty() {
            super::dir::inode_of_dirfd(proc, dirfd)?
        } else {
            proc.lookup_inode_at_by(dirfd, path, follow, walker)?
        };
        let metadata = inode.metadata()?;
        let requested = Self::access_mode(mode)?;
        let verdict = proc.check_access(&metadata, requested, use_effective);
        linux_object::fs::fd_permission(verdict, &inode, self.zircon_process())?;
        Ok(0)
    }

    /// Change file mode by descriptor.
    pub fn sys_fchmod(&self, fd: FileDesc, mode: usize) -> SysResult {
        let proc = self.linux_process();
        let inode = proc.get_file(fd)?.inode();
        let mut metadata = inode.metadata()?;
        proc.chmod_metadata(&mut metadata, mode as u16)?;
        set_mode(&inode, &metadata)?;
        Ok(0)
    }

    /// Change file mode relative to a directory file descriptor.
    pub fn sys_fchmodat(
        &self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        mode: usize,
        flags: usize,
    ) -> SysResult {
        let path = path.as_c_str()?;
        let flags = super::dir::at_flags(flags, super::dir::FCHMODAT_FLAGS)?;
        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        let proc = self.linux_process();
        // `AT_EMPTY_PATH` names the file `dirfd` is open on (`do_fchmodat`
        // takes `LOOKUP_EMPTY`); without it an empty path is `ENOENT`.
        let inode = if flags.contains(AtFlags::EMPTY_PATH) && path.is_empty() {
            super::dir::inode_of_dirfd(proc, dirfd)?
        } else {
            proc.lookup_inode_at(dirfd, path, follow)?
        };
        let mut metadata = inode.metadata()?;
        proc.chmod_metadata(&mut metadata, mode as u16)?;
        set_mode(&inode, &metadata)?;
        Ok(0)
    }

    /// Change file owner/group by descriptor.
    pub fn sys_fchown(&self, fd: FileDesc, uid: usize, gid: usize) -> SysResult {
        let proc = self.linux_process();
        let inode = proc.get_file(fd)?.inode();
        let mut metadata = inode.metadata()?;
        proc.chown_metadata(&mut metadata, uid as u32, gid as u32)?;
        inode.set_metadata(&metadata)?;
        Ok(0)
    }

    /// `lchown(2)`: the owner of a symbolic link and not of what it names.
    /// glibc's and musl's `lchown` make it by number here.
    #[cfg(target_arch = "x86_64")]
    pub fn sys_lchown(&self, path: UserInPtr<u8>, uid: usize, gid: usize) -> SysResult {
        let nofollow = AtFlags::SYMLINK_NOFOLLOW.bits();
        self.sys_fchownat(FileDesc::CWD, path, uid, gid, nofollow)
    }

    /// Change file owner/group relative to a directory file descriptor.
    pub fn sys_fchownat(
        &self,
        dirfd: FileDesc,
        path: UserInPtr<u8>,
        uid: usize,
        gid: usize,
        flags: usize,
    ) -> SysResult {
        let path = path.as_c_str()?;
        let flags = super::dir::at_flags(flags, super::dir::FCHOWNAT_FLAGS)?;
        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        let proc = self.linux_process();
        // `AT_EMPTY_PATH` names the file `dirfd` is open on (`do_fchownat`
        // takes `LOOKUP_EMPTY`); without it an empty path is `ENOENT`.
        let inode = if flags.contains(AtFlags::EMPTY_PATH) && path.is_empty() {
            super::dir::inode_of_dirfd(proc, dirfd)?
        } else {
            proc.lookup_inode_at(dirfd, path, follow)?
        };
        let mut metadata = inode.metadata()?;
        proc.chown_metadata(&mut metadata, uid as u32, gid as u32)?;
        inode.set_metadata(&metadata)?;
        Ok(0)
    }

    /// `utimes(2)`: set a file's access and modification times from a pair of
    /// `struct timeval` (microseconds), the older sibling of `utimensat`.
    ///
    /// Not a legacy curiosity: glibc's `utimes` is this syscall (musl's is
    /// `utimensat`), so on a glibc userspace every archive extractor and
    /// installer that restores timestamps calls it -- Firefox's font cache
    /// unpacking hit it and logged "Unable to revert mtime" for every
    /// directory it touched.
    #[cfg(target_arch = "x86_64")]
    pub fn sys_utimes(
        &mut self,
        pathname: UserInPtr<u8>,
        times: UserInPtr<[linux_object::time::TimeVal; 2]>,
    ) -> SysResult {
        info!("utimes: pathname={:?}, times={:?}", pathname, times);
        let path = pathname.as_c_str()?;
        let inode = self.linux_process().lookup_inode(path)?;
        // A null `times` means "now", exactly as in utimensat; a pair of
        // timevals is a pair of explicit times, microseconds and all
        // (`do_utimes` -> `utimes_common` with `ATTR_ATIME_SET`).
        let times = if times.is_null() {
            None
        } else {
            Some(times.read()?)
        };
        let request = utimes_times(times, TimeSpec::now())?;
        self.apply_utimes(&inode, request)
    }

    /// change file timestamps with nanosecond precision
    pub fn sys_utimensat(
        &mut self,
        dirfd: FileDesc,
        pathname: UserInPtr<u8>,
        times: UserInOutPtr<[TimeSpec; 2]>,
        flags: usize,
    ) -> SysResult {
        info!(
            "utimensat(raw): dirfd: {:?}, pathname: {:?}, times: {:?}, flags: {:#x}",
            dirfd, pathname, times, flags
        );
        let proc = self.linux_process();
        let times = if times.is_null() {
            None
        } else {
            Some(times.read()?)
        };
        let request = utimensat_times(times, TimeSpec::now())?;
        let inode = if pathname.is_null() {
            // `futimens(3)`, which glibc spells `utimensat(fd, NULL, times, 0)`;
            // a flag word here is EINVAL (`do_utimes`), whatever it holds.
            if flags != 0 {
                return Err(LxError::EINVAL);
            }
            info!("futimens: fd: {:?}, times: {:?}", dirfd, times);
            proc.get_file(dirfd)?.inode()
        } else {
            let pathname = pathname.as_c_str()?;
            info!(
                "utimensat: dirfd: {:?}, pathname: {:?}, times: {:?}, flags: {:#x}",
                dirfd, pathname, times, flags
            );
            let flags = super::dir::at_flags(flags, super::dir::UTIMENSAT_FLAGS)?;
            let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
            // `AT_EMPTY_PATH` names the file `dirfd` is open on
            // (`do_utimes_path` takes `LOOKUP_EMPTY`); without it an empty
            // path is `ENOENT`.
            if flags.contains(AtFlags::EMPTY_PATH) && pathname.is_empty() {
                super::dir::inode_of_dirfd(proc, dirfd)?
            } else {
                proc.lookup_inode_at(dirfd, pathname, follow)?
            }
        };
        self.apply_utimes(&inode, request)
    }

    /// The tail every `utime` family call shares once it has the inode and
    /// the [`TimesToSet`]: the permission check of `utimes_common`, then the
    /// metadata write. Both `OMIT` is a success that touches nothing, before
    /// any permission is looked at, as in Linux.
    fn apply_utimes(&self, inode: &Arc<dyn INode>, request: TimesToSet) -> SysResult {
        let (atime, mtime, explicit) = match request {
            TimesToSet::Nothing => return Ok(0),
            TimesToSet::Touch(now) => (Some(now), Some(now), false),
            TimesToSet::Set { atime, mtime } => (atime, mtime, true),
        };
        let mut metadata = inode.metadata()?;
        self.linux_process().check_utimes(&metadata, explicit)?;
        if let Some(t) = atime {
            metadata.atime = rcore_fs::vfs::Timespec {
                sec: t.sec as i64,
                nsec: t.nsec as i32,
            };
        }
        if let Some(t) = mtime {
            metadata.mtime = rcore_fs::vfs::Timespec {
                sec: t.sec as i64,
                nsec: t.nsec as i32,
            };
        }
        inode.set_metadata(&metadata)?;
        Ok(0)
    }

    /// Get filesystem statistics
    /// (see [linux man statfs(2)](https://man7.org/linux/man-pages/man2/statfs.2.html)).
    ///
    /// The `statfs` system call returns information about a mounted filesystem.
    /// `path` is the pathname of **any file** within the mounted filesystem.
    /// `buf` is a pointer to a `StatFs` structure.
    pub fn sys_statfs(&self, path: UserInPtr<u8>, mut buf: UserOutPtr<StatFs>) -> SysResult {
        let path = path.as_c_str()?;
        info!("statfs: path={:?}, buf={:?}", path, buf);

        let inode = self.linux_process().lookup_inode(path)?;
        let info = inode.fs().info();
        buf.write(info.into())?;
        Ok(0)
    }

    /// Get filesystem statistics
    /// (see [linux man statfs(2)](https://man7.org/linux/man-pages/man2/statfs.2.html)).
    ///
    /// The `fstatfs` system call returns information about a mounted filesystem.
    /// `fd` is the descriptor referencing an open file.
    /// `buf` is a pointer to a `StatFs` structure.
    pub fn sys_fstatfs(&self, fd: FileDesc, mut buf: UserOutPtr<StatFs>) -> SysResult {
        info!("statfs: fd={:?}, buf={:?}", fd, buf);

        let info = self.linux_process().get_file(fd)?.inode().fs().info();
        buf.write(info.into())?;
        Ok(0)
    }
}

const F_LINUX_SPECIFIC_BASE: usize = 1024;

/// The file system statistics struct defined in linux
/// (see [linux man statfs(2)](https://man7.org/linux/man-pages/man2/statfs.2.html)).
#[repr(C)]
pub struct StatFs {
    f_type: i64,
    f_bsize: i64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: (i32, i32),
    f_namelen: isize,
    f_frsize: isize,
    f_flags: isize,
    f_spare: [isize; 4],
}

// 保证 `StatFs` 的定义和常见的 linux 一致
static_assertions::const_assert_eq!(120, core::mem::size_of::<StatFs>());

impl From<FsInfo> for StatFs {
    fn from(info: FsInfo) -> Self {
        StatFs {
            // Linux `f_type` is the filesystem magic (`statfs(2)`). Each VFS
            // fills `FsInfo::ftype`; a zero still falls back to EXT4 so
            // `df -T` never sees "unknown".
            f_type: if info.ftype != 0 {
                info.ftype as i64
            } else {
                0xEF53
            },
            f_bsize: info.bsize as _,
            f_blocks: info.blocks as _,
            f_bfree: info.bfree as _,
            f_bavail: info.bavail as _,
            f_files: info.files as _,
            f_ffree: info.ffree as _,
            // 一个由 OS 决定的号码，用于区分文件系统
            f_fsid: (0, 0),
            f_namelen: info.namemax as _,
            f_frsize: info.frsize as _,
            // TODO 需要先实现挂载
            f_flags: 0,
            f_spare: [0; 4],
        }
    }
}

/// `F_SETFL(O_DIRECT)` on a pipe: same answer as `pipe2(O_DIRECT)`.
///
/// This kernel has no packet-mode pipes. `pipe2` already refuses the bit;
/// letting `fcntl` set it made `F_GETFL` report packet mode that never
/// existed.
fn setfl_pipe_direct(file_like: &Arc<dyn FileLike>, next: OpenFlags) -> LxResult<()> {
    if !next.contains(OpenFlags::DIRECT) {
        return Ok(());
    }
    if let Some(file) = file_like.downcast_ref::<File>() {
        if file.inode().downcast_ref::<Pipe>().is_some() {
            return Err(LxError::EINVAL);
        }
    }
    Ok(())
}

#[cfg(test)]
mod setfl_pipe_direct_tests;

#[cfg(test)]
mod statfs_type_tests;

numeric_enum_macro::numeric_enum! {
    #[repr(usize)]
    #[allow(non_camel_case_types)]
    #[derive(Eq, PartialEq, Debug, Copy, Clone)]
    /// fcntl flags
    pub enum FcntlCmd {
        /// dup
        DUPFD = 0,
        /// get close_on_exec
        GETFD = 1,
        /// set/clear close_on_exec
        SETFD = 2,
        /// get file->f_flags
        GETFL = 3,
        /// set file->f_flags
        SETFL = 4,
        /// Get record locking info.
        GETLK = 5,
        /// Set record locking info (non-blocking).
        SETLK = 6,
        /// Set record locking info (blocking).
        SETLKW = 7,
        /// like F_DUPFD, but additionally set the close-on-exec flag
        DUPFD_CLOEXEC = F_LINUX_SPECIFIC_BASE + 6,
    }
}

/// Tee X-server log/error lines into the dmesg ring (prefixed `XLOG:`) so the
/// reason a graphics server (Xorg) aborts is visible via `dmesg`, even when its
/// own logfile is unreachable. Scans a small prefix of each write for the
/// markers Xorg uses for warnings, errors and fatals.
fn tee_x_diag(buf: &[u8]) {
    let scan = &buf[..buf.len().min(1024)];
    let has = |needle: &[u8]| scan.windows(needle.len()).any(|w| w == needle);
    // Xorg's own log markers …
    let x_marker =
        has(b"(EE)") || has(b"(WW)") || has(b"Fatal") || has(b"no screens") || has(b"(II) ");
    // … plus the messages the *dynamic linker* prints to stderr when a program
    // dies before it ever reaches main(). Xorg pulls in far more shared
    // libraries than a typical CLI app, so a single missing `.so` or unresolved
    // symbol makes musl's ld abort with one of these — and that path produces
    // no Xorg log at all, which is exactly the "X won't start, no logs"
    // symptom. Surface those into dmesg so the failing library/symbol is named.
    let ld_error = has(b"Error loading shared library")
        || has(b"Error relocating")
        || has(b"symbol not found")
        || has(b"No such file")
        || has(b"cannot open shared object")
        || has(b"version `")
        || has(b"undefined symbol");
    // … plus the failure text an X *CLIENT* prints. None of the markers above
    // ever appear for a client that cannot reach the server: it dies with
    // Xlib's own wording instead. A client launched from a menu, a script or
    // a terminal the reporter has since closed writes that wording to a
    // stderr nobody is capturing, so the single line naming the failure is
    // lost and the bug reads as "it just doesn't work". Tee it into dmesg,
    // which is the channel that survives.
    //
    // "open display" alone covers every phrasing in the wild ("couldn't open
    // display", "cannot open display", "Can't open display", "unable to open
    // display"); "XIO:"/"X connection to" catch a connection that dies
    // mid-session; the RGB/GLX-visual refusals are what mesa-demos print when
    // the server answers but offers no usable visual.
    // …and the GL loader's own failures, which is where a client that reached
    // the server and then could not get a usable context reports itself:
    // "MESA: error: ZINK: vkQueueSubmit failed (VK_ERROR_DEVICE_LOST)",
    // libGL/libEGL loader errors, a DRI3 handshake that fell back or failed.
    // Without these the kernel's own lines and the client's explanation live
    // in two different places, and only one of them survives the session.
    let x_client_error = has(b"open display")
        || has(b"XIO:")
        || has(b"X connection to")
        || has(b"Xlib:")
        || has(b"xcb_connection")
        || has(b"couldn't get an RGB")
        || has(b"RGB GLX visual")
        || has(b"MESA: error")
        || has(b"libGL error")
        || has(b"libEGL warning")
        || has(b"failed to load driver")
        || has(b"DRI3");
    // Budget the client group only: a program looping on the same refusal
    // (a respawning service, a shell loop) must not flood the console. The
    // server/linker groups are unbudgeted as before — they fire once per
    // start attempt.
    if x_client_error && !x_marker && !ld_error {
        use core::sync::atomic::{AtomicU32, Ordering};
        static CLIENT_BUDGET: AtomicU32 = AtomicU32::new(0);
        if CLIENT_BUDGET.fetch_add(1, Ordering::Relaxed) >= 24 {
            return;
        }
    }
    if x_marker || ld_error || x_client_error {
        if let Ok(s) = core::str::from_utf8(scan) {
            for line in s.split('\n').filter(|l| !l.is_empty()).take(6) {
                kernel_hal::klog_info!("XLOG: {}", line);
            }
        }
    }
}

/// `struct flock` from `<fcntl.h>` (x86-64/generic 64-bit layout: the i16
/// pair, 4 bytes padding, two i64s, an i32 and tail padding — 32 bytes).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Flock {
    /// F_RDLCK / F_WRLCK / F_UNLCK.
    pub l_type: i16,
    /// SEEK_SET / SEEK_CUR / SEEK_END base for `l_start`.
    pub l_whence: i16,
    /// Offset relative to `l_whence`.
    pub l_start: i64,
    /// Byte count; 0 = to EOF and beyond, negative = the range before start.
    pub l_len: i64,
    /// Owner pid, filled in by F_GETLK.
    pub l_pid: i32,
}

/// Round an `F_SETPIPE_SZ` request the way pipe(7) documents: up to the next
/// power of two, never below one page, refused above `fs.pipe-max-size`
/// (1 MiB, the value `/proc/sys/fs/pipe-max-size` advertises) with `EPERM`
/// and refused entirely for zero with `EINVAL`. Pure, so the rounding table
/// is unit-testable.
fn pipe_size_round(arg: usize) -> Result<usize, LxError> {
    const PIPE_MIN_SIZE: usize = 4096;
    const PIPE_MAX_SIZE: usize = 1024 * 1024;
    if arg == 0 {
        return Err(LxError::EINVAL);
    }
    // checked_: a request near usize::MAX has no next power of two, and the
    // unchecked variant would panic the kernel on user-controlled input.
    let size = arg
        .max(PIPE_MIN_SIZE)
        .checked_next_power_of_two()
        .ok_or(LxError::EPERM)?;
    if size > PIPE_MAX_SIZE {
        return Err(LxError::EPERM);
    }
    Ok(size)
}

/// What `write(2)`/`writev(2)` do with an error from the file, given what
/// they already wrote and whether the fd is a pipe that blocks.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AfterWriteError {
    /// Park until the pipe has room, then write the rest.
    Wait,
    /// Report the bytes already taken; the error waits for the next call.
    Partial,
    /// Report the error.
    Fail,
}

/// `pipe_write`: a blocking writer never sees `EAGAIN`, it waits; every
/// other fd keeps the partial-write rule, and an error before any byte went
/// in is the call's answer.
pub(crate) fn after_write_error(e: LxError, waits: bool, written: usize) -> AfterWriteError {
    if e == LxError::EAGAIN && waits {
        AfterWriteError::Wait
    } else if written > 0 {
        AfterWriteError::Partial
    } else {
        AfterWriteError::Fail
    }
}

/// `SIGPIPE` goes with `EPIPE` from a pipe (`pipe_write`) or a socket
/// (`unix_stream_sendmsg`, `sk_stream_error`), not with the `EPIPE` a sound
/// device uses to say "underrun" (ALSA's `-EPIPE` is an `XRUN`, and no
/// signal comes with it).
pub(crate) fn sigpipe_due(e: LxError, stream: bool) -> bool {
    e == LxError::EPIPE && stream
}

/// The calls whose `EFBIG` comes with a `SIGXFSZ`: the ones that write a
/// file's bytes (`generic_write_check_limits` sends it when a write starts at
/// or past `RLIMIT_FSIZE`). The file answers `EFBIG` for the limit and the
/// call sends the signal only when that is its answer: a call that wrote
/// some bytes before reaching the limit returns their count, and no signal.
///
/// An `EFBIG` a filesystem gives for its own largest file comes with the
/// signal here too, which Linux does not send; no filesystem this kernel
/// mounts has a largest file this side of the 64-bit offsets.
pub(crate) fn writes_a_file(sys: &crate::Sys) -> bool {
    use crate::Sys;
    matches!(
        sys,
        Sys::WRITE
            | Sys::PWRITE64
            | Sys::WRITEV
            | Sys::PWRITEV
            | Sys::PWRITEV2
            | Sys::SENDFILE
            | Sys::COPY_FILE_RANGE
            | Sys::SPLICE
    )
}

/// Whether setting a file of `size` bytes to `new_len` breaks a
/// `RLIMIT_FSIZE` of `limit`. Only growing does: `inode_newsize_ok` looks at
/// the limit when `i_size < offset`, so a file already past the limit can
/// still be cut down to a length that is too.
pub(crate) fn grows_past_fsize(limit: u64, size: u64, new_len: u64) -> bool {
    limit != linux_object::process::RLIM_INFINITY && new_len > size && new_len > limit
}

#[cfg(test)]
mod staging_tests;

#[cfg(test)]
mod pipe_write_tests;

#[cfg(test)]
mod pipe_size_tests;

/// Two more arguments that the syscall declares wider than this tree read them:
/// `lseek`'s `whence` (an `int`, read as a `u8`) and `faccessat`'s `mode`
/// (validated by Linux, masked here).
///
/// Both failed the same way — silently and with a plausible answer. A bad
/// `whence` seeked to the offset instead of erroring, and a bad `mode` became
/// F_OK, which is the one answer a permission probe must never invent.
#[cfg(test)]
mod seek_and_access_tests;

#[cfg(test)]
mod truncate_type_tests;

/// The two length calls and the two filesystem-statistics calls, asked
/// through the syscalls: `truncate_type` and `ftruncate_type` have pure
/// tests above, but which syscall asks which of them -- they disagree about
/// a directory and about permissions -- is only visible end to end, and the
/// eight numbers of `statfs` had never been read back at all.
#[cfg(test)]
mod truncate_and_statfs_tests;

/// Where the two copying syscalls put their arguments. `copy_range` has
/// `copy_range_tests` for the loop itself; what nothing could see from
/// there is that `sendfile` hands the offset to the **input** and writes
/// the output at its own cursor, that an explicit offset comes back in the
/// caller's variable, and that both ends check their own window.
#[cfg(test)]
mod sendfile_and_copy_tests;

/// Which sync each of the five durability syscalls asks for. They all
/// return `Ok(0)` on a ramfs and differ only in the method they reach for,
/// so the module hands them a file that counts what it was asked, which
/// is also what makes `readahead`'s errors -- its whole contract, since
/// there is no page cache here to fill -- askable.
#[cfg(test)]
mod durability_tests;

/// What `fallocate` does to the file. `fallocate_mode_tests` decodes the
/// mode word; what nothing could see from there is that two of those modes
/// promise the range **reads back as zeros afterwards**, and that a caller
/// probing for a mode on a read-only descriptor is told about the mode.
#[cfg(test)]
mod fallocate_wiring_tests;

/// `SYNC_IOC_MERGE` is `_IOWR('>', 3, struct sync_merge_data)`: type `'>'`
/// (0x3e), nr 3, 48 bytes. Matched on type and nr, like the DRM syncobj fd
/// ioctls: the struct is UABI-frozen, and a size-pinned constant is how this
/// tree has lost ioctls before.
pub(crate) fn is_sync_ioc_merge(cmd: u32) -> bool {
    (cmd >> 8) & 0xff == 0x3e && cmd & 0xff == 3
}

/// `struct sync_merge_data` (`<linux/sync_file.h>`), what `SYNC_IOC_MERGE`
/// reads and writes back: `char name[32]; __s32 fd2; __s32 fence;
/// __u32 flags; __u32 pad;`.
///
/// It lives here, and not inside `Syscall::sys_sync_file_merge` where it was
/// declared, because the order of these fields is the ABI and a struct local
/// to a function body cannot be named by a test. The layout is pinned in
/// `drm_ioctl_layout_tests`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SyncMergeData {
    /// The name Linux gives the merged fence, cut at 31 bytes and
    /// NUL-terminated by `sync_file_merge_op` before it is used and written
    /// back, as `data.name[sizeof(data.name) - 1] = '\0'` does.
    pub name: [u8; 32],
    pub fd2: i32,
    pub fence: i32,
    pub flags: u32,
    pub pad: u32,
}

/// `struct drm_mode_create_lease` (`drm.h`), the argument of
/// `DRM_IOCTL_MODE_CREATE_LEASE`: `__u64 object_ids; __u32 object_count;
/// __u32 flags; __u32 lessee_id; __s32 fd;`.
///
/// Here rather than inside `Syscall::sys_drm_prime` for the reason
/// [`SyncMergeData`] gives.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeCreateLease {
    object_ids: u64,
    object_count: u32,
    flags: u32,
    lessee_id: u32,
    fd: i32,
}

/// `struct drm_syncobj_handle` (`drm.h`), the argument of both syncobj fd
/// ioctls: `__u32 handle; __u32 flags; __s32 fd; __u32 pad;`.
///
/// These 16 bytes are the frozen prefix. The UABI struct is append-only and
/// newer kernels grew a tail, which is why `Syscall::sys_drm_syncobj_fd`
/// matches on the ioctl nr and type and never on the size it encodes -- and
/// why what has to be written down is that the prefix is these four words in
/// this order. Here rather than inside that method for the reason
/// [`SyncMergeData`] gives.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub pad: u32,
}

/// `struct drm_syncobj_eventfd` (`drm.h`), the argument of
/// `DRM_IOCTL_SYNCOBJ_EVENTFD`: `__u32 handle; __u32 flags; __u64 point;
/// __s32 fd; __u32 pad;`.
///
/// Here rather than inside `Syscall::sys_drm_syncobj_eventfd` for the reason
/// [`SyncMergeData`] gives.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmSyncobjEventfd {
    handle: u32,
    flags: u32,
    point: u64,
    fd: i32,
    pad: u32,
}

/// Whether `cmd` is `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (`Some(true)`) or
/// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (`Some(false)`), the two dma-buf ioctls
/// that need the fd table. Exact numbers: both structs are two `u32`s and
/// frozen, so the encoded size cannot move as `drm_syncobj_handle`'s did.
pub(crate) fn dma_buf_sync_file_ioctl(cmd: u32) -> Option<bool> {
    use linux_object::fs::{DMA_BUF_IOCTL_EXPORT_SYNC_FILE, DMA_BUF_IOCTL_IMPORT_SYNC_FILE};
    match cmd {
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE => Some(true),
        DMA_BUF_IOCTL_IMPORT_SYNC_FILE => Some(false),
        _ => None,
    }
}

/// The one flag `SYNCOBJ_HANDLE_TO_FD` and `SYNCOBJ_FD_TO_HANDLE` know:
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE` and
/// `DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE`, bit 0 on both.
const SYNCOBJ_FD_SYNC_FILE: u32 = 1 << 0;

/// What both syncobj fd ioctls refuse before looking at the handle or the
/// fd, as `drm_syncobj_handle_to_fd_ioctl` / `drm_syncobj_fd_to_handle_ioctl`
/// do: a non-zero `pad`, and `flags` other than 0 or the one bit. Every
/// other flag is EINVAL -- which includes the `_TIMELINE` variants newer
/// kernels grew (bit 1, with a `point` past the 16-byte prefix): this tree
/// has no timeline sync_file, and a client that asked for one used to get
/// its request honoured as the plain, whole-syncobj kind, so the point it
/// wanted to export or import was silently not the one that travelled.
pub(crate) fn syncobj_fd_args_check(flags: u32, pad: u32) -> Result<(), LxError> {
    if pad != 0 {
        return Err(LxError::EINVAL);
    }
    if flags != 0 && flags != SYNCOBJ_FD_SYNC_FILE {
        return Err(LxError::EINVAL);
    }
    Ok(())
}

/// The errno for a `handle` that is not the caller's, which is how Linux's
/// three lookups answer: EINVAL from `drm_syncobj_handle_to_fd` (the
/// whole-syncobj export), ENOENT from `drm_syncobj_find_fence` (the
/// sync_file export, which used to say EINVAL here too) and from
/// `drm_syncobj_import_sync_file_fence` (the import's destination).
pub(crate) fn syncobj_fd_missing_handle_errno(handle_to_fd: bool, sync_file: bool) -> LxError {
    if handle_to_fd && !sync_file {
        LxError::EINVAL
    } else {
        LxError::ENOENT
    }
}

/// `sync_file_ioctl_merge` after the copy from userspace, on the caller's
/// fd table `proc` as process `pid`: `first` is the sync_file the ioctl was
/// issued on, `data.fd2` the other one, and `data.fence` comes back as the
/// new fd, close-on-exec, of a sync_file that is ready once both are.
///
/// Its errors, in its order: `flags` or `pad` set is EINVAL; then
/// `sync_file_fdget(fd2)`, which is empty for an fd that names no file as
/// much as for a file that is not a sync_file (a syncobj fd included), is
/// ENOENT. The first of those used to come back as the fd table's EBADF.
/// The name is cut at 31 bytes and NUL-terminated before it is used and
/// written back, as `data.name[sizeof(data.name) - 1] = '\0'` does.
pub(crate) fn sync_file_merge_op(
    proc: &LinuxProcess,
    pid: KoID,
    first: &linux_object::fs::SyncobjHandle,
    data: &mut SyncMergeData,
) -> LxResult {
    use linux_object::fs::SyncobjHandle;

    if data.flags != 0 || data.pad != 0 {
        return Err(LxError::EINVAL);
    }
    let second = proc
        .get_file_like(FileDesc::from(data.fd2 as usize))
        .map_err(|_| LxError::ENOENT)?;
    let Some(second) = second.downcast_ref::<SyncobjHandle>() else {
        return Err(LxError::ENOENT);
    };
    data.name[31] = 0;
    // ENOTTY on a syncobj fd (it has no ioctl), ENOENT when `fd2` is not
    // a sync_file; the name goes on the new fence for `FILE_INFO`.
    let merged = first.merge(second, &data.name)?;
    let fd = proc.add_file(merged)?;
    // Bounded, LOG=error-visible trace: the first merges of a boot are the
    // GLX client's second round of acquires, the step that used to fail.
    {
        static MERGE_TRACE_BUDGET: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);
        let n = MERGE_TRACE_BUDGET.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if n < 8 {
            kernel_hal::klog_info!(
                "[drm] SYNC_IOC_MERGE #{} pid={} fd2={} -> fd={}",
                n + 1,
                pid,
                data.fd2,
                i32::from(fd),
            );
        }
    }
    data.fence = i32::from(fd);
    Ok(())
}

/// `SYNCOBJ_HANDLE_TO_FD` / `SYNCOBJ_FD_TO_HANDLE` after the argument check,
/// on the caller's fd table `proc` as process `pid`; `h.fd` (an export) or
/// `h.handle` (an import) comes back filled in.
///
/// The `_SYNC_FILE` variants (same bit on both ioctls) move a single FENCE
/// rather than the syncobj itself: export takes the fence that is current on
/// `handle` and wraps it in an fd; import gives that fence to a DIFFERENT,
/// already-existing syncobj. That is how Mesa hands completed work across a
/// process boundary (a GLX/DRI3 client and the X server exchanging frames),
/// so a GL client under Xwayland exercises this pair on paths a
/// Wayland-native client never touches. A fence here is "syncobj S reached
/// point N", so a sync_file fd is exactly that pair, and
/// `zcore_drivers::scheme::syncobj` resolves it on import (immediately when
/// already satisfied, as a recorded dependency otherwise).
///
/// The errors are the ones `drm_syncobj.c` gives, in its order:
/// - an export of a handle that is not the caller's is EINVAL for the
///   whole syncobj (`drm_syncobj_handle_to_fd`) and ENOENT for its fence
///   (`drm_syncobj_find_fence`);
/// - an import looks at the fd FIRST: one that names no file is EINVAL
///   (`fdget` / `sync_file_get_fence` come back empty; it used to be the
///   fd table's EBADF), and so is a file of the wrong kind, a syncobj fd
///   where a fence is wanted included (`sync_file_get_fence` knows only
///   sync_files; a syncobj fd used to be taken as "whatever its object
///   carries now"), or a sync_file where the object is wanted;
/// - only then the destination handle of an `IMPORT_SYNC_FILE`, ENOENT
///   when it is not the caller's (it used to be looked up first).
pub(crate) fn drm_syncobj_fd_op(
    proc: &LinuxProcess,
    pid: KoID,
    is_handle_to_fd: bool,
    h: &mut DrmSyncobjHandle,
) -> LxResult {
    use kernel_hal::drivers::scheme::syncobj;
    use linux_object::fs::SyncobjHandle;

    let sync_file = h.flags & SYNCOBJ_FD_SYNC_FILE != 0;
    // Bounded, LOG=error-visible trace of the export/import ops NVK issues
    // during physical-device / semaphore init. This is the LAST syncobj path
    // NOT covered by `drm_scheme.rs`'s `trace_syncobj`: HANDLE_TO_FD /
    // FD_TO_HANDLE are intercepted HERE (they need the process fd table),
    // before the inode `io_control` those traces live in. An export->import
    // round-trip is exactly how Mesa auto-detects a syncobj's EXTERNAL-handle
    // features, the one detection step we could never see. `opaque` =
    // OPAQUE_FD whole-syncobj share; `sync_file` = single-fence transfer.
    {
        static SYNCOBJ_FD_TRACE_BUDGET: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);
        let n = SYNCOBJ_FD_TRACE_BUDGET.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if n < 32 {
            kernel_hal::klog_info!(
                "[drm] SYNCOBJ_{} #{} pid={} handle={} fd={} flags={:#x} ({})",
                if is_handle_to_fd {
                    "HANDLE_TO_FD"
                } else {
                    "FD_TO_HANDLE"
                },
                n + 1,
                pid,
                h.handle,
                h.fd,
                h.flags,
                if sync_file { "sync_file" } else { "opaque" },
            );
        }
    }
    if is_handle_to_fd {
        // `h.handle` is one of the CALLER'S syncobjs (`drm_syncobj_find`):
        // another process's is not there.
        if !syncobj::usable_by(pid, h.handle) {
            let errno = syncobj_fd_missing_handle_errno(true, sync_file);
            warn!(
                "[drm] SYNCOBJ_HANDLE_TO_FD{} {:?}: handle={} is not a live syncobj of pid {}",
                if sync_file { "(EXPORT_SYNC_FILE)" } else { "" },
                errno,
                h.handle,
                pid
            );
            return Err(errno);
        }
        if sync_file {
            // The file gets the FENCE the syncobj carries now, as a syncobj
            // of its own (`export_fence`), never a live reading of the
            // source: a RESET of the source, or its re-arm by the next
            // submit, must not take the fence away from a sync_file already
            // exported (Mesa resets a binary semaphore in the same call that
            // exports its SYNC_FD). The file owns the carrier's one reference
            // and `destroy`s it on close (Drop on SyncobjHandle).
            let Some(carrier) = syncobj::export_fence(h.handle) else {
                warn!(
                    "[drm] SYNCOBJ_HANDLE_TO_FD(EXPORT_SYNC_FILE) ENOENT: handle={} went away",
                    h.handle
                );
                return Err(LxError::ENOENT);
            };
            let new_fd = match proc.add_file(SyncobjHandle::new_sync_file(carrier, 1)) {
                Ok(fd) => fd,
                Err(e) => {
                    let _ = syncobj::destroy(carrier);
                    return Err(e);
                }
            };
            h.fd = i32::from(new_fd);
            return Ok(());
        }
        // Fd holds a syncobj ref so SYNCOBJ_DESTROY cannot free it while
        // the exported fd is still live (Drop on SyncobjHandle dec_refs).
        if !syncobj::add_ref(h.handle) {
            return Err(LxError::EINVAL);
        }
        let new_fd = match proc.add_file(SyncobjHandle::new(h.handle)) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("[drm] SYNCOBJ_HANDLE_TO_FD add_file {:?}", e);
                let _ = syncobj::destroy(h.handle);
                return Err(e);
            }
        };
        h.fd = i32::from(new_fd);
        return Ok(());
    }
    // FD_TO_HANDLE: the fd first. `fdget` (opaque) and `sync_file_get_fence`
    // (import) answer an fd that names nothing with EINVAL, as they do a
    // file of another kind.
    let target = proc
        .get_file_like(FileDesc::from(h.fd as usize))
        .map_err(|_| {
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE EINVAL: fd={} is not in the fd table",
                h.fd
            );
            LxError::EINVAL
        })?;
    let file = target
        .downcast_ref::<SyncobjHandle>()
        .ok_or(LxError::EINVAL)?;
    if sync_file {
        // Only a sync_file carries a fence to import; a syncobj fd is the
        // object, and `sync_file_get_fence` has nothing to give for it.
        let Some(point) = file.sync_file_point else {
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE(IMPORT_SYNC_FILE) EINVAL: fd={} is a syncobj fd, not a sync_file",
                h.fd
            );
            return Err(LxError::EINVAL);
        };
        // The DESTINATION must already exist and be the caller's (Linux does
        // not create one for this variant): `drm_syncobj_find`, ENOENT.
        if !syncobj::usable_by(pid, h.handle) {
            let errno = syncobj_fd_missing_handle_errno(false, true);
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE(IMPORT_SYNC_FILE) {:?}: handle={} is not a live syncobj of pid {}",
                errno, h.handle, pid
            );
            return Err(errno);
        }
        if !syncobj::import_snapshot(h.handle, file.handle, point) {
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE(IMPORT_SYNC_FILE) EINVAL: dst handle={} or src handle={} not live",
                h.handle, file.handle
            );
            return Err(LxError::EINVAL);
        }
        return Ok(());
    }
    if file.sync_file_point.is_some() {
        // A sync_file fd imported WITHOUT the flag would alias the two
        // objects (the importer would signal the exporter's syncobj)
        // instead of copying one fence. Linux rejects the mismatch
        // (`f_op != &drm_syncobj_file_fops`); so do we, loudly, because
        // silently aliasing them produces fences that appear to signal
        // early, the worst failure to debug from a frame that merely
        // looks wrong.
        warn!(
            "[drm] SYNCOBJ_FD_TO_HANDLE EINVAL: fd={} is a sync_file, not a syncobj \
             (missing IMPORT_SYNC_FILE flag)",
            h.fd
        );
        return Err(LxError::EINVAL);
    }
    // The importer gets a reference of its own (Linux: a new handle in its
    // table), so its later DESTROY frees only that.
    let Some(handle) = file.import_opaque(pid) else {
        warn!(
            "[drm] SYNCOBJ_FD_TO_HANDLE EINVAL: fd={} names syncobj handle={} that no longer exists",
            h.fd, file.handle
        );
        return Err(LxError::EINVAL);
    };
    h.handle = handle;
    Ok(())
}

#[cfg(test)]
mod syncobj_fd_ioctl_tests;

#[cfg(test)]
mod syncobj_fd_table_tests;

#[cfg(test)]
mod sync_file_merge_table_tests;

#[cfg(test)]
mod sync_file_ioctl_tests;

/// `posix_fadvise(2)` advice values. All of zCore's target arches
/// (x86_64/aarch64/riscv64) share the asm-generic numbering; s390 is the one
/// architecture that renumbers DONTNEED and NOREUSE, and this tree does not
/// target it.
const POSIX_FADV_NORMAL: usize = 0;
const POSIX_FADV_RANDOM: usize = 1;
const POSIX_FADV_SEQUENTIAL: usize = 2;
const POSIX_FADV_WILLNEED: usize = 3;
const POSIX_FADV_DONTNEED: usize = 4;
const POSIX_FADV_NOREUSE: usize = 5;

/// Whether `advice` is a value `posix_fadvise(2)` defines.
///
/// `madvise` in this same tree already answers this question for its own word
/// (`madvise_advice_known`, `vm.rs`), and `sync_file_range` answers it for
/// its flags thirty lines below. `fadvise` did not ask it at all, so every
/// number in the word -- a typo, a value from another architecture's
/// numbering, a `madvise` constant reached for by mistake -- came back as a
/// hint successfully taken. The two words are NOT the same list, which is the
/// whole reason each needs its own: `madvise` defines 8 (`MADV_FREE`) and
/// `fadvise` stops at 5.
pub(crate) fn fadvise_advice_known(advice: usize) -> bool {
    matches!(
        advice,
        POSIX_FADV_NORMAL
            | POSIX_FADV_RANDOM
            | POSIX_FADV_SEQUENTIAL
            | POSIX_FADV_WILLNEED
            | POSIX_FADV_DONTNEED
            | POSIX_FADV_NOREUSE
    )
}

/// How many zero bytes `fallocate` hands the file per write.
const FALLOCATE_ZERO_CHUNK: usize = 64 * 1024;

/// `fallocate(2)` modes (`include/uapi/linux/falloc.h`).
const FALLOC_FL_KEEP_SIZE: usize = 0x01;
const FALLOC_FL_PUNCH_HOLE: usize = 0x02;
const FALLOC_FL_COLLAPSE_RANGE: usize = 0x08;
const FALLOC_FL_ZERO_RANGE: usize = 0x10;
const FALLOC_FL_INSERT_RANGE: usize = 0x20;
const FALLOC_FL_UNSHARE_RANGE: usize = 0x40;

/// `FALLOC_FL_SUPPORTED_MASK` (`fs/open.c`). `FALLOC_FL_NO_HIDE_STALE` (0x04)
/// is deliberately NOT in it: no upstream filesystem implements it, so Linux
/// turns it away like any bit it does not know.
const FALLOC_FL_SUPPORTED_MASK: usize = FALLOC_FL_KEEP_SIZE
    | FALLOC_FL_PUNCH_HOLE
    | FALLOC_FL_COLLAPSE_RANGE
    | FALLOC_FL_ZERO_RANGE
    | FALLOC_FL_INSERT_RANGE
    | FALLOC_FL_UNSHARE_RANGE;

/// What a `fallocate(2)` `mode` word is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallocateOp {
    /// Make sure `[offset, offset + len)` is backed, growing the file to reach
    /// it unless `keep_size`.
    Allocate {
        /// `FALLOC_FL_KEEP_SIZE`: the file's length must not change.
        keep_size: bool,
    },
    /// The range must read back as zeros afterwards. `FALLOC_FL_PUNCH_HOLE`
    /// (which Linux requires to carry `KEEP_SIZE`) and `FALLOC_FL_ZERO_RANGE`
    /// both land here; only the second may grow the file.
    ZeroRange {
        /// `FALLOC_FL_KEEP_SIZE`: the file's length must not change.
        keep_size: bool,
    },
}

/// Read a `fallocate(2)` `mode` word, as `vfs_fallocate` does.
///
/// The three refusals that carry `EOPNOTSUPP` rather than `EINVAL` are not a
/// slip of the pen upstream: they say "this kernel could define that
/// combination and does not", which is what a caller probing for a feature
/// needs to hear, while `EINVAL` says "that combination will never mean
/// anything".
pub(crate) fn fallocate_op(mode: usize) -> Result<FallocateOp, LxError> {
    if mode & !FALLOC_FL_SUPPORTED_MASK != 0 {
        return Err(LxError::EOPNOTSUPP);
    }
    let keep_size = mode & FALLOC_FL_KEEP_SIZE != 0;
    if mode & FALLOC_FL_PUNCH_HOLE != 0 {
        // Punching and zeroing the same range are two different orders, and a
        // punch that is allowed to change the size is not a punch.
        if mode & FALLOC_FL_ZERO_RANGE != 0 || !keep_size {
            return Err(LxError::EOPNOTSUPP);
        }
    }
    // Collapse and insert must each be the only flag in the word; unshare
    // admits `KEEP_SIZE` beside it and nothing else.
    if mode & FALLOC_FL_COLLAPSE_RANGE != 0 && mode & !FALLOC_FL_COLLAPSE_RANGE != 0 {
        return Err(LxError::EINVAL);
    }
    if mode & FALLOC_FL_INSERT_RANGE != 0 && mode & !FALLOC_FL_INSERT_RANGE != 0 {
        return Err(LxError::EINVAL);
    }
    if mode & FALLOC_FL_UNSHARE_RANGE != 0
        && mode & !(FALLOC_FL_UNSHARE_RANGE | FALLOC_FL_KEEP_SIZE) != 0
    {
        return Err(LxError::EINVAL);
    }
    // Shifting every byte after `offset` is not something these filesystems
    // can do, and `EOPNOTSUPP` is exactly what a Linux filesystem without
    // support for it answers -- which is most of them.
    if mode & (FALLOC_FL_COLLAPSE_RANGE | FALLOC_FL_INSERT_RANGE) != 0 {
        return Err(LxError::EOPNOTSUPP);
    }
    if mode & (FALLOC_FL_PUNCH_HOLE | FALLOC_FL_ZERO_RANGE) != 0 {
        return Ok(FallocateOp::ZeroRange { keep_size });
    }
    // `UNSHARE_RANGE` asks for the range to stop being shared with a snapshot.
    // Nothing here shares extents, so plain allocation is the whole of it.
    Ok(FallocateOp::Allocate { keep_size })
}

/// The `flags` word of `copy_file_range(2)`, which defines none.
pub(crate) fn copy_file_range_flags(flags: usize) -> Result<(), LxError> {
    if flags != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(())
}

#[cfg(test)]
mod fadvise_tests;

/// What a `utimensat(2)` / `utimes(2)` asks to be done to the file's times,
/// decoded as `utimes_common` does before it looks at the file: the
/// question of who may do it is [`LinuxProcess::check_utimes`]'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimesToSet {
    /// Both `UTIME_OMIT`: success, and nothing is touched or checked.
    Nothing,
    /// A null `times`, or both `UTIME_NOW`: both times become this instant,
    /// and write access is enough (`ATTR_TOUCH`).
    Touch(TimeSpec),
    /// At least one time the caller chose (`ATTR_ATIME_SET` / `ATTR_MTIME_SET`):
    /// only the owner may. `None` is an `UTIME_OMIT` half, left as it is.
    Set {
        atime: Option<TimeSpec>,
        mtime: Option<TimeSpec>,
    },
}

/// `include/uapi/linux/stat.h`.
pub(crate) const UTIME_NOW: usize = (1 << 30) - 1;
pub(crate) const UTIME_OMIT: usize = (1 << 30) - 2;
const NSEC_PER_SEC: usize = 1_000_000_000;

/// `nsec_valid()`: `UTIME_NOW`, `UTIME_OMIT`, or a real nanosecond count.
fn nsec_valid(nsec: usize) -> bool {
    nsec == UTIME_NOW || nsec == UTIME_OMIT || nsec < NSEC_PER_SEC
}

/// The `times` of `utimensat(2)`, read the way `utimes_common` reads them.
///
/// Anything with a nanosecond field out of range is `EINVAL` before any
/// other consideration; both `UTIME_NOW` is the same as a null pointer;
/// both `UTIME_OMIT` asks for nothing; and an `UTIME_NOW` next to an
/// explicit value is that instant, set explicitly (the pair still needs
/// the owner: it is not a touch).
pub(crate) fn utimensat_times(times: Option<[TimeSpec; 2]>, now: TimeSpec) -> LxResult<TimesToSet> {
    let Some([a, m]) = times else {
        return Ok(TimesToSet::Touch(now));
    };
    if !nsec_valid(a.nsec) || !nsec_valid(m.nsec) {
        return Err(LxError::EINVAL);
    }
    if a.nsec == UTIME_NOW && m.nsec == UTIME_NOW {
        return Ok(TimesToSet::Touch(now));
    }
    if a.nsec == UTIME_OMIT && m.nsec == UTIME_OMIT {
        return Ok(TimesToSet::Nothing);
    }
    let one = |t: TimeSpec| match t.nsec {
        UTIME_OMIT => None,
        UTIME_NOW => Some(now),
        _ => Some(t),
    };
    Ok(TimesToSet::Set {
        atime: one(a),
        mtime: one(m),
    })
}

/// The `times` of `utimes(2)`: two `struct timeval`, both explicit, with a
/// microsecond field that must be one (`do_utimes` -> `get_timespec64`:
/// `EINVAL` past 999999). A null pointer is a touch.
#[cfg(any(target_arch = "x86_64", test))]
pub(crate) fn utimes_times(
    times: Option<[linux_object::time::TimeVal; 2]>,
    now: TimeSpec,
) -> LxResult<TimesToSet> {
    let Some([a, m]) = times else {
        return Ok(TimesToSet::Touch(now));
    };
    let one = |t: linux_object::time::TimeVal| -> LxResult<TimeSpec> {
        if t.usec >= 1_000_000 {
            return Err(LxError::EINVAL);
        }
        Ok(TimeSpec {
            sec: t.sec,
            nsec: t.usec * 1000,
        })
    };
    Ok(TimesToSet::Set {
        atime: Some(one(a)?),
        mtime: Some(one(m)?),
    })
}

#[cfg(test)]
mod utimes_times_tests;

#[cfg(test)]
mod utimes_gate_tests;

/// `utimes` through the syscall. Its pure half has `utimes_times_tests`
/// beside it; what only this can see is that the `timeval` microseconds
/// become nanoseconds on the way to the inode, and that a null pathname
/// here is a bad pointer and not the `futimens` of `utimensat`.
#[cfg(test)]
mod utimes_wiring_tests;

#[cfg(test)]
mod fallocate_mode_tests;

#[cfg(test)]
mod copy_file_range_flag_tests;

/// `sendfile(2)` and `copy_file_range(2)` on a ramfs file and a bounded,
/// socket-like output: own file for the size ceiling (`xtask tamano`), no
/// change to the tests.
#[cfg(test)]
mod copy_range_tests;

#[cfg(test)]
mod benches;

/// `pwritev2` flags (`include/uapi/linux/fs.h`).
const RWF_DSYNC: usize = 0x2;
const RWF_SYNC: usize = 0x4;
const RWF_ATOMIC: usize = 0x40;

/// `generic_atomic_write_valid`: a power-of-two length within the unit,
/// at an offset aligned to it.
fn atomic_write_valid(len: usize, pos: u64, min: usize, max: usize) -> bool {
    len.is_power_of_two() && len >= min && len <= max && pos.is_multiple_of(len as u64)
}

#[cfg(test)]
mod rwf_atomic_tests;

#[cfg(test)]
mod vfs_ioctl_tests;

#[cfg(test)]
mod record_lock_wiring_tests;

#[cfg(test)]
mod fcntl_families_tests;

#[cfg(test)]
mod read_write_refusal_tests;

/// The syscall halves of the two pipe-write rules `pipe_write_tests` decides
/// with pure helpers: the wait for room and the signal behind the `EPIPE`.
#[cfg(test)]
mod pipe_write_wiring_tests;

/// What the positional calls answer when the file takes less than it was
/// given, which nothing already in this tree does.
#[cfg(test)]
mod short_write_tests;
