//! Syscalls for files
#![deny(missing_docs)]
use super::*;
use bitflags::bitflags;
use linux_object::fs::vfs::{FileType, FsError};
use linux_object::fs::*;

mod dir;
mod fd;
/// Shared by `socket`/`socketpair` (and the anonymous-fd constructors in `fd`)
/// so a stray bit is `EINVAL`, not a silently truncated `OpenFlags`.
pub(crate) use fd::{anon_fd_flags, open_flags, ANON_CLOEXEC, ANON_NONBLOCK};
#[allow(clippy::module_inception)]
mod file;
pub(crate) use file::{after_write_error, sigpipe_due, AfterWriteError};
mod mount;
mod pidfd;
mod poll;
mod splice;
mod stat;
mod xattr;
pub(crate) use xattr::{XattrOp, XattrTarget};

// Shared with the FreeBSD personality's `getdirentries`, which only exists on
// x86_64; elsewhere the re-export would be an unused import under
// `deny(warnings)`.
#[cfg(target_arch = "x86_64")]
pub(crate) use self::dir::collect_dirents;
use self::dir::{at_flags, AtFlags, FSTATAT_FLAGS, STATX_FLAGS};
pub(crate) use self::poll::poll_timeout_msecs;
