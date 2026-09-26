use super::*;
use crate::outparams::hand_out_one;
use core::fmt::Debug;
use core::mem::size_of;

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use bitflags::bitflags;

use kernel_hal::context::{UserContext, UserContextField};
use linux_object::error::LxResult;
use linux_object::fs::{FileLike, PidFd};
use linux_object::process::{
    prio_target, wait_child_any_interest, wait_child_interest, Credentials, LinuxProcess,
    PrioTarget, SchedFacts, SchedRequest, WaitInterest, CAP_LAST_CAP, CAP_SETGID, RLIMIT_NICE,
    RLIMIT_RTPRIO,
};
use linux_object::signal::SigInfo;
use linux_object::thread::{CurrentThreadExt, RobustList, ThreadExt};
use linux_object::time::RUsage;
use linux_object::time::TimeSpec;
use linux_object::{fs::INodeExt, loader::LinuxElfLoader};
use zircon_object::object::{KernelObject, KoID, Signal};
use zircon_object::task::{
    Status, Thread, MAX_NICE, MAX_RT_PRIO, MIN_NICE, MIN_RT_PRIO, SCHED_BATCH, SCHED_DEADLINE,
    SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL, SCHED_RR,
};
use zircon_object::vm::USER_STACK_PAGES;

const P_ALL: i32 = 0;
const P_PID: i32 = 1;
const P_PGID: i32 = 2;
// Linux <linux/wait.h>: P_PIDFD == 3 (this was wrongly 5, so waitid() with a
// pidfd — as glib's g_child_watch_source_new() uses — matched no arm and
// returned EINVAL).
const P_PIDFD: i32 = 3;

/// `SCHED_RESET_ON_FORK`: OR-ed into the policy by `sched_setscheduler` /
/// `sched_setattr`. Accepted but not modelled (we never fork-reset).
const SCHED_RESET_ON_FORK: usize = 0x4000_0000;

/// Linux `struct sched_attr` (the v0 / 48-byte layout) used by
/// `sched_setattr(2)` / `sched_getattr(2)`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SchedAttr {
    /// Size of this structure in bytes.
    pub size: u32,
    /// Scheduling policy (a `SCHED_*` constant).
    pub sched_policy: u32,
    /// Scheduling flags (e.g. `SCHED_FLAG_RESET_ON_FORK`).
    pub sched_flags: u64,
    /// Nice value for the fair policies (-20..=19).
    pub sched_nice: i32,
    /// Static priority for the real-time policies (1..=99).
    pub sched_priority: u32,
    /// `SCHED_DEADLINE` runtime in nanoseconds (unused here).
    pub sched_runtime: u64,
    /// `SCHED_DEADLINE` relative deadline in nanoseconds (unused here).
    pub sched_deadline: u64,
    /// `SCHED_DEADLINE` period in nanoseconds (unused here).
    pub sched_period: u64,
}

/// What a process that failed `execve` AFTER its address space was replaced
/// finishes with: Linux kills it with SIGSEGV (`force_sigsegv` in
/// `flush_old_exec`'s failure path), so its parent sees a death by signal,
/// `WIFSIGNALED` with `WTERMSIG == 11`. This used to be the literal `139`,
/// the number a SHELL prints for that, stored as if the process had called
/// `exit(139)`.
fn exit_code_after_failed_exec() -> i64 {
    linux_object::process::exit_code_killed_by(linux_object::signal::Signal::SIGSEGV as u8)
}

/// What `nanosleep(2)` leaves in `rem` when a signal cuts the sleep short:
/// the time that was still to go, so a loop that restarts the sleep on
/// `EINTR` with `rem` picks up where it left off.
fn nanosleep_remaining(deadline: core::time::Duration, now: core::time::Duration) -> TimeSpec {
    TimeSpec::from_duration(deadline.saturating_sub(now))
}

/// Sleep until `deadline` or until a signal is pending (see
/// `interruptible_sleep_until`); on `EINTR` the remaining time goes to
/// `rem`, when the caller gave one. Shared by `nanosleep` and the relative
/// form of `clock_nanosleep`.
pub(crate) async fn sleep_or_eintr(
    thread: &Arc<Thread>,
    deadline: core::time::Duration,
    mut rem: UserOutPtr<TimeSpec>,
) -> LxResult<()> {
    match linux_object::process::interruptible_sleep_until(thread, deadline).await {
        Ok(()) => Ok(()),
        Err(e) => {
            if e == LxError::EINTR {
                rem.write_if_not_null(nanosleep_remaining(
                    deadline,
                    kernel_hal::timer::timer_now(),
                ))?;
            }
            Err(e)
        }
    }
}

/// The child's final CPU usage as `wait4(2)` and `waitid(2)` hand it out:
/// what `time(1)` prints. The struct is written in the FULL Linux layout,
/// the fields this kernel does not account for as zero.
fn child_rusage(cpu: linux_object::process::ChildCpu) -> RUsage {
    RUsage {
        utime: core::time::Duration::from_nanos(cpu.utime_ns).into(),
        stime: core::time::Duration::from_nanos(cpu.stime_ns).into(),
        ..RUsage::default()
    }
}

/// The argument of `prctl(PR_SET_KEEPCAPS)`: `kernel/sys.c` takes 0 or 1
/// and nothing else (`if (arg2 > 1) return -EINVAL`), unlike the other
/// boolean options, which read "nonzero". Neither option existed here.
pub(crate) fn keepcaps_arg(a2: usize) -> LxResult<bool> {
    match a2 {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(LxError::EINVAL),
    }
}

/// `cap_prctl_drop`, what `prctl(PR_CAPBSET_DROP, cap)` refuses: a caller
/// without `CAP_SETPCAP` (`EPERM`), asked before the number is even looked
/// at, then a number that is not a capability (`EINVAL`).
///
/// Anyone was answered 0. A sandbox that drops its bounding set as an
/// unprivileged user (a browser's content process, `bwrap` without setuid)
/// believed it had, and a probe of the unprivileged answer read "allowed".
pub(crate) fn capbset_drop_verdict(cap: usize, may_set_pcap: bool) -> LxResult<()> {
    if !may_set_pcap {
        return Err(LxError::EPERM);
    }
    if cap > CAP_LAST_CAP as usize {
        return Err(LxError::EINVAL);
    }
    Ok(())
}

/// The slack `prctl(PR_SET_TIMERSLACK, arg2)` stores: `arg2` is a `long`,
/// and `if (arg2 <= 0) current->timer_slack_ns = current->default_timer_slack_ns;
/// else current->timer_slack_ns = arg2;`. Here 0 is the stored spelling of
/// "the default", which `PR_GET_TIMERSLACK` already reports as 50 µs.
///
/// The register went into the `u64` as it was, so `-1` became an
/// 18-billion-second slack that `PR_GET_TIMERSLACK` then reported back.
pub(crate) fn timerslack_arg(a2: usize) -> u64 {
    if (a2 as isize) <= 0 {
        0
    } else {
        a2 as u64
    }
}

/// The name `prctl(PR_SET_NAME)` stores from the bytes the caller passed:
/// `set_task_comm` copies `TASK_COMM_LEN - 1` bytes at most, to the first
/// NUL, and the kernel keeps them as bytes. `comm` is a `String` here, so
/// a byte that is not UTF-8 becomes `?` rather than a refusal.
///
/// The name used to go through `as_c_str`, which refused any invalid UTF-8
/// with `EINVAL`: a JVM's `pthread_setname_np`, which cuts a thread name to
/// 15 bytes and can cut a multibyte character in half doing so, and any
/// Latin-1 name, failed where Linux stores the bytes.
pub(crate) fn comm_from_user_bytes(bytes: &[u8]) -> alloc::string::String {
    use linux_object::thread::TASK_COMM_LEN;
    let end = bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(bytes.len())
        .min(TASK_COMM_LEN - 1);
    let mut comm = alloc::string::String::with_capacity(end);
    for chunk in bytes[..end].utf8_chunks() {
        comm.push_str(chunk.valid());
        for _ in 0..chunk.invalid().len() {
            comm.push('?');
        }
    }
    comm
}

/// The end of `wait4(2)`: the pid of the child found, with its status and
/// CPU time in the two out-pointers, or 0 with both left alone.
///
/// `kernel_wait4` writes `*wstatus` only `if (ret > 0 && stat_addr)`.
/// `WNOHANG` finding nothing used to write 0 there, so a caller that kept
/// the last status in the variable (a `while (waitpid(-1, &status,
/// WNOHANG) > 0)` loop that reads `status` afterwards is the common shape)
/// had it zeroed by the call that ended the loop. The `rusage`
/// used to be dropped entirely, leaving callers to read whatever stack
/// garbage sat in their buffer.
pub(crate) fn wait4_finish(
    mut wstatus: UserOutPtr<i32>,
    mut rusage: UserOutPtr<RUsage>,
    found: Option<(KoID, i32, linux_object::process::ChildCpu)>,
) -> SysResult {
    let Some((pid, code, cpu)) = found else {
        return Ok(0);
    };
    wstatus.write_if_not_null(code)?;
    rusage.write_if_not_null(child_rusage(cpu))?;
    Ok(pid as usize)
}

/// What `waitid(2)` leaves in its two out-pointers.
pub(crate) struct WaitidReport {
    /// `infop`, always written: the `SIGCHLD` `siginfo_t` of the child
    /// reported, or, when `WNOHANG` found nothing, a `siginfo_t` with
    /// `si_signo` and `si_pid` zero, which is how the caller tells the two
    /// apart (`waitid(2)`, and `sys_waitid` in `kernel/exit.c`, which
    /// writes the six fields whatever `kernel_waitid` found).
    pub info: SigInfo,
    /// `rusage`, written only when a child was reported: `sys_waitid`
    /// copies it out under `if (err > 0)`.
    pub rusage: Option<RUsage>,
}

/// The `waitid(2)` report for what `do_wait` found: the child's pid, real
/// uid, status word and CPU time, or nothing.
pub(crate) fn waitid_report(
    child: Option<(i32, u32, i32, linux_object::process::ChildCpu)>,
) -> WaitidReport {
    match child {
        Some((pid, uid, status, cpu)) => WaitidReport {
            info: SigInfo::child_state_change(pid, uid, status),
            rusage: Some(child_rusage(cpu)),
        },
        None => WaitidReport {
            info: SigInfo::default(),
            rusage: None,
        },
    }
}

fn is_child_process(
    parent: &zircon_object::task::Process,
    child: &zircon_object::task::Process,
) -> bool {
    parent.linux().has_child(child.id())
}

fn comm_from_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The `pid` argument of `setpgid`/`getpgid`/`getsid`: `0` means the caller.
///
/// `pid_t` is SIGNED, and `find_task_by_vpid` can only ever fail on a negative
/// one. Read as a `usize` -- which is how these three used to take it -- a
/// negative pid arrived sign-extended, so `getpgid(-1)` asked about pid
/// `0xffff_ffff_ffff_ffff` and `setpgid(-1, 0)` would have made that its own
/// group leader.
fn resolve_pid_arg(caller_pid: KoID, pid: i32) -> LxResult<KoID> {
    match pid {
        0 => Ok(caller_pid),
        p if p > 0 => Ok(p as KoID),
        _ => Err(LxError::ESRCH),
    }
}

/// The two arguments of `setpgid(2)`, resolved: which process, and into which
/// group. `pgid == 0` means "a group of the target's own", and a NEGATIVE
/// pgid is `EINVAL` -- checked first, before the pid is even looked up, as
/// `kernel/sys.c:do_setpgid` does. It is the one thing separating
/// `setpgid(0, -1)` from silently filing the caller under group
/// `0xffff_ffff_ffff_ffff`, where no signal sent to any real group can reach
/// it.
fn setpgid_args(caller_pid: KoID, pid: i32, pgid: i32) -> LxResult<(KoID, KoID)> {
    if pgid < 0 {
        return Err(LxError::EINVAL);
    }
    let target = resolve_pid_arg(caller_pid, pid)?;
    let new_pgid = if pgid == 0 { target } else { pgid as KoID };
    Ok((target, new_pgid))
}

/// `wait4`/`waitid` option bits, spelled as `include/uapi/linux/wait.h` does.
mod wait_opts {
    /// Return at once if no child has changed state.
    pub const WNOHANG: u32 = 0x0000_0001;
    /// Report a child stopped by a signal. `WSTOPPED` is the same bit.
    pub const WUNTRACED: u32 = 0x0000_0002;
    /// Report a child that has terminated. **`waitid` only.**
    pub const WEXITED: u32 = 0x0000_0004;
    /// Report a stopped child resumed by `SIGCONT`.
    pub const WCONTINUED: u32 = 0x0000_0008;
    /// Leave the child waitable: report its status without reaping it.
    /// **`waitid` only.**
    pub const WNOWAIT: u32 = 0x0100_0000;
    /// Do not wait on children of other threads in this group.
    pub const WNOTHREAD: u32 = 0x2000_0000;
    /// Wait on every child, whatever its exit signal.
    pub const WALL: u32 = 0x4000_0000;
    /// Wait only on children that do not deliver `SIGCHLD`.
    pub const WCLONE: u32 = 0x8000_0000;

    /// What `kernel_wait4` accepts. `WEXITED` and `WNOWAIT` are NOT on it:
    /// `wait4` always reaps and always reports an exit, so asking for either
    /// by name is `EINVAL`.
    pub const WAIT4: u32 = WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE;
    /// What `do_waitid` accepts: everything `wait4` does, plus the two bits
    /// that only mean something when the caller can say what it wants.
    pub const WAITID: u32 = WAIT4 | WEXITED | WNOWAIT;
    /// One of these must be named by `waitid`: it has no default interest,
    /// unlike `wait4`, which always means "exited".
    pub const WAITID_REQUIRED: u32 = WEXITED | WUNTRACED | WCONTINUED;
}

/// What a `wait*` options word asks for.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct WaitOptions {
    /// `WNOHANG`.
    pub nohang: bool,
    /// Whether the child's status is consumed (`WNOWAIT` clear).
    pub reap: bool,
    /// Which state changes are reported.
    pub interest: (bool, bool, bool),
}

/// The options `wait4(2)` was given, or `EINVAL`.
///
/// `kernel_wait4` checks the word against its own mask before it looks at a
/// single child, and that mask is shorter than `waitid`'s by two bits. This
/// used to be `WaitFlags::from_bits_truncate`, which drops what it does not
/// know: `wait4(pid, &st, WNOWAIT, NULL)` reaped the zombie it was asked to
/// leave alone (an extension of this kernel's own, since Linux answers
/// `EINVAL`), and `WEXITED`, which belongs to `waitid`, passed as well.
///
/// The interest is `(exited, stopped, continued)`, and `exited` is always
/// set: `kernel_wait4` ORs `WEXITED` into its own flags whatever the caller
/// asked for.
pub(crate) fn wait4_options(options: u32) -> Result<WaitOptions, LxError> {
    if options & !wait_opts::WAIT4 != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(WaitOptions {
        nohang: options & wait_opts::WNOHANG != 0,
        reap: true,
        interest: (
            true,
            options & wait_opts::WUNTRACED != 0,
            options & wait_opts::WCONTINUED != 0,
        ),
    })
}

/// The options `waitid(2)` was given, or `EINVAL`.
///
/// Two checks, in `do_waitid`'s order: a bit outside the mask, then a word
/// that names none of `WEXITED`, `WSTOPPED` and `WCONTINUED` -- a wait for
/// nothing at all, which Linux refuses rather than block on forever.
pub(crate) fn waitid_options(options: u32) -> Result<WaitOptions, LxError> {
    if options & !wait_opts::WAITID != 0 || options & wait_opts::WAITID_REQUIRED == 0 {
        return Err(LxError::EINVAL);
    }
    Ok(WaitOptions {
        nohang: options & wait_opts::WNOHANG != 0,
        reap: options & wait_opts::WNOWAIT == 0,
        interest: (
            options & wait_opts::WEXITED != 0,
            options & wait_opts::WUNTRACED != 0,
            options & wait_opts::WCONTINUED != 0,
        ),
    })
}

