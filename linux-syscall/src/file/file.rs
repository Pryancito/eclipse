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
use linux_object::{process::FsInfo, time::TimeSpec};

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

        let is_seekable =
            if let Ok(file) = file_like.clone().downcast_arc::<linux_object::fs::File>() {
                if let Ok(meta) = file.metadata() {
                    meta.type_ == linux_object::fs::vfs::FileType::File
                        || meta.type_ == linux_object::fs::vfs::FileType::BlockDevice
                } else {
                    false
                }
            } else {
                false
            };

        let chunk_size = len.min(super::SYSCALL_IO_MAX);
        // Hybrid stack/heap buffer: line-oriented apps (busybox shell, getline,
        // fgetc) drive a stream of small reads — keep those alloc-free. The
        // ~64 KiB ceiling case still goes via the buddy allocator.
        const STACK_BUF: usize = 512;
        let mut stack_buf = [0u8; STACK_BUF];
        let mut heap_buf: alloc::vec::Vec<u8> = if chunk_size > STACK_BUF {
            vec![0u8; chunk_size]
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
            let n = file_like.read(&mut buf[..current_len]).await?;
            if n == 0 {
                break;
            }
            if n > 0 && usize::from(fd) == 0 && buf[0] == 0x03 {
                // Convert ETX into a terminal interrupt.
                // We set the pending latch and let the centralized handler deliver SIGINT.
                linux_object::fs::stdio::ctrl_c_pending_set();
                return Err(LxError::EINTR);
            }
            base.add(read_len).write_array(&buf[..n])?;
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
    pub fn sys_write(&self, fd: FileDesc, base: UserInPtr<u8>, len: usize) -> SysResult {
        info!("write: fd={:?}, base={:?}, len={:#x}", fd, base, len);
        // Diagnostic: surface X-server log/error lines into the dmesg ring so the
        // reason a graphics server aborts is visible even without its logfile.
        if let Ok(peek) = base.as_slice(len.min(512)) {
            tee_x_diag(peek);
        }
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        let chunk_size = len.min(super::SYSCALL_IO_MAX);
        let mut written = 0usize;
        while written < len {
            let n = (len - written).min(chunk_size);
            let w = file_like.write(base.add(written).as_slice(n)?)?;
            // A write of 0 would otherwise spin forever; stop and report the
            // bytes written so far (short write).
            if w == 0 {
                break;
            }
            written += w;
        }
        Ok(written)
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
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;

        let chunk_size = len.min(super::SYSCALL_IO_MAX);
        // Same hybrid buffer as sys_read — short positional reads (e.g. ELF
        // header probes during dlopen, libc pread of small struct slots) hit
        // the stack path.
        const STACK_BUF: usize = 512;
        let mut stack_buf = [0u8; STACK_BUF];
        let mut heap_buf: alloc::vec::Vec<u8> = if chunk_size > STACK_BUF {
            vec![0u8; chunk_size]
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
        self.linux_process()
            .get_file_like(fd)?
            .write_at(offset, base.as_slice(len)?)
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
            vec![0u8; total_len]
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
    pub fn sys_writev(
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
        if iovs.total_len() > super::SYSCALL_IO_MAX {
            return Err(LxError::EINVAL);
        }
        let buf = iovs.read_to_vec()?;
        tee_x_diag(&buf);
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        let len = file_like.write(&buf)?;
        Ok(len)
    }

    /// repositions the offset of the open file associated with the file descriptor fd
    /// to the argument offset according to the directive whence
    pub fn sys_lseek(&self, fd: FileDesc, offset: i64, whence: u8) -> SysResult {
        const SEEK_SET: u8 = 0;
        const SEEK_CUR: u8 = 1;
        const SEEK_END: u8 = 2;

        let pos = match whence {
            SEEK_SET => SeekFrom::Start(offset as u64),
            SEEK_END => SeekFrom::End(offset),
            SEEK_CUR => SeekFrom::Current(offset),
            _ => return Err(LxError::EINVAL),
        };
        info!("lseek: fd={:?}, pos={:?}", fd, pos);

        let proc = self.linux_process();
        let file = proc.get_file(fd)?;
        let offset = file.seek(pos)?;
        Ok(offset as usize)
    }

    /// cause the regular file named by path to be truncated to a size of precisely length bytes.
    pub fn sys_truncate(&self, path: UserInPtr<u8>, len: usize) -> SysResult {
        let path = path.as_c_str()?;
        info!("truncate: path={:?}, len={}", path, len);
        let proc = self.linux_process();
        let inode = proc.lookup_inode(path)?;
        let metadata = inode.metadata()?;
        proc.check_access(&metadata, 0o2, true)?;
        inode.resize(len)?;
        Ok(0)
    }

    /// cause the regular file referenced by fd to be truncated to a size of precisely length bytes.
    pub fn sys_ftruncate(&self, fd: FileDesc, len: usize) -> SysResult {
        info!("ftruncate: fd={:?}, len={}", fd, len);
        let proc = self.linux_process();
        proc.get_file(fd)?.set_len(len as u64)?;
        Ok(0)
    }

    /// Announce an intention to access file data in a specific pattern
    /// (`posix_fadvise`). The hint is purely advisory, so we validate the
    /// descriptor and otherwise treat it as a no-op returning success. This
    /// silences the spurious `unknown syscall: FADVISE64` errors emitted by
    /// tools such as `e2fsck`.
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
        // Honour Linux's EBADF for an invalid descriptor; ignore the hint itself.
        let _ = self.linux_process().get_file_like(fd)?;
        Ok(0)
    }

    /// Manipulate the allocated disk space for the file referenced by `fd`
    /// (`fallocate`). We support the default mode by growing a regular file so
    /// that `offset + len` bytes are backed; every other mode (and any non
    /// regular file such as a block device) is treated as a successful no-op.
    /// That is enough for `resize2fs`/`e2fsck`, which only rely on the size
    /// effect, and avoids the `unknown syscall: FALLOCATE` errors.
    pub fn sys_fallocate(&self, fd: FileDesc, mode: usize, offset: usize, len: usize) -> SysResult {
        info!(
            "fallocate: fd={:?}, mode={:#x}, offset={}, len={}",
            fd, mode, offset, len
        );
        let file = self.linux_process().get_file(fd)?;
        // Only the plain allocate mode (mode == 0) implies the file may need to
        // grow. KEEP_SIZE, the hole-punch/zero-range variants, and any request
        // against a non-regular file (e.g. a block device) must leave the size
        // untouched, so they fall through to a successful no-op.
        if mode == 0 {
            let meta = file.metadata()?;
            if meta.type_ == linux_object::fs::vfs::FileType::File {
                let end = offset.checked_add(len).ok_or(LxError::EINVAL)?;
                if end > meta.size {
                    file.set_len(end as u64)?;
                }
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
        let proc = self.linux_process();
        let in_file = proc.get_file(in_fd)?;
        let out_file = proc.get_file(out_fd)?;
        let mut buffer = [0u8; 1024];

        // for in_offset and out_offset
        // null means update file offset
        // non-null means update {in,out}_offset instead

        let mut read_offset = if !in_offset.is_null() {
            in_offset.read()?
        } else {
            in_file.seek(SeekFrom::Current(0))?
        };

        let orig_out_file_offset = out_file.seek(SeekFrom::Current(0))?;
        let write_offset = if !out_offset.is_null() {
            let offset = out_offset.read()?;
            out_file.seek(SeekFrom::Start(offset))?
        } else {
            0
        };

        // read from specified offset and write new offset back
        let mut bytes_read = 0;
        let mut total_written = 0;
        while bytes_read < count {
            let len = buffer.len().min(count - bytes_read);
            let read_len = in_file.read_at(read_offset, &mut buffer[..len]).await?;
            if read_len == 0 {
                break;
            }
            bytes_read += read_len;
            read_offset += read_len as u64;

            let mut bytes_written = 0;
            let mut rlen = read_len;
            while bytes_written < read_len {
                let write_len = out_file.write(&buffer[bytes_written..(bytes_written + rlen)])?;
                if write_len == 0 {
                    info!(
                        "copy_file_range:END_ERR in={:?}, out={:?}, in_offset={:?}, out_offset={:?}, count={} = bytes_read {}, bytes_written {}, write_len {}",
                        in_fd, out_fd, in_offset, out_offset, count, bytes_read, bytes_written, write_len
                    );
                    return Err(LxError::EBADF);
                }
                bytes_written += write_len;
                rlen -= write_len;
            }
            total_written += bytes_written;
        }

        if !in_offset.is_null() {
            in_offset.write(read_offset)?;
        } else {
            in_file.seek(SeekFrom::Current(bytes_read as i64))?;
        }
        out_offset.write_if_not_null(write_offset + total_written as u64)?;
        if !out_offset.is_null() {
            out_file.seek(SeekFrom::Start(orig_out_file_offset))?;
        }
        Ok(total_written)
    }

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

    /// Set parameters of device files.
    pub fn sys_ioctl(
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
        // Trace into the dmesg ring (always recorded, never echoed to the
        // screen). If an ioctl blocks, dmesg shows an `ENTER` with no matching
        // `LEAVE` — that request is the one that hangs. Unhandled ioctls are
        // recorded as errors, the same way an invalid syscall number is.
        kernel_hal::klog_info!(
            "ioctl ENTER fd={:?} request={:#x} arg={:#x}",
            fd, request, arg1
        );
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        // `TIOCGWINSZ` (get terminal window size).
        const TIOCGWINSZ: usize = 0x5413;
        const TCGETS: usize = 0x5401;
        const TCSETS: usize = 0x5402;
        const TCSETSW: usize = 0x5403;
        const TCSETSF: usize = 0x5404;
        let ret = match file_like.ioctl(request, arg1, arg2, arg3) {
            // Some programs (e.g. the X server and its helpers) insist on a
            // valid window size and keep retrying `TIOCGWINSZ` in a loop when it
            // fails — even when the fd is a pipe, socket or char device (DRM/fb)
            // rather than a tty. Different backends reject it differently
            // (ENOTTY, ENOSYS, or EINVAL from a device's io_control), so satisfy
            // all of them by reporting the console size instead of failing.
            //
            // Input device nodes (`/dev/input/mice`, `event*`) are excluded:
            // faking a window size there makes musl's `isatty()` (a TIOCGWINSZ
            // probe) report a tty, and kdrive/TinyX then treats the mouse as a
            // serial port and loops over serial mouse protocols.
            Err(LxError::ENOSYS) | Err(LxError::ENOTTY) | Err(LxError::EINVAL)
                if request == TIOCGWINSZ && arg1 != 0 && !file_like.is_input_device() =>
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
            // TinyX calls `tcgetattr()` on the console fd during keyboard setup;
            // if the fd backend rejects `TCGETS`, return sane cooked defaults.
            Err(LxError::ENOSYS) | Err(LxError::ENOTTY) | Err(LxError::EINVAL)
                if request == TCGETS && arg1 != 0 =>
            {
                use linux_object::fs::ioctl::Termios;
                let mut ptr: UserOutPtr<Termios> = arg1.into();
                ptr.write(Termios::default_tty())?;
                Ok(0)
            }
            // TinyX puts the console in raw mode with `tcsetattr()` after reading
            // the old settings; accept the update on the active VT even when the
            // fd is not wired as a tty backend (e.g. fb0 / socket fd numbers).
            Err(LxError::ENOSYS) | Err(LxError::ENOTTY) | Err(LxError::EINVAL)
                if matches!(request, TCSETS | TCSETSW | TCSETSF) && arg1 != 0 =>
            {
                use linux_object::fs::ioctl::Termios;
                use linux_object::fs::stdio;
                let termios = UserInPtr::<Termios>::from(arg1).read()?;
                if request == TCSETSF {
                    stdio::set_active_vt_termios_flush(termios);
                } else {
                    stdio::set_active_vt_termios(termios);
                }
                Ok(0)
            }
            // An unhandled ioctl maps to `ENOSYS` ("function not implemented")
            // via the generic FsError conversion, but the POSIX/Linux convention
            // for an ioctl that does not apply to a device is `ENOTTY`
            // ("inappropriate ioctl for device"). Returning `ENOSYS` makes some
            // programs treat it as fatal or retry in a loop, so normalise it.
            Err(LxError::ENOSYS) => Err(LxError::ENOTTY),
            other => other,
        };
        match &ret {
            Ok(v) => kernel_hal::klog_info!(
                "ioctl LEAVE fd={:?} request={:#x} -> Ok({})",
                fd, request, v
            ),
            // Failed/unhandled ioctls go through `error!` so they are printed on
            // the console (and serial), not only into the dmesg ring — the same
            // way an invalid syscall number is surfaced. But throttle a program
            // busy-looping on the same failing ioctl (e.g. polling an unhandled
            // request) so it can't flood the console thousands of times a
            // second: log the first occurrence, then only every 4096th repeat.
            Err(e) => {
                use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
                // `TIOCGWINSZ` returning `ENOTTY` is the normal "not a terminal"
                // answer — e.g. musl's `isatty()` probes input/fb/char devices
                // this way. Record it quietly in the dmesg ring instead of
                // printing an error on the console for every isatty() call.
                if request == TIOCGWINSZ {
                    kernel_hal::klog_info!(
                        "ioctl LEAVE fd={:?} request={:#x} -> ERR {:?} (not a tty)",
                        fd, request, e
                    );
                } else {
                    static LAST_REQ: AtomicU64 = AtomicU64::new(u64::MAX);
                    static REPEATS: AtomicUsize = AtomicUsize::new(0);
                    let n = if LAST_REQ.swap(request as u64, Ordering::Relaxed) == request as u64 {
                        REPEATS.fetch_add(1, Ordering::Relaxed) + 1
                    } else {
                        REPEATS.store(0, Ordering::Relaxed);
                        0
                    };
                    if n == 0 || n % 4096 == 0 {
                        error!(
                            "ioctl LEAVE fd={:?} request={:#x} -> ERR {:?} (unhandled/failed){}",
                            fd,
                            request,
                            e,
                            if n > 0 { " [repeating, throttled]" } else { "" }
                        );
                    }
                }
            }
        }
        ret
    }

    /// Manipulate a file descriptor.
    /// - cmd – cmd flag
    /// - arg – additional parameters based on cmd
    pub fn sys_fcntl(&self, fd: FileDesc, cmd: usize, arg: usize) -> SysResult {
        info!("fcntl: fd={:?}, cmd={}, arg={}", fd, cmd, arg);
        let proc = self.linux_process();
        let file_like = proc.get_file_like(fd)?;
        if let Ok(cmd) = FcntlCmd::try_from(cmd) {
            match cmd {
                FcntlCmd::GETFD => Ok(file_like.flags().close_on_exec() as usize),
                FcntlCmd::SETFD => {
                    let mut flags = file_like.flags();
                    if (arg & 1) != 0 {
                        flags |= OpenFlags::CLOEXEC;
                    } else {
                        flags -= OpenFlags::CLOEXEC;
                    }
                    file_like.set_flags(flags)?;
                    Ok(0)
                }
                FcntlCmd::GETFL => Ok(file_like.flags().bits()),
                FcntlCmd::SETFL => {
                    file_like.set_flags(OpenFlags::from_bits_truncate(arg))?;
                    Ok(0)
                }
                FcntlCmd::DUPFD | FcntlCmd::DUPFD_CLOEXEC => {
                    let new_fd = proc.get_free_fd_from(arg);
                    self.sys_dup2(fd, new_fd)?;
                    let dup = proc.get_file_like(new_fd)?;
                    let mut flags = dup.flags();
                    if cmd == FcntlCmd::DUPFD_CLOEXEC {
                        flags |= OpenFlags::CLOEXEC;
                    } else {
                        flags -= OpenFlags::CLOEXEC;
                    }
                    dup.set_flags(flags)?;
                    Ok(new_fd.into())
                }
                _ => Err(LxError::EINVAL),
            }
        } else {
            Err(LxError::EINVAL)
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
        let flags = AtFlags::from_bits_truncate(flags);
        info!(
            "faccessat: dirfd={:?}, path={:?}, mode={:#o}, flags={:?}",
            dirfd, path, mode, flags
        );
        let proc = self.linux_process();
        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        let inode = proc.lookup_inode_at(dirfd, path, follow)?;
        let metadata = inode.metadata()?;
        let requested = (mode & 0o7) as u16;
        let use_effective = flags.contains(AtFlags::EACCESS);
        proc.check_access(&metadata, requested, use_effective)?;
        Ok(0)
    }

    /// Change file mode by descriptor.
    pub fn sys_fchmod(&self, fd: FileDesc, mode: usize) -> SysResult {
        let proc = self.linux_process();
        let inode = proc.get_file(fd)?.inode();
        let mut metadata = inode.metadata()?;
        proc.chmod_metadata(&mut metadata, mode as u16)?;
        inode.set_metadata(&metadata)?;
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
        let flags = AtFlags::from_bits_truncate(flags);
        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        let proc = self.linux_process();
        let inode = proc.lookup_inode_at(dirfd, path, follow)?;
        let mut metadata = inode.metadata()?;
        proc.chmod_metadata(&mut metadata, mode as u16)?;
        inode.set_metadata(&metadata)?;
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
        let flags = AtFlags::from_bits_truncate(flags);
        let follow = !flags.contains(AtFlags::SYMLINK_NOFOLLOW);
        let proc = self.linux_process();
        let inode = proc.lookup_inode_at(dirfd, path, follow)?;
        let mut metadata = inode.metadata()?;
        proc.chown_metadata(&mut metadata, uid as u32, gid as u32)?;
        inode.set_metadata(&metadata)?;
        Ok(0)
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
        const UTIME_NOW: usize = 0x3fffffff;
        const UTIME_OMIT: usize = 0x3ffffffe;
        let proc = self.linux_process();
        let mut times = if times.is_null() {
            let epoch = TimeSpec::now();
            [epoch, epoch]
        } else {
            let times = times.read()?;
            [times[0], times[1]]
        };
        let inode = if pathname.is_null() {
            let fd = dirfd;
            info!("futimens: fd: {:?}, times: {:?}", fd, times);
            proc.get_file(fd)?.inode()
        } else {
            let pathname = pathname.as_c_str()?;
            info!(
                "utimensat: dirfd: {:?}, pathname: {:?}, times: {:?}, flags: {:#x}",
                dirfd, pathname, times, flags
            );
            let follow = if flags == 0 {
                true
            } else if flags == AtFlags::SYMLINK_NOFOLLOW.bits() {
                false
            } else {
                return Err(LxError::EINVAL);
            };
            proc.lookup_inode_at(dirfd, pathname, follow)?
        };
        let mut metadata = inode.metadata()?;
        if times[0].nsec != UTIME_OMIT {
            if times[0].nsec == UTIME_NOW {
                times[0] = TimeSpec::now();
            }
            metadata.atime = rcore_fs::vfs::Timespec {
                sec: times[0].sec as i64,
                nsec: times[0].nsec as i32,
            };
        }
        if times[1].nsec != UTIME_OMIT {
            if times[1].nsec == UTIME_NOW {
                times[1] = TimeSpec::now();
            }
            metadata.mtime = rcore_fs::vfs::Timespec {
                sec: times[1].sec as i64,
                nsec: times[1].nsec as i32,
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
            // TODO 文件系统的魔数，需要 rcore-fs 提供一个渠道获取
            // 但是这个似乎并没有什么用处，新的 vfs 相关函数都去掉了，也许永远填个常数就好了
            f_type: 0,
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
    let x_marker = has(b"(EE)") || has(b"(WW)") || has(b"Fatal") || has(b"no screens")
        || has(b"(II) ");
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
    if x_marker || ld_error {
        if let Ok(s) = core::str::from_utf8(scan) {
            for line in s.split('\n').filter(|l| !l.is_empty()).take(6) {
                kernel_hal::klog_info!("XLOG: {}", line);
            }
        }
    }
}
