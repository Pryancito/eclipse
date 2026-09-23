//! `pidfd_open`, `pidfd_send_signal`, `pidfd_getfd`

use super::*;
use linux_object::fs::{OpenFlags, PidFd, PIDFD_THREAD};
use linux_object::process::send_signal_to_process;
use linux_object::signal::{SigInfo, Signal};
use zircon_object::object::KoID;
use zircon_object::task::{Status, ROOT_JOB};

/// Reduce `pidfd_open(2)`'s flag word to the open flags the new fd carries.
///
/// Split out of `sys_pidfd_open` because everything it decides is decided here:
/// the rest of that syscall needs a live process to say anything at all.
fn pidfd_open_flags(flags: u32) -> linux_object::error::LxResult<OpenFlags> {
    const NONBLOCK: u32 = OpenFlags::NON_BLOCK.bits() as u32;
    if flags & !(NONBLOCK | PIDFD_THREAD) != 0 {
        return Err(LxError::EINVAL);
    }
    // PIDFD_THREAD would make the fd refer to one thread; `PidFd` holds a
    // process, so accepting it would hand back an fd that waits on the wrong
    // thing and reports the wrong exit.
    if flags & PIDFD_THREAD != 0 {
        return Err(LxError::EINVAL);
    }
    // pidfd_open(2): the fd is always close-on-exec, whatever the caller asked.
    let mut open_flags = OpenFlags::CLOEXEC;
    if flags & NONBLOCK != 0 {
        open_flags |= OpenFlags::NON_BLOCK;
    }
    Ok(open_flags)
}

impl Syscall<'_> {
    /// Create a pollable file descriptor referring to `pid`.
    pub fn sys_pidfd_open(&self, pid: usize, flags: u32) -> SysResult {
        if pid == 0 || pid > i32::MAX as usize {
            return Err(LxError::EINVAL);
        }
        let open_flags = pidfd_open_flags(flags)?;
        let process = ROOT_JOB.find_process(pid as KoID).ok_or(LxError::ESRCH)?;
        let pidfd = PidFd::new(process, open_flags);
        let fd = self.linux_process().add_file(pidfd)?;
        Ok(fd.into())
    }

    /// Send `sig` to the process referred to by `pidfd`.
    pub fn sys_pidfd_send_signal(
        &self,
        pidfd: FileDesc,
        signum: usize,
        info: UserInPtr<SigInfo>,
        flags: u32,
    ) -> SysResult {
        if flags != 0 {
            return Err(LxError::EINVAL);
        }
        let signal = Signal::from_syscall_arg(signum)?;
        let proc = self.linux_process();
        let pidfd = PidFd::from_file_like(proc.get_file_like(pidfd)?)?;
        let target = pidfd.target().clone();
        if matches!(target.status(), Status::Exited(_)) {
            return Err(LxError::ESRCH);
        }
        let _ = info;
        match signal {
            // pidfd_send_signal(fd, 0, ...): the liveness check above is the
            // whole answer, as it is for kill(pid, 0).
            None => {}
            Some(Signal::SIGKILL) => {
                let retcode = (128 + Signal::SIGKILL as i32) as i64;
                target.exit(retcode);
            }
            Some(sig) => send_signal_to_process(target.id() as usize, sig)?,
        }
        Ok(0)
    }

    /// Duplicate `targetfd` from the process referred to by `pidfd`.
    pub fn sys_pidfd_getfd(&self, pidfd: FileDesc, targetfd: i32, flags: u32) -> SysResult {
        if flags != 0 || targetfd < 0 {
            return Err(LxError::EINVAL);
        }
        let caller = self.linux_process();
        let pidfd = PidFd::from_file_like(caller.get_file_like(pidfd)?)?;
        let target_proc = pidfd.target();
        if matches!(target_proc.status(), Status::Exited(_)) {
            return Err(LxError::ESRCH);
        }
        // Like `dup`, this installs the target's own open file description
        // here (`pidfd_getfd` calls `get_file`), so the two processes share
        // the offset and the status flags. `pidfd_getfd(2)`: "the
        // close-on-exec flag is set on the file descriptor" -- on THIS
        // descriptor, not on the description the target is still using.
        let file = target_proc
            .try_linux()
            .ok_or(LxError::ESRCH)?
            .get_file_like(targetfd.into())?;
        let new_fd = caller.add_file_cloexec(file, true)?;
        Ok(new_fd.into())
    }
}

/// `pidfd_open(2)`'s flag word, which was carrying a constant that did not mean
/// what it was named.
#[cfg(test)]
mod pidfd_open_flag_tests {
    use super::*;

    #[test]
    fn pidfd_thread_is_o_excl_the_way_linux_spells_it() {
        // `include/uapi/linux/pidfd.h`: `#define PIDFD_THREAD O_EXCL`. The tree
        // had 2, which is O_RDWR's bit pattern and no flag pidfd_open knows.
        assert_eq!(PIDFD_THREAD, 0o200);
        assert_eq!(PIDFD_THREAD, OpenFlags::EXCLUSIVE.bits() as u32);
    }

    #[test]
    fn no_flags_still_gives_a_close_on_exec_fd() {
        // pidfd_open(2) sets O_CLOEXEC unconditionally: a pidfd that survived
        // exec would keep a dead child's exit status reachable from the new
        // program.
        let flags = pidfd_open_flags(0).unwrap();
        assert!(flags.contains(OpenFlags::CLOEXEC));
        assert!(!flags.contains(OpenFlags::NON_BLOCK));
    }

    #[test]
    fn pidfd_nonblock_reaches_the_open_flags() {
        let flags = pidfd_open_flags(OpenFlags::NON_BLOCK.bits() as u32).unwrap();
        assert!(flags.contains(OpenFlags::NON_BLOCK));
        assert!(flags.contains(OpenFlags::CLOEXEC));
    }

    #[test]
    fn pidfd_thread_is_refused_rather_than_ignored() {
        // Accepting it would hand back a process pidfd to a caller that asked
        // for a thread one, and it would look like it worked until the fd
        // reported the wrong exit.
        assert_eq!(pidfd_open_flags(PIDFD_THREAD), Err(LxError::EINVAL));
        assert_eq!(
            pidfd_open_flags(PIDFD_THREAD | OpenFlags::NON_BLOCK.bits() as u32),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn an_unknown_flag_is_refused() {
        // Including the old value of PIDFD_THREAD, so a program built against
        // a header where it was 2 does not quietly get a working fd.
        for bad in [1u32, 2, 4, OpenFlags::CLOEXEC.bits() as u32, 1 << 31] {
            assert_eq!(pidfd_open_flags(bad), Err(LxError::EINVAL), "flag {}", bad);
        }
    }
}