/// Syscalls for process.
///
/// # Menu
///
/// - [`fork`](Self::sys_fork)
/// - [`vfork`](Self::sys_vfork)
/// - [`clone`](Self::sys_clone)
/// - [`wait4`](Self::sys_wait4)
/// - [`execve`](Self::sys_execve)
/// - [`gettid`](Self::sys_gettid)
/// - [`getpid`](Self::sys_getpid)
/// - [`getppid`](Self::sys_getppid)
/// - [`exit`](Self::sys_exit)
/// - [`exit_group`](Self::sys_exit_group)
/// - [`nanosleep`](Self::sys_nanosleep)
/// - [`set_tid_address`](Self::sys_set_tid_address)
impl Syscall<'_> {
    /// `fork` creates a new process by duplicating the calling process
    /// (see [linux man fork(2)](https://www.man7.org/linux/man-pages/man2/fork.2.html)).
    /// The new process is referred to as the child process.
    /// The calling process is referred to as the parent process.
    ///
    /// The child process and the parent process run in separate memory spaces.
    /// At the time of `fork` both memory spaces have the same content.
    /// Memory writes, file mappings ([`Self::sys_mmap`]) and unmappings ([`Self::sys_munmap`])
    /// performed by one of the processes do not affect the other.
    ///
    /// The child process is an exact duplicate of the parent process except for the following points:
    ///
    /// - The child has its own unique process ID, and this PID does not match the ID of any existing process.
    /// - The child's parent process ID is the same as the parent's process ID.
    /// - Process resource utilizations ([`Self::sys_getrusage`]) and CPU time counters ([`Self::sys_times`]) are reset to zero in the child.
    /// - The child does not inherit semaphore adjustments from its parent ([`Self::sys_semop`]).
    /// - The child does not inherit process-associated record locks from its parent ([`Self::sys_fcntl`]).
    ///   (On the other hand, it does inherit [`Self::sys_fcntl`] open file description locks and [`Self::sys_flock`] locks from its parent.)
    ///
    /// Note the following further points:
    ///
    /// - The child process is created with a single thread—the one that called fork().
    ///   The entire virtual address space of the parent is replicated in the child,
    ///   including the states of mutexes and condition variables.
    /// - After a `fork` in a multithreaded program,
    ///   the child can safely call only async-signal-safe functions
    ///   until such time as it calls [`Self::sys_execve`].
    /// - The child inherits copies of the parent's set of open file descriptors.
    ///   Each file descriptor in the child refers to the same open file description (see [`Self::sys_open`])
    ///   as the corresponding file descriptor in the parent.
    ///   This means that the two file descriptors share open file status flags and file offset.
    fn fork_impl(
        &self,
        newsp: usize,
        newtls: usize,
        ctid: ChildTidRequest,
    ) -> LxResult<(Arc<Process>, KoID)> {
        info!("fork: newsp={:#x} newtls={:#x}", newsp, newtls);
        let new_proc = Process::fork_from(self.zircon_process())?;
        let path = new_proc.linux().execute_path();
        if !path.is_empty() {
            new_proc.set_name(comm_from_path(&path));
        }
        let tid = self.start_forked_thread(&new_proc, newsp, newtls, ctid)?;
        info!("fork: {} -> {}", self.zircon_process().id(), new_proc.id());
        Ok((new_proc, tid))
    }

    async fn vfork_impl(
        &self,
        newsp: usize,
        newtls: usize,
        ctid: ChildTidRequest,
    ) -> LxResult<(Arc<Process>, KoID)> {
        info!("vfork: newsp={:#x} newtls={:#x}", newsp, newtls);
        // A real vfork shares the parent's address space until execve or exit. The VMAR
        // implementation cannot replace a shared address space on execve, so use a copy
        // here while retaining vfork's parent-suspension semantics.
        let new_proc = Process::fork_from(self.zircon_process())?;
        let tid = self.start_forked_thread(&new_proc, newsp, newtls, ctid)?;

        let new_proc_obj: Arc<dyn KernelObject> = new_proc.clone();
        info!(
            "vfork: {} -> {}. Waiting for execve SIGNALED",
            self.zircon_process().id(),
            new_proc.id()
        );
        new_proc_obj
            .wait_signal(Signal::USER_SIGNAL_0 | Signal::PROCESS_TERMINATED)
            .await; // wait for execve or termination
        Ok((new_proc, tid))
    }

    /// The one thread a forked (or vforked) child starts with: this thread's
    /// context with the stack, TLS and return value the caller asked for,
    /// and the TID bookkeeping of `ctid` done BEFORE it runs a single user
    /// instruction. Returns the child's thread id.
    ///
    /// The child's copy of the address space already exists (`fork_from`),
    /// so `CLONE_CHILD_SETTID` cannot go through this thread's `UserOutPtr`:
    /// that writes the parent's memory, which the child does not see. It is
    /// written into the child's VMAR instead; see [`publish_child_tid`].
    fn start_forked_thread(
        &self,
        new_proc: &Arc<Process>,
        newsp: usize,
        newtls: usize,
        ctid: ChildTidRequest,
    ) -> LxResult<KoID> {
        // What the child thread takes from this one -- the signal mask above
        // all, which a `fork` inherits (sigprocmask(2)). `false`: a fork does
        // not share the address space, so the alternate signal stack comes
        // across too. (For a vfork, Linux's rule is `(clone_flags &
        // (CLONE_VM|CLONE_VFORK)) == CLONE_VM`, and a vfork sets BOTH bits,
        // so the stack comes across there just the same -- doubly right here,
        // where the address space is copied anyway.)
        let inherited = self.thread.lock_linux().forked_child();
        let new_thread = Thread::create_linux_with(new_proc, inherited)?;
        let mut new_ctx = self.thread.context_cloned()?;
        if newsp != 0 {
            new_ctx.set_field(UserContextField::StackPointer, newsp);
        }
        if newtls != 0 {
            new_ctx.set_field(UserContextField::ThreadPointer, newtls);
        }
        new_ctx.set_field(UserContextField::ReturnValue, 0);
        // A FreeBSD child returns 0 in %rax, 1 in %rdx and a clear carry flag
        // (cpu_fork, sys/amd64/amd64/vm_machdep.c). libc's fork() stub branches
        // on carry, so a stale CF inherited from the parent's context would make
        // the child believe fork() failed. Linux needs only %rax = 0.
        #[cfg(target_arch = "x86_64")]
        if self.linux_process().abi() == linux_object::process::Abi::Freebsd {
            let g = new_ctx.general_mut();
            g.rdx = 1;
            g.rflags &= !1;
        }
        new_thread.with_context(|ctx| *ctx = new_ctx)?;
        publish_child_tid(&new_thread, ctid);
        new_thread.start(self.thread_fn)?;
        // hunter: inherit the parent's syscall whitelist into the child so a
        // process cannot shed its policy merely by forking, and seed a fresh
        // anomaly window for the new pid.
        hunter::task_fork(self.zircon_process().id(), new_proc.id());
        Ok(new_thread.id())
    }

    /// `sys_fork` creates a child process.
    pub fn sys_fork(&self, newsp: usize, newtls: usize) -> SysResult {
        self.fork_impl(newsp, newtls, ChildTidRequest::none())
            .map(|(proc, _)| proc.id() as usize)
    }

    /// `sys_vfork` creates a child process and blocks the parent until the child terminates or execs.
    pub async fn sys_vfork(&self, newsp: usize, newtls: usize) -> SysResult {
        self.vfork_impl(newsp, newtls, ChildTidRequest::none())
            .await
            .map(|(proc, _)| proc.id() as usize)
    }

    /// `sys_clone` create a new thread in the current process.
    /// The new thread's stack pointer will be set to `newsp`,
    /// and thread pointer will be set to `newtls`.
    /// The child TID will be stored at both `parent_tid` and `child_tid`.
    ///
    /// > **NOTE!** This system call is not exactly the same as `clone` in Linux.
    ///
    /// > **NOTE!** This is partially implemented for `musl` only.
    pub async fn sys_clone(
        &self,
        flags: usize,
        newsp: usize,
        mut parent_tid: UserOutPtr<i32>,
        newtls: usize,
        child_tid: UserOutPtr<i32>,
    ) -> SysResult {
        let clone_flags = CloneFlags::from_bits_truncate(flags);
        info!(
            "clone: flags={:#x}, newsp={:#x}, parent_tid={:?}, child_tid={:?}, newtls={:#x}",
            flags, newsp, parent_tid, child_tid, newtls
        );
        if clone_flags.contains(CloneFlags::PIDFD) && clone_flags.contains(CloneFlags::THREAD) {
            return Err(LxError::EINVAL);
        }
        // Legacy clone reports the pidfd through the parent_tid slot, so the
        // two cannot both be asked for (`legacy_clone_args_valid`).
        if clone_flags.contains(CloneFlags::PIDFD)
            && clone_flags.contains(CloneFlags::PARENT_SETTID)
        {
            return Err(LxError::EINVAL);
        }
        // This kernel has no namespaces. `from_bits_truncate` silently DROPPED
        // the CLONE_NEW* bits, so a caller asking for a new mount/user/pid
        // namespace got a plain fork and was told it succeeded -- a lie with
        // consequences, since the whole point of those flags is isolation the
        // child does not actually have.
        //
        // Linux built without namespace support answers EINVAL, and that is
        // both the honest answer and the useful one: bubblewrap turns it into
        //   bwrap: Creating new namespace failed, likely because the kernel
        //          does not support user namespaces.
        // which is the string glycin matches to decide the sandbox is
        // unavailable and run its image loaders directly. It also matches the
        // ENOSYS this kernel returns from `unshare` -- the two entry points to
        // the same feature must not disagree.
        const UNSUPPORTED_NS: CloneFlags = CloneFlags::from_bits_truncate(
            CloneFlags::NEWNS.bits()
                | CloneFlags::NEWCGROUP.bits()
                | CloneFlags::NEWUTS.bits()
                | CloneFlags::NEWIPC.bits()
                | CloneFlags::NEWUSER.bits()
                | CloneFlags::NEWPID.bits()
                | CloneFlags::NEWNET.bits(),
        );
        if clone_flags.intersects(UNSUPPORTED_NS) {
            warn!(
                "clone: rejecting unsupported namespace flags {:#x} (no namespaces in this kernel)",
                flags & UNSUPPORTED_NS.bits()
            );
            return Err(LxError::EINVAL);
        }
        // Fork-like clones: if the THREAD bit is not set, the caller wants a
        // new process. This covers SIGCHLD (0x11), VFORK|VM|SIGCHLD (0x4111)
        // and other combinations used by musl/glibc fork/posix_spawn/system().
        if !clone_flags.contains(CloneFlags::THREAD) {
            // The TLS argument is meaningful ONLY with CLONE_SETTLS -- exactly
            // as the thread path below already treats it. Without the gate the
            // child's thread pointer was set from whatever happened to be in
            // the tls register, and callers legitimately leave it undefined:
            // bubblewrap's `raw_clone` is
            //     syscall (__NR_clone, flags, child_stack)
            // and musl's variadic `syscall()` always reads SIX varargs, so the
            // unpassed parent_tid/child_tid/tls come from the register save
            // area. bwrap's child got a thread pointer of 0xffffff9c -- a
            // leftover AT_FDCWD (-100) -- and died on its very first TLS access:
            //     unhandled page fault @ 0xffffff9c(READ|USER) proc=bwrap
            //     pc=0x477225   ->   mov %fs:0x0,%rbx
            // inside musl's __syscall_cp_c, i.e. before any of bwrap's own code
            // could run. parent_tid and child_tid were already gated on their
            // flags for the same reason; tls was the one that was not.
            let tls = if clone_flags.contains(CloneFlags::SETTLS) {
                newtls
            } else {
                0
            };
            // The TID bookkeeping flags are not a thread-only affair: glibc's
            // fork() is
            //     clone(CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID | SIGCHLD,
            //           0, NULL, &THREAD_SELF->tid, 0)
            // and nothing in the child ever calls gettid() to fix `tid` up --
            // the kernel is expected to have stored it (arch_fork, _Fork.c).
            // Ignored here, the child kept its parent's tid in its TCB, and
            // everything that reads pthread_self()->tid named the wrong
            // task: pthread_setname_np opened another process's
            // /proc/self/task/<tid>/comm, pthread_setaffinity_np and
            // pthread_setschedparam reached for a thread that is not in this
            // process (ESRCH), pthread_getcpuclockid clocked the parent.
            // (musl fixes its own `tid` up with gettid() after the clone,
            // which is why nothing in the base image noticed.)
            let ctid = ChildTidRequest::from_clone(clone_flags, child_tid);
            let (process, tid) = if clone_flags.contains(CloneFlags::VFORK) {
                info!("sys_clone: dispatching to sys_vfork for flags {:#x}", flags);
                self.vfork_impl(newsp, tls, ctid).await?
            } else {
                info!("sys_clone: dispatching to sys_fork for flags {:#x}", flags);
                self.fork_impl(newsp, tls, ctid)?
            };
            let pid = process.id() as usize;
            // In the parent, once the child exists (Linux writes it before the
            // child runs, but from the same clone_process, i.e. in the same
            // window -- a caller that races its own child on this word gets
            // the same answer there).
            if clone_flags.contains(CloneFlags::PARENT_SETTID) {
                parent_tid.write_if_not_null(tid as i32)?;
            }

            if clone_flags.contains(CloneFlags::PIDFD) {
                let pidfd =
                    linux_object::fs::PidFd::new(process, linux_object::fs::OpenFlags::CLOEXEC);
                let fd = self.linux_process().add_file(pidfd)?;
                // Taken back if the caller never gets the number. (Linux also
                // unwinds the whole child here; this one is already forked, so
                // the child survives a faulting `parent_tid` — a divergence
                // left alone on purpose, since undoing a live process is not
                // something to bolt onto an error path.)
                hand_out_one(
                    fd,
                    |fd| {
                        parent_tid.write(fd.into())?;
                        Ok(())
                    },
                    |fd| {
                        let _ = self.linux_process().close_file(fd);
                    },
                )?;
            }
            return Ok(pid);
        }
        // Thread creation. Accept any CLONE_THREAD combination instead of the
        // two exact musl flag values: glibc's pthread_create passes 0x3d0f00
        // (no CLONE_DETACHED), and falling back to fork() for it silently
        // created a separate process whose "threads" could never synchronize
        // through futexes with the parent.
        // `true`: a thread shares the address space, so it starts with NO
        // alternate signal stack of its own -- two threads pointing their
        // signal frames at the same pages would overwrite each other. The
        // signal mask does come across (pthread_create(3)).
        let inherited = self.thread.lock_linux().new_thread();
        let new_thread = Thread::create_linux_with(self.zircon_process(), inherited)?;
        let tid = new_thread.id();
        // Everything from here to `start` can fail, and every one of those
        // failures is on a pointer or a context userspace chose. A thread that
        // was created and never started is a thread nobody ever takes out of
        // the process's list: `Thread::kill` on it finds no coroutine and no
        // waker, so it just goes Dying, `remove_thread` never runs, the list
        // never empties, and `Process::exit` -- which only calls `terminate()`
        // when `threads.is_empty()` -- never releases the address space. The
        // process publishes its exit status, so `wait4` works and `ps` shows
        // nothing, and its whole VMAR stays alive for as long as the machine
        // does. `clone(CLONE_THREAD | CLONE_VM | CLONE_SIGHAND |
        // CLONE_PARENT_SETTID)` with `parent_tid` pointing at an unmapped page
        // is that leak in one unprivileged line, and it loops.
        //
        // `terminate_abandoned` is the hook for exactly this: "this thread
        // will never run, take it out of the list". The fork path above
        // documents its own weaker divergence (a live child survives a
        // faulting `parent_tid`); this one is not a divergence, it is a leak.
        match self.clone_start(
            &new_thread,
            tid,
            clone_flags,
            newsp,
            newtls,
            parent_tid,
            child_tid,
        ) {
            Ok(tid) => Ok(tid),
            Err(e) => {
                new_thread.terminate_abandoned();
                Err(e)
            }
        }
    }

    /// The tail of [`Self::sys_clone`]'s thread path: build the new thread's
    /// context, honour the TID bookkeeping flags, and start it.
    ///
    /// Split out so that every `?` in it is a failure the caller can undo --
    /// see there for what a created-but-never-started thread costs.
    #[allow(clippy::too_many_arguments)]
    fn clone_start(
        &self,
        new_thread: &Arc<Thread>,
        tid: KoID,
        clone_flags: CloneFlags,
        newsp: usize,
        newtls: usize,
        mut parent_tid: UserOutPtr<i32>,
        mut child_tid: UserOutPtr<i32>,
    ) -> LxResult<usize> {
        let mut new_ctx = self.thread.context_cloned()?;
        new_ctx.set_field(UserContextField::StackPointer, newsp);
        if clone_flags.contains(CloneFlags::SETTLS) {
            new_ctx.set_field(UserContextField::ThreadPointer, newtls);
        }
        new_ctx.set_field(UserContextField::ReturnValue, 0);
        // A FreeBSD child returns 0 in %rax, 1 in %rdx and a clear carry flag
        // (cpu_fork, sys/amd64/amd64/vm_machdep.c). libc's fork() stub branches
        // on carry, so a stale CF inherited from the parent's context would make
        // the child believe fork() failed. Linux needs only %rax = 0.
        #[cfg(target_arch = "x86_64")]
        if self.linux_process().abi() == linux_object::process::Abi::Freebsd {
            let g = new_ctx.general_mut();
            g.rdx = 1;
            g.rflags &= !1;
        }
        new_thread.with_context(|ctx| *ctx = new_ctx)?;

        info!("clone: {} -> {}", self.thread.id(), tid);
        // Honor the TID bookkeeping flags BEFORE the thread starts running:
        // the child and the parent's pthread library may read these
        // immediately. In particular, ctid must only be written here when
        // CLONE_CHILD_SETTID is set — musl points ctid at its global
        // __thread_list_lock (for the CLONE_CHILD_CLEARTID exit wake), and
        // unconditionally storing the TID there corrupts that lock.
        if clone_flags.contains(CloneFlags::PARENT_SETTID) {
            parent_tid.write_if_not_null(tid as i32)?;
        }
        if clone_flags.contains(CloneFlags::CHILD_SETTID) {
            child_tid.write_if_not_null(tid as i32)?;
        }
        if clone_flags.contains(CloneFlags::CHILD_CLEARTID) {
            new_thread.set_tid_address(child_tid);
        }
        new_thread.start(self.thread_fn)?;
        Ok(tid as usize)
    }

    /// `clone3` — the extensible successor of `clone`. glibc ≥ 2.34 tries it
    /// FIRST for `fork()`, `pthread_create()` and `posix_spawn()` and only
    /// falls back to legacy `clone` on ENOSYS, so answering it natively keeps
    /// glibc userspace on its primary path (and the log free of
    /// `unknown syscall: CLONE3`).
    ///
    /// Reads `struct clone_args` from `uargs` (`size` bytes, ≥ 64):
    /// flags / pidfd / child_tid / parent_tid / exit_signal / stack /
    /// stack_size / tls, all u64. Differences from legacy `clone` handled
    /// here:
    ///  * flags and the exit signal are separate fields (legacy packs the
    ///    signal into the low byte of flags);
    ///  * `stack` is the LOW address of the child stack and the kernel
    ///    computes the initial SP as `stack + stack_size` (legacy passes the
    ///    top directly);
    ///  * the pidfd (CLONE_PIDFD) has its own output pointer instead of
    ///    sharing `parent_tid`.
    ///
    /// Everything else is delegated to [`sys_clone`](Self::sys_clone).
    pub async fn sys_clone3(&self, uargs: UserInPtr<u64>, size: usize) -> SysResult {
        let size = clone3_size(size)?;
        // Anything the caller put past version 0 is a field this kernel has
        // never heard of. Dropping it silently handed back a child that was
        // not the one asked for; `copy_struct_from_user` answers E2BIG.
        if size > CLONE_ARGS_SIZE_VER0 {
            let tail = UserInPtr::<u8>::from(uargs.as_addr() + CLONE_ARGS_SIZE_VER0)
                .read_array(size - CLONE_ARGS_SIZE_VER0)?;
            if !extensible_tail_is_empty(&tail) {
                return Err(LxError::E2BIG);
            }
        }
        let words = uargs.read_array(CLONE_ARGS_SIZE_VER0 / 8)?;
        let words = <[u64; 8]>::try_from(words).unwrap();
        let args = clone3_to_clone(words)?;
        info!(
            "clone3: flags={:#x} exit_signal={} stack={:#x} stack_size={:#x}",
            words[0], words[4], words[5], words[6]
        );
        self.sys_clone(
            args.flags,
            args.newsp,
            args.parent_slot.into(),
            args.tls,
            args.child_tid.into(),
        )
        .await
    }

    /// `sys_wait4` suspends execution of the calling thread
    /// until a child specified by `pid` argument has changed state
    /// (see [linux man wait4(2)](https://www.man7.org/linux/man-pages/man2/wait4.2.html)).
    /// By default, `sys_wait4` waits only for terminated children,
    /// but this behavior is modifiable via the options argument, as described below.
    ///
    /// The value of `pid` can be:
    ///
    /// - **-1**: meaning wait for any child process.
    /// - **0**: meaning wait for any child process whose process group ID is equal to
    ///   that of the calling process at the time of the call to `sys_wait4`.
    /// - **>0**: meaning wait for the child whose process ID is equal to the value of `pid`.
    ///
    /// The value of options is an OR of zero or more of the following constants:
    ///
    /// - **NOHANG**    = 0x000_0001;
    ///
    ///   TODO
    ///
    /// - **STOPPED**   = 0x000_0002;
    ///
    ///   TODO
    ///
    /// - **EXITED**    = 0x000_0004;
    ///
    ///   TODO
    ///
    /// - **CONTINUED** = 0x000_0008;
    ///
    ///   TODO
    ///
    /// - **NOWAIT**    = 0x100_0000;
    ///
    ///   TODO
    ///
    /// On success, returns the process ID of the child whose state has changed;
    /// if `NOHANG` flag was specified and one or more child(ren) specified by pid exist,
    /// but have not yet changed state, then 0 is returned.
    /// On failure, -1 is returned.
    pub async fn sys_wait4(
        &self,
        pid: i32,
        wstatus: UserOutPtr<i32>,
        options: u32,
        rusage: UserOutPtr<RUsage>,
    ) -> SysResult {
        #[derive(Debug)]
        enum WaitTarget {
            AnyChild,
            AnyChildInGroup,
            Pgid(KoID),
            Pid(KoID),
        }
        // Validated before anything else, as `kernel_wait4` does: a bit
        // outside its mask is EINVAL whatever `pid` names.
        let opts = wait4_options(options)?;
        let target = match pid {
            -1 => WaitTarget::AnyChild,
            0 => WaitTarget::AnyChildInGroup,
            p if p > 0 => WaitTarget::Pid(p as KoID),
            // pid < -1: any child in process group |pid|.
            p => WaitTarget::Pgid((-p) as KoID),
        };
        let WaitOptions {
            nohang,
            reap,
            interest: (exited, stopped, continued),
        } = opts;
        let interest = WaitInterest {
            exited,
            stopped,
            continued,
        };
        // Hot path (shells, fork+exec, sysbench worker reaping): keep at debug
        // so a default `LOG=warn` boot doesn't pay a synchronous serial write
        // on every wait.
        debug!(
            "wait4: target={:?}, wstatus={:?}, options={:#x}",
            target, wstatus, options,
        );
        let result = match target {
            WaitTarget::AnyChild => {
                wait_child_any_interest(self.zircon_process(), nohang, reap, interest, None).await
            }
            WaitTarget::AnyChildInGroup => {
                let pgid = linux_object::process::get_process_pgid(self.zircon_process().id())
                    .unwrap_or(self.zircon_process().id());
                wait_child_any_interest(self.zircon_process(), nohang, reap, interest, Some(pgid))
                    .await
            }
            WaitTarget::Pgid(pgid) => {
                wait_child_any_interest(self.zircon_process(), nohang, reap, interest, Some(pgid))
                    .await
            }
            WaitTarget::Pid(pid) => {
                wait_child_interest(self.zircon_process(), pid, nohang, reap, interest)
                    .await
                    .map(|(code, cpu)| (pid, code, cpu))
            }
        };
        let found = match result {
            Ok(tuple) => Some(tuple),
            // WNOHANG: no child ready yet, which is 0 (waitpid(2)).
            Err(LxError::EAGAIN) if nohang => None,
            Err(e) => return Err(e),
        };
        wait4_finish(wstatus, rusage, found)
    }

    /// Wait for a child state change (`waitid(2)`). Supports `P_PID`, `P_PIDFD`, and `P_ALL`.
    ///
    /// The fifth argument is `struct rusage *`, the child's CPU time, as in
    /// `wait4(2)`; the dispatcher used to stop at the fourth.
    pub async fn sys_waitid(
        &self,
        idtype: i32,
        id: usize,
        mut infop: UserOutPtr<SigInfo>,
        options: u32,
        mut rusage: UserOutPtr<RUsage>,
    ) -> SysResult {
        let WaitOptions {
            nohang,
            reap,
            interest: (exited, stopped, continued),
        } = waitid_options(options)?;
        let interest = WaitInterest {
            exited,
            stopped,
            continued,
        };
        let caller = self.zircon_process();

        let res = match idtype {
            P_PID => {
                // `kernel_waitid`: an `id_t` read as a `pid_t`, positive.
                let id = crate::intarg::waitid_id(id, false)?;
                match wait_child_interest(caller, id as KoID, nohang, reap, interest).await {
                    Ok((code, cpu)) => Ok(Some((id as KoID, code, cpu))),
                    Err(LxError::EAGAIN) if nohang => Ok(None),
                    Err(e) => Err(e),
                }
            }
            P_PIDFD => {
                let pidfd = PidFd::from_file_like(self.linux_process().get_file_like(id.into())?)?;
                let target = pidfd.target();
                if !is_child_process(caller, target) {
                    return Err(LxError::ECHILD);
                }
                if FileLike::flags(pidfd.as_ref()).non_block()
                    && !matches!(target.status(), Status::Exited(_))
                    && !nohang
                {
                    return Err(LxError::EAGAIN);
                }
                match wait_child_interest(caller, target.id(), nohang, reap, interest).await {
                    Ok((code, cpu)) => Ok(Some((target.id(), code, cpu))),
                    Err(LxError::EAGAIN) if nohang => Ok(None),
                    Err(e) => Err(e),
                }
            }
            P_ALL => match wait_child_any_interest(caller, nohang, reap, interest, None).await {
                Ok((pid, code, cpu)) => Ok(Some((pid, code, cpu))),
                Err(LxError::EAGAIN) if nohang => Ok(None),
                Err(e) => Err(e),
            },
            P_PGID => {
                // Zero is the caller's own group; a negative id is EINVAL.
                let id = crate::intarg::waitid_id(id, true)?;
                let pgid = if id == 0 {
                    linux_object::process::get_process_pgid(caller.id()).unwrap_or(caller.id())
                } else {
                    id as KoID
                };
                match wait_child_any_interest(caller, nohang, reap, interest, Some(pgid)).await {
                    Ok((pid, code, cpu)) => Ok(Some((pid, code, cpu))),
                    Err(LxError::EAGAIN) if nohang => Ok(None),
                    Err(e) => Err(e),
                }
            }
            _ => return Err(LxError::EINVAL),
        };

        // The WHOLE status word goes into the report: `si_code` and
        // `si_status` are taken out of it together
        // (`child_si_code_and_status`), because which number `si_status`
        // carries depends on which of the three things happened. Shifting it
        // down eight bits here threw that away and left every child reported
        // as `CLD_EXITED`.
        let report = waitid_report(res?.map(|(pid, status, cpu)| {
            (
                pid as i32,
                linux_object::process::real_uid_of(pid),
                status,
                cpu,
            )
        }));
        // `infop` is written whether or not a child was found: a `WNOHANG`
        // that found nothing leaves `si_pid` zero, and used to leave the
        // caller's struct untouched, with the previous call's child in it.
        infop.write_if_not_null(report.info)?;
        if let Some(rusage_of_child) = report.rusage {
            rusage.write_if_not_null(rusage_of_child)?;
        }
        Ok(0)
    }

    /// `sys_execve` executes the program referred to by `path`
    /// (see [linux man execve(2)](https://www.man7.org/linux/man-pages/man2/execve.2.html)).
    /// This causes the program that is currently being run
    /// by the calling process to be replaced with a new program,
    /// with newly initialized stack, heap, and (initialized and uninitialized) data segments.
    ///
    /// `path` argument must be a binary executable file.
    ///
    /// `argv` is an array of argument strings passed to the new program.
    /// By convention, the first of these strings (i.e., `argv[0]`)
    /// should contain the filename associated with the file being executed.
    ///
    /// `envp` is an array of strings, conventionally of the form `key=value`,
    /// which are passed as environment to the new program.
    ///
    /// > **NOTE!** Differ from linux, `argv` & `envp` can not be NULL.
    ///
    /// > **NOTE!** For multi-thread programs,
    /// > A call to any exec function from a process with more than one thread
    /// > shall result in all threads being terminated and the new executable image
    /// > being loaded and executed.
    pub fn sys_execve(
        &mut self,
        path: UserInPtr<u8>,
        argv: UserInPtr<UserInPtr<u8>>,
        envp: UserInPtr<UserInPtr<u8>>,
    ) -> SysResult {
        let path_str = path.as_c_str().inspect_err(|&e| {
            error!("execve: path.as_c_str() failed: {:?}", e);
        })?;
        // Normal program launch — keep at debug so shells / fork+exec loops
        // don't pay a synchronous serial write per exec at the default LOG=warn.
        debug!("EXECVE: path={:?}", path_str);
        let args = argv.read_cstring_array().inspect_err(|&e| {
            error!("execve: argv.read_cstring_array() failed: {:?}", e);
        })?;
        let mut envs: Vec<String> = Vec::new();
        if !envp.is_null() {
            envs = envp.read_cstring_array().inspect_err(|&e| {
                error!("execve: envp.read_cstring_array() failed: {:?}", e);
            })?;
        }
        // Bound argv+envp so a huge argument list -- a glob like `rm dir/*`
        // expanding to thousands of paths -- fails with E2BIG instead of
        // overrunning the initial stack image the loader builds. Without this
        // the stack builder's `assert!` tripped and PANICKED THE KERNEL from an
        // ordinary userspace command. Matches Linux's "1/4 of the stack" rule:
        // the user stack is USER_STACK_PAGES (128) * 4 KiB = 512 KiB, so cap the
        // arg/env bytes (plus one 8-byte table pointer per entry) at 128 KiB.
        const ARG_MAX: usize = 128 * 1024;
        let arg_bytes: usize = args.iter().map(|s| s.len() + 1 + 8).sum::<usize>()
            + envs.iter().map(|s| s.len() + 1 + 8).sum::<usize>();
        if arg_bytes > ARG_MAX {
            warn!(
                "execve: argv+envp too large ({} > {} bytes) for {:?} -> E2BIG",
                arg_bytes, ARG_MAX, path_str
            );
            return Err(LxError::E2BIG);
        }
        info!(
            "execve: path: {:?}, args: {:?}, envs: {:?}",
            path_str, args, envs
        );
        // Record every program launch in the dmesg ring (path + argv) so a
        // captured trace shows the exact exec chain — crucially whether the X
        // server (`Xorg`/`X`/`Xorg.wrap`) is ever invoked and with what
        // arguments. `info!`/`debug!` are filtered out at the default WARN
        // level and never reach the ring, so use the unconditional klog path.
        kernel_hal::klog_info!(
            "EXECVE[{}] {:?} argv={:?}",
            self.zircon_process().id(),
            path_str,
            args
        );
        if args.is_empty() {
            error!("execve: args is empty");
            return Err(LxError::EINVAL);
        }
        if args[0].is_empty() {
            warn!("execve: argv[0] is empty for path {:?}", path_str);
        }

        // POSIX/Linux: execve in a multithreaded process kills every other
        // thread in the group before the address space is replaced. Leaving
        // them alive ran the old RIP/stack against the new image.
        {
            let proc = self.zircon_process();
            let me = self.thread.id();
            for tid in proc.thread_ids() {
                if tid == me {
                    continue;
                }
                if let Ok(obj) = proc.get_child(tid) {
                    if let Ok(t) = obj.downcast_arc::<Thread>() {
                        zircon_object::task::Task::kill(t.as_ref());
                    }
                }
            }
        }

        // Read program file
        let proc = self.linux_process();
        // ROOT-CAUSE FIX for the "intermittent" kernel corruption (see
        // docs/README-crash-repro.md). busybox in standalone-shell mode
        // re-executes its applets via execve("/proc/self/exe"). Storing that
        // LITERAL string as the new image's `execute_path` makes the magic
        // link self-referential: the next lookup of "/proc/self/exe" in that
        // process (an open, a readlink, or — fatally — the execve of the NEXT
        // applet, e.g. busybox `timeout` spawning `sleep`) resolves
        // execute_path == "/proc/self/exe" and recurses in `lookup_inode_at`
        // without bound. The coroutine stack is a guard-page-less heap
        // allocation (currently 2 MiB usable + soft canary), so the runaway
        // recursion silently writes thousands of
        // stack frames DOWNWARD over neighbouring heap allocations — spraying
        // return addresses (0xffffff00...), ASCII "/proc/self/exe" bytes and
        // small values over live pointers. That is the wild-write signature
        // behind the `timeout -s TERM 1 sleep 5` triple fault, the mangled
        // #GP RIPs, the 0x87 MNode vtable and the `cat /proc/self/exe` hang.
        // Fix: canonicalize the magic link NOW — the current execute_path is
        // by construction the real on-disk binary path — so the literal
        // "/proc/self/exe" is never stored.
        let path_str = if path_str == "/proc/self/exe" {
            let real = proc.execute_path();
            if real.is_empty() {
                return Err(LxError::ENOENT);
            }
            alloc::borrow::Cow::Owned(real)
        } else {
            alloc::borrow::Cow::Borrowed(path_str)
        };
        let path_str: &str = &path_str;
        let inode = proc.lookup_inode(path_str)?;
        let metadata = inode.metadata()?;
        proc.check_access(&metadata, 0o1, true)?;
        // `mnt_may_suid(bprm->file->f_path.mnt)`: a set-user-ID bit on a file
        // that lives on a `nosuid` mount grants nothing. Asked here, where the
        // path the caller named is still in hand, and made absolute first
        // because the mount is chosen by path prefix.
        let may_suid = !linux_object::fs::path_is_nosuid(
            &proc
                .get_absolute_path(FileDesc::CWD, path_str)
                .unwrap_or_else(|_| path_str.to_string()),
        );
        let vmo = inode.read_as_vmo_cached()?;

        // Everything below `vmar.clear()` is past the point of no return: the
        // caller's address space is gone by then, so a failure after it cannot
        // be reported BACK to the caller -- the code that would receive the
        // error is no longer mapped. Resolve the shebang interpreter chain
        // here, while the caller is still whole, so the common failure (a
        // script naming an interpreter that is not installed) comes back as an
        // ordinary ENOENT and the shell reports "not found" instead of the
        // process dying on the instruction after the syscall. Linux draws the
        // same line: the `bprm` is fully built before `begin_new_exec` commits.
        {
            let mut head = [0u8; 512];
            let n = inode.read_at(0, &mut head).unwrap_or(0);
            LinuxElfLoader {
                syscall_entry: self.syscall_entry,
                stack_pages: USER_STACK_PAGES,
                root_inode: proc.root_inode().clone(),
                // Preflight walks the shebang chain and builds no stack, so
                // there is no aux vector to be right about here -- and there
                // could not be one yet: the set-user-ID transition that
                // decides it has not happened.
                identity: Default::default(),
            }
            .preflight_interpreters(&head[..n])?;
        }

        proc.remove_cloexec_files();
        // POSIX: caught signals are reset to their default disposition across
        // exec (SIG_IGN stays). Otherwise the child keeps inherited handler
        // addresses and a delivered signal jumps into stale handler code.
        proc.reset_signal_actions_for_exec();

        // 注意！即将销毁旧应用程序的用户空间，现在将必要的信息拷贝到内核！
        // Notice! About to destroy the user space of the old application, now copy the necessary information into kernel!
        let path_str = path_str.to_string();
        let vmar = self.zircon_process().vmar();
        let (load, privileged) = {
            // mmap_lock across the whole swap: tearing down the old image and
            // loading the new one is one layout mutation — a sibling thread's
            // concurrent fork must never clone the half-empty in-between state
            // (see LinuxProcess::aspace_lock).
            let _aspace = proc.aspace_lock().lock();
            vmar.clear()?;
            // `bprm->secureexec`: whether the image about to run is more
            // privileged than whoever asked for it. It has to be decided
            // HERE, before the stack is built, because the C library reads
            // the answer out of the aux vector on that stack (`AT_SECURE`)
            // and refuses the caller's LD_PRELOAD when it is set. Linux draws
            // the same order: `begin_new_exec()` commits the credentials, and
            // `create_elf_tables()` writes the aux vector afterwards.
            let privileged = proc.apply_exec_metadata(&metadata, may_suid);
            let loaded = LinuxElfLoader {
                syscall_entry: self.syscall_entry,
                stack_pages: USER_STACK_PAGES,
                root_inode: proc.root_inode().clone(),
                identity: proc.aux_identity(privileged),
            }
            .load(&vmar, &vmo, args.clone(), envs.clone(), path_str)
            .inspect_err(|&e| {
                error!("execve: LinuxElfLoader::load failed: {:?}", e);
            });
            (loaded, privileged)
        };
        // Past the point of no return (see the preflight above): `vmar.clear()`
        // has already destroyed the caller's image, so returning this error
        // would resume a process with nothing mapped -- it faults immediately
        // on the return address, which is the "unhandled page fault ...
        // [unmapped] -> SIGSEGV" that followed every failed exec. Linux kills
        // the task with SIGSEGV here rather than returning; do the same, so
        // the parent sees a dead child instead of a live one wandering through
        // an empty address space.
        let (entry, sp, initial_brk, execute_path, abi) = match load {
            Ok(loaded) => loaded,
            Err(e) => {
                error!(
                    "execve: {:?} failed AFTER the address space was replaced; \
                     killing pid {} (it has no image left to return to)",
                    e,
                    self.zircon_process().id()
                );
                self.zircon_process().exit(exit_code_after_failed_exec());
                return Err(e);
            }
        };
        // The new image may speak a different ABI than the caller (e.g. a Linux
        // shell exec'ing a FreeBSD binary); adopt the freshly-detected one.
        proc.set_abi(abi);
        proc.set_execute_path(&execute_path);
        proc.set_cmdline(args);
        proc.set_environ(envs);
        proc.set_brk(initial_brk);
        // CRUCIAL: reset the heap's *mapped* upper bound too. `execve` replaced
        // the whole address space (`vmar.clear()` above), so the previous image's
        // heap-chunk reservation is gone. Leaving `mapped_brk` stale (e.g.
        // 0x8d1000 from the old image) makes the next `sys_brk` believe the chunk
        // is still "within the reserved mapping" and skip the real `map_at` — so
        // musl writes into an unmapped heap and SIGSEGVs. This is the
        // deterministic `sh` crash in musl mallocng's __malloc_alloc_meta seen
        // when `sh -c gendepends.sh` re-execs into the script.
        proc.set_mapped_brk(initial_brk);
        // The rest of what `execve` forgets, process side -- the attachments
        // to the address space that `vmar.clear()` above destroyed, and the
        // parent-death signal a parent chose for a program that no longer
        // exists. The other two halves ran before the swap
        // (`remove_cloexec_files`, `reset_signal_actions_for_exec`).
        proc.reset_for_exec(privileged);
        // timer_create(2): POSIX timers are disarmed and deleted by an
        // execve. They fire at a pid, and this pid is a different program
        // now, so one left armed delivers its signal to an image that never
        // asked for it. (`setitimer` timers survive, and are left alone.)
        crate::time::drop_posix_timers_of(self.zircon_process().id());
        self.zircon_process()
            .set_name(comm_from_path(&execute_path));
        // And thread side: the alternate signal stack, the robust futex list,
        // the `set_tid_address` word and the in-handler state all name the
        // image that `vmar.clear()` just destroyed, plus the comm override so
        // /proc/<pid>/comm and PR_GET_NAME fall back to the new basename.
        // try_lock_linux: PID 1 / init can re-exec (e.g. `exec`ing its real
        // payload) and may carry no LinuxThread ext; there is nothing to
        // forget in that case, so skip quietly instead of unwrap-panicking.
        if let Some(mut lt) = self.thread.try_lock_linux() {
            lt.reset_for_exec();
        }
        // hunter: a new image is now in place — re-apply any default syscall
        // whitelist and reset the anomaly window so a benign-then-malicious
        // exec cannot launder accumulated detection state.
        hunter::task_exec(self.zircon_process().id(), &execute_path);

        self.zircon_process().signal_set(Signal::USER_SIGNAL_0);
        // FreeBSD/amd64 enters with a pointer to argc in %rdi and an 8-mod-16
        // stack (exec_setregs); Linux points %rsp at argc with cleared argument
        // registers.
        #[cfg(target_arch = "x86_64")]
        let (start_sp, arg0) = if abi == linux_object::process::Abi::Freebsd {
            (((sp - 8) & !0xf) + 8, sp)
        } else {
            (sp, 0)
        };
        #[cfg(not(target_arch = "x86_64"))]
        let (start_sp, arg0) = {
            let _ = abi;
            (sp, 0)
        };
        self.thread.with_context(|ctx| {
            *ctx = UserContext::new();
            ctx.setup_uspace(entry, start_sp, &[arg0, 0, 0]);
        })?;
        Ok(0)
    }

    //    pub fn sys_yield(&self) -> SysResult {
    //        thread::yield_now();
    //        Ok(0)
    //    }
    //

    /// `sys_gettid` returns the caller's thread ID (TID)
    /// (see [linux man gettid(2)](https://www.man7.org/linux/man-pages/man2/gettid.2.html)).
    /// In a single-threaded process, the thread ID is equal to the process ID (PID, as returned by [`Self::sys_getpid`]).
    /// In a multithreaded process, all threads have the same PID, but each one has a unique TID.
    pub fn sys_gettid(&self) -> SysResult {
        info!("gettid:");
        let tid = self.thread.id();
        Ok(tid as usize)
    }

    /// `sys_getpid` returns the process ID (PID) of the calling process
    /// (see [linux man getpid(2)](https://www.man7.org/linux/man-pages/man2/getpid.2.html)).
    pub fn sys_getpid(&self) -> SysResult {
        info!("getpid:");
        let proc = self.zircon_process();
        let pid = proc.id();
        Ok(pid as usize)
    }

    /// `sys_getppid` returns the process ID of the parent of the calling process
    /// (see [linux man getppid(2)](https://www.man7.org/linux/man-pages/man2/getpid.2.html)).
    /// This will be either the ID of the process that created this process using fork(),
    /// or, if that process has already terminated, 0.
    pub fn sys_getppid(&self) -> SysResult {
        info!("getppid:");
        let proc = self.linux_process();
        let ppid = proc.parent().map(|p| p.id()).unwrap_or(0);
        Ok(ppid as usize)
    }

    /// `sys_exit` system call terminates only the calling thread
    /// (see [linux man _exit(2)](https://www.man7.org/linux/man-pages/man2/exit.2.html),
    /// this syscall is same as a raw `_exit` in glibc),
    /// and actions such as reparenting child processes or sending
    /// SIGCHLD to the parent process are performed only if this is the
    /// last thread in the thread group.
    pub fn sys_exit(&mut self, exit_code: i32) -> SysResult {
        info!("exit: code={}", exit_code);
        self.thread.exit_linux(exit_code);
        Ok(0)
    }

    /// `sys_exit_group` is equivalent to [`Self::sys_exit`]
    /// except that it terminates not only the calling thread
    /// (see [linux man exit_group(2)](https://www.man7.org/linux/man-pages/man2/exit_group.2.html),
    /// but all threads in the calling process's thread group.
    /// As a result, the entire calling process will exit.
    pub fn sys_exit_group(&mut self, exit_code: i32) -> SysResult {
        info!("exit_group: code={}", exit_code);
        let proc = self.zircon_process();
        // hunter state is released centrally in zircon Process::terminate so it
        // covers every teardown path (signal-kill, exception, _exit), not just
        // exit_group — see zircon-object/src/task/process.rs.
        proc.exit(exit_code as i64);
        Ok(0)
    }

    /// Allows the calling thread to sleep for
    /// an interval specified with nanosecond precision
    /// (see [linux man nanosleep(2)](https://www.man7.org/linux/man-pages/man2/nanosleep.2.html).
    ///
    /// `nanosleep` suspends the execution of the calling thread
    /// until either at least the time specified in `req` has elapsed,
    /// or the delivery of a signal that triggers the invocation of a handler
    /// in the calling thread or that terminates the process.
    ///
    /// To represent a duration, see TimeSpec.
    pub async fn sys_nanosleep(
        &self,
        req: UserInPtr<TimeSpec>,
        rem: UserOutPtr<TimeSpec>,
    ) -> SysResult {
        info!("nanosleep: deadline={:?}", req);
        // A `timespec` out of range is EINVAL, not a sleep of some other
        // length: `tv_nsec` has to be a fraction of a second and `tv_sec`
        // must not be negative.
        let duration = req.read()?.try_into_duration()?;
        let deadline = kernel_hal::timer::deadline_after(duration);
        // One timer for the whole sleep, and the thread's signal-wake bit
        // beside it: no per-tick rescheduling, and a signal that arrives
        // while the task is dormant ends the sleep then, not at the
        // deadline. This used to be one uninterruptible `sleep_until` with
        // a signal check after it, and `rem` was not even an argument: a
        // daemon in `sleep(60)` saw its SIGTERM a minute late, and a
        // `nanosleep` loop that restarts on EINTR with `rem` restarted from
        // whatever was in the buffer.
        sleep_or_eintr(self.thread, deadline, rem).await?;
        Ok(0)
    }

    //    pub fn sys_set_priority(&self, priority: usize) -> SysResult {
    //        let pid = thread::current().id();
    //        thread_manager().set_priority(pid, priority as u8);
    //        Ok(0)
    //    }

    /// Bitmask of CPUs that are currently online (logical ids that reached
    /// `secondary_init`). Prefer HAL's online bitset over `(1<<cpu_count())-1`,
    /// which includes APs that never came up.
    fn online_cpu_mask() -> u64 {
        kernel_hal::cpu_online_mask()
    }

    /// Resolve the thread targeted by a `sched_*affinity` call.
    ///
    /// Per Linux semantics the `pid` argument is a TID. `pid == 0` (handled by
    /// the callers) means the calling thread. Otherwise we search every live
    /// process for a thread whose TID matches; failing that we treat `pid` as a
    /// process id and return its leader thread, so `taskset -p <pid>` works for
    /// single-threaded processes.
    fn find_thread_by_tid(&self, pid: usize) -> Option<Arc<Thread>> {
        let id = pid as KoID;
        for proc in linux_object::process::all_live_processes() {
            if let Ok(obj) = proc.get_child(id) {
                if let Ok(thread) = obj.downcast_arc::<Thread>() {
                    return Some(thread);
                }
            }
        }
        let proc = zircon_object::task::ROOT_JOB.find_process(id)?;
        let first = *proc.thread_ids().first()?;
        proc.get_child(first).ok()?.downcast_arc::<Thread>().ok()
    }

    /// `sched_setaffinity` sets the CPU affinity mask of the thread `pid`.
    ///
    /// The mask is masked down to the set of online CPUs; an empty effective
    /// mask is rejected with `EINVAL`. See
    /// [linux man sched_setaffinity(2)](https://www.man7.org/linux/man-pages/man2/sched_setaffinity.2.html).
    pub fn sys_sched_setaffinity(
        &self,
        pid: usize,
        cpusetsize: usize,
        mask_ptr: UserInPtr<u8>,
    ) -> SysResult {
        info!(
            "sched_setaffinity: pid={} cpusetsize={} mask_ptr={:?}",
            pid, cpusetsize, mask_ptr
        );
        if cpusetsize == 0 {
            return Err(LxError::EINVAL);
        }
        // Only the low 64 CPUs are representable (MAX_CORE_NUM == 64).
        let n = cpusetsize.min(8);
        let bytes = mask_ptr.read_array(n)?;
        let mut mask = 0u64;
        for (i, b) in bytes.iter().enumerate() {
            mask |= (*b as u64) << (i * 8);
        }
        let eff = mask & Self::online_cpu_mask();
        if eff == 0 {
            return Err(LxError::EINVAL);
        }
        let thread = self.sched_target(pid)?;
        // __sched_setaffinity() asks `check_same_owner()` and falls back to
        // CAP_SYS_NICE, which is the same question `setpriority` asks.
        let linux = thread.proc().try_linux().ok_or(LxError::ESRCH)?;
        if !LinuxProcess::may_set_priority_of(
            &self.linux_process().credentials(),
            &linux.credentials(),
        ) {
            return Err(LxError::EPERM);
        }
        thread.set_affinity(eff).map_err(|_| LxError::EINVAL)?;
        Ok(0)
    }

    /// `sched_getaffinity` writes the CPU affinity mask of the thread `pid`
    /// into the user buffer and returns the number of bytes written.
    ///
    /// See [linux man sched_getaffinity(2)](https://www.man7.org/linux/man-pages/man2/sched_getaffinity.2.html).
    pub fn sys_sched_getaffinity(
        &self,
        pid: usize,
        cpusetsize: usize,
        mut mask_ptr: UserOutPtr<u8>,
    ) -> SysResult {
        info!(
            "sched_getaffinity: pid={} cpusetsize={} mask_ptr={:?}",
            pid, cpusetsize, mask_ptr
        );
        if cpusetsize == 0 {
            return Err(LxError::EINVAL);
        }
        let pid = crate::intarg::task_pid(pid)?;
        let mask = if pid == 0 || pid as u64 == self.thread.id() {
            self.thread.affinity()
        } else {
            self.find_thread_by_tid(pid)
                .ok_or(LxError::ESRCH)?
                .affinity()
        } & Self::online_cpu_mask();
        // The kernel cpumask is 8 bytes wide for up to 64 CPUs; copy out at most
        // that many (libc zero-fills any remaining bytes of its cpu_set_t).
        let n = cpusetsize.min(8);
        let bytes = mask.to_le_bytes();
        mask_ptr.write_array(&bytes[..n])?;
        Ok(n)
    }

    /// Resolve the thread targeted by a `sched_*` / `*priority` call.
    ///
    /// `pid == 0` (or the caller's own TID) selects the calling thread; any
    /// other value is looked up with [`find_thread_by_tid`](Self::find_thread_by_tid),
    /// which also accepts a process id for single-threaded programs. Missing
    /// targets yield `ESRCH`, matching Linux.
    fn sched_target(&self, pid: usize) -> Result<Arc<Thread>, LxError> {
        // A `pid_t`: the low 32 bits of the register. The `sched_*` calls
        // that answer EINVAL for a negative one have already said so through
        // `intarg::sched_pid`; what reaches here negative is a task that
        // `find_task_by_vpid` does not find.
        let pid = crate::intarg::task_pid(pid)?;
        if pid == 0 || pid as u64 == self.thread.id() {
            Ok(self.thread.inner())
        } else {
            self.find_thread_by_tid(pid).ok_or(LxError::ESRCH)
        }
    }

    /// Validate a `(policy, sched_priority, nice)` triple against Linux's rules
    /// and return the normalised `(policy, rt_priority, nice)` to store.
    ///
    /// Real-time policies require `sched_priority` in `1..=99`; the fair
    /// policies require it to be `0`. A nice value outside `-20..=19` is
    /// `EINVAL`.
    ///
    /// Not a clamp: `__sched_setscheduler` refuses it outright,
    /// `if (attr->sched_nice < MIN_NICE || attr->sched_nice > MAX_NICE)
    /// return -EINVAL;`. Clamping is right for `setpriority(2)`, which Linux
    /// really does clamp (and [`Self::sys_setpriority`] with it), and wrong
    /// here: `sched_setattr(0, {policy = SCHED_OTHER, nice = 100})` came back
    /// SUCCESSFUL with nice 19, so a caller probing for the accepted range --
    /// which is how a library finds out what it may ask for -- got told 100
    /// was fine and then ran at a priority it never chose. The other two
    /// callers hand in `thread.sched_nice()`, a value that is in range by
    /// construction, so this only ever fires on what `sched_setattr` was
    /// given.
    fn sched_validate(policy: u8, sched_priority: i32, nice: i32) -> Result<(u8, u8, i8), LxError> {
        if !(MIN_NICE as i32..=MAX_NICE as i32).contains(&nice) {
            return Err(LxError::EINVAL);
        }
        let nice = nice as i8;
        match policy {
            SCHED_FIFO | SCHED_RR => {
                if !(MIN_RT_PRIO as i32..=MAX_RT_PRIO as i32).contains(&sched_priority) {
                    return Err(LxError::EINVAL);
                }
                Ok((policy, sched_priority as u8, nice))
            }
            SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE => {
                if sched_priority != 0 {
                    return Err(LxError::EINVAL);
                }
                Ok((policy, 0, nice))
            }
            _ => Err(LxError::EINVAL),
        }
    }

    /// `user_check_sched_setscheduler()` for a request whose parameters have
    /// already been through [`Self::sched_validate`], which is the order
    /// Linux uses: `EINVAL` for a request that makes no sense, and only then
    /// `EPERM` for one the caller may not make.
    fn check_sched_permission(
        &self,
        thread: &Arc<Thread>,
        policy: u8,
        nice: i8,
        rt_priority: u8,
    ) -> LxResult<()> {
        // A target whose process is tearing down has no credentials to judge.
        let linux = thread.proc().try_linux().ok_or(LxError::ESRCH)?;
        let now = SchedFacts {
            policy: thread.sched_policy(),
            nice: thread.sched_nice(),
            rt_priority: thread.sched_rt_priority(),
            rlimit_nice: linux.rlimit(RLIMIT_NICE, None, false)?.cur,
            rlimit_rtprio: linux.rlimit(RLIMIT_RTPRIO, None, false)?.cur,
        };
        LinuxProcess::may_set_scheduler(
            &self.linux_process().credentials(),
            &linux.credentials(),
            &now,
            &SchedRequest {
                policy,
                nice,
                rt_priority,
            },
        )
    }

    /// `(min, max)` `sched_priority` for `policy`, or `EINVAL` for an unknown one.
    fn rt_priority_bounds(policy: usize) -> Result<(u8, u8), LxError> {
        if policy == SCHED_FIFO as usize || policy == SCHED_RR as usize {
            Ok((MIN_RT_PRIO, MAX_RT_PRIO))
        } else if policy == SCHED_NORMAL as usize
            || policy == SCHED_BATCH as usize
            || policy == SCHED_IDLE as usize
        {
            Ok((0, 0))
        } else {
            Err(LxError::EINVAL)
        }
    }

    /// `sched_setscheduler` sets both the scheduling policy and (real-time)
    /// priority of thread `pid`. The thread's nice value is preserved.
    ///
    /// See [linux man sched_setscheduler(2)](https://www.man7.org/linux/man-pages/man2/sched_setscheduler.2.html).
    pub fn sys_sched_setscheduler(
        &self,
        pid: usize,
        policy: usize,
        param: UserInPtr<i32>,
    ) -> SysResult {
        // The high SCHED_RESET_ON_FORK bit is accepted but not modelled.
        let base = policy & !SCHED_RESET_ON_FORK;
        if base > u8::MAX as usize {
            return Err(LxError::EINVAL);
        }
        // `do_sched_setscheduler`: `!param || pid < 0` is EINVAL before the
        // parameters are read (EFAULT) and before the pid is looked up.
        let pid = crate::intarg::sched_param_pid(pid, param.is_null())?;
        let sched_priority = param.read()?;
        info!(
            "sched_setscheduler: pid={} policy={} priority={}",
            pid, base, sched_priority
        );
        let thread = self.sched_target(pid)?;
        let (p, rt, nice) =
            Self::sched_validate(base as u8, sched_priority, thread.sched_nice() as i32)?;
        self.check_sched_permission(&thread, p, nice, rt)?;
        thread.set_sched(p, nice, rt);
        Ok(0)
    }

    /// `sched_getscheduler` returns the scheduling policy of thread `pid`.
    ///
    /// See [linux man sched_getscheduler(2)](https://www.man7.org/linux/man-pages/man2/sched_getscheduler.2.html).
    pub fn sys_sched_getscheduler(&self, pid: usize) -> SysResult {
        let thread = self.sched_target(crate::intarg::sched_pid(pid)?)?;
        Ok(thread.sched_policy() as usize)
    }

    /// `sched_setparam` sets the (real-time) priority of thread `pid` without
    /// changing its policy.
    ///
    /// See [linux man sched_setparam(2)](https://www.man7.org/linux/man-pages/man2/sched_setparam.2.html).
    pub fn sys_sched_setparam(&self, pid: usize, param: UserInPtr<i32>) -> SysResult {
        let pid = crate::intarg::sched_param_pid(pid, param.is_null())?;
        let sched_priority = param.read()?;
        let thread = self.sched_target(pid)?;
        let (p, rt, nice) = Self::sched_validate(
            thread.sched_policy(),
            sched_priority,
            thread.sched_nice() as i32,
        )?;
        self.check_sched_permission(&thread, p, nice, rt)?;
        thread.set_sched(p, nice, rt);
        Ok(0)
    }

    /// `sched_getparam` writes the (real-time) priority of thread `pid` into the
    /// user-supplied `struct sched_param`.
    ///
    /// See [linux man sched_getparam(2)](https://www.man7.org/linux/man-pages/man2/sched_getparam.2.html).
    pub fn sys_sched_getparam(&self, pid: usize, mut param: UserOutPtr<i32>) -> SysResult {
        let pid = crate::intarg::sched_param_pid(pid, param.is_null())?;
        let thread = self.sched_target(pid)?;
        param.write(thread.sched_rt_priority() as i32)?;
        Ok(0)
    }

    /// `sched_get_priority_max` returns the maximum `sched_priority` usable with
    /// `policy` (99 for FIFO/RR, 0 for the fair policies).
    pub fn sys_sched_get_priority_max(&self, policy: usize) -> SysResult {
        Ok(Self::rt_priority_bounds(policy)?.1 as usize)
    }

    /// `sched_get_priority_min` returns the minimum `sched_priority` usable with
    /// `policy` (1 for FIFO/RR, 0 for the fair policies).
    pub fn sys_sched_get_priority_min(&self, policy: usize) -> SysResult {
        Ok(Self::rt_priority_bounds(policy)?.0 as usize)
    }

    /// `sched_rr_get_interval` writes the round-robin timeslice of thread `pid`
    /// into `interval`. Non-`SCHED_RR` threads report a zero interval.
    ///
    /// See [linux man sched_rr_get_interval(2)](https://www.man7.org/linux/man-pages/man2/sched_rr_get_interval.2.html).
    pub fn sys_sched_rr_get_interval(
        &self,
        pid: usize,
        mut interval: UserOutPtr<TimeSpec>,
    ) -> SysResult {
        let thread = self.sched_target(crate::intarg::sched_pid(pid)?)?;
        let ts = if thread.sched_policy() == SCHED_RR {
            TimeSpec {
                sec: 0,
                nsec: 100_000_000,
            }
        } else {
            TimeSpec { sec: 0, nsec: 0 }
        };
        interval.write(ts)?;
        Ok(0)
    }

    /// `sched_setattr` sets policy, nice and real-time priority of thread `pid`
    /// from a `struct sched_attr`. `SCHED_DEADLINE` is rejected (`EINVAL`) since
    /// this scheduler has no deadline runqueue.
    ///
    /// See [linux man sched_setattr(2)](https://www.man7.org/linux/man-pages/man2/sched_setattr.2.html).
    pub fn sys_sched_setattr(
        &self,
        pid: usize,
        attr: UserInPtr<SchedAttr>,
        flags: usize,
    ) -> SysResult {
        // `!uattr || pid < 0 || flags` is EINVAL, before the struct is read.
        let pid = crate::intarg::sched_param_pid(pid, attr.is_null())?;
        if flags != 0 {
            return Err(LxError::EINVAL);
        }
        // The size is read on its own first, as `sched_copy_attr` does it:
        // it decides how much of the struct is there to read at all.
        let size = sched_setattr_size(UserInPtr::<u32>::from(attr.as_addr()).read()?)?;
        if size > SCHED_ATTR_SIZE_VER0 {
            let tail = UserInPtr::<u8>::from(attr.as_addr() + SCHED_ATTR_SIZE_VER0)
                .read_array(size - SCHED_ATTR_SIZE_VER0)?;
            if !extensible_tail_is_empty(&tail) {
                return Err(LxError::E2BIG);
            }
        }
        let a = attr.read()?;
        let plan = sched_setattr_plan(a.sched_flags, a.sched_policy)?;
        info!(
            "sched_setattr: pid={} policy={:?} flags={:#x} reset_on_fork={} nice={}",
            pid,
            plan.policy,
            a.sched_flags,
            // Read for the log only: fork-reset is not modelled (see
            // `SCHED_RESET_ON_FORK`).
            a.sched_flags & SCHED_FLAG_RESET_ON_FORK != 0,
            a.sched_nice
        );
        let thread = self.sched_target(pid)?;
        // `SETPARAM_POLICY`: the thread's own policy stands in for the one
        // in the struct. `SCHED_RESET_ON_FORK` in the policy word is
        // accepted but not modelled, as in `sched_setscheduler`.
        let policy = match plan.policy {
            None => thread.sched_policy() as usize,
            Some(p) => (p & !(SCHED_RESET_ON_FORK as u32)) as usize,
        };
        if policy == SCHED_DEADLINE as usize || policy > u8::MAX as usize {
            return Err(LxError::EINVAL);
        }
        // `get_params`: with KEEP_PARAMS the thread's own priority and nice
        // are what gets validated and permission-checked, not the caller's.
        let (priority, nice) = if plan.keep_params {
            (
                thread.sched_rt_priority() as i32,
                thread.sched_nice() as i32,
            )
        } else {
            (a.sched_priority as i32, a.sched_nice)
        };
        let (p, rt, nice) = Self::sched_validate(policy as u8, priority, nice)?;
        self.check_sched_permission(&thread, p, nice, rt)?;
        // `KEEP_PARAMS` keeps the PARAMETERS, and that is all it keeps: the
        // class change goes through. `sys_sched_setattr` uses the flag only to
        // overwrite `attr`'s priority and nice from the task (`get_params`)
        // and then runs the ordinary `sched_setattr` path, which applies
        // `attr.sched_policy`. Skipping the store made
        // `sched_setattr(tid, {policy = SCHED_FIFO, flags = KEEP_PARAMS})` --
        // "move this thread to real time and leave its numbers alone", which
        // is what a program with a nice it already tuned asks for -- a silent
        // no-op that returned 0. Storing is safe either way: with the flag,
        // `nice` and `rt` ARE the thread's own, read out of it a few lines up.
        thread.set_sched(p, nice, rt);
        Ok(0)
    }

    /// `sched_getattr` writes thread `pid`'s scheduling attributes into a
    /// caller-supplied `struct sched_attr` of `size` bytes.
    ///
    /// See [linux man sched_getattr(2)](https://www.man7.org/linux/man-pages/man2/sched_getattr.2.html).
    pub fn sys_sched_getattr(
        &self,
        pid: usize,
        mut attr: UserOutPtr<SchedAttr>,
        size: usize,
        flags: usize,
    ) -> SysResult {
        // `!uattr || pid < 0 || usize out of range || flags` is EINVAL.
        let pid = crate::intarg::sched_param_pid(pid, attr.is_null())?;
        if flags != 0 {
            return Err(LxError::EINVAL);
        }
        sched_getattr_size(size)?;
        let thread = self.sched_target(pid)?;
        let a = SchedAttr {
            size: SCHED_ATTR_SIZE_VER0 as u32,
            sched_policy: thread.sched_policy() as u32,
            sched_flags: 0,
            sched_nice: thread.sched_nice() as i32,
            sched_priority: thread.sched_rt_priority() as u32,
            sched_runtime: 0,
            sched_deadline: 0,
            sched_period: 0,
        };
        attr.write(a)?;
        Ok(0)
    }

    /// Every live thread of `proc`.
    fn threads_of(proc: &Arc<Process>) -> Vec<Arc<Thread>> {
        proc.thread_ids()
            .into_iter()
            .filter_map(|id| proc.get_child(id).ok())
            .filter_map(|obj| obj.downcast_arc::<Thread>().ok())
            .collect()
    }

    /// The tasks a `setpriority`/`getpriority` `which`/`who` pair names.
    ///
    /// The `which`/`who` pair is resolved by [`prio_target`]; this only does
    /// the looking up. An empty set is `ESRCH`, which is what Linux's
    /// `error = -ESRCH` before the walk amounts to when the walk visits
    /// nobody.
    pub(crate) fn priority_targets(&self, which: usize, who: usize) -> LxResult<Vec<Arc<Thread>>> {
        let target = prio_target(
            which,
            who,
            self.thread.id(),
            linux_object::process::effective_pgid(self.zircon_process()),
            self.linux_process().uid(),
        )?;
        let targets = match target {
            // find_task_by_vpid(): a pid that names nothing simply leaves the
            // set empty, which is the same ESRCH by another road. sched_target
            // and not find_thread_by_tid, so the caller's own id still short-
            // circuits to the running thread instead of being searched for.
            PrioTarget::Thread(tid) => self
                .sched_target(tid as usize)
                .ok()
                .into_iter()
                .collect::<Vec<_>>(),
            PrioTarget::Group(pgid) => linux_object::process::all_live_processes()
                .iter()
                .filter(|p| linux_object::process::effective_pgid(p) == pgid)
                .flat_map(Self::threads_of)
                .collect(),
            PrioTarget::User(uid) => linux_object::process::all_live_processes()
                .iter()
                .filter(|p| p.try_linux().map(|lp| lp.uid()) == Some(uid))
                .flat_map(Self::threads_of)
                .collect(),
        };
        if targets.is_empty() {
            return Err(LxError::ESRCH);
        }
        Ok(targets)
    }

    /// `set_one_prio()`: the two gates, then the store.
    fn renice_one(&self, caller: &Credentials, thread: &Arc<Thread>, nice: i8) -> LxResult<()> {
        let proc = thread.proc();
        // A task whose process is already tearing down has no credentials to
        // judge, and is gone as far as this call is concerned.
        let linux = proc.try_linux().ok_or(LxError::ESRCH)?;
        LinuxProcess::set_priority_verdict(
            caller,
            &linux.credentials(),
            thread.sched_nice(),
            linux.rlimit(RLIMIT_NICE, None, false)?.cur,
            nice,
        )?;
        thread.set_sched(thread.sched_policy(), nice, thread.sched_rt_priority());
        Ok(())
    }

    /// `setpriority` sets the nice value of every task `which`/`who` names.
    ///
    /// `prio` is clamped to the nice range `-20..=19` before anything else,
    /// as Linux does. Each task is judged on its own
    /// ([`LinuxProcess::set_priority_verdict`]) and the verdicts are folded
    /// together by [`LinuxProcess::fold_priority_verdict`], so a group with
    /// one member out of reach still renices the rest and still reports the
    /// failure.
    ///
    /// See [linux man setpriority(2)](https://www.man7.org/linux/man-pages/man2/setpriority.2.html).
    pub fn sys_setpriority(&self, which: usize, who: usize, prio: i32) -> SysResult {
        let nice = prio.clamp(MIN_NICE as i32, MAX_NICE as i32) as i8;
        info!("setpriority: which={} who={} nice={}", which, who, nice);
        let caller = self.linux_process().credentials();
        let targets = self.priority_targets(which, who)?;
        let mut result: LxResult<()> = Err(LxError::ESRCH);
        for thread in targets {
            let one = self.renice_one(&caller, &thread, nice);
            result = LinuxProcess::fold_priority_verdict(result, one);
        }
        result?;
        Ok(0)
    }

    /// `getpriority` returns the nice value of the most favoured task
    /// `which`/`who` names, encoded as `20 - nice` so the raw syscall return
    /// stays non-negative (glibc converts it back to the nice value).
    ///
    /// "Most favoured" is why it is a maximum: the encoding runs backwards,
    /// so the largest number is the smallest nice.
    ///
    /// See [linux man getpriority(2)](https://www.man7.org/linux/man-pages/man2/getpriority.2.html).
    pub fn sys_getpriority(&self, which: usize, who: usize) -> SysResult {
        let best = self
            .priority_targets(which, who)?
            .iter()
            .map(|t| LinuxProcess::nice_to_rlimit(t.sched_nice()))
            .max()
            .ok_or(LxError::ESRCH)?;
        Ok(best as usize)
    }

    /// `set_tid_address` sets the clear_child_tid value for the calling thread to `tidptr`,
    /// and return the caller's thread ID
    /// (see [linux man set_tid_address(2)](https://www.man7.org/linux/man-pages/man2/set_tid_address.2.html).
    pub fn sys_set_tid_address(&self, tidptr: UserOutPtr<i32>) -> SysResult {
        info!("set_tid_address: {:?}", tidptr);
        self.thread.set_tid_address(tidptr);
        let tid = self.thread.id();
        Ok(tid as usize)
    }

    /// Get robust list.
    pub fn sys_get_robust_list(
        &self,
        pid: i32,
        head_ptr: UserOutPtr<UserOutPtr<RobustList>>,
        len_ptr: UserOutPtr<usize>,
    ) -> SysResult {
        let thread = if pid == 0 {
            self.thread.inner()
        } else if pid < 0 {
            return Err(LxError::ESRCH);
        } else {
            self.find_thread_by_tid(pid as usize)
                .ok_or(LxError::ESRCH)?
        };
        thread.get_robust_list(head_ptr, len_ptr)
    }

    /// Set robust list.
    pub fn sys_set_robust_list(&self, head: UserInPtr<RobustList>, len: usize) -> SysResult {
        if len != size_of::<RobustList>() {
            return Err(LxError::EINVAL);
        }
        self.thread.set_robust_list(head, len);
        Ok(0)
    }

    /// `getuid` returns the real user ID of the calling process.
    pub fn sys_getuid(&self) -> SysResult {
        debug!("getuid");
        Ok(self.linux_process().uid() as usize)
    }

    /// `geteuid` returns the effective user ID of the calling process.
    pub fn sys_geteuid(&self) -> SysResult {
        debug!("geteuid");
        Ok(self.linux_process().euid() as usize)
    }

    /// `getgid` returns the real group ID of the calling process.
    pub fn sys_getgid(&self) -> SysResult {
        debug!("getgid");
        Ok(self.linux_process().gid() as usize)
    }

    /// `getegid` returns the effective group ID of the calling process.
    pub fn sys_getegid(&self) -> SysResult {
        debug!("getegid");
        Ok(self.linux_process().egid() as usize)
    }

    /// `umask` updates and returns the previous creation mask.
    pub fn sys_umask(&self, mask: usize) -> SysResult {
        Ok(self.linux_process().set_umask(mask as u16) as usize)
    }

    /// `setuid` changes the calling process user identity.
    pub fn sys_setuid(&self, uid: usize) -> SysResult {
        self.linux_process().set_uid(crate::intarg::set_id(uid)?)?;
        Ok(0)
    }

    /// `setgid` changes the calling process group identity.
    pub fn sys_setgid(&self, gid: usize) -> SysResult {
        self.linux_process().set_gid(crate::intarg::set_id(gid)?)?;
        Ok(0)
    }

    /// `setreuid` changes the real/effective user IDs.
    pub fn sys_setreuid(&self, ruid: usize, euid: usize) -> SysResult {
        self.linux_process().set_reuid(ruid as u32, euid as u32)?;
        Ok(0)
    }

    /// `setregid` changes the real/effective group IDs.
    pub fn sys_setregid(&self, rgid: usize, egid: usize) -> SysResult {
        self.linux_process().set_regid(rgid as u32, egid as u32)?;
        Ok(0)
    }

    /// `setresuid` changes the real/effective/saved user IDs.
    pub fn sys_setresuid(&self, ruid: usize, euid: usize, suid: usize) -> SysResult {
        self.linux_process()
            .set_resuid(ruid as u32, euid as u32, suid as u32)?;
        Ok(0)
    }

    /// `setresgid` changes the real/effective/saved group IDs.
    pub fn sys_setresgid(&self, rgid: usize, egid: usize, sgid: usize) -> SysResult {
        self.linux_process()
            .set_resgid(rgid as u32, egid as u32, sgid as u32)?;
        Ok(0)
    }

    /// `getgroups` returns supplementary group IDs.
    pub fn sys_getgroups(&self, size: usize, mut list: UserOutPtr<u32>) -> SysResult {
        let size = crate::intarg::groups_size(size, false)?;
        let groups = self.linux_process().groups();
        if size == 0 {
            return Ok(groups.len());
        }
        if size < groups.len() {
            return Err(LxError::EINVAL);
        }
        list.write_array(groups.as_slice())?;
        Ok(groups.len())
    }

    /// `setgroups` updates supplementary group IDs.
    pub fn sys_setgroups(&self, size: usize, list: UserInPtr<u32>) -> SysResult {
        // Linux: `if (!may_setgroups()) return -EPERM;`, which is
        // `ns_capable(CAP_SETGID)`. Asked through the same predicate
        // `capget` publishes, so a program that reads its own set and a
        // program that just calls get the same answer.
        if !self.linux_process().capable(CAP_SETGID) {
            return Err(LxError::EPERM);
        }
        let size = crate::intarg::groups_size(size, true)?;
        let groups = if size == 0 {
            Vec::new()
        } else {
            list.read_array(size)?
        };
        let groups = crate::intarg::groups_list(groups)?;
        self.linux_process().set_groups(groups);
        Ok(0)
    }

    /// `setpgid` sets the PGID of the process specified by pid to pgid.
    /// `pid == 0` targets the caller; `pgid == 0` makes the target its own
    /// group leader. Job-control shells rely on this to put each foreground job
    /// into its own process group so a Ctrl-C reaches the job, not the shell.
    pub fn sys_setpgid(&self, pid: i32, pgid: i32) -> SysResult {
        debug!("setpgid: pid={}, pgid={}", pid, pgid);
        let (target, new_pgid) = setpgid_args(self.zircon_process().id(), pid, pgid)?;
        linux_object::process::set_process_pgid(self.zircon_process(), target, new_pgid)?;
        Ok(0)
    }

    /// `getpgid` returns the PGID of the process specified by pid.
    pub fn sys_getpgid(&self, pid: i32) -> SysResult {
        debug!("getpgid: pid={}", pid);
        let target = resolve_pid_arg(self.zircon_process().id(), pid)?;
        let pgid = linux_object::process::get_process_pgid(target)?;
        Ok(pgid as usize)
    }

    /// `setsid` creates a new session if the calling process is not a process
    /// group leader: the caller becomes leader of a new session and of a new
    /// process group, both ids equal to its pid (see setsid(2)).
    pub fn sys_setsid(&self) -> SysResult {
        let pid = self.zircon_process().id();
        let proc = self.linux_process();
        // POSIX: a process-group leader may not create a new session (its pid
        // already names an existing group). The daemonize idiom fork()s first
        // precisely so the child is not a leader.
        linux_object::process::setsid_verdict(pid, &linux_object::process::live_effective_pgids())?;
        proc.become_session_leader(pid);
        info!("setsid: pid {} starts a new session", pid);
        Ok(pid as usize)
    }

    /// `getsid` returns the session ID of the process specified by pid
    /// (0 = the calling process); see getsid(2).
    pub fn sys_getsid(&self, pid: i32) -> SysResult {
        debug!("getsid: pid={}", pid);
        let target = resolve_pid_arg(self.zircon_process().id(), pid)?;
        let sid = linux_object::process::get_process_sid(target)?;
        Ok(sid as usize)
    }

    /// Operations on a process or thread (see prctl(2) and, for
    /// `PR_SET_NO_NEW_PRIVS`, Documentation/userspace-api/no_new_privs.rst).
    ///
    /// The options below are implemented against real per-process/per-thread
    /// state; anything else is refused with `EINVAL`, exactly like a kernel
    /// that does not know the option. (The previous behaviour — returning 0
    /// for *every* option — silently claimed e.g. seccomp had been engaged.)
    pub fn sys_prctl(&self, option: i32, a2: usize, a3: usize, a4: usize, a5: usize) -> SysResult {
        use linux_object::thread::TASK_COMM_LEN;

        const PR_SET_PDEATHSIG: i32 = 1;
        const PR_GET_PDEATHSIG: i32 = 2;
        const PR_GET_DUMPABLE: i32 = 3;
        const PR_SET_DUMPABLE: i32 = 4;
        const PR_SET_NAME: i32 = 15;
        const PR_GET_NAME: i32 = 16;
        const PR_GET_SECCOMP: i32 = 21;
        const PR_SET_SECCOMP: i32 = 22;
        const PR_CAPBSET_READ: i32 = 23;
        const PR_CAPBSET_DROP: i32 = 24;
        const PR_SET_TIMERSLACK: i32 = 29;
        const PR_GET_TIMERSLACK: i32 = 30;
        const PR_SET_CHILD_SUBREAPER: i32 = 36;
        const PR_GET_CHILD_SUBREAPER: i32 = 37;
        const PR_SET_NO_NEW_PRIVS: i32 = 38;
        const PR_GET_NO_NEW_PRIVS: i32 = 39;
        const PR_GET_TID_ADDRESS: i32 = 40;
        const PR_GET_KEEPCAPS: i32 = 7;
        const PR_SET_KEEPCAPS: i32 = 8;
        const PR_SET_THP_DISABLE: i32 = 41;
        const PR_GET_THP_DISABLE: i32 = 42;
        /// Default timer slack, ns (Linux: 50 µs for every fresh task).
        const TIMERSLACK_DEFAULT_NS: u64 = 50_000;

        debug!(
            "prctl: option={}, args={:#x},{:#x},{:#x},{:#x}",
            option, a2, a3, a4, a5
        );
        let proc = self.linux_process();
        match option {
            PR_SET_PDEATHSIG => {
                proc.set_pdeathsig(pdeathsig_from_arg(a2)?);
                Ok(0)
            }
            PR_GET_PDEATHSIG => {
                let mut out: UserOutPtr<i32> = a2.into();
                out.write(proc.pdeathsig() as i32)?;
                Ok(0)
            }
            PR_GET_DUMPABLE => Ok(proc.dumpable() as usize),
            PR_SET_DUMPABLE => {
                // prctl(2): since Linux 2.6.13 only SUID_DUMP_DISABLE (0) and
                // SUID_DUMP_USER (1) may be set this way.
                if a2 > 1 {
                    return Err(LxError::EINVAL);
                }
                proc.set_dumpable(a2 as u8);
                Ok(0)
            }
            PR_SET_NAME => {
                // `strncpy_from_user(comm, arg2, sizeof(comm) - 1)`: at most
                // 15 bytes are read, byte by byte, and the copy stops at
                // the first NUL, so a name of 15 bytes with no terminator
                // at the end of a mapping is read without touching what
                // lies past it.
                let name_ptr: UserInPtr<u8> = a2.into();
                let mut bytes = alloc::vec::Vec::with_capacity(TASK_COMM_LEN - 1);
                for i in 0..TASK_COMM_LEN - 1 {
                    let b = name_ptr.add(i).read()?;
                    if b == 0 {
                        break;
                    }
                    bytes.push(b);
                }
                self.thread.lock_linux().comm = comm_from_user_bytes(&bytes);
                Ok(0)
            }
            PR_GET_NAME => {
                let comm = self.thread.lock_linux().comm.clone();
                // Fall back to the executable's basename, mirroring what
                // /proc/<pid>/comm reports for a never-named thread.
                let name = if comm.is_empty() {
                    let path = proc.execute_path();
                    path.rsplit('/').next().unwrap_or_default().to_string()
                } else {
                    comm
                };
                // The buffer is specified to hold at least 16 bytes; write the
                // name NUL-terminated and NUL-padded like the kernel does.
                let mut buf = [0u8; TASK_COMM_LEN];
                let bytes = name.as_bytes();
                let n = bytes.len().min(TASK_COMM_LEN - 1);
                buf[..n].copy_from_slice(&bytes[..n]);
                let mut out: UserOutPtr<u8> = a2.into();
                out.write_array(&buf)?;
                Ok(0)
            }
            // No seccomp machinery: answer exactly like a kernel built
            // without CONFIG_SECCOMP.
            PR_GET_SECCOMP | PR_SET_SECCOMP => Err(LxError::EINVAL),
            PR_CAPBSET_READ => {
                if a2 > CAP_LAST_CAP as usize {
                    return Err(LxError::EINVAL);
                }
                // Root-run kernel: every valid capability is in the bounding
                // set (consistent with sys_capget).
                Ok(1)
            }
            PR_CAPBSET_DROP => {
                capbset_drop_verdict(a2, proc.capable(linux_object::process::CAP_SETPCAP))?;
                // There is no stored bounding set to shrink; accepting keeps
                // privilege-dropping daemons on their happy path, consistent
                // with sys_capset.
                Ok(0)
            }
            PR_SET_TIMERSLACK => {
                self.thread.lock_linux().timerslack_ns = timerslack_arg(a2);
                Ok(0)
            }
            PR_GET_TIMERSLACK => {
                let slack = self.thread.lock_linux().timerslack_ns;
                Ok(if slack == 0 {
                    TIMERSLACK_DEFAULT_NS as usize
                } else {
                    slack as usize
                })
            }
            PR_SET_CHILD_SUBREAPER => {
                proc.set_child_subreaper(a2 != 0);
                Ok(0)
            }
            PR_GET_CHILD_SUBREAPER => {
                let mut out: UserOutPtr<i32> = a2.into();
                out.write(proc.is_child_subreaper() as i32)?;
                Ok(0)
            }
            PR_SET_NO_NEW_PRIVS => {
                // prctl(2): arg2 must be 1 (the flag can never be cleared) and
                // the remaining arguments must be zero.
                if a2 != 1 || a3 != 0 || a4 != 0 || a5 != 0 {
                    return Err(LxError::EINVAL);
                }
                proc.set_no_new_privs();
                Ok(0)
            }
            PR_GET_NO_NEW_PRIVS => {
                if a2 != 0 || a3 != 0 || a4 != 0 || a5 != 0 {
                    return Err(LxError::EINVAL);
                }
                Ok(proc.no_new_privs() as usize)
            }
            PR_GET_TID_ADDRESS => {
                let mut out: UserOutPtr<usize> = a2.into();
                out.write(self.thread.lock_linux().tid_address())?;
                Ok(0)
            }
            PR_SET_THP_DISABLE => {
                if a3 != 0 || a4 != 0 || a5 != 0 {
                    return Err(LxError::EINVAL);
                }
                proc.set_thp_disable(a2 != 0);
                Ok(0)
            }
            PR_GET_THP_DISABLE => Ok(proc.thp_disable() as usize),
            PR_SET_KEEPCAPS => {
                proc.set_keep_caps(keepcaps_arg(a2)?);
                Ok(0)
            }
            PR_GET_KEEPCAPS => Ok(proc.keep_caps() as usize),
            _ => {
                debug!("prctl: unknown option {}", option);
                Err(LxError::EINVAL)
            }
        }
    }

    /// `chmod` changes the mode of the file specified by path.
    pub fn sys_chmod(&self, path: UserInPtr<u8>, mode: usize) -> SysResult {
        let path = path.as_c_str()?;
        debug!("chmod: path={:?}, mode={:#o}", path, mode);
        let proc = self.linux_process();
        let inode = proc.lookup_inode(path)?;
        let mut metadata = inode.metadata()?;
        proc.chmod_metadata(&mut metadata, mode as u16)?;
        inode.set_metadata(&metadata)?;
        Ok(0)
    }

    /// `getresuid` returns the real, effective, and saved user IDs.
    pub fn sys_getresuid(
        &self,
        mut ruid: UserOutPtr<u32>,
        mut euid: UserOutPtr<u32>,
        mut suid: UserOutPtr<u32>,
    ) -> SysResult {
        debug!(
            "getresuid: ruid={:?}, euid={:?}, suid={:?}",
            ruid, euid, suid
        );
        let creds = self.linux_process().credentials();
        ruid.write(creds.ruid)?;
        euid.write(creds.euid)?;
        suid.write(creds.suid)?;
        Ok(0)
    }

    /// `getresgid` returns the real, effective, and saved group IDs.
    pub fn sys_getresgid(
        &self,
        mut rgid: UserOutPtr<u32>,
        mut egid: UserOutPtr<u32>,
        mut sgid: UserOutPtr<u32>,
    ) -> SysResult {
        debug!(
            "getresgid: rgid={:?}, egid={:?}, sgid={:?}",
            rgid, egid, sgid
        );
        let creds = self.linux_process().credentials();
        rgid.write(creds.rgid)?;
        egid.write(creds.egid)?;
        sgid.write(creds.sgid)?;
        Ok(0)
    }

    /// `setfsuid` sets the user ID used for filesystem checks.
    pub fn sys_setfsuid(&self, fsuid: usize) -> SysResult {
        debug!("setfsuid: fsuid={}", fsuid);
        // Truncated to a `uid_t` the way the syscall boundary does, so the
        // `-1` a program writes reaches the rule as the `(uid_t)-1` that
        // `uid_valid()` rejects, whatever the register width.
        Ok(self.linux_process().set_fsuid(fsuid as u32) as usize)
    }

    /// `setfsgid` sets the group ID used for filesystem checks.
    pub fn sys_setfsgid(&self, fsgid: usize) -> SysResult {
        debug!("setfsgid: fsgid={}", fsgid);
        Ok(self.linux_process().set_fsgid(fsgid as u32) as usize)
    }
}

