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
/// The `struct itimerspec` of the `timerfd` calls, shared with the i386
/// compat layer: `timerfd_gettime64`/`timerfd_settime64` pass the SAME
/// structure at the native width from a 32-bit process, so they reach the
/// handlers here rather than carrying a shape of their own (see
/// `compat32::time64`). x86_64-only because that layer is.
#[cfg(target_arch = "x86_64")]
pub(crate) use fd::ITimerSpec;
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
///
/// Gated on the architecture as well as on `test`, like `StatMode` above and
/// for the same reason: `compat32` is x86_64 only, so without the second
/// gate this is an unused import in an aarch64 or riscv64 test build, which
/// `deny(warnings)` makes a compile error. `make clippy` does not catch it
/// --- it builds the kernel target, where `cfg(test)` code does not exist
/// --- so the first thing to say so is the `Zircon Core Test Libos`
/// job.
#[cfg(all(test, target_arch = "x86_64"))]
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
    /// `argv` and `envp` as the calling process writes them: an array of
    /// pointers ended by a `NULL`, four bytes each for a 32-bit process and
    /// eight for a native one.
    ///
    /// The same rule as [`read_iovecs`](Self::read_iovecs), and for the same
    /// reason: the width is the process's, not the call site's, so `execve`
    /// and `execveat` both get it without either one remembering. Without
    /// this an `execve` from a 32-bit program read its `argv` at the native
    /// width, which does not fail --- it builds the new program's arguments
    /// out of addresses the caller never wrote.
    pub(crate) fn read_cstring_array(
        &self,
        array: UserInPtr<UserInPtr<u8>>,
    ) -> LxResult<alloc::vec::Vec<alloc::string::String>> {
        if self.linux_process().abi().is_32bit() {
            array.read_cstring_array32()
        } else {
            array.read_cstring_array()
        }
        .map_err(Into::into)
    }

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
