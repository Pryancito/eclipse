//! `RLIMIT_FSIZE` on the way into a regular file: how much of a write the
//! caller's limit lets through.
//!
//! Linux asks it of every write to a regular file, whatever the syscall
//! (`generic_write_check_limits`): a write that starts at or past the limit
//! is `EFBIG`, and one that crosses it is cut short at the limit. Only
//! `truncate`, `ftruncate` and `pwrite` asked here, and `pwrite` answered
//! `EFBIG` for a write that crossed the limit instead of writing up to it:
//! `write`, `writev` and the rest went past it as if there were none. The
//! `SIGXFSZ` that comes with the `EFBIG` is the syscall's to send, since a
//! write here can be one chunk of a call that already wrote some.

use alloc::sync::Arc;
use rcore_fs::vfs::{FileType, INode};
use zircon_object::task::Thread;

use crate::error::{LxError, LxResult};
use crate::process::{ProcessExt, RLIMIT_FSIZE, RLIM_INFINITY};

/// How many of `len` bytes a write at `offset` may put in `inode`: all of
/// them, unless `inode` is a regular file and the calling process has a
/// limit, and then [`write_room`]'s answer.
///
/// A pipe is let through before the limit is looked up, since a pipe is
/// most of what is written and the limit is behind the process's lock.
pub(super) fn fsize_room(inode: &Arc<dyn INode>, offset: u64, len: usize) -> LxResult<usize> {
    if len == 0 || inode.downcast_ref::<crate::fs::Pipe>().is_some() {
        return Ok(len);
    }
    let Some(limit) = current_fsize_limit() else {
        return Ok(len);
    };
    if inode.metadata()?.type_ != FileType::File {
        return Ok(len);
    }
    write_room(limit, offset, len)
}

/// `generic_write_check_limits` under a finite `limit`: a write at or past
/// it is `EFBIG`, and one that crosses it stops there.
pub(crate) fn write_room(limit: u64, offset: u64, len: usize) -> LxResult<usize> {
    if offset >= limit {
        return Err(LxError::EFBIG);
    }
    Ok((len as u64).min(limit - offset) as usize)
}

/// The calling process's `RLIMIT_FSIZE`, or `None` when it has none, or
/// when no process is calling (a kernel context).
fn current_fsize_limit() -> Option<u64> {
    let thread = kernel_hal::thread::get_current_thread()?
        .downcast::<Thread>()
        .ok()?;
    let limit = thread
        .proc()
        .try_linux()?
        .rlimit(RLIMIT_FSIZE, None, false)
        .ok()?;
    (limit.cur != RLIM_INFINITY).then_some(limit.cur)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::fs::{File, FileLike, OpenFlags};
    use crate::process::{LinuxProcess, RLimit};
    use crate::thread::ThreadExt;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::{Process, ROOT_JOB};

    #[test]
    fn a_write_stops_at_the_limit_and_one_past_it_is_efbig() {
        assert_eq!(write_room(4096, 0, 8192), Ok(4096));
        assert_eq!(write_room(4096, 4000, 200), Ok(96));
        assert_eq!(write_room(4096, 100, 10), Ok(10));
        assert_eq!(write_room(4096, 4096, 1), Err(LxError::EFBIG));
        assert_eq!(write_room(4096, u64::MAX, 1), Err(LxError::EFBIG));
    }

    /// `f` run as a thread of a process whose `RLIMIT_FSIZE` is `limit`,
    /// with `/data` open for reading and writing in its root.
    fn as_a_process_limited_to<R>(pid: u64, limit: u64, f: impl FnOnce(&File) -> R) -> R {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "fsize",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let thread = Thread::create_linux(&proc).unwrap();
        proc.linux()
            .rlimit(
                RLIMIT_FSIZE,
                Some(RLimit {
                    cur: limit,
                    max: limit,
                }),
                false,
            )
            .unwrap();
        let inode = proc
            .linux()
            .root_inode()
            .create("data", FileType::File, 0o644)
            .unwrap();
        let file = File::new(inode, OpenFlags::RDWR, "/data".into());
        let r = async_std::task::block_on(async {
            kernel_hal::thread::set_current_thread(Some(thread.clone()));
            let r = f(&file);
            kernel_hal::thread::set_current_thread(None);
            r
        });
        thread.terminate_abandoned();
        r
    }

    #[test]
    fn write_and_pwrite_go_as_far_as_the_limit() {
        let got = as_a_process_limited_to(45_131, 4096, |file| {
            let big = alloc::vec![7u8; 8192];
            (
                file.write(&big),
                file.write(&big[..1]),
                file.write_at(4000, &big[..200]),
                file.write_at(4096, &[]),
                file.write_at(5000, &big[..1]),
                file.metadata().map(|m| m.size),
                file.fill_at(5000, &[0u8; 8]),
            )
        });
        assert_eq!(
            got,
            (
                Ok(4096),
                Err(LxError::EFBIG),
                Ok(96),
                Ok(0),
                Err(LxError::EFBIG),
                Ok(4096),
                Ok(8)
            ),
            "(a write across the limit, one at it, a pwrite across it, an empty \
             one at it, one past it, the size, and fallocate's zeroing past it)"
        );
    }

    #[test]
    fn no_limit_is_no_limit() {
        let got = as_a_process_limited_to(45_132, RLIM_INFINITY, |file| {
            file.write(&alloc::vec![7u8; 8192])
        });
        assert_eq!(got, Ok(8192));
    }

    /// A kernel context has no process, and no limit.
    #[test]
    fn nor_is_a_write_from_no_process() {
        use rcore_fs::vfs::FileSystem;
        let inode: Arc<dyn INode> = RamFS::new()
            .root_inode()
            .create("data", FileType::File, 0o644)
            .unwrap();
        assert_eq!(fsize_room(&inode, 1 << 40, 8192), Ok(8192));
    }
}