/// Validate `prctl(PR_SET_PDEATHSIG, arg2)` and return the byte to latch.
///
/// Not the same rule as `kill(2)`: prctl declares `arg2` as `unsigned long`, so
/// nothing is truncated to `int` first and a number with rubbish in its high
/// half is simply out of range. 0 disarms. `arg2 as u8` latched SIGKILL for
/// `arg2 = 265`, which then fires at the parent's death.
fn pdeathsig_from_arg(arg: usize) -> linux_object::error::LxResult<u8> {
    use linux_object::signal::Signal as LinuxSignal;
    if arg > LinuxSignal::RTMAX {
        return Err(LxError::EINVAL);
    }
    Ok(LinuxSignal::from_syscall_arg(arg)?.map_or(0, |sig| sig as u8))
}

bitflags! {
    pub struct CloneFlags: usize {
        ///
        const CSIGNAL =         0xff;
        /// the calling process and the child process run in the same memory space
        const VM =              1 << 8;
        /// the caller and the child process share the same filesystem information
        const FS =              1 << 9;
        /// the calling process and the child process share the same file descriptor table
        const FILES =           1 << 10;
        /// the calling process and the child process share the same table of signal handlers.
        const SIGHAND =         1 << 11;
        /// return a pidfd referring to the child process
        const PIDFD =           1 << 12;
        /// the calling process is being traced
        const PTRACE =          1 << 13;
        /// the execution of the calling process is suspended until the child releases its virtual memory resources
        const VFORK =           1 << 14;
        /// the parent of the new child will be the same as that of the call‐ing process.
        const PARENT =          1 << 15;
        /// the child is placed in the same thread group as the calling process.
        const THREAD =          1 << 16;
        /// cloned child is started in a new mount namespace
        const NEWNS	=           1 << 17;
        /// the child and the calling process share a single list of System V semaphore adjustment values.
        const SYSVSEM =         1 << 18;
        /// architecture dependent, The TLS (Thread Local Storage) descriptor is set to tls.
        const SETTLS =          1 << 19;
        /// Store the child thread ID at the location in the parent's memory.
        const PARENT_SETTID =   1 << 20;
        /// Clear (zero) the child thread ID
        const CHILD_CLEARTID =  1 << 21;
        /// the parent not to receive a signal when the child terminated
        const DETACHED =        1 << 22;
        /// a tracing process cannot force CLONE_PTRACE on this child process.
        const UNTRACED =        1 << 23;
        /// Store the child thread ID
        const CHILD_SETTID =    1 << 24;
        /// Create the process in a new cgroup namespace.
        const NEWCGROUP =       1 << 25;
        /// create the process in a new UTS namespace
        const NEWUTS =          1 << 26;
        /// create the process in a new IPC namespace.
        const NEWIPC =          1 << 27;
        /// create the process in a new user namespace
        const NEWUSER =         1 << 28;
        /// create the process in a new PID namespace
        const NEWPID =          1 << 29;
        /// create the process in a new net‐work namespace.
        const NEWNET =          1 << 30;
        /// the new process shares an I/O context with the calling process.
        const IO =              1 << 31;
    }
}

