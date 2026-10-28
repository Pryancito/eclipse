//! `pidfd_open`, `pidfd_send_signal`, `pidfd_getfd`

use super::*;
use crate::signal::{may_queue_siginfo, queued_from_user, SigInfoHead};
use linux_object::error::LxResult;
use linux_object::fs::{OpenFlags, PidFd, PIDFD_THREAD};
use linux_object::process::LinuxProcess;
use linux_object::signal::{SigInfo, Signal};
use zircon_object::object::{KernelObject, KoID};
use zircon_object::task::{Status, Thread};

/// Reduce `pidfd_open(2)`'s flag word to the open flags the new fd carries.
///
/// Split out of `sys_pidfd_open` because everything it decides is decided here:
/// the rest of that syscall needs a live process to say anything at all.
fn pidfd_open_flags(flags: u32) -> linux_object::error::LxResult<OpenFlags> {
    const NONBLOCK: u32 = OpenFlags::NON_BLOCK.bits() as u32;
    if flags & !(NONBLOCK | PIDFD_THREAD) != 0 {
        return Err(LxError::EINVAL);
    }
    // pidfd_open(2): the fd is always close-on-exec, whatever the caller
    // asked, and open for reading and writing. `PIDFD_THREAD` stays in it, as
    // `O_EXCL`, the way Linux keeps it in `f_flags`.
    let mut open_flags = OpenFlags::CLOEXEC | OpenFlags::RDWR;
    if flags & NONBLOCK != 0 {
        open_flags |= OpenFlags::NON_BLOCK;
    }
    if flags & PIDFD_THREAD != 0 {
        open_flags |= OpenFlags::EXCLUSIVE;
    }
    Ok(open_flags)
}

/// The process `pidfd_open(pid, flags)` gives a descriptor on, and the
/// thread when `pid` names one that is not its process's leader.
///
/// A zombie too: its pidfd is readable at once, and `waitid(P_PIDFD)`
/// collects it. A thread that is not its group's leader is there, but is not
/// a process: `pidfd_prepare()` says `ENOENT` for it without `PIDFD_THREAD`
/// (measured on Linux 6.18), where this said `ESRCH`, "no such process", to
/// a caller that had just read the tid out of `/proc/<pid>/task`. With
/// `PIDFD_THREAD` it is that thread's.
#[allow(clippy::type_complexity)]
fn pidfd_open_target(
    pid: KoID,
    thread: bool,
) -> LxResult<(Arc<zircon_object::task::Process>, Option<Arc<Thread>>)> {
    use linux_object::process::{find_task, find_thread};
    match find_task(pid) {
        Some(process) => Ok((process, None)),
        None => match find_thread(pid) {
            Some((process, found)) if thread => Ok((process, Some(found))),
            Some(_) => Err(LxError::ENOENT),
            None => Err(LxError::ESRCH),
        },
    }
}

/// `pidfd_send_signal(2)`'s scope flags (Linux 6.9): the thread the pid
/// names, its thread group, or the process group it leads.
const PIDFD_SIGNAL_THREAD: u32 = 1 << 0;
const PIDFD_SIGNAL_THREAD_GROUP: u32 = 1 << 1;
const PIDFD_SIGNAL_PROCESS_GROUP: u32 = 1 << 2;

/// Whom a `pidfd_send_signal` reaches: `PIDTYPE_PID`, `PIDTYPE_TGID` or
/// `PIDTYPE_PGID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Thread,
    Process,
    ProcessGroup,
}

