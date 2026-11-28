//! The mount helper: a process that `mount(2)` of a filesystem the kernel
//! has no code for asks to make the mount.
//!
//! FAT is not the kernel's any more: `eclipse-fatfs` serves it
//! ([`super::serve`]). A program that calls `mount(2)` with `vfat` --
//! `mount -t vfat`, the installer, a desktop's automounter -- still gets
//! a mount, from the kernel's side unchanged: the call goes to the helper
//! (`eclipse-mountd`, a service of init), which starts the server, and
//! returns when the server has made the mount or the helper says why not.
//!
//! The helper registers once with `ECLIPSE_MOUNT_HELP`, with a FIFO and a
//! window like a filesystem server's, and is asked on them as one is
//! ([`Link`]): one message per mount, [`OP_MOUNT`], the names
//! `source\0target\0type` (the source an absolute path), the flags of
//! `mount(2)` in `a` (`MS_RDONLY` set for an `ro` among the options) and
//! its option string in the data area, `b` bytes of it. It answers
//! 0, or a negative `errno`. A helper that dies leaves none: `mount(2)` of
//! those types is `ENODEV` until the next one registers.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use lock::Mutex;
use zircon_object::ipc::Fifo;
use zircon_object::vm::VmObject;

use super::link::{Fail, Link};
use crate::error::{LxError, LxResult};
use crate::process::LinuxProcess;

/// Make the mount: names `source\0target\0type`, `a` the flags, the data
/// area the options.
pub const OP_MOUNT: u32 = 1;

/// Start a server that takes over the served mount at `target`, which has
/// none: the names and the rest as for [`OP_MOUNT`], the flags only
/// `MS_RDONLY`. 0 once the server has the mount, or a negative `errno`.
pub const OP_ADOPT: u32 = 2;

/// `MS_RDONLY`.
const MS_RDONLY: u64 = 1;

/// How long a mount may take before the helper is taken for hung: a
/// server's start and the first read of its volume, or a network
/// filesystem's greeting, which waits a minute for its server.
const HUNG_AFTER_MS: u64 = 120_000;

/// The helper, while there is one.
static HELPER: Mutex<Option<Arc<Link>>> = Mutex::new(None);

/// A helper that registered, for whoever watches for it to go.
pub struct Helper {
    link: Arc<Link>,
}

impl Helper {
    /// How the helper is ended when it answers nothing for as long as a
    /// mount may take: it is then a helper that went.
    pub fn killed_with(&self, kill: alloc::boxed::Box<dyn Fn() + Send + Sync>) {
        self.link.killed_with(kill);
    }

    /// The helper went: its FIFO closed. `mount(2)` of its types is
    /// `ENODEV` from here, until the next.
    pub fn left(&self) {
        self.link.close();
        let mut helper = HELPER.lock();
        if helper.as_ref().is_some_and(|h| Arc::ptr_eq(h, &self.link)) {
            *helper = None;
        }
    }
}

/// The process `proc` is the mount helper, asked on `fifo` and `window`.
///
/// `EPERM` to a process that may not `mount(2)`, `EBUSY` while another
/// helper is there, `EINVAL` for a FIFO or window a server's could not be.
pub fn register(proc: &LinuxProcess, fifo: Arc<Fifo>, window: Arc<VmObject>) -> LxResult<Helper> {
    if !proc.capable(crate::process::CAP_SYS_ADMIN) {
        return Err(LxError::EPERM);
    }
    let link = Arc::new(Link::new(fifo, window).map_err(|_| LxError::EINVAL)?);
    link.hung_after_ms.store(HUNG_AFTER_MS, Ordering::Relaxed);
    let mut helper = HELPER.lock();
    // A look at the last one's FIFO first: one that was killed is gone
    // before the task that waits on it has said so.
    if helper.as_ref().is_some_and(|h| !h.looked_dead()) {
        return Err(LxError::EBUSY);
    }
    *helper = Some(link.clone());
    info!("mount helper: registered");
    Ok(Helper { link })
}

/// Whether there is a helper to ask.
pub fn present() -> bool {
    HELPER.lock().as_ref().is_some_and(|h| !h.is_dead())
}

/// Have the helper mount `source` (an absolute path) at `target` as
/// `fstype`, with `mount(2)`'s `flags` and option string `data`.
///
/// `ENODEV` when there is no helper, or it went while it was asked: the
/// type is one the kernel has no code for, as it would be on Linux without
/// the module.
pub fn ask(source: &str, target: &str, fstype: &str, flags: usize, data: &str) -> LxResult<()> {
    call(OP_MOUNT, source, target, fstype, flags as u64, data)?;
    super::made_with_options(target, data);
    Ok(())
}

/// Have the helper start a server for the served mount of `source` at
/// `target` as `fstype` that has none ([`OP_ADOPT`]), with the options
/// `data` of the `mount(2)` that made it. `ENODEV` when there is no helper.
pub fn adopt(
    source: &str,
    target: &str,
    fstype: &str,
    read_only: bool,
    data: &str,
) -> LxResult<()> {
    let flags = if read_only { MS_RDONLY } else { 0 };
    call(OP_ADOPT, source, target, fstype, flags, data)
}

fn call(op: u32, source: &str, target: &str, fstype: &str, flags: u64, data: &str) -> LxResult<()> {
    let link = HELPER
        .lock()
        .clone()
        .filter(|h| !h.is_dead())
        .ok_or(LxError::ENODEV)?;
    let mut names = Vec::with_capacity(source.len() + target.len() + fstype.len() + 2);
    for (i, part) in [source, target, fstype].iter().enumerate() {
        if part.as_bytes().contains(&0) {
            return Err(LxError::EINVAL);
        }
        if i > 0 {
            names.push(0);
        }
        names.extend_from_slice(part.as_bytes());
    }
    match link.call_with(op, &names, flags, data.len() as u32, data.as_bytes()) {
        Ok(_) => Ok(()),
        Err(Fail::Errno(n)) => Err(LxError::from_errno(n)),
        Err(Fail::Said(e)) => Err(LxError::from(e)),
        Err(Fail::Gone) => Err(LxError::ENODEV),
    }
}

/// What a test that installs a fake helper holds: there is one helper.
#[cfg(test)]
pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// The helper `link`, as [`register`] makes it, for a test's fake one.
#[cfg(test)]
pub(super) fn install(link: Arc<Link>) -> Helper {
    *HELPER.lock() = Some(link.clone());
    Helper { link }
}

#[cfg(test)]
#[path = "helper_tests.rs"]
mod tests;