/// `prctl(PR_SET_PDEATHSIG)` took its argument as a byte, so any number whose
/// low byte happened to name a signal was latched as that signal.
/// Size of `struct clone_args` version 0 (Linux `CLONE_ARGS_SIZE_VER0`).
pub(crate) const CLONE_ARGS_SIZE_VER0: usize = 64;

use crate::extensible_tail_is_empty;
use kernel_hal::PAGE_SIZE;

/// Size of `struct sched_attr` version 0 (Linux `SCHED_ATTR_SIZE_VER0`), and
/// the whole of what this kernel knows of that struct.
pub(crate) const SCHED_ATTR_SIZE_VER0: usize = 48;

/// How many bytes of `struct clone_args` `clone3` will read, or the errno
/// Linux answers for that `size`.
pub(crate) fn clone3_size(size: usize) -> LxResult<usize> {
    if size > PAGE_SIZE {
        return Err(LxError::E2BIG);
    }
    if size < CLONE_ARGS_SIZE_VER0 {
        return Err(LxError::EINVAL);
    }
    Ok(size)
}

/// The same question for `sched_setattr`, which answers it differently in two
/// ways, both of them deliberate in `sched_copy_attr`:
///
///  * a `size` of zero means version 0. It is called an "ABI compatibility
///    quirk" in the source and it is load-bearing: the first `sched_setattr`
///    users shipped before the field was defined.
///  * a `size` out of range is **`E2BIG`**, not `EINVAL` -- and
///    `sched_getattr`, one function away, answers `EINVAL` for what looks
///    like the same question. It is not the same question: `setattr` reads
///    the size out of the struct, where it describes what the caller built,
///    and `getattr` takes it as an argument, where it describes the buffer.
pub(crate) fn sched_setattr_size(size: u32) -> LxResult<usize> {
    let size = if size == 0 {
        SCHED_ATTR_SIZE_VER0
    } else {
        size as usize
    };
    if !(SCHED_ATTR_SIZE_VER0..=PAGE_SIZE).contains(&size) {
        return Err(LxError::E2BIG);
    }
    Ok(size)
}

