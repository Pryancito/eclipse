//! `unshare(2)` of what a process can share without namespaces.
//!
//! It answered `ENOSYS` to every call. Namespaces still are not here, and a
//! call that asks for one still gets `ENOSYS` (see the `Sys::UNSHARE` arm);
//! the rest is what Linux 6.18 answers (`clone-survey`, measured there with
//! glibc and musl):
//!
//! * a flag `check_unshare_flags` does not know is `EINVAL`, and nothing at
//!   all is 0;
//! * `CLONE_VM` takes `CLONE_SIGHAND` with it and `CLONE_SIGHAND` takes
//!   `CLONE_THREAD`, and the three can only be "unshared" by a process with
//!   one thread and an address space of its own -- `EINVAL` otherwise --
//!   where there is nothing to do;
//! * `CLONE_FS` and `CLONE_FILES` have nothing to do either: `clone` refuses
//!   to share them with another process, so only the process's own threads
//!   share them;
//! * `CLONE_SYSVSEM` is `exit_sem`: the `SEM_UNDO` adjustments the process
//!   owes go back now, as at its exit, and the next `semop` with `SEM_UNDO`
//!   starts a new list.
//!
//! What Linux does and this kernel cannot is give ONE thread of several its
//! own working directory, descriptor table or undo list: those are the
//! process's here, not the thread's. That is `EINVAL`, which is the answer
//! Linux itself gives for the unsharing it does not implement ("pretend it
//! works if there is nothing to unshare"), and not a 0 after which a
//! thread's `chdir` or `close` would still move every other thread.

use super::*;

/// `CLONE_NEWTIME`, which `clone` cannot take (its bit is in `CSIGNAL`).
const NEWTIME: usize = 0x80;

/// Every flag `check_unshare_flags` accepts.
const UNSHARE_KNOWN: usize = CloneFlags::THREAD.bits()
    | CloneFlags::FS.bits()
    | CloneFlags::SIGHAND.bits()
    | CloneFlags::VM.bits()
    | CloneFlags::FILES.bits()
    | CloneFlags::SYSVSEM.bits()
    | UNSUPPORTED_CLONE_NS.bits()
    | NEWTIME;

/// What a process is sharing that `unshare` has to look at.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Sharing {
    /// It has another thread.
    pub(super) threads: bool,
    /// Its address space is another process's too (a `vfork` child before
    /// its `execve`).
    pub(super) memory: bool,
}

/// What `unshare(flags)` comes to for a process that shares `sharing`:
/// whether the `SEM_UNDO` list goes (`exit_sem`), or why not.
pub(super) fn unshare_plan(flags: usize, sharing: Sharing) -> LxResult<bool> {
    if flags & !UNSHARE_KNOWN != 0 {
        return Err(LxError::EINVAL);
    }
    if flags & (UNSUPPORTED_CLONE_NS.bits() | NEWTIME) != 0 {
        return Err(LxError::ENOSYS);
    }
    let mut f = CloneFlags::from_bits_truncate(flags);
    if f.contains(CloneFlags::VM) {
        f |= CloneFlags::SIGHAND;
    }
    if f.contains(CloneFlags::SIGHAND) {
        f |= CloneFlags::THREAD;
    }
    let per_thread = CloneFlags::THREAD | CloneFlags::FS | CloneFlags::FILES | CloneFlags::SYSVSEM;
    if (f.intersects(per_thread) && sharing.threads)
        || (f.contains(CloneFlags::VM) && sharing.memory)
    {
        return Err(LxError::EINVAL);
    }
    Ok(f.contains(CloneFlags::SYSVSEM))
}

impl Syscall<'_> {
    /// `unshare(2)`, without namespaces: see the module.
    pub fn sys_unshare(&self, flags: usize) -> SysResult {
        let proc = self.zircon_process();
        let sharing = Sharing {
            threads: proc.thread_count() > 1,
            memory: proc.shares_address_space(),
        };
        if unshare_plan(flags, sharing)? {
            self.linux_process().semaphores_unshare();
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests;
