//! `pidfd_open`, `pidfd_send_signal`, `pidfd_getfd`

use super::*;
use crate::signal::{may_queue_siginfo, queued_from_user, SigInfoHead};
use linux_object::error::LxResult;
use linux_object::fs::{OpenFlags, PidFd, PIDFD_THREAD};
use linux_object::process::LinuxProcess;
use linux_object::signal::{SigInfo, Signal};
use zircon_object::object::{KernelObject, KoID};
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

/// What `pidfd_send_signal(2)` does with the `siginfo` it was handed.
///
/// ```c
/// if (info) {
///         ret = copy_siginfo_from_user_any(&kinfo, info);
///         if (unlikely(ret))
///                 goto err;
///         ret = -EINVAL;
///         if (unlikely(sig != kinfo.si_signo))
///                 goto err;
///         /* Only allow sending arbitrary signals to yourself. */
///         ret = -EPERM;
///         if ((task_pid(current) != pid) &&
///             (kinfo.si_code >= 0 || kinfo.si_code == SI_TKILL))
///                 goto err;
/// } else {
///         prepare_kill_siginfo(sig, &kinfo);
/// }
/// ```
///
/// A null `info` is the ordinary case -- what `pidfd_send_signal(fd, sig,
/// NULL, 0)` means is "a kill from me", which the kernel fills in itself, so
/// there is nothing to disagree with and nothing to forge. The whole of this
/// was missing: the argument was taken and then dropped with `let _ = info`,
/// which is the one shape of bug that looks exactly like correct code.
///
/// The two answers are in Linux's order, so a call that is wrong twice over
/// hears about the mismatched number first. That order is the observable
/// part: `EINVAL` says "your struct does not describe this call" and `EPERM`
/// says "you may not say that about somebody else", and a caller that got
/// `EPERM` for a typo would go looking for privileges it does not need.
fn check_pidfd_siginfo(
    signum: usize,
    head: Option<SigInfoHead>,
    target_is_self: bool,
) -> LxResult<()> {
    let Some(head) = head else {
        return Ok(());
    };
    // Widened rather than cast down: `si_signo` is signed and `signum` is a
    // register, so comparing them as `i32` would let -1 answer for
    // 0xffff_ffff.
    if head.signo as i64 != signum as i64 {
        return Err(LxError::EINVAL);
    }
    if !may_queue_siginfo(head.code, target_is_self) {
        return Err(LxError::EPERM);
    }
    Ok(())
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
    ///
    /// `kill_pid_info()` is where this ends in Linux -- the same function
    /// `kill(2)` reaches -- so it asks the same two questions: may the caller
    /// signal that process, and what does this signal actually do. Neither was
    /// asked here. A pidfd is not a permission: `pidfd_open(2)` takes any pid
    /// and checks nothing, on purpose, because the check belongs to whatever
    /// you then do with the fd. So `pidfd_send_signal` was a way to send any
    /// signal to any process, and `SIGKILL` went straight to `Process::exit`,
    /// which is how `kill -9 1` is refused and this was not: a pidfd on init
    /// removed the supervisor.
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
        let pidfd = PidFd::from_file_like(self.linux_process().get_file_like(pidfd)?)?;
        let target = pidfd.target().clone();
        if matches!(target.status(), Status::Exited(_)) {
            return Err(LxError::ESRCH);
        }
        let user = if info.is_null() {
            None
        } else {
            Some(info.read()?)
        };
        check_pidfd_siginfo(
            signum,
            user.as_ref().map(SigInfoHead::of),
            target.id() == self.zircon_process().id(),
        )?;
        // Including `pidfd_send_signal(fd, 0, ...)`, which is a permission
        // probe and not just a liveness one: answering from the status alone
        // told a caller that a process it may not touch is alive.
        //
        // The caller's `siginfo_t`, when it gave one, is what gets delivered:
        // `pidfd_send_signal` is the pidfd form of `rt_sigqueueinfo`, and a
        // sender that filled in `si_value` expects the handler to read it.
        let info = signal.and_then(|sig| user.map(|u| queued_from_user(sig, u)));
        self.signal_one_process_with_info(&self.kill_context(signal), &target, info)
            .map(|_| 0)
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
        // `__pidfd_fget()` asks `ptrace_may_access(task,
        // PTRACE_MODE_ATTACH_REALCREDS)` and answers `EPERM`; nothing asked
        // here, so any process could take any open file out of any other --
        // its listening socket, its log, the pipe it is reading. Before the
        // fd is looked up, because `EPERM` comes first in Linux: a caller with
        // no business in that process should not be able to find out which of
        // its descriptors are open.
        let target_linux = target_proc.try_linux().ok_or(LxError::ESRCH)?;
        if !LinuxProcess::may_attach_to(
            &caller.credentials(),
            &target_linux.credentials(),
            target_proc.id() == self.zircon_process().id(),
        ) {
            return Err(LxError::EPERM);
        }
        // Like `dup`, this installs the target's own open file description
        // here (`pidfd_getfd` calls `get_file`), so the two processes share
        // the offset and the status flags. `pidfd_getfd(2)`: "the
        // close-on-exec flag is set on the file descriptor" -- on THIS
        // descriptor, not on the description the target is still using.
        let file = target_linux.get_file_like(targetfd.into())?;
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

/// `pidfd_send_signal(2)`'s `siginfo` argument, which was taken and dropped.
#[cfg(test)]
mod pidfd_siginfo_tests {
    use super::*;

    /// `SI_QUEUE`, what `sigqueue(3)` stamps and the one code userland is
    /// meant to send.
    const SI_QUEUE: i32 = -1;
    /// `SI_TKILL`, negative and still refused: it would claim the signal came
    /// from a `tkill`, which carries the sender's identity.
    const SI_TKILL: i32 = -6;
    /// `SI_USER` (0) and `SI_KERNEL` (0x80) are the kernel's half of the space.
    const SI_USER: i32 = 0;
    const SI_KERNEL: i32 = 0x80;

    const SIGTERM: usize = 15;

    fn head(signo: i32, code: i32) -> SigInfoHead {
        SigInfoHead {
            signo,
            errno: 0,
            code,
        }
    }

    #[test]
    fn a_null_siginfo_is_the_ordinary_call() {
        // `prepare_kill_siginfo()`: the kernel fills in a kill-shaped siginfo
        // itself, so there is nothing to disagree with and nothing to forge.
        // This is what every caller that is not `sigqueue` does.
        assert_eq!(check_pidfd_siginfo(SIGTERM, None, false), Ok(()));
        assert_eq!(check_pidfd_siginfo(SIGTERM, None, true), Ok(()));
    }

    #[test]
    fn the_struct_has_to_describe_this_very_call() {
        // `if (unlikely(sig != kinfo.si_signo)) goto err;` -- one delivery
        // carrying two different signal numbers is a programming error
        // whichever of the two the kernel were to believe.
        assert_eq!(
            check_pidfd_siginfo(SIGTERM, Some(head(9, SI_QUEUE)), false),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            check_pidfd_siginfo(SIGTERM, Some(head(9, SI_QUEUE)), true),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_matching_number_and_an_honest_code_go_through() {
        assert_eq!(
            check_pidfd_siginfo(SIGTERM, Some(head(SIGTERM as i32, SI_QUEUE)), false),
            Ok(())
        );
    }

    #[test]
    fn a_kernel_code_cannot_be_forged_at_another_process() {
        for code in [SI_USER, SI_KERNEL, i32::MAX] {
            assert_eq!(
                check_pidfd_siginfo(SIGTERM, Some(head(SIGTERM as i32, code)), false),
                Err(LxError::EPERM),
                "si_code {}",
                code
            );
        }
    }

    #[test]
    fn a_tkill_cannot_be_impersonated_either() {
        assert_eq!(
            check_pidfd_siginfo(SIGTERM, Some(head(SIGTERM as i32, SI_TKILL)), false),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn anything_goes_at_your_own_process() {
        for code in [SI_USER, SI_KERNEL, SI_TKILL, SI_QUEUE, i32::MIN] {
            assert_eq!(
                check_pidfd_siginfo(SIGTERM, Some(head(SIGTERM as i32, code)), true),
                Ok(()),
                "si_code {} at self",
                code
            );
        }
    }

    #[test]
    fn the_wrong_number_is_reported_before_the_wrong_code() {
        // Wrong twice over: Linux answers the mismatch first, and the order is
        // the observable part. `EPERM` for a typo sends the caller looking for
        // privileges it does not need.
        assert_eq!(
            check_pidfd_siginfo(SIGTERM, Some(head(9, SI_KERNEL)), false),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_negative_signo_cannot_answer_for_a_huge_signal_number() {
        // `si_signo` is signed and the signal number arrives in a register, so
        // the two are compared widened. Narrowed to `i32` instead, -1 would
        // match 0xffff_ffff and the struct would be accepted as describing a
        // call it does not.
        assert_eq!(
            check_pidfd_siginfo(0xffff_ffff, Some(head(-1, SI_QUEUE)), false),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn signal_zero_is_checked_like_any_other() {
        // The existence probe. A zero `si_signo` describes it correctly...
        assert_eq!(
            check_pidfd_siginfo(0, Some(head(0, SI_QUEUE)), false),
            Ok(())
        );
        // ...and forging a code on it is refused just the same, or the probe
        // would be a way to say things about a process you may not signal.
        assert_eq!(
            check_pidfd_siginfo(0, Some(head(0, SI_USER)), false),
            Err(LxError::EPERM)
        );
        assert_eq!(
            check_pidfd_siginfo(0, Some(head(SIGTERM as i32, SI_QUEUE)), false),
            Err(LxError::EINVAL)
        );
    }
}