/// `SCHED_FLAG_RESET_ON_FORK`: the fork-reset bit as `sched_attr` carries it.
pub(crate) const SCHED_FLAG_RESET_ON_FORK: u64 = 0x01;
/// `SCHED_FLAG_KEEP_POLICY`: keep the thread's policy, ignore `sched_policy`.
pub(crate) const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
/// `SCHED_FLAG_KEEP_PARAMS`: keep the thread's priority and nice, ignore
/// `sched_priority` and `sched_nice`. The policy still changes: the flag only
/// makes `sys_sched_setattr` copy the task's own parameters over the caller's
/// (`get_params`) before the ordinary `sched_setattr` path runs.
pub(crate) const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
/// `SCHED_FLAG_ALL`: every flag `sched_setattr` knows (`RESET_ON_FORK`,
/// `RECLAIM`, `DL_OVERRUN`, `KEEP_POLICY`, `KEEP_PARAMS`, `UTIL_CLAMP_MIN`,
/// `UTIL_CLAMP_MAX`). A flag outside it is `EINVAL`.
pub(crate) const SCHED_FLAG_ALL: u64 = 0x7f;

/// What `sched_setattr` will do to the thread, decided from `sched_flags`
/// and `sched_policy` the way `sys_sched_setattr` and `__sched_setscheduler`
/// decide it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SchedAttrPlan {
    /// The policy to set, or `None` to keep the thread's own
    /// (`SCHED_FLAG_KEEP_POLICY`, `SETPARAM_POLICY`).
    pub policy: Option<u32>,
    /// `SCHED_FLAG_KEEP_PARAMS`: validate, check permission against and store
    /// the thread's own priority and nice instead of the caller's. The policy
    /// is stored either way.
    pub keep_params: bool,
}

