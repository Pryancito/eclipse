//! `process_vm_readv(2)` and `process_vm_writev(2)`: copying between the
//! caller's memory and another process's, without either one doing the copy.
//!
//! They answered `ENOSYS`. A debugger or a profiler that reads a target's
//! stack this way, and an MPI transport that copies between ranks, had to
//! fall back to something slower or did not work.
//!
//! The order of the questions is Linux's (`process_vm_rw`,
//! `process_vm_rw_core`): the flags, the caller's own iovecs, nothing to copy
//! answers 0, then the target's iovecs, then the process, then whether the
//! caller may reach into it, which is `ptrace_may_access(task,
//! PTRACE_MODE_ATTACH_REALCREDS)`. The copy goes as far as it can and says
//! how far: a fault after some bytes is those bytes, a fault before any is
//! `EFAULT`.

use super::*;
use kernel_hal::user::IoVecIn;
use linux_object::process::{find_task, process_of_thread, remote_memory, LinuxProcess};
use zircon_object::task::Status;
use zircon_object::vm::{VmAddressRegion, PAGE_SIZE};

/// `UIO_MAXIOV`.
const IOV_MAX: usize = 1024;

impl Syscall<'_> {
    /// Copy from process `pid`'s memory, at `remote`, into the caller's, at
    /// `local`.
    pub fn sys_process_vm_readv(
        &self,
        pid: usize,
        local: UserInPtr<IoVecIn>,
        liovcnt: usize,
        remote: UserInPtr<IoVecIn>,
        riovcnt: usize,
        flags: usize,
    ) -> SysResult {
        self.process_vm(pid, (local, liovcnt), (remote, riovcnt), flags, false)
    }

    /// Copy from the caller's memory, at `local`, into process `pid`'s, at
    /// `remote`.
    pub fn sys_process_vm_writev(
        &self,
        pid: usize,
        local: UserInPtr<IoVecIn>,
        liovcnt: usize,
        remote: UserInPtr<IoVecIn>,
        riovcnt: usize,
        flags: usize,
    ) -> SysResult {
        self.process_vm(pid, (local, liovcnt), (remote, riovcnt), flags, true)
    }

    fn process_vm(
        &self,
        pid: usize,
        local: (UserInPtr<IoVecIn>, usize),
        remote: (UserInPtr<IoVecIn>, usize),
        flags: usize,
        to_remote: bool,
    ) -> SysResult {
        if flags != 0 {
            return Err(LxError::EINVAL);
        }
        let local = iovecs(local.0, local.1)?;
        if local.iter().all(|&(_, len)| len == 0) {
            return Ok(0);
        }
        let remote = iovecs(remote.0, remote.1)?;
        if remote.iter().all(|&(_, len)| len == 0) {
            return Ok(0);
        }
        let target = find_task(pid as KoID)
            .or_else(|| process_of_thread(pid as KoID))
            .ok_or(LxError::ESRCH)?;
        // A zombie has no memory left to copy (`mm_access` finds no `mm`).
        if matches!(target.status(), Status::Exited(_)) {
            return Err(LxError::ESRCH);
        }
        let target_linux = target.try_linux().ok_or(LxError::ESRCH)?;
        let caller = self.linux_process();
        if !LinuxProcess::may_attach_to(
            &caller.credentials(),
            &target_linux.credentials(),
            target.id() == self.zircon_process().id(),
            target_linux.dumpable(),
        ) {
            return Err(LxError::EPERM);
        }
        let copied = copy(&target.vmar(), &local, &remote, to_remote);
        if copied == 0 {
            return Err(LxError::EFAULT);
        }
        Ok(copied)
    }
}

/// The `count` iovecs at `ptr` as `(address, length)`, read as
/// `import_iovec` reads them: more than [`IOV_MAX`] is `EINVAL` before the
/// array is looked at, and none at all is no array to look at.
fn iovecs(ptr: UserInPtr<IoVecIn>, count: usize) -> LxResult<Vec<(usize, usize)>> {
    if count > IOV_MAX {
        return Err(LxError::EINVAL);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let iovecs = ptr.read_iovecs(count)?;
    Ok(iovecs.iter().map(|v| (v.addr(), v.len())).collect())
}

/// Copy between the caller's `local` iovecs and `vmar`'s `remote` ones, in
/// the direction `to_remote` says, until either side runs out or a page on
/// either side cannot be copied. Returns the bytes copied.
fn copy(
    vmar: &VmAddressRegion,
    local: &[(usize, usize)],
    remote: &[(usize, usize)],
    to_remote: bool,
) -> usize {
    let mut buf = alloc::vec![0u8; PAGE_SIZE];
    let mut copied = 0;
    let mut here = local.iter().copied().filter(|&(_, len)| len != 0);
    let Some(mut at) = here.next() else {
        return 0;
    };
    for &(mut there, mut left) in remote {
        while left != 0 {
            if at.1 == 0 {
                match here.next() {
                    Some(next) => at = next,
                    None => return copied,
                }
            }
            let n = left.min(at.1).min(PAGE_SIZE - there % PAGE_SIZE);
            let done = if to_remote {
                let Ok(bytes) = UserInPtr::<u8>::from(at.0).read_array(n) else {
                    return copied;
                };
                remote_memory::write(vmar, there, &bytes)
            } else {
                let got = remote_memory::read(vmar, there, &mut buf[..n]);
                let mut out = UserOutPtr::<u8>::from(at.0);
                if out.write_array(&buf[..got]).is_err() {
                    return copied;
                }
                got
            };
            copied += done;
            if done < n {
                return copied;
            }
            at = (at.0 + n, at.1 - n);
            there += n;
            left -= n;
        }
    }
    copied
}

#[cfg(test)]
mod tests;