/// The scope `flags` asks for, or `None` for the pidfd's own: a thread's
/// for a `PIDFD_THREAD` pidfd, a process's for any other. Decided before the
/// descriptor is looked at, so two scopes or an unknown bit are `EINVAL`
/// even on a descriptor nobody opened.
fn pidfd_signal_scope(flags: u32) -> LxResult<Option<Scope>> {
    match flags {
        0 => Ok(None),
        PIDFD_SIGNAL_THREAD => Ok(Some(Scope::Thread)),
        PIDFD_SIGNAL_THREAD_GROUP => Ok(Some(Scope::Process)),
        PIDFD_SIGNAL_PROCESS_GROUP => Ok(Some(Scope::ProcessGroup)),
        _ => Err(LxError::EINVAL),
    }
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
        let pidfd = match pidfd_open_target(pid as KoID, open_flags.contains(OpenFlags::EXCLUSIVE))?
        {
            (process, None) => PidFd::new(process, open_flags),
            (process, Some(thread)) => PidFd::of_thread(process, thread, open_flags),
        };
        let fd = self.linux_process().add_file(pidfd)?;
        Ok(fd.into())
    }

    /// Send `sig` to the thread, the process or the process group `pidfd`
    /// and `flags` name.
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
        // Linux's order: the flag word, the descriptor, the `siginfo`, then
        // whether anybody is there, and only then the signal number
        // (`check_kill_permission`'s `valid_signal`), so signal 999 at
        // `/dev/null` is `EBADF`, not `EINVAL`.
        let scope = pidfd_signal_scope(flags)?;
        let pidfd = PidFd::from_file_like(self.linux_process().get_file_like(pidfd)?)?;
        let scope = scope.unwrap_or(if pidfd.is_thread() {
            Scope::Thread
        } else {
            Scope::Process
        });
        let user = if info.is_null() {
            None
        } else {
            Some(info.read()?)
        };
        // "Only allow sending arbitrary signals to yourself":
        // `task_pid(current) != pid || type > PIDTYPE_TGID`. Yourself is the
        // calling THREAD, as for `rt_tgsigqueueinfo`, so a thread that is not
        // the leader may not forge a code at its own process's pidfd; and a
        // process group is never yourself.
        check_pidfd_siginfo(
            signum,
            user.as_ref().map(SigInfoHead::of),
            pidfd.task_id() == self.thread.id() && scope != Scope::ProcessGroup,
        )?;
        // Until it is reaped, a dead process is a zombie that takes nothing
        // and answers 0, as through `kill(2)`; reaped, or a thread that has
        // exited, the pid is gone. Not for a process group, which outlives
        // the pid that names it as long as it has members.
        if scope != Scope::ProcessGroup && pidfd.gone() {
            return Err(LxError::ESRCH);
        }
        let signal = Signal::from_syscall_arg(signum)?;
        // Including `pidfd_send_signal(fd, 0, ...)`, which is a permission
        // probe and not just a liveness one: answering from the status alone
        // told a caller that a process it may not touch is alive.
        //
        // The caller's `siginfo_t`, when it gave one, is what gets delivered:
        // `pidfd_send_signal` is the pidfd form of `rt_sigqueueinfo`, and a
        // sender that filled in `si_value` expects the handler to read it.
        let cx = self.kill_context(signal);
        let info = signal.and_then(|sig| user.map(|u| queued_from_user(sig, u)));
        match scope {
            Scope::Process => self.signal_one_process_with_info(&cx, pidfd.target(), info),
            Scope::Thread => {
                self.signal_one_thread_with_info(&cx, pidfd.target(), pidfd.thread(), info)
            }
            Scope::ProcessGroup => self.signal_group(&cx, pidfd.task_id(), info),
        }
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
            target_linux.dumpable(),
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
    fn pidfd_thread_stays_in_the_flags_as_o_excl() {
        // Where `F_GETFL` shows it and `pidfd_send_signal` reads it: on
        // Linux 6.18 a thread pidfd says 0o202, and 0o4202 with
        // `PIDFD_NONBLOCK`. It used to be refused with `EINVAL`.
        let flags = pidfd_open_flags(PIDFD_THREAD).unwrap();
        assert!(flags.contains(OpenFlags::EXCLUSIVE | OpenFlags::CLOEXEC));
        assert!(!flags.contains(OpenFlags::NON_BLOCK));
        let flags = pidfd_open_flags(PIDFD_THREAD | OpenFlags::NON_BLOCK.bits() as u32).unwrap();
        assert!(flags.contains(OpenFlags::EXCLUSIVE | OpenFlags::NON_BLOCK));
        assert!(!pidfd_open_flags(0).unwrap().contains(OpenFlags::EXCLUSIVE));
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

/// Which pid `pidfd_open` refuses, and with what. Fixed pids in 49_0xx.
#[cfg(test)]
mod pidfd_open_target_tests {
    use super::*;
    use linux_object::thread::ThreadExt;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::{Thread, ROOT_JOB};

    #[test]
    fn a_process_is_found_by_its_pid() {
        let proc = zircon_object::task::Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            49_001,
            "p",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        for thread in [false, true] {
            let (found, of) = pidfd_open_target(49_001, thread).unwrap();
            assert!(Arc::ptr_eq(&found, &proc));
            assert!(of.is_none(), "a pid is its process, PIDFD_THREAD or not");
        }
    }

    #[test]
    fn a_thread_that_is_not_the_leader_is_enoent_without_pidfd_thread() {
        let proc = zircon_object::task::Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            49_002,
            "p",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let leader = Thread::create_linux(&proc).unwrap();
        let worker = Thread::create_linux(&proc).unwrap();
        assert_eq!(leader.id(), 49_002);
        assert_ne!(worker.id(), 49_002);
        assert_eq!(
            pidfd_open_target(worker.id(), false).err(),
            Some(LxError::ENOENT)
        );
        let (found, of) = pidfd_open_target(worker.id(), true).unwrap();
        assert!(Arc::ptr_eq(&found, &proc), "the thread's own process");
        assert!(Arc::ptr_eq(&of.unwrap(), &worker), "and the thread itself");
        assert!(Arc::ptr_eq(
            &pidfd_open_target(leader.id(), false).unwrap().0,
            &proc
        ));
        worker.terminate_abandoned();
        assert_eq!(
            pidfd_open_target(worker.id(), true).err(),
            Some(LxError::ESRCH),
            "an exited thread is nobody"
        );
    }

    #[test]
    fn a_pid_nobody_has_is_esrch() {
        for thread in [false, true] {
            assert_eq!(
                pidfd_open_target(49_099, thread).err(),
                Some(LxError::ESRCH)
            );
        }
    }
}

/// `pidfd_send_signal(2)`'s flag word (Linux 6.9): one scope or none.
#[cfg(test)]
mod pidfd_signal_scope_tests {
    use super::*;

    #[test]
    fn the_three_scopes_are_linux_s_bits() {
        // `include/uapi/linux/pidfd.h`.
        assert_eq!(
            [
                PIDFD_SIGNAL_THREAD,
                PIDFD_SIGNAL_THREAD_GROUP,
                PIDFD_SIGNAL_PROCESS_GROUP
            ],
            [1, 2, 4]
        );
    }

    #[test]
    fn each_bit_alone_names_its_scope_and_none_is_the_pidfd_s_own() {
        assert_eq!(pidfd_signal_scope(0), Ok(None));
        assert_eq!(pidfd_signal_scope(1), Ok(Some(Scope::Thread)));
        assert_eq!(pidfd_signal_scope(2), Ok(Some(Scope::Process)));
        assert_eq!(pidfd_signal_scope(4), Ok(Some(Scope::ProcessGroup)));
    }

    #[test]
    fn two_scopes_or_a_bit_nobody_has_is_einval() {
        // `hweight32(flags & PIDFD_SEND_SIGNAL_FLAGS) > 1`, and
        // `flags & ~PIDFD_SEND_SIGNAL_FLAGS`.
        for bad in [3u32, 5, 6, 7, 8, 1 << 31] {
            assert_eq!(pidfd_signal_scope(bad), Err(LxError::EINVAL), "{bad:#x}");
        }
    }
}

/// `PIDFD_THREAD` pidfds through the syscalls, and the scopes of
/// `pidfd_send_signal`. Fixed pids in 49_2xx.
#[cfg(test)]
mod thread_tests;

/// The three through the syscall: that `pidfd_getfd` asks whether the caller
/// may attach at all, that the caller's `siginfo_t` reaches the check, and
/// which argument each one rejects first.
#[cfg(test)]
mod wiring_tests;

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

#[cfg(test)]
mod benches {
    //! The decisions `pidfd_open` and `pidfd_send_signal` make before they
    //! need a live process. All are small, and all are here because they
    //! were missing: `pidfd_open` accepted flags it then ignored, and
    //! `pidfd_send_signal` took its `siginfo` and dropped it with
    //! `let _ = info`, which is the one shape of bug that looks exactly like
    //! correct code.
    //!
    //! Every row black-boxes its inputs. Two cycles (~0.6 ns) is `b.iter`'s
    //! own loop; a row there with an opaque input is a real compare and
    //! branch, a row there with a constant input is a call that was folded
    //! away and never ran.

    use super::*;
    use test::{black_box, Bencher};

    const NONBLOCK: u32 = OpenFlags::NON_BLOCK.bits() as u32;

    /// The ordinary `pidfd_open(pid, 0)`: no flags, and the fd comes back
    /// close-on-exec whatever the caller asked.
    #[bench]
    fn pidfd_open_flags_of_zero(b: &mut Bencher) {
        b.iter(|| black_box(pidfd_open_flags(black_box(0))));
    }

    #[bench]
    fn pidfd_open_flags_nonblock(b: &mut Bencher) {
        b.iter(|| black_box(pidfd_open_flags(black_box(NONBLOCK))));
    }

    /// `PIDFD_THREAD`, which stays in the flags as `O_EXCL`.
    #[bench]
    fn pidfd_open_flags_thread(b: &mut Bencher) {
        b.iter(|| black_box(pidfd_open_flags(black_box(PIDFD_THREAD))));
    }

    #[bench]
    fn pidfd_open_flags_unknown_bit(b: &mut Bencher) {
        b.iter(|| black_box(pidfd_open_flags(black_box(1 << 20))));
    }

    /// `pidfd_send_signal`'s flag word: none, the commonest call.
    #[bench]
    fn pidfd_signal_scope_of_zero(b: &mut Bencher) {
        b.iter(|| black_box(pidfd_signal_scope(black_box(0))));
    }

    /// Two scopes at once, which is `EINVAL` before the descriptor is read.
    #[bench]
    fn pidfd_signal_scope_two_at_once(b: &mut Bencher) {
        b.iter(|| {
            black_box(pidfd_signal_scope(black_box(
                PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_PROCESS_GROUP,
            )))
        });
    }

    /// `pidfd_send_signal(fd, sig, NULL, 0)`: the common case, where the
    /// kernel fills the `siginfo` in itself so there is nothing to disagree
    /// with and nothing to forge. One `Option` test.
    #[bench]
    fn check_pidfd_siginfo_with_no_info(b: &mut Bencher) {
        b.iter(|| {
            black_box(check_pidfd_siginfo(
                black_box(9),
                black_box(None),
                black_box(false),
            ))
        });
    }

    /// A `siginfo` that agrees with the signal number and names a kill from
    /// the caller: both checks run and both pass.
    #[bench]
    fn check_pidfd_siginfo_accepted(b: &mut Bencher) {
        let head = SigInfoHead {
            signo: 9,
            errno: 0,
            code: 0,
        };
        b.iter(|| {
            black_box(check_pidfd_siginfo(
                black_box(9),
                black_box(Some(head)),
                black_box(true),
            ))
        });
    }

    /// The mismatched number, which must be `EINVAL` and must be answered
    /// *first*: a caller that got `EPERM` for a typo would go looking for
    /// privileges it does not need.
    #[bench]
    fn check_pidfd_siginfo_mismatched_number(b: &mut Bencher) {
        let head = SigInfoHead {
            signo: 15,
            errno: 0,
            code: 0,
        };
        b.iter(|| {
            black_box(check_pidfd_siginfo(
                black_box(9),
                black_box(Some(head)),
                black_box(true),
            ))
        });
    }

    /// Forging a `si_code` at somebody else, which is `EPERM`.
    #[bench]
    fn check_pidfd_siginfo_refused_at_another_process(b: &mut Bencher) {
        let head = SigInfoHead {
            signo: 9,
            errno: 0,
            code: 0,
        };
        b.iter(|| {
            black_box(check_pidfd_siginfo(
                black_box(9),
                black_box(Some(head)),
                black_box(false),
            ))
        });
    }
}