/// `sched_setattr`'s reading of `sched_flags`: a flag outside
/// `SCHED_FLAG_ALL` is `EINVAL`; `SCHED_FLAG_KEEP_POLICY` makes the policy
/// `SETPARAM_POLICY`, which is "the one the thread has"; and
/// `SCHED_FLAG_KEEP_PARAMS` copies the thread's priority and nice over the
/// caller's (`get_params`). A negative
/// `sched_policy` (`(int)attr.sched_policy < 0`) is `EINVAL` before any of
/// that, and before the pid is looked up.
///
/// The flags were never read. `KEEP_POLICY` is how a program changes the
/// nice of a thread without knowing its policy (systemd's `Nice=` on a
/// service that set `SCHED_FIFO` for itself, `chrt`-less renicing in
/// PipeWire's module-rt): with `sched_policy` left at zero, as the manual
/// says it may be, the thread was dropped from `SCHED_FIFO` to
/// `SCHED_OTHER`. And a flag Linux does not know was accepted.
pub(crate) fn sched_setattr_plan(sched_flags: u64, sched_policy: u32) -> LxResult<SchedAttrPlan> {
    if (sched_policy as i32) < 0 {
        return Err(LxError::EINVAL);
    }
    if sched_flags & !SCHED_FLAG_ALL != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(SchedAttrPlan {
        policy: if sched_flags & SCHED_FLAG_KEEP_POLICY != 0 {
            None
        } else {
            Some(sched_policy)
        },
        keep_params: sched_flags & SCHED_FLAG_KEEP_PARAMS != 0,
    })
}

/// And for `sched_getattr`: `EINVAL`, and no zero quirk. See
/// [`sched_setattr_size`].
pub(crate) fn sched_getattr_size(size: usize) -> LxResult<usize> {
    if !(SCHED_ATTR_SIZE_VER0..=PAGE_SIZE).contains(&size) {
        return Err(LxError::EINVAL);
    }
    Ok(size)
}

/// The legacy-`clone` arguments that a `clone3` `struct clone_args` decodes to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Clone3Args {
    /// Legacy `flags` word: the clone flags with the exit signal in its low byte.
    pub flags: usize,
    /// Initial stack pointer for the child (0 = share the caller's).
    pub newsp: usize,
    /// What legacy `clone` calls `parent_tid`: the pidfd slot under `CLONE_PIDFD`.
    pub parent_slot: usize,
    /// New TLS base.
    pub tls: usize,
    /// `child_tid` pointer.
    pub child_tid: usize,
}

/// Decodes the eight `u64` of `struct clone_args` into legacy `clone`
/// arguments, or the `errno` Linux answers for that struct.
///
/// What a fork-like clone owes the caller about the child's TID, read off
/// the `CLONE_CHILD_SETTID` and `CLONE_CHILD_CLEARTID` bits: whether the
/// child's tid is stored at `ptr` (in the CHILD, before it runs) and whether
/// that word is zeroed and futex-woken when the child's thread exits.
///
/// Both refer to `ptr` in the child's address space, which for a fork is a
/// copy the parent's `UserOutPtr` cannot reach; see [`publish_child_tid`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChildTidRequest {
    /// The word's address, 0 for none.
    ptr: usize,
    set: bool,
    clear: bool,
}

impl ChildTidRequest {
    /// A plain `fork(2)` / `vfork(2)`: no flags, nothing to do.
    pub(crate) fn none() -> Self {
        Self {
            ptr: 0,
            set: false,
            clear: false,
        }
    }

    /// The request the clone flags make of the `child_tid` argument.
    pub(crate) fn from_clone(flags: CloneFlags, ptr: UserOutPtr<i32>) -> Self {
        Self {
            ptr: ptr.as_addr(),
            set: flags.contains(CloneFlags::CHILD_SETTID),
            clear: flags.contains(CloneFlags::CHILD_CLEARTID),
        }
    }
}

/// Do a [`ChildTidRequest`] for `thread`, the (not yet started) sole thread
/// of a forked child, in that child's own address space.
///
/// `CLONE_CHILD_SETTID`: the tid goes through the child's VMAR, because the
/// parent's `UserOutPtr` writes the parent's copy of the page. Linux does
/// this store from the child itself (`schedule_tail`) and ignores a fault
/// on it, so an unmapped word is logged and skipped rather than failing a
/// fork whose child already exists. `CLONE_CHILD_CLEARTID`: the word is
/// registered exactly as `set_tid_address(2)` would, and thread exit does
/// the zero-and-wake.
pub(crate) fn publish_child_tid(thread: &Arc<Thread>, ctid: ChildTidRequest) {
    if ctid.ptr == 0 {
        return;
    }
    if ctid.set {
        let word = (thread.id() as i32).to_ne_bytes();
        let vaddr = ctid.ptr;
        match thread.proc().vmar().write_memory(vaddr, &word) {
            Ok(n) if n == word.len() => {}
            other => warn!(
                "clone: CLONE_CHILD_SETTID word {:#x} not writable in child {}: {:?}",
                vaddr,
                thread.proc().id(),
                other
            ),
        }
    }
    if ctid.clear {
        thread.set_tid_address(UserOutPtr::from(ctid.ptr));
    }
}

/// Pulled out of [`Syscall::sys_clone3`] because it is the whole of what
/// `clone3` does differently, and because until this batch **none of it ran**:
/// the dispatch table carried `Sys::CLONE3 => Err(LxError::ENOSYS)` above the
/// arm that called the implementation, with `#[allow(unreachable_patterns)]`
/// on the live arm to keep the build quiet under `deny(warnings)`. The stub
/// answered every `clone3`, and the implementation below it was decoration.
///
/// Which mattered, because it carried a way for any process to take the kernel
/// down: the child's stack pointer is `stack + stack_size`, both read straight
/// out of userspace, and a plain `+` on a debug build panics on overflow.
/// `clone3` with `stack = u64::MAX` and `stack_size = 1` was a kernel panic
/// waiting for the day someone deleted the stub.
pub(crate) fn clone3_to_clone(words: [u64; 8]) -> LxResult<Clone3Args> {
    let [flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls] = words;
    // The exit signal lives in its own field; flags carrying low-byte bits
    // is invalid here, as is an out-of-range signal number.
    if flags & 0xff != 0 || exit_signal > 64 {
        return Err(LxError::EINVAL);
    }
    // Linux's `clone3_stack_valid`: both zero, or neither.
    if (stack == 0) != (stack_size == 0) {
        return Err(LxError::EINVAL);
    }
    let newsp = if stack != 0 {
        stack.checked_add(stack_size).ok_or(LxError::EINVAL)?
    } else {
        0
    };
    let clone_flags = CloneFlags::from_bits_truncate(flags as usize);
    // Legacy clone reports the pidfd through the parent_tid slot; clone3
    // gives it a dedicated field. PARENT_SETTID and PIDFD together can't
    // be expressed through the legacy entry point — glibc never combines
    // them, so reject rather than misdeliver one of the two.
    let parent_slot = if clone_flags.contains(CloneFlags::PIDFD) {
        if clone_flags.contains(CloneFlags::PARENT_SETTID) {
            return Err(LxError::EINVAL);
        }
        pidfd
    } else {
        parent_tid
    };
    Ok(Clone3Args {
        flags: flags as usize | exit_signal as usize,
        newsp: newsp as usize,
        parent_slot: parent_slot as usize,
        tls: tls as usize,
        child_tid: child_tid as usize,
    })
}

#[cfg(test)]
mod fork_child_tid_tests {
    use super::*;
    use kernel_hal::PAGE_SIZE;
    use linux_object::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::ROOT_JOB;
    use zircon_object::vm::{MMUFlags, VmObject};

    /// glibc's `arch_fork`: `CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID | SIGCHLD`.
    const GLIBC_FORK: usize = 0x0120_0011;
    /// The word every fork-time bit will be stored at (the parent's page is
    /// seeded with this so the test can tell whose copy got written).
    const SENTINEL: i32 = 0x5a5a_5a5a;

    fn flags(bits: usize) -> CloneFlags {
        CloneFlags::from_bits_truncate(bits)
    }

    /// A parent with one thread and one writable page holding a word set to
    /// [`SENTINEL`]; returns it with that word's address.
    fn a_parent(pid: KoID) -> (Arc<Process>, usize) {
        let parent = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "parent",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let _main = Thread::create_linux(&parent).unwrap();
        // At a fixed, non-zero offset: an auto-placed page can land at 0,
        // which every caller of these functions reads as "no pointer".
        let page = parent
            .vmar()
            .map(
                Some(0x10_0000),
                VmObject::new_paged(1),
                0,
                PAGE_SIZE,
                MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
            )
            .unwrap();
        assert_ne!(page, 0);
        // Inside the page, not at its start: a tid word is a field of the
        // TCB, and a page-aligned one would let a rounded address pass.
        let word = page + 0x40;
        assert_eq!(
            parent
                .vmar()
                .write_memory(word, &SENTINEL.to_ne_bytes())
                .unwrap(),
            4
        );
        (parent, word)
    }

    fn word_at(proc: &Arc<Process>, vaddr: usize) -> i32 {
        let mut b = [0u8; 4];
        assert_eq!(proc.vmar().read_memory(vaddr, &mut b).unwrap(), 4);
        i32::from_ne_bytes(b)
    }

    /// A forked child of `parent` with its one, unstarted thread.
    fn a_forked_child(parent: &Arc<Process>) -> (Arc<Process>, Arc<Thread>) {
        let child = Process::fork_from(parent).unwrap();
        let thread = Thread::create_linux(&child).unwrap();
        (child, thread)
    }

