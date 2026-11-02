//! Syscalls for files
#![deny(missing_docs)]
use super::*;
use bitflags::bitflags;
use linux_object::fs::vfs::{FileType, FsError};
use linux_object::fs::*;

mod dir;
mod drm_prime;
mod fd;
mod hunter_gate;
/// Shared by `socket`/`socketpair` (and the anonymous-fd constructors in `fd`)
/// so a stray bit is `EINVAL`, not a silently truncated `OpenFlags`.
pub(crate) use fd::{anon_fd_flags, open_flags, ANON_CLOEXEC, ANON_NONBLOCK};
#[allow(clippy::module_inception)]
mod file;
pub(crate) use file::{after_write_error, sigpipe_due, writes_a_file, AfterWriteError};
/// The other four lock commands and the structure they all point at, shared
/// with the i386 compat layer, which answers the six from a `struct flock` of
/// its own shape (see `compat32::fcntl`). x86_64-only because that layer is:
/// elsewhere these would be imports nobody uses, and `deny(warnings)` turns an
/// unused import into an error.
#[cfg(target_arch = "x86_64")]
pub(crate) use file::{Flock, F_GETLK, F_OFD_GETLK, F_OFD_SETLK, F_SETLK};
/// Shared with `restart`, which reports on the two lock commands that wait:
/// `sys_fcntl` is what decides it, so the numbers live there and not twice.
pub(crate) use file::{F_OFD_SETLKW, F_SETLKW};
mod mount;
#[cfg(target_arch = "x86_64")]
mod nvidia;
mod pidfd;
mod poll;
mod splice;
mod stat;
/// Shared with the i386 compat layer, which lays the same type-and-mode word
/// into a `struct stat64` of its own. One table of `S_IF*` bits, not two.
#[cfg(target_arch = "x86_64")]
pub(crate) use self::stat::StatMode;
/// Shared with the i386 compat layer's alignment tests, which ask `statx`
/// for a field at a destination this kernel would otherwise refuse and need
/// the offset of that field. Test-only: production has no reason to name the
/// type outside the module that fills it.
#[cfg(test)]
pub(crate) use self::stat::Statx;
mod xattr;
pub(crate) use xattr::{XattrOp, XattrTarget};

impl Syscall<'_> {
    /// Read a `readv`-shaped scatter/gather list, in whichever width the
    /// calling process's ABI uses.
    ///
    /// A native `struct iovec` is two 8-byte words; a 32-bit process's is two
    /// 4-byte ones. Reading the narrow list as the wide one does not fail ---
    /// that is what makes it dangerous --- it pairs the halves wrongly, so
    /// the first buffer's address comes out as entry 0's base with entry 0's
    /// *length* in its high half, and its length as the whole of entry 1.
    /// `writev` then gathers from an address nothing named, for hundreds of
    /// gigabytes, and reports a count.
    ///
    /// The width is asked of the process and not of the call site, which is
    /// `in_compat_syscall()` in Linux and is the only arrangement that stays
    /// right: `readv`, `preadv`, `sendmsg` and every later caller get it
    /// without each one remembering to.
    ///
    /// This covers the list and not what surrounds it. `sendmsg` reads its
    /// `msg_iov` out of a `struct msghdr`, which is a 32-bit structure of its
    /// own and still needs its own conversion; a 32-bit process cannot reach
    /// that call yet (`socketcall` answers `ENOSYS`), so routing the list
    /// through here makes it one less thing wrong rather than one thing
    /// right.
    pub(crate) fn read_iovecs<P: 'static + kernel_hal::user::Policy>(
        &self,
        iov_ptr: UserInPtr<kernel_hal::user::IoVec<P>>,
        iov_count: usize,
    ) -> LxResult<kernel_hal::user::IoVecs<P>> {
        if self.linux_process().abi().is_32bit() {
            iov_ptr.read_iovecs32(iov_count)
        } else {
            iov_ptr.read_iovecs(iov_count)
        }
        .map_err(Into::into)
    }
}

// Shared with the FreeBSD personality's `getdirentries`, which only exists on
// x86_64; elsewhere the re-export would be an unused import under
// `deny(warnings)`.
#[cfg(target_arch = "x86_64")]
pub(crate) use self::dir::collect_dirents;
/// Shared with `execveat`, which takes the same `AT_*` word.
pub(crate) use self::dir::{at_flags, AtFlags};
use self::dir::{FSTATAT_FLAGS, STATX_FLAGS};
pub(crate) use self::poll::poll_timeout_msecs;