    #[test]
    fn glibcs_fork_flags_ask_for_both_the_store_and_the_clear() {
        let req = ChildTidRequest::from_clone(flags(GLIBC_FORK), 0x7000_0010.into());
        assert_eq!(
            req,
            ChildTidRequest {
                ptr: 0x7000_0010,
                set: true,
                clear: true
            }
        );
        // musl's fork is a bare SIGCHLD: nothing asked, nothing done.
        let req = ChildTidRequest::from_clone(flags(0x11), 0x7000_0010.into());
        assert_eq!(
            req,
            ChildTidRequest {
                ptr: 0x7000_0010,
                set: false,
                clear: false
            }
        );
        assert_eq!(ChildTidRequest::none().ptr, 0);
    }

    #[test]
    fn the_childs_tid_lands_in_the_childs_copy_of_the_page_not_the_parents() {
        // The very bug: the store went through the parent's pointer, i.e.
        // into the parent's copy, and the child's TCB kept the parent's tid.
        let (parent, page) = a_parent(43_361);
        let (child, thread) = a_forked_child(&parent);
        publish_child_tid(
            &thread,
            ChildTidRequest::from_clone(flags(GLIBC_FORK), page.into()),
        );
        assert_eq!(word_at(&child, page), thread.id() as i32, "child's copy");
        assert_eq!(word_at(&parent, page), SENTINEL, "parent's copy");
        // (The first thread of a process carries the pid as its tid, as on
        // Linux, so the word also reads as the child's getpid().)
        assert_eq!(thread.id(), child.id());
    }

    #[test]
    fn the_clear_on_exit_is_registered_as_set_tid_address_would() {
        let (parent, page) = a_parent(43_362);
        let (_child, thread) = a_forked_child(&parent);
        assert!(thread.tid_address().is_null());
        publish_child_tid(
            &thread,
            ChildTidRequest::from_clone(flags(GLIBC_FORK), page.into()),
        );
        assert_eq!(thread.tid_address().as_addr(), page);
    }

    #[test]
    fn without_the_flags_the_word_is_left_alone() {
        // A `clone(SIGCHLD, ..., ctid)` with a live ctid and neither bit
        // (musl leaves the register holding whatever): nothing may be written
        // and nothing registered, or exit would zero a word nobody gave it.
        let (parent, page) = a_parent(43_363);
        let (child, thread) = a_forked_child(&parent);
        publish_child_tid(
            &thread,
            ChildTidRequest::from_clone(flags(0x11), page.into()),
        );
        assert_eq!(word_at(&child, page), SENTINEL);
        assert!(thread.tid_address().is_null());
        // CLEARTID alone registers and does not store (pthread-style callers
        // that pass the two separately).
        publish_child_tid(
            &thread,
            ChildTidRequest::from_clone(flags(0x0020_0011), page.into()),
        );
        assert_eq!(word_at(&child, page), SENTINEL);
        assert_eq!(thread.tid_address().as_addr(), page);
    }

    #[test]
    fn a_word_the_child_has_not_mapped_is_skipped_not_fatal() {
        // Linux stores from the child (`schedule_tail`) and ignores the
        // fault; the fork has already happened, so failing it here would
        // leave a child the caller was told does not exist.
        let (parent, page) = a_parent(43_364);
        let (child, thread) = a_forked_child(&parent);
        publish_child_tid(
            &thread,
            ChildTidRequest::from_clone(flags(GLIBC_FORK), (page + 0x10_0000).into()),
        );
        assert_eq!(word_at(&child, page), SENTINEL);
        // The clear is still registered: exit tolerates an unmapped word.
        assert_eq!(thread.tid_address().as_addr(), page + 0x10_0000);
    }

    #[test]
    fn a_null_word_is_a_no_op_whatever_the_flags() {
        let (parent, page) = a_parent(43_365);
        let (child, thread) = a_forked_child(&parent);
        publish_child_tid(
            &thread,
            ChildTidRequest::from_clone(flags(GLIBC_FORK), 0.into()),
        );
        assert_eq!(word_at(&child, page), SENTINEL);
        assert!(thread.tid_address().is_null());
    }
}

#[cfg(test)]
mod pdeathsig_tests {
    use super::*;
    use linux_object::signal::Signal as LinuxSignal;

    #[test]
    fn zero_disarms() {
        assert_eq!(pdeathsig_from_arg(0), Ok(0));
    }

    #[test]
    fn every_signal_is_latched_as_itself() {
        for n in 1..=LinuxSignal::RTMAX {
            assert_eq!(pdeathsig_from_arg(n), Ok(n as u8), "PR_SET_PDEATHSIG {}", n);
        }
    }

    #[test]
    fn a_number_above_nsig_is_rejected_not_truncated() {
        // 265 & 0xff == 9: the child asked for a signal that does not exist and
        // got SIGKILL on its parent's death.
        assert_eq!(pdeathsig_from_arg(265), Err(LxError::EINVAL));
        assert_eq!(pdeathsig_from_arg(65), Err(LxError::EINVAL));
        assert_eq!(pdeathsig_from_arg(256), Err(LxError::EINVAL));
    }

    #[test]
    fn prctl_does_not_truncate_to_int_the_way_kill_does() {
        // prctl(2) declares arg2 `unsigned long`, so unlike kill(2) there is no
        // 32-bit narrowing before `valid_signal()`: the high half makes the
        // number out of range instead of disappearing.
        assert_eq!(
            pdeathsig_from_arg(0xdead_beef_0000_0009),
            Err(LxError::EINVAL)
        );
        assert_eq!(pdeathsig_from_arg(0x1_0000_0000), Err(LxError::EINVAL));
        // The same two words through kill(2)'s rule, for contrast.
        assert_eq!(
            LinuxSignal::from_syscall_arg(0xdead_beef_0000_0009),
            Ok(Some(LinuxSignal::SIGKILL))
        );
        assert_eq!(LinuxSignal::from_syscall_arg(0x1_0000_0000), Ok(None));
    }

    #[test]
    fn a_negative_argument_is_out_of_range() {
        // prctl never sees it as negative; it sees a very large unsigned.
        assert_eq!(pdeathsig_from_arg(usize::MAX), Err(LxError::EINVAL));
        assert_eq!(pdeathsig_from_arg(-9i64 as usize), Err(LxError::EINVAL));
    }
}

#[cfg(test)]
mod clone3_tests {
    use super::*;

    /// `struct clone_args` version 0, as eight `u64`, in field order.
    fn args(
        flags: u64,
        pidfd: u64,
        child_tid: u64,
        parent_tid: u64,
        exit_signal: u64,
        stack: u64,
        stack_size: u64,
        tls: u64,
    ) -> [u64; 8] {
        [
            flags,
            pidfd,
            child_tid,
            parent_tid,
            exit_signal,
            stack,
            stack_size,
            tls,
        ]
    }

    fn ok(words: [u64; 8]) -> Clone3Args {
        clone3_to_clone(words).expect("should decode")
    }

    #[test]
    fn a_stack_that_overflows_is_refused_instead_of_panicking_the_kernel() {
        // `stack + stack_size` are both read straight out of userspace, and a
        // plain `+` panics on a debug build. Any process could have taken the
        // kernel down with this the moment clone3 became reachable.
        assert_eq!(
            clone3_to_clone(args(0, 0, 0, 0, 0, u64::MAX, 1, 0)),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            clone3_to_clone(args(0, 0, 0, 0, 0, 1, u64::MAX, 0)),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            clone3_to_clone(args(0, 0, 0, 0, 0, u64::MAX, u64::MAX, 0)),
            Err(LxError::EINVAL)
        );
        // The largest pair that does not overflow is still a legal request.
        let a = ok(args(0, 0, 0, 0, 0, u64::MAX - 1, 1, 0));
        assert_eq!(a.newsp, u64::MAX as usize);
    }

    #[test]
    fn the_child_stack_pointer_is_the_top_of_the_range() {
        // Legacy clone is handed the top of the stack; clone3 gives the low
        // address and a size, and the kernel adds them.
        let a = ok(args(0, 0, 0, 0, 0, 0x7000_0000, 0x10_0000, 0));
        assert_eq!(a.newsp, 0x7010_0000);
        // No stack at all means "share the caller's", not "top of nothing".
        let a = ok(args(0, 0, 0, 0, 0, 0, 0, 0));
        assert_eq!(a.newsp, 0);
    }

    #[test]
    fn a_stack_without_a_size_or_a_size_without_a_stack_is_refused() {
        assert_eq!(
            clone3_to_clone(args(0, 0, 0, 0, 0, 0x7000_0000, 0, 0)),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            clone3_to_clone(args(0, 0, 0, 0, 0, 0, 0x10_0000, 0)),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn the_exit_signal_is_folded_back_into_the_legacy_flags_word() {
        // clone3 gives the exit signal its own field; legacy clone packs it
        // into the low byte of flags, which is what sys_clone reads.
        let a = ok(args(0x0011_0f00, 0, 0, 0, 17, 0, 0, 0));
        assert_eq!(a.flags & 0xff, 17, "SIGCHLD must survive the conversion");
        assert_eq!(a.flags & !0xff, 0x0011_0f00);
    }

    #[test]
    fn flags_carrying_a_signal_in_their_low_byte_are_refused() {
        // In clone3 the low byte of flags is reserved; a caller putting the
        // signal there as well would have it counted twice.
        assert_eq!(
            clone3_to_clone(args(0x0011_0f11, 0, 0, 0, 0, 0, 0, 0)),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_signal_number_that_does_not_exist_is_refused() {
        assert!(clone3_to_clone(args(0, 0, 0, 0, 64, 0, 0, 0)).is_ok());
        assert_eq!(
            clone3_to_clone(args(0, 0, 0, 0, 65, 0, 0, 0)),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            clone3_to_clone(args(0, 0, 0, 0, u64::MAX, 0, 0, 0)),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn the_pidfd_travels_in_the_parent_tid_slot_only_when_asked_for() {
        let pidfd_flag = CloneFlags::PIDFD.bits() as u64;
        let a = ok(args(pidfd_flag, 0xfd00, 0, 0xba5e, 0, 0, 0, 0));
        assert_eq!(a.parent_slot, 0xfd00, "CLONE_PIDFD uses its own field");
        let a = ok(args(0, 0xfd00, 0, 0xba5e, 0, 0, 0, 0));
        assert_eq!(a.parent_slot, 0xba5e, "without it, parent_tid as usual");
    }

    #[test]
    fn asking_for_a_pidfd_and_a_parent_tid_at_once_is_refused_not_guessed() {
        // The legacy entry point has one slot for both, so one of the two
        // would be written to the other's pointer.
        let both = (CloneFlags::PIDFD.bits() | CloneFlags::PARENT_SETTID.bits()) as u64;
        assert_eq!(
            clone3_to_clone(args(both, 0x1000, 0, 0x2000, 0, 0, 0, 0)),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn the_remaining_fields_are_passed_through_untouched() {
        let a = ok(args(0x0001_0000, 0, 0xc71d, 0xba5e, 0, 0, 0, 0x715));
        assert_eq!(a.child_tid, 0xc71d);
        assert_eq!(a.tls, 0x715);
        assert_eq!(a.parent_slot, 0xba5e);
    }

    #[test]
    fn version_zero_of_the_struct_is_sixty_four_bytes() {
        assert_eq!(CLONE_ARGS_SIZE_VER0, 64);
        assert_eq!(CLONE_ARGS_SIZE_VER0 / 8, 8, "eight u64 fields");
    }
}

#[cfg(test)]
mod wait_option_tests {
    //! `wait4` and `waitid` read the same options word and Linux holds them
    //! to two different masks. `waitid` checked its own, but inline, where no
    //! test could reach it; `wait4` ran the word through
    //! `WaitFlags::from_bits_truncate` and kept whatever was left.

    use super::*;

    const WNOHANG: u32 = 0x0000_0001;
    const WUNTRACED: u32 = 0x0000_0002;
    const WEXITED: u32 = 0x0000_0004;
    const WCONTINUED: u32 = 0x0000_0008;
    const WNOWAIT: u32 = 0x0100_0000;
    const WNOTHREAD: u32 = 0x2000_0000;
    const WALL: u32 = 0x4000_0000;
    const WCLONE: u32 = 0x8000_0000;

    /// `__WALL` is 0x40000000 and `__WCLONE` is 0x80000000. They were the
    /// other way round in `waitid`'s own `bitflags`. Neither changes what
    /// this kernel does yet, which is exactly why a swap sat there unnoticed
    /// and would have been believed by the first caller to act on it.
    #[test]
    fn the_two_underscore_flags_are_the_bits_linux_gives_them() {
        assert_eq!(wait_opts::WALL, 0x4000_0000);
        assert_eq!(wait_opts::WCLONE, 0x8000_0000);
        assert_eq!(wait_opts::WNOTHREAD, 0x2000_0000);
        // `WSTOPPED` is `WUNTRACED` under another name, which is why one
        // field carries both syscalls' meaning.
        assert_eq!(wait_opts::WUNTRACED, 0x0000_0002);
        // And the two masks, written out, so a bit cannot move between them
        // without this failing.
        assert_eq!(wait_opts::WAIT4, 0xE000_000B);
        assert_eq!(wait_opts::WAITID, 0xE100_000F);
        assert_eq!(wait_opts::WAITID_REQUIRED, 0x0000_000E);
    }

    /// `WEXITED` and `WNOWAIT` belong to `waitid`. `kernel_wait4`'s mask
    /// leaves both out, so naming either is EINVAL there.
    #[test]
    fn wait4_refuses_the_two_bits_that_are_waitids() {
        assert_eq!(wait4_options(WEXITED), Err(LxError::EINVAL));
        assert_eq!(wait4_options(WNOWAIT), Err(LxError::EINVAL));
        assert_eq!(wait4_options(WNOHANG | WNOWAIT), Err(LxError::EINVAL));
        // And they are fine on waitid, which is the point of the two masks.
        assert!(waitid_options(WEXITED).is_ok());
        assert!(waitid_options(WEXITED | WNOWAIT).is_ok());
    }

    /// `wait4(pid, &st, WNOWAIT, NULL)` used to leave the zombie in place --
    /// an extension of this kernel's own, since the call is EINVAL in Linux.
    /// A `wait4` always reaps.
    #[test]
    fn a_wait4_always_reaps_and_always_reports_an_exit() {
        let o = wait4_options(0).unwrap();
        assert!(o.reap);
        assert_eq!(o.interest, (true, false, false));
        assert!(!o.nohang);
        let o = wait4_options(WNOHANG | WUNTRACED | WCONTINUED).unwrap();
        assert!(o.nohang);
        assert!(o.reap);
        assert_eq!(o.interest, (true, true, true));
    }

    #[test]
    fn wait4_takes_every_bit_on_its_own_mask_and_nothing_else() {
        assert!(
            wait4_options(WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE).is_ok()
        );
        for stray in [0x10u32, 0x20, 0x1_0000, 0x1000_0000] {
            assert_eq!(wait4_options(stray), Err(LxError::EINVAL), "{:#x}", stray);
        }
    }

    /// `waitid` has no default interest, so a word naming none of the three
    /// is a wait for nothing. Linux refuses it rather than block forever.
    #[test]
    fn a_waitid_that_asks_for_nothing_is_refused() {
        assert_eq!(waitid_options(0), Err(LxError::EINVAL));
        assert_eq!(waitid_options(WNOHANG), Err(LxError::EINVAL));
        assert_eq!(waitid_options(WNOWAIT | WALL), Err(LxError::EINVAL));
        for one in [WEXITED, WUNTRACED, WCONTINUED] {
            assert!(waitid_options(one).is_ok(), "{:#x}", one);
        }
    }

    #[test]
    fn waitid_reports_exactly_what_it_was_asked_for() {
        let o = waitid_options(WEXITED).unwrap();
        assert_eq!(o.interest, (true, false, false));
        assert!(o.reap);
        let o = waitid_options(WUNTRACED | WNOWAIT | WNOHANG).unwrap();
        assert_eq!(o.interest, (false, true, false));
        assert!(!o.reap);
        assert!(o.nohang);
        let o = waitid_options(WEXITED | WUNTRACED | WCONTINUED).unwrap();
        assert_eq!(o.interest, (true, true, true));
    }

    #[test]
    fn waitid_refuses_a_bit_outside_its_mask_too() {
        for stray in [0x10u32, 0x0200_0000, 0x1000_0000] {
            assert_eq!(
                waitid_options(stray | WEXITED),
                Err(LxError::EINVAL),
                "{:#x}",
                stray
            );
        }
    }
}

#[cfg(test)]
mod prctl_arg_tests {
    //! Three `prctl(2)` options that took their argument as it came.

    use super::*;

    /// `PR_CAPBSET_DROP` needs `CAP_SETPCAP`, judged before the number:
    /// an unprivileged caller hears `EPERM` even for a number that is not
    /// a capability, and root hears `EINVAL` for that one.
    #[test]
    fn dropping_from_the_bounding_set_needs_cap_setpcap_first() {
        assert_eq!(capbset_drop_verdict(0, false), Err(LxError::EPERM));
        assert_eq!(capbset_drop_verdict(999, false), Err(LxError::EPERM));
        assert_eq!(capbset_drop_verdict(0, true), Ok(()));
        assert_eq!(capbset_drop_verdict(CAP_LAST_CAP as usize, true), Ok(()));
        assert_eq!(
            capbset_drop_verdict(CAP_LAST_CAP as usize + 1, true),
            Err(LxError::EINVAL)
        );
        assert_eq!(capbset_drop_verdict(usize::MAX, true), Err(LxError::EINVAL));
    }

    /// `PR_SET_TIMERSLACK`'s argument is a `long`: zero or negative means
    /// the default, and nothing else is clamped.
    #[test]
    fn a_non_positive_timer_slack_is_the_default() {
        assert_eq!(timerslack_arg(0), 0);
        assert_eq!(timerslack_arg(usize::MAX), 0, "-1 is the default, not 2^64");
        assert_eq!(timerslack_arg(isize::MIN as usize), 0);
        assert_eq!(timerslack_arg(1), 1);
        assert_eq!(timerslack_arg(50_000), 50_000);
        assert_eq!(timerslack_arg(isize::MAX as usize), isize::MAX as u64);
    }

    /// `PR_SET_NAME` stores bytes: to the first NUL, at most 15, and a byte
    /// that is not UTF-8 is kept as a `?` rather than refused.
    #[test]
    fn the_comm_is_bytes_to_the_first_nul_and_never_refused() {
        assert_eq!(comm_from_user_bytes(b"worker junk"), "worker");
        assert_eq!(comm_from_user_bytes(b"worker"), "worker");
        assert_eq!(comm_from_user_bytes(b""), "");
        assert_eq!(
            comm_from_user_bytes(b"0123456789abcdefXYZ"),
            "0123456789abcde",
            "15 bytes at most"
        );
        // Latin-1 "Señal": the ñ is one byte that is not UTF-8.
        assert_eq!(comm_from_user_bytes(b"Se\xf1al"), "Se?al");
        // A JVM cutting "Thread-ñ" at 15 bytes leaves half a character.
        let mut cut = b"pool-1-thread-".to_vec();
        cut.push(0xc3); // first byte of a two-byte sequence, no second
        assert_eq!(comm_from_user_bytes(&cut), "pool-1-thread-?");
        // Whole multibyte characters survive.
        assert_eq!(comm_from_user_bytes("señal".as_bytes()), "señal");
    }
}

#[cfg(test)]
mod keepcaps_tests {
    //! `prctl(PR_SET_KEEPCAPS)`'s argument.

    use super::*;

    /// 0 and 1 and nothing else, unlike the "nonzero" boolean options.
    #[test]
    fn keepcaps_takes_zero_or_one_and_nothing_else() {
        assert_eq!(keepcaps_arg(0), Ok(false));
        assert_eq!(keepcaps_arg(1), Ok(true));
        assert_eq!(keepcaps_arg(2), Err(LxError::EINVAL));
        assert_eq!(keepcaps_arg(usize::MAX), Err(LxError::EINVAL));
    }
}

#[cfg(test)]
mod wait4_finish_tests {
    //! `wait4(2)`'s out-pointers when `WNOHANG` finds nothing.

    use super::*;
    use linux_object::process::ChildCpu;

    /// Nothing found is 0 and both out-pointers untouched: the status the
    /// caller kept there stays.
    #[test]
    fn nothing_found_leaves_the_status_and_the_rusage_alone() {
        let mut status: i32 = 0x1234;
        let mut rusage = RUsage::default();
        rusage.utime.sec = 9;
        let ws = UserOutPtr::from(&mut status as *mut i32 as usize);
        let ru = UserOutPtr::from(&mut rusage as *mut RUsage as usize);
        assert_eq!(wait4_finish(ws, ru, None), Ok(0));
        assert_eq!(status, 0x1234, "the status the caller kept");
        assert_eq!(rusage.utime.sec, 9);
        // Null pointers are fine either way.
        assert_eq!(
            wait4_finish(UserOutPtr::from(0), UserOutPtr::from(0), None),
            Ok(0)
        );
    }

    /// A child found is its pid, its status and its CPU time.
    #[test]
    fn a_child_found_is_its_pid_with_status_and_cpu_time_written() {
        let mut status: i32 = 0x1234;
        let mut rusage = RUsage::default();
        let ws = UserOutPtr::from(&mut status as *mut i32 as usize);
        let ru = UserOutPtr::from(&mut rusage as *mut RUsage as usize);
        let cpu = ChildCpu {
            utime_ns: 1_500_000_000,
            stime_ns: 250_000,
        };
        assert_eq!(wait4_finish(ws, ru, Some((4242, 7 << 8, cpu))), Ok(4242));
        assert_eq!(status, 7 << 8);
        assert_eq!((rusage.utime.sec, rusage.utime.usec), (1, 500_000));
        assert_eq!((rusage.stime.sec, rusage.stime.usec), (0, 250));
        // With null pointers the pid still comes back.
        let cpu = ChildCpu {
            utime_ns: 0,
            stime_ns: 0,
        };
        assert_eq!(
            wait4_finish(UserOutPtr::from(0), UserOutPtr::from(0), Some((5, 0, cpu))),
            Ok(5)
        );
    }
}

#[cfg(test)]
mod waitid_report_tests {
    //! The two out-pointers of `waitid(2)`. The fifth argument, `struct
    //! rusage *`, was never read by the dispatcher, so `waitid` could not
    //! report a child's CPU time; and `infop` was written only when a child
    //! was found, so a `WNOHANG` that found nothing handed the caller back
    //! whatever the previous call had left there.

    use super::*;
    use linux_object::process::ChildCpu;
    use linux_object::signal::Signal as LinuxSignal;

    fn word(info: &SigInfo, at: usize) -> i32 {
        let b = info.as_bytes();
        i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    /// `sys_waitid` in `kernel/exit.c` writes `si_signo`, `si_errno`,
    /// `si_code`, `si_pid`, `si_uid` and `si_status` whatever
    /// `kernel_waitid` found; when it found nothing they are all zero, and
    /// `si_pid == 0` is how `waitid(2)` says to tell.
    #[test]
    fn nothing_found_zeroes_infop_and_leaves_rusage_alone() {
        let report = waitid_report(None);
        assert!(report.info.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(word(&report.info, 16), 0, "si_pid");
        assert!(report.rusage.is_none());
    }

    /// A child found is a `SIGCHLD` `siginfo_t` with its pid where glibc
    /// reads `si_pid`, and its CPU time in the `rusage`.
    #[test]
    fn a_reported_child_carries_its_pid_and_its_cpu_time() {
        let cpu = ChildCpu {
            utime_ns: 1_500_000_000,
            stime_ns: 250_000,
        };
        let report = waitid_report(Some((4242, 1000, 7 << 8, cpu)));
        assert_eq!(report.info.signo, LinuxSignal::SIGCHLD as i32);
        assert_eq!(word(&report.info, 16), 4242, "si_pid");
        assert_eq!(word(&report.info, 20), 1000, "si_uid");
        assert_eq!(word(&report.info, 24), 7, "si_status");
        let rusage = report.rusage.expect("a child was reported");
        assert_eq!((rusage.utime.sec, rusage.utime.usec), (1, 500_000));
        assert_eq!((rusage.stime.sec, rusage.stime.usec), (0, 250));
        assert!(rusage.other.iter().all(|&f| f == 0), "the rest is zero");
    }

    /// The same struct `wait4` writes: `time(1)` over either call agrees.
    #[test]
    fn waitid_and_wait4_hand_out_the_same_rusage() {
        let cpu = ChildCpu {
            utime_ns: 3_000_000_000,
            stime_ns: 2_000_000_000,
        };
        let via_wait4 = child_rusage(cpu);
        let via_waitid = waitid_report(Some((1, 0, 0, cpu))).rusage.unwrap();
        assert_eq!(via_wait4.utime.sec, via_waitid.utime.sec);
        assert_eq!(via_wait4.stime.sec, via_waitid.stime.sec);
        assert_eq!(via_wait4.stime.sec, 2);
    }
}

#[cfg(test)]
mod extensible_struct_tests {
    //! The `size` a caller puts beside an extensible struct says which
    //! version of it they built. Three syscalls here take one, and until this
    //! batch none of them read it for anything but a lower bound -- so the
    //! fields past what this kernel knows were dropped in silence and the
    //! call reported success.

    use super::*;

    /// The two struct versions this kernel knows, and the fact that the
    /// `sched_attr` it defines *is* version 0 -- which is what makes
    /// "everything past `SCHED_ATTR_SIZE_VER0`" the right thing to demand be
    /// zero.
    #[test]
    fn the_two_version_zero_sizes_are_the_uapi_ones() {
        assert_eq!(CLONE_ARGS_SIZE_VER0, 64);
        assert_eq!(SCHED_ATTR_SIZE_VER0, 48);
        assert_eq!(size_of::<SchedAttr>(), SCHED_ATTR_SIZE_VER0);
        assert_eq!(CLONE_ARGS_SIZE_VER0 % 8, 0, "read back as eight u64");
    }

    /// A caller that sets a field this kernel has never heard of is asking
    /// for a feature it will not get.
    #[test]
    fn a_tail_this_kernel_does_not_know_must_be_all_zero() {
        assert!(extensible_tail_is_empty(&[]));
        assert!(extensible_tail_is_empty(&[0; 64]));
        // Wherever the byte is.
        assert!(!extensible_tail_is_empty(&[1]));
        assert!(!extensible_tail_is_empty(&[1, 0, 0, 0]));
        assert!(!extensible_tail_is_empty(&[0, 0, 0, 1]));
        let mut middle = [0u8; 32];
        middle[17] = 0x80;
        assert!(!extensible_tail_is_empty(&middle));
    }

    /// `clone3`: too small is EINVAL, too big is E2BIG. The two are different
    /// answers because they are different mistakes -- one is a struct that
    /// cannot be a `clone_args` at all, the other a size no struct has.
    #[test]
    fn clone3_bounds_its_struct_at_both_ends() {
        assert_eq!(clone3_size(0), Err(LxError::EINVAL));
        assert_eq!(clone3_size(CLONE_ARGS_SIZE_VER0 - 1), Err(LxError::EINVAL));
        assert_eq!(clone3_size(CLONE_ARGS_SIZE_VER0), Ok(CLONE_ARGS_SIZE_VER0));
        assert_eq!(
            clone3_size(88),
            Ok(88),
            "a newer version is read and vetted"
        );
        assert_eq!(clone3_size(PAGE_SIZE), Ok(PAGE_SIZE));
        assert_eq!(clone3_size(PAGE_SIZE + 1), Err(LxError::E2BIG));
        assert_eq!(clone3_size(usize::MAX), Err(LxError::E2BIG));
    }

    /// `sched_setattr`'s ABI compatibility quirk: a zero `size` means version
    /// 0, because the first users of the syscall shipped before the field was
    /// defined.
    #[test]
    fn a_sched_attr_with_no_size_at_all_is_version_zero() {
        assert_eq!(sched_setattr_size(0), Ok(SCHED_ATTR_SIZE_VER0));
        // ...and that is the only value below the minimum that is accepted.
        assert_eq!(sched_setattr_size(1), Err(LxError::E2BIG));
        assert_eq!(
            sched_setattr_size(SCHED_ATTR_SIZE_VER0 as u32 - 1),
            Err(LxError::E2BIG)
        );
    }

    /// The asymmetry, pinned: `sched_setattr` answers E2BIG and
    /// `sched_getattr` EINVAL for what looks like the same question. It is
    /// not the same question -- `setattr` reads the size out of the struct,
    /// where it describes what the caller built; `getattr` takes it as an
    /// argument, where it describes the buffer.
    #[test]
    fn the_two_halves_of_sched_attr_refuse_a_bad_size_differently() {
        assert_eq!(sched_setattr_size(4), Err(LxError::E2BIG));
        assert_eq!(sched_getattr_size(4), Err(LxError::EINVAL));
        assert_eq!(sched_setattr_size(u32::MAX), Err(LxError::E2BIG));
        assert_eq!(sched_getattr_size(usize::MAX), Err(LxError::EINVAL));
        // And the zero quirk is `setattr`'s alone: a getattr buffer of zero
        // bytes is a buffer of zero bytes.
        assert_eq!(sched_setattr_size(0), Ok(SCHED_ATTR_SIZE_VER0));
        assert_eq!(sched_getattr_size(0), Err(LxError::EINVAL));
    }

    /// Both accept everything from version 0 up to a page.
    #[test]
    fn a_sched_attr_may_be_anything_from_version_zero_up_to_a_page() {
        for size in [SCHED_ATTR_SIZE_VER0, 56, 64, PAGE_SIZE] {
            assert_eq!(sched_setattr_size(size as u32), Ok(size), "{size}");
            assert_eq!(sched_getattr_size(size), Ok(size), "{size}");
        }
        assert_eq!(
            sched_setattr_size(PAGE_SIZE as u32 + 1),
            Err(LxError::E2BIG)
        );
        assert_eq!(sched_getattr_size(PAGE_SIZE + 1), Err(LxError::EINVAL));
    }

    /// A page is the bound because that is what Linux uses, and because it is
    /// what keeps a caller from naming a copy the kernel then has to make.
    #[test]
    fn a_page_is_the_bound_for_every_one_of_them() {
        assert!(clone3_size(PAGE_SIZE).is_ok());
        assert!(sched_setattr_size(PAGE_SIZE as u32).is_ok());
        assert!(sched_getattr_size(PAGE_SIZE).is_ok());
        assert!(clone3_size(PAGE_SIZE + 1).is_err());
        assert!(sched_setattr_size(PAGE_SIZE as u32 + 1).is_err());
        assert!(sched_getattr_size(PAGE_SIZE + 1).is_err());
    }
}

#[cfg(test)]
mod sched_attr_flag_tests {
    //! `sched_setattr(2)`'s `sched_flags`, which were never read.

    use super::*;

    /// A flag Linux does not know is `EINVAL`; the known ones are not, alone
    /// or together.
    #[test]
    fn an_unknown_flag_is_einval() {
        assert_eq!(sched_setattr_plan(0x80, 0), Err(LxError::EINVAL));
        assert_eq!(sched_setattr_plan(1 << 63, 0), Err(LxError::EINVAL));
        assert_eq!(
            sched_setattr_plan(SCHED_FLAG_KEEP_POLICY | 0x100, 0),
            Err(LxError::EINVAL)
        );
        assert!(sched_setattr_plan(SCHED_FLAG_ALL, 0).is_ok());
        assert!(sched_setattr_plan(SCHED_FLAG_RESET_ON_FORK, 0).is_ok());
        // Their uapi values.
        assert_eq!(SCHED_FLAG_RESET_ON_FORK, 0x01);
        assert_eq!(SCHED_FLAG_KEEP_POLICY, 0x08);
        assert_eq!(SCHED_FLAG_KEEP_PARAMS, 0x10);
        assert_eq!(SCHED_FLAG_ALL, 0x7f);
    }

    /// `SCHED_FLAG_KEEP_POLICY` keeps the thread's policy whatever
    /// `sched_policy` says; without it `sched_policy` is the policy.
    #[test]
    fn keep_policy_ignores_the_policy_in_the_struct() {
        assert_eq!(
            sched_setattr_plan(SCHED_FLAG_KEEP_POLICY, 0),
            Ok(SchedAttrPlan {
                policy: None,
                keep_params: false
            })
        );
        assert_eq!(
            sched_setattr_plan(SCHED_FLAG_KEEP_POLICY, 1),
            Ok(SchedAttrPlan {
                policy: None,
                keep_params: false
            })
        );
        assert_eq!(
            sched_setattr_plan(0, 1),
            Ok(SchedAttrPlan {
                policy: Some(1),
                keep_params: false
            })
        );
        assert_eq!(
            sched_setattr_plan(SCHED_FLAG_KEEP_PARAMS, 2),
            Ok(SchedAttrPlan {
                policy: Some(2),
                keep_params: true
            })
        );
        assert_eq!(
            sched_setattr_plan(SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS, 2),
            Ok(SchedAttrPlan {
                policy: None,
                keep_params: true
            })
        );
    }

    /// `(int)attr.sched_policy < 0` is `EINVAL`, even under `KEEP_POLICY`,
    /// and before the flags are looked at.
    #[test]
    fn a_negative_policy_is_einval_before_the_flags() {
        assert_eq!(sched_setattr_plan(0, 0x8000_0000), Err(LxError::EINVAL));
        assert_eq!(sched_setattr_plan(0, u32::MAX), Err(LxError::EINVAL));
        assert_eq!(
            sched_setattr_plan(SCHED_FLAG_KEEP_POLICY, u32::MAX),
            Err(LxError::EINVAL)
        );
        assert!(
            sched_setattr_plan(0, 0x7fff_ffff).is_ok(),
            "the sign, not the range"
        );
    }
}

#[cfg(test)]
mod setpgid_argument_tests {
    //! The two signed arguments of `setpgid(2)` and the pid argument of
    //! `getpgid`/`getsid`. All three used to take a `usize`, so the sign a
    //! `pid_t` carries was gone before anything looked at it.

    use super::{resolve_pid_arg, setpgid_args};
    use linux_object::error::LxError;

    const ME: u64 = 100;

    #[test]
    fn a_zero_pid_means_the_caller_and_a_zero_pgid_a_group_of_its_own() {
        assert_eq!(setpgid_args(ME, 0, 0), Ok((ME, ME)));
        assert_eq!(resolve_pid_arg(ME, 0), Ok(ME));
    }

    #[test]
    fn a_zero_pgid_names_the_target_not_the_caller() {
        // `setpgid(child, 0)` makes the CHILD a group leader. Resolving the
        // zero against the caller would have filed the child under the
        // shell's own group instead -- the shell's Ctrl-C would then reach it
        // for as long as it lived.
        assert_eq!(setpgid_args(ME, 42, 0), Ok((42, 42)));
    }

    /// `if (pgid < 0) return -EINVAL`, and it is the FIRST thing `do_setpgid`
    /// does. Read as a `usize` the number arrived sign-extended, so this was
    /// `setpgid(0, 0xffff_ffff_ffff_ffff)` returning success: the caller left
    /// every real process group, and no signal sent to any group could reach
    /// it again.
    #[test]
    fn a_negative_group_is_refused_instead_of_becoming_a_huge_one() {
        assert_eq!(setpgid_args(ME, 0, -1), Err(LxError::EINVAL));
        assert_eq!(setpgid_args(ME, 0, i32::MIN), Err(LxError::EINVAL));
    }

    /// And it is refused before the pid is looked at, so a caller cannot tell
    /// a bad group from a missing process by which error comes back.
    #[test]
    fn the_group_is_judged_before_the_process_is_looked_up() {
        assert_eq!(setpgid_args(ME, -7, -1), Err(LxError::EINVAL));
        assert_eq!(setpgid_args(ME, -7, 0), Err(LxError::ESRCH));
    }

    /// A negative pid names no process: `find_task_by_vpid` can only fail.
    /// It used to name `0xffff_ffff_ffff_ffff`, which no process has either
    /// -- but `getpgid(-1)` is meant to say ESRCH, and `setpgid(-1, 0)` would
    /// have gone looking for a process to make a group leader.
    #[test]
    fn a_negative_pid_is_no_process() {
        assert_eq!(resolve_pid_arg(ME, -1), Err(LxError::ESRCH));
        assert_eq!(resolve_pid_arg(ME, i32::MIN), Err(LxError::ESRCH));
        assert_eq!(setpgid_args(ME, -1, 0), Err(LxError::ESRCH));
    }

    #[test]
    fn a_positive_pid_is_itself() {
        assert_eq!(resolve_pid_arg(ME, 1), Ok(1));
        assert_eq!(resolve_pid_arg(ME, i32::MAX), Ok(i32::MAX as u64));
    }
}

#[cfg(test)]
mod failed_exec_exit_tests {
    //! A process whose `execve` failed after its address space was gone
    //! finished with the literal `139`: `WIFEXITED` with status 139, the
    //! shell's number, instead of the death by SIGSEGV Linux gives it.

    use super::exit_code_after_failed_exec;
    use linux_object::process::wait_status_exited;

    #[test]
    fn a_failed_exec_is_a_death_by_sigsegv_not_an_exit_139() {
        let status = wait_status_exited(exit_code_after_failed_exec());
        let wifsignaled = (status & 0x7f) != 0 && (status & 0x7f) != 0x7f;
        assert!(wifsignaled, "status {:#x} says WIFEXITED", status);
        assert_eq!(status & 0x7f, 11, "WTERMSIG");
    }
}

#[cfg(test)]
mod nanosleep_rem_tests {
    //! `nanosleep(2)` took no `rem` at all, and its sleep could not be cut
    //! short by a signal: a signal that arrived in the middle was seen at
    //! the deadline, and a loop that restarts on `EINTR` with `rem`
    //! restarted from whatever was in the buffer.
    extern crate std;

    use super::*;
    use core::time::Duration;
    use linux_object::process::send_signal_to_process;
    use linux_object::signal::{Signal as LinuxSignal, SignalAction, SignalActionFlags, Sigset};
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::ROOT_JOB;

    // `libos` addresses are ordinary host addresses, so a local is the
    // caller's buffer.
    fn user_out<T>(slot: &mut T) -> UserOutPtr<T> {
        UserOutPtr::from(slot as *mut T as usize)
    }

    #[test]
    fn what_is_left_is_the_deadline_minus_now_and_never_negative() {
        let rem = nanosleep_remaining(Duration::from_millis(2_750), Duration::from_millis(1_000));
        assert_eq!((rem.sec, rem.nsec), (1, 750_000_000));
        let rem = nanosleep_remaining(Duration::from_secs(1), Duration::from_secs(5));
        assert_eq!((rem.sec, rem.nsec), (0, 0));
    }

    #[test]
    fn a_signal_ends_the_sleep_and_leaves_the_rest_in_rem() {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            43_501,
            "sleeper",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let thread = Thread::create_linux(&proc).unwrap();
        proc.linux().set_signal_action(
            LinuxSignal::SIGUSR1,
            SignalAction {
                handler: 0x1000,
                flags: SignalActionFlags::empty(),
                restorer: 0,
                mask: Sigset::default(),
            },
        );
        let pid = proc.id() as usize;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = send_signal_to_process(pid, LinuxSignal::SIGUSR1);
        });
        let mut slot = TimeSpec { sec: 99, nsec: 99 };
        let deadline = kernel_hal::timer::timer_now() + Duration::from_secs(3);
        let start = std::time::Instant::now();
        let r = async_std::task::block_on(sleep_or_eintr(&thread, deadline, user_out(&mut slot)));
        assert_eq!(r, Err(LxError::EINTR));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "slept past the signal"
        );
        assert_eq!(
            slot.sec, 2,
            "rem {:?} is not what was left of the 3 s",
            slot
        );
        assert!(
            slot.nsec > 500_000_000 && slot.nsec < 1_000_000_000,
            "rem {:?}",
            slot
        );
    }
}
