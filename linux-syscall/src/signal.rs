//! Syscalls of signal
//!
//! - rt_sigaction
//! - rt_sigreturn
//! - rt_sigprocmask
//! - rt_sigtimedwait
//! - kill
//! - tkill
//! - sigaltstack

use super::*;
use crate::outparams::commit_and_report_old;
use linux_object::error::LxResult;
use linux_object::process::{Credentials, LinuxProcess};
use linux_object::signal::{
    SigInfo, Signal, SignalAction, SignalCode, SignalStack, SignalStackFlags, Sigset,
};
use linux_object::thread::ThreadExt;
use linux_object::time::TimeSpec;
use numeric_enum_macro::numeric_enum;
use zircon_object::object::KernelObject;
use zircon_object::task::{Thread, ROOT_JOB};

/// The `sigsetsize` every syscall that takes a `sigset_t` from userspace
/// carries, and what it must be.
///
/// The word is the caller telling the kernel how wide its `sigset_t` is, and
/// the kernel refusing any other width is how the two stay in step: a libc
/// built against a different `_NSIG` does not get eight bytes read out of a
/// four-byte object, it gets `EINVAL`.
///
/// It was asked in six places in this file and in `install_temp_sigmask`, all
/// spelled out by hand -- and in `signalfd4`, the eighth caller, not at all.
/// One function, so the next one to arrive cannot forget.
pub(crate) fn check_sigsetsize(sigsetsize: usize) -> Result<(), LxError> {
    if sigsetsize != core::mem::size_of::<Sigset>() {
        return Err(LxError::EINVAL);
    }
    Ok(())
}

/// The arch-independent 12-byte prefix every `siginfo_t` layout starts with
/// (`signo`, `errno`, `code`). `rt_sigqueueinfo` reads only this much: the
/// permission rule is decided on `si_code` alone, and the union payload cannot
/// be carried by the bitmask pending set anyway. Read as plain integers — the
/// user-supplied code is unconstrained, so it must not be transmuted into the
/// kernel's `SignalCode` enum.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigInfoHead {
    /// Signal number as the sender filled it in (informational).
    pub signo: i32,
    /// `si_errno`, normally 0.
    pub errno: i32,
    /// `si_code`: must be < 0 (and not SI_TKILL) when targeting another
    /// process.
    pub code: i32,
}

/// `pid_t` is the `int` of the uAPI, so `SYSCALL_DEFINE` casts the register to
/// it before anything looks at the number: the high half is dropped and the
/// sign kept.
///
/// Taking all 64 bits instead changes what the call *means*. `kill(-1, sig)`
/// is the broadcast, and a caller that keeps its pid in a 32-bit slot puts
/// `0xffff_ffff` in the register: Linux reads `-1` and signals every process,
/// while a 64-bit read gets 4294967295 and answers `ESRCH` for a pid that
/// cannot exist. The same drop of the high half is already done for the signal
/// number (`Signal::from_syscall_arg`) and for the arguments of `poll` and
/// `select`.
fn pid_arg(raw: isize) -> i32 {
    // Truncating cast, on purpose: this is the `(int)` of the uAPI.
    raw as i32
}

/// The target `kill(2)` picks from the sign of its first argument.
#[derive(Debug, PartialEq, Eq)]
enum SendTarget {
    /// `kill(0, sig)`: the caller's own process group.
    EveryProcessInGroup,
    /// `kill(-1, sig)`: every process the caller may signal.
    EveryProcess,
    /// `kill(-pgid, sig)`: the group named by the leader's pid.
    EveryProcessInGroupByPID(KoID),
    /// `kill(pid, sig)`: one process.
    Pid(KoID),
}

/// Which of the four `kill(2)` targets a pid names.
///
/// Total by construction. The old `match` ended in `_ => unimplemented!()`,
/// and negated with `-p` on the full register: `kill(isize::MIN, sig)` had no
/// positive counterpart, so an unprivileged call overflowed the negation.
/// Narrowing to `pid_t` first is what makes `unsigned_abs` total here.
fn kill_target(pid: i32) -> SendTarget {
    match pid {
        0 => SendTarget::EveryProcessInGroup,
        -1 => SendTarget::EveryProcess,
        p if p > 0 => SendTarget::Pid(p as KoID),
        p => SendTarget::EveryProcessInGroupByPID(p.unsigned_abs() as KoID),
    }
}

/// `tkill(2)`/`tgkill(2)`: "This is only valid for single tasks", so Linux
/// refuses a non-positive id with `EINVAL` before it looks anything up.
///
/// Reading the id as a `usize` turned a negative one into an enormous KoID
/// that no lookup matches, so the answer was `ESRCH` — "that thread is gone"
/// — for a call that is simply malformed. A runtime that probes a tid it has
/// not stored yet reads the two apart.
fn single_task_id(raw: usize) -> Result<KoID, LxError> {
    match pid_arg(raw as isize) {
        id if id > 0 => Ok(id as KoID),
        _ => Err(LxError::EINVAL),
    }
}

/// What every target of one `kill(2)` call is judged against.
///
/// Read once, before the walk: `check_kill_permission()` asks about the
/// SENDER, and the sender does not change while the call runs.
pub(crate) struct KillContext {
    caller: Arc<zircon_object::task::Process>,
    credentials: Credentials,
    sid: KoID,
    signal: Option<Signal>,
}

/// What a `SIGKILL` that has found a live process actually does.
#[derive(Debug, PartialEq, Eq)]
enum KillOutcome {
    /// Ignored: `SIGKILL` sent to init from another process.
    IgnoredForInit,
    /// The caller is ending itself.
    Caller,
    /// Another process.
    Target,
}

/// The decision behind a direct `SIGKILL`, in one place because two syscalls
/// reach it.
///
/// `sys_kill` refuses to kill init from another process — a root shell's
/// `kill -9 1` removed the supervisor, after which nothing restarts services
/// or shuts the machine down. `rt_sigqueueinfo` called `Process::exit`
/// straight out and had no such guard, so `sigqueue(1, SIGKILL, v)` did
/// exactly what `kill -9 1` is refused. Self-kill is checked first, so init
/// can still end itself.
fn sigkill_outcome(caller_pid: KoID, target_pid: KoID) -> KillOutcome {
    if caller_pid == target_pid {
        KillOutcome::Caller
    } else if target_pid == linux_object::process::INIT_PID {
        KillOutcome::IgnoredForInit
    } else {
        KillOutcome::Target
    }
}

/// Deliver a `SIGKILL` that has already found its target, ending the process
/// directly rather than making the signal pending.
///
/// Both `kill(2)` and `rt_sigqueueinfo(2)` end up here; before, only the first
/// one had the init guard.
fn deliver_direct_sigkill(
    caller: &Arc<zircon_object::task::Process>,
    process: &Arc<zircon_object::task::Process>,
) {
    let retcode = (128 + Signal::SIGKILL as i32) as i64;
    match sigkill_outcome(caller.id(), process.id()) {
        KillOutcome::Caller => {
            // Same trace as send_signal_to_process: this path ends the target
            // directly, so it would otherwise leave no record of who sent the
            // SIGKILL.
            linux_object::process::trace_direct_kill(process, caller);
            caller.exit(retcode);
        }
        KillOutcome::Target => {
            linux_object::process::trace_direct_kill(process, caller);
            process.exit(retcode);
        }
        KillOutcome::IgnoredForInit => {
            warn!("SIGKILL for init (pid 1) from pid {} ignored", caller.id());
        }
    }
}

/// Make `signal` pending on one specific thread, which is what `tkill`,
/// `tgkill` and `rt_tgsigqueueinfo` all do once they have resolved one.
///
/// `lock_linux`, never `try_lock_linux`: a lock another CPU happens to hold
/// right now is not "no such thread". `rt_tgsigqueueinfo` answered `ESRCH`
/// there and threw the signal away — the same drop whose removal from
/// `send_signal_to_process` carries the comment "that drop is why lunarbar's
/// `kill 1` did nothing". Contention is likeliest exactly when the target is
/// busy, which is when a signal matters.
fn queue_signal_to_thread(thread: &Arc<Thread>, signal: Signal, info: SigInfo) {
    thread.lock_linux().queue_signal(signal, Some(info));
}

/// `si_code` of a signal `tkill`/`tgkill` sent, which userland may not forge.
const SI_TKILL: i32 = -6;

/// `rt_sigqueueinfo(2)`/`rt_tgsigqueueinfo(2)`/`pidfd_send_signal(2)`: userland
/// may not pretend a signal came from the kernel, nor impersonate a `tkill`, at
/// another process. Written once because all three make the same call:
///
/// ```c
/// if ((info->si_code >= 0 || info->si_code == SI_TKILL) &&
///     (task_pid_vnr(current) != pid))
///         return -EPERM;
/// ```
///
/// `target_is_self` is the one thing the three do NOT agree on, so each caller
/// answers it with its own ids. `rt_sigqueueinfo` and `pidfd_send_signal` name
/// a process, so "self" is the caller's own thread group;
/// `rt_tgsigqueueinfo` names ONE THREAD, and Linux compares
/// `task_pid_vnr(current)` -- the caller's own tid -- against it, so a sibling
/// thread of your own process is somebody else there. This kernel numbers
/// threads and processes out of one counter but in disjoint sets, so each call
/// site has to say which id it means; passing the wrong one reads as
/// harmless.
pub(crate) fn may_queue_siginfo(code: i32, target_is_self: bool) -> bool {
    target_is_self || (code < 0 && code != SI_TKILL)
}

/// `MINSIGSTKSZ` on the architectures this kernel runs.
const MIN_SIGSTACK_SIZE: usize = 2048;

/// The flags `sigaltstack(2)` accepts in `ss_flags`.
const VALID_SIGSTACK_FLAGS: SignalStackFlags = SignalStackFlags::from_bits_truncate(
    SignalStackFlags::AUTODISARM.bits() | SignalStackFlags::DISABLE.bits(),
);

/// Validate the stack `sigaltstack(2)` was handed, in the order Linux does it.
///
/// `do_sigaltstack` answers `EPERM` for a thread running *on* the alternate
/// stack before it reads a single field of the new one, then rejects unknown
/// flags, and only then — and only when the call is enabling a stack rather
/// than disabling one — compares the size. Checking the size first gave
/// `ENOMEM` to a call that is wrong twice over where Linux gives `EINVAL`,
/// and a thread on its own alternate stack learned that its size was too
/// small instead of that it may not ask at all.
fn check_sigaltstack(ss: SignalStack, on_alternate_stack: bool) -> Result<(), LxError> {
    if on_alternate_stack {
        return Err(LxError::EPERM);
    }
    if !VALID_SIGSTACK_FLAGS.contains(ss.flags) {
        return Err(LxError::EINVAL);
    }
    if !ss.flags.contains(SignalStackFlags::DISABLE) && ss.size < MIN_SIGSTACK_SIZE {
        return Err(LxError::ENOMEM);
    }
    Ok(())
}

impl Syscall<'_> {
    /// Used to change the action taken by a process on receipt of a specific signal.
    pub fn sys_rt_sigaction(
        &self,
        signum: usize,
        act: UserInPtr<SignalAction>,
        mut oldact: UserOutPtr<SignalAction>,
        sigsetsize: usize,
    ) -> SysResult {
        // sigaction(2) has no use for the probe: `do_sigaction()` rejects
        // signal 0 (`sig < 1`) alongside everything out of range.
        let signal = Signal::from_syscall_arg(signum)?.ok_or(LxError::EINVAL)?;
        info!(
            "rt_sigaction: signal={:?}, act={:?}, oldact={:?}, sigsetsize={}, thread={}",
            signal,
            act,
            oldact,
            sigsetsize,
            self.thread.id()
        );
        check_sigsetsize(sigsetsize)?;
        if signal == Signal::SIGKILL || signal == Signal::SIGSTOP {
            return Err(LxError::EINVAL);
        }
        let proc = self.linux_process();
        let old = proc.signal_action(signal);
        commit_and_report_old(old, &mut oldact, || {
            if let Some(act) = act.read_if_not_null()? {
                info!("new action: {:?} -> {:x?}", signal, act);
                // `SignalAction::stored` is the fifth door into "which signals
                // may be blocked", and the only one that was not shut: a
                // `sa_mask` becomes the thread's blocked set on every delivery
                // of this signal, so an unfiltered one holds off SIGKILL for as
                // long as the handler runs.
                proc.set_signal_action(signal, act.stored());
            }
            Ok(())
        })?;
        Ok(0)
    }

    /// Used to fetch and/or change the signal mask of the calling thread
    pub fn sys_rt_sigprocmask(
        &mut self,
        how: i32,
        set: UserInPtr<Sigset>,
        mut oldset: UserOutPtr<Sigset>,
        sigsetsize: usize,
    ) -> SysResult {
        numeric_enum! {
            #[repr(i32)]
            #[derive(Debug)]
            enum How {
                Block = 0,
                Unblock = 1,
                SetMask = 2,
            }
        }
        info!(
            "rt_sigprocmask: how={}, set={:?}, oldset={:?}, sigsetsize={}, thread={}",
            how,
            set,
            oldset,
            sigsetsize,
            self.thread.id()
        );
        check_sigsetsize(sigsetsize)?;
        let old = self.thread.lock_linux().signal_mask();
        commit_and_report_old(old, &mut oldset, || {
            if set.is_null() {
                return Ok(());
            }
            let set = set.read()?;
            // `how` is validated INSIDE `sigprocmask()`, which Linux calls
            // only `if (nset)`. So `rt_sigprocmask(<rubbish>, NULL, &old, 8)`
            // -- how a program reads its own mask without touching it --
            // succeeds there, and rejecting it here was a guard harder than
            // the kernel's: one that refuses programs the kernel accepts.
            let how = How::try_from(how).map_err(|_| LxError::EINVAL)?;
            let mut thread = self.thread.lock_linux();
            // SIGKILL and SIGSTOP can never be blocked; the three helpers
            // below all drop them, and `sigprocmask(2)` is explicit that the
            // attempt is ignored rather than refused.
            match how {
                How::Block => thread.block_signals(&set),
                How::Unblock => thread.unblock_signals(&set),
                How::SetMask => thread.set_signal_mask(set),
            }
            Ok(())
        })?;
        Ok(0)
    }

    /// Allows a process to define a new alternate signal stack
    /// and/or retrieve the state of an existing alternate signal stack
    pub fn sys_sigaltstack(
        &self,
        ss: UserInPtr<SignalStack>,
        mut old_ss: UserOutPtr<SignalStack>,
    ) -> SysResult {
        info!("sigaltstack: ss={:?}, old_ss={:?}", ss, old_ss);
        // `SS_ONSTACK` and `SS_DISABLE` come from the stack pointer the caller
        // is using right now, not from anything stored -- see
        // `SignalStack::as_reported_from`. Without this the flag was never set
        // by anyone, so `sigaltstack(NULL, &old)` always answered "no stack
        // installed" and the EPERM below could never fire.
        let sp = self
            .thread
            .with_context(|ctx| ctx.get_field(kernel_hal::context::UserContextField::StackPointer))
            .unwrap_or(0);
        let old = self
            .thread
            .lock_linux()
            .signal_alternate_stack
            .as_reported_from(sp);
        commit_and_report_old(old, &mut old_ss, || {
            if ss.is_null() {
                return Ok(());
            }
            let mut ss = ss.read()?;
            check_sigaltstack(ss, old.flags.contains(SignalStackFlags::ONSTACK))?;
            // `SS_DISABLE` forgets the stack, it does not merely park it:
            // Linux zeroes `ss_sp`/`ss_size` here, so the next
            // `sigaltstack(NULL, &old)` reports nothing installed rather
            // than an address the program may already have freed.
            if ss.flags.contains(SignalStackFlags::DISABLE) {
                ss.sp = 0;
                ss.size = 0;
            }
            self.thread.lock_linux().signal_alternate_stack = ss;
            Ok(())
        })?;
        Ok(0)
    }

    /// The `siginfo_t` of a signal this thread's process is sending:
    /// its pid and real uid, and how (`SI_TKILL`, `SI_QUEUE`).
    fn sent_by_me(&self, signal: Signal, code: SignalCode) -> SigInfo {
        SigInfo::from_user(
            signal,
            self.zircon_process().id() as i32,
            self.linux_process().credentials().ruid,
            code,
        )
    }

    pub(crate) fn kill_context(&self, signal: Option<Signal>) -> KillContext {
        let caller = self.zircon_process().clone();
        let sid = linux_object::process::effective_sid(&caller);
        KillContext {
            credentials: self.linux_process().credentials(),
            caller,
            sid,
            signal,
        }
    }

    /// `check_kill_permission()` for one target process, with no delivery.
    pub(crate) fn may_signal_process(
        &self,
        cx: &KillContext,
        process: &Arc<zircon_object::task::Process>,
    ) -> LxResult<()> {
        let Some(linux) = process.try_linux() else {
            // A process whose Linux extension has already gone has no
            // credentials left to judge. `effective_pgid` tolerates the same
            // teardown race; refusing here would turn it into an EPERM.
            return Ok(());
        };
        LinuxProcess::may_signal(
            &cx.credentials,
            &linux.credentials(),
            process.id() == cx.caller.id(),
            linux_object::process::effective_sid(process) == cx.sid,
            cx.signal,
        )
    }

    /// The gate, then the delivery, for one process.
    pub(crate) fn signal_one_process(
        &self,
        cx: &KillContext,
        process: &Arc<zircon_object::task::Process>,
    ) -> LxResult<()> {
        self.may_signal_process(cx, process)?;
        match cx.signal {
            // kill(pid, 0): finding the process was the whole answer.
            None => Ok(()),
            Some(Signal::SIGKILL) => {
                deliver_direct_sigkill(&cx.caller, process);
                Ok(())
            }
            // `si_pid`/`si_uid` are the caller's: `kill(2)` leaves its pid
            // and REAL uid for the handler.
            Some(sig) => linux_object::process::send_signal_to_process_with_info(
                process.id() as usize,
                sig,
                Some(SigInfo::from_user(
                    sig,
                    cx.caller.id() as i32,
                    cx.credentials.ruid,
                    SignalCode::USER,
                )),
            ),
        }
    }

    /// `kill(pid, sig)` with a positive pid.
    fn signal_pid(&self, cx: &KillContext, pid: KoID) -> LxResult<()> {
        let Some(process) = ROOT_JOB.find_process(pid) else {
            // A child that exited but was not waited for is gone from the
            // job yet still a valid target: Linux delivers nothing and
            // returns 0. Firefox's parent logged "failed to send SIGKILL
            // to process N" for every content process it had already
            // seen die, and its profile lock's `kill(pid, 0)` probe
            // treats ESRCH as "stale lock" but any other answer as
            // "still running".
            if self.linux_process().is_zombie_child(pid) {
                return Ok(());
            }
            return Err(LxError::ESRCH);
        };
        self.signal_one_process(cx, &process)
    }

    /// `__kill_pgrp_info()`: `kill(0, sig)` and `kill(-pgid, sig)` reach
    /// EVERY process of the group, not its leader.
    ///
    /// The group is the one `send_signal_to_pgrp` walks for Ctrl-C, by the
    /// same `effective_pgid`. The walk is written out again here rather than
    /// shared because a signal sent by a process has a sender to judge and
    /// one sent by a terminal has none.
    fn signal_group(&self, cx: &KillContext, pgid: KoID) -> LxResult<()> {
        let mut result: LxResult<()> = Err(LxError::ESRCH);
        for process in linux_object::process::all_live_processes() {
            if linux_object::process::effective_pgid(&process) != pgid {
                continue;
            }
            let one = self.signal_one_process(cx, &process);
            result = LinuxProcess::fold_group_signal(result, one);
        }
        result
    }

    /// `kill(-1, sig)` reaches every process EXCEPT the caller and init
    /// (PID 1), as on Linux. The old code included the caller: a previous
    /// eclipse-init shutdown() did `kill(-1, SIGTERM)`, a grace sleep,
    /// `kill(-1, SIGKILL)` and only then reboot(2), so PID 1 killed itself
    /// and never reached reboot(2). Init no longer broadcasts; it
    /// force-reboots like busybox `reboot -f`.
    fn signal_broadcast(&self, cx: &KillContext) -> LxResult<()> {
        let mut counted = false;
        let mut result: LxResult<()> = Ok(());
        for process in linux_object::process::all_live_processes() {
            if process.id() == cx.caller.id() || process.id() == linux_object::process::INIT_PID {
                continue;
            }
            counted = true;
            let one = self.signal_one_process(cx, &process);
            result = LinuxProcess::fold_broadcast_signal(result, one);
        }
        if counted {
            result
        } else {
            Err(LxError::ESRCH)
        }
    }

    /// Send a signal to a process specified by pid
    pub fn sys_kill(&self, pid: isize, signum: usize) -> SysResult {
        // `None` is signal 0: deliver nothing, but still report whether the
        // target exists. (An invalid number is EINVAL here, before the target
        // is resolved; Linux resolves first and answers ESRCH. Only a call
        // that is wrong twice over can tell the difference.)
        let signal = Signal::from_syscall_arg(signum)?;
        info!(
            "kill: thread {} kill process {} with signal {:?}",
            self.thread.id(),
            pid,
            signal
        );
        let cx = self.kill_context(signal);
        match kill_target(pid_arg(pid)) {
            SendTarget::Pid(pid) => self.signal_pid(&cx, pid),
            SendTarget::EveryProcessInGroup => {
                self.signal_group(&cx, linux_object::process::effective_pgid(&cx.caller))
            }
            SendTarget::EveryProcessInGroupByPID(pgid) => self.signal_group(&cx, pgid),
            SendTarget::EveryProcess => self.signal_broadcast(&cx),
        }
        .map(|_| 0)
    }

    /// Send a signal to a thread specified by tid
    pub fn sys_tkill(&mut self, tid: usize, signum: usize) -> SysResult {
        let tid = single_task_id(tid)?;
        let signal = Signal::from_syscall_arg(signum)?;
        info!(
            "tkill: thread {} kill thread {} with signal {:?}",
            self.thread.id(),
            tid,
            signum
        );
        let parent = self.zircon_process().clone();
        match parent.get_child(tid) {
            Ok(obj) => {
                let thread: Arc<Thread> = match obj.downcast_arc() {
                    Ok(t) => t,
                    Err(_) => return Err(LxError::ESRCH),
                };
                // tkill(tid, 0) probes the thread and delivers nothing.
                if let Some(signal) = signal {
                    queue_signal_to_thread(
                        &thread,
                        signal,
                        self.sent_by_me(signal, SignalCode::TKILL),
                    );
                }
                Ok(0)
            }
            Err(_) => Err(LxError::ESRCH),
        }
    }

    /// Send a signal to a thread specified by tgid (i.e., process) and pid
    /// Note: the job of the target process should be the same as the calling thread
    ///
    /// This one reaches into ANOTHER process, so `check_kill_permission()`
    /// applies exactly as it does to `kill(2)`. (`tkill(2)` next door does
    /// not: it only ever looks inside the caller's own process, which is
    /// narrower than Linux and needs no gate of its own.)
    pub fn sys_tgkill(&mut self, tgid: usize, tid: usize, signum: usize) -> SysResult {
        let tgid = single_task_id(tgid)?;
        let tid = single_task_id(tid)?;
        let signal = Signal::from_syscall_arg(signum)?;
        info!(
            "tgkill: thread {} kill thread {} in process {} with signal {:?}",
            self.thread.id(),
            tid,
            tgid,
            signum
        );
        let parent = self.zircon_process().clone();
        let Ok(process) = parent
            .job()
            .get_child(tgid)
            .map_err(|_| LxError::ESRCH)
            .and_then(|obj| {
                obj.downcast_arc::<zircon_object::task::Process>()
                    .map_err(|_| LxError::ESRCH)
            })
        else {
            return Err(LxError::ESRCH);
        };
        let Ok(obj) = process.get_child(tid) else {
            return Err(LxError::ESRCH);
        };
        let thread: Arc<Thread> = match obj.downcast_arc() {
            Ok(t) => t,
            Err(_) => return Err(LxError::ESRCH),
        };
        self.may_signal_process(&self.kill_context(signal), &process)?;
        // tgkill(tgid, tid, 0) probes the thread and delivers nothing.
        if let Some(signal) = signal {
            queue_signal_to_thread(&thread, signal, self.sent_by_me(signal, SignalCode::TKILL));
        }
        Ok(0)
    }

    /// Return from handling some signal
    pub fn sys_rt_sigreturn(&mut self) -> SysResult {
        info!(
            "sigreturn: thread {} returns from handling the signal",
            self.thread.id()
        );
        let (old_ctx, siginfo_ptr, uctx_ptr) = match self.thread.fetch_backup_context() {
            Some(v) => v,
            None => return Err(LxError::EINVAL),
        };
        self.thread
            .with_context(|ctx| {
                self.thread.lock_linux().restore_after_handle_signal(
                    ctx,
                    &old_ctx,
                    siginfo_ptr,
                    uctx_ptr,
                )
            })
            .map_err(|_| LxError::EINVAL)?;
        // sigreturn has no return value of its own: the generic syscall-return
        // path writes our result into rax AFTER the context restore above, so
        // returning a fixed 0 would clobber the interrupted syscall's restored
        // result. Observed: a SIGWINCH handler returning into a just-completed
        // read(2) turned its rax=1 into 0 — busybox lineedit took the 0 as
        // EOF and the shell exited the moment foot resized its window. Return
        // the restored rax so the generic write-back is a no-op.
        let rax = self
            .thread
            .with_context(|ctx| ctx.get_field(kernel_hal::context::UserContextField::ReturnValue))
            .map_err(|_| LxError::EINVAL)?;
        Ok(rax)
    }

    /// Temporarily replace the signal mask of the calling thread with `mask`
    /// and suspend the thread until a signal is delivered whose action is to
    /// invoke a handler or to terminate the process.
    ///
    /// Always returns `-EINTR` once a signal is delivered. The original mask is
    /// restored after the handler returns, via the saved-mask mechanism in
    /// `LinuxThread` (see `handle_signal`).
    pub async fn sys_rt_sigsuspend(
        &mut self,
        mask: UserInPtr<Sigset>,
        sigsetsize: usize,
    ) -> SysResult {
        check_sigsetsize(sigsetsize)?;
        let newmask = mask.read()?;
        info!(
            "rt_sigsuspend: mask={:#x}, thread={}",
            newmask.val(),
            self.thread.id()
        );
        // Install the temporary mask and remember the previous one so it is
        // restored once the awakening signal handler returns.
        {
            let mut thread = self.thread.lock_linux();
            let old_mask = thread.signal_mask();
            // Drops SIGKILL and SIGSTOP, which can never be blocked.
            thread.set_signal_mask(newmask);
            thread.saved_sigmask = Some(old_mask);
        }
        // Block until a signal becomes deliverable under the temporary mask
        // (or the thread/process is being torn down). `check_signals` reports
        // this as `EINTR`, which is exactly the return value `sigsuspend` owes
        // its caller.
        loop {
            linux_object::process::check_signals()?;
            let deadline = kernel_hal::timer::deadline_after(core::time::Duration::from_millis(10));
            kernel_hal::thread::sleep_until(deadline).await;
        }
    }

    /// Suspend the calling thread until a signal is delivered that either
    /// terminates the process or causes a signal handler to be invoked.
    ///
    /// Always returns `-EINTR`.
    pub async fn sys_pause(&mut self) -> SysResult {
        info!("pause: thread {}", self.thread.id());
        loop {
            linux_object::process::check_signals()?;
            let deadline = kernel_hal::timer::deadline_after(core::time::Duration::from_millis(10));
            kernel_hal::thread::sleep_until(deadline).await;
        }
    }

    /// Examine the set of signals that are pending for delivery to the calling
    /// thread — raised while blocked and not yet delivered (see sigpending(2)).
    pub fn sys_rt_sigpending(&self, mut set: UserOutPtr<Sigset>, sigsetsize: usize) -> SysResult {
        check_sigsetsize(sigsetsize)?;
        let thread = self.thread.lock_linux();
        // Pending here means "sent but withheld by the mask": what is both in
        // the undelivered set and currently blocked. Unblocked entries are on
        // their way to delivery and are not reported, matching Linux.
        let pending = Sigset::new(thread.signals.val() & thread.signal_mask().val());
        drop(thread);
        info!("rt_sigpending: pending={:#x}", pending.val());
        set.write(pending)?;
        Ok(0)
    }

    /// Queue a signal plus caller-filled `siginfo` to a process
    /// (see rt_sigqueueinfo(2); the libc wrapper is `sigqueue(3)`).
    ///
    /// The pending set is a bitmask (no per-signal siginfo queue), so the
    /// accompanying value payload is not preserved — but the signal itself is
    /// delivered with full disposition handling. The previous behaviour
    /// returned success while dropping the signal entirely.
    pub fn sys_rt_sigqueueinfo(
        &self,
        pid: usize,
        signum: usize,
        info: UserInPtr<SigInfoHead>,
    ) -> SysResult {
        let head = info.read()?;
        info!(
            "rt_sigqueueinfo: pid={}, sig={}, si_code={}",
            pid, signum, head.code
        );
        let pid = pid_arg(pid as isize);
        if !may_queue_siginfo(head.code, pid as i64 == self.zircon_process().id() as i64) {
            return Err(LxError::EPERM);
        }
        let process = ROOT_JOB.find_process(pid as KoID).ok_or(LxError::ESRCH)?;
        // `kill_proc_info()`, the same one `kill(2)` walks into: the
        // credentials, then the delivery -- including the refusal to end init
        // from another process, which this syscall used to bypass by calling
        // `Process::exit` straight out. The signal number is parsed after the
        // target is found so that `ESRCH` keeps winning over `EINVAL`, as it
        // did before.
        let signal = Signal::from_syscall_arg(signum)?;
        self.signal_one_process(&self.kill_context(signal), &process)
            .map(|_| 0)
    }

    /// Queue a signal plus `siginfo` to a specific thread of a thread group
    /// (see rt_tgsigqueueinfo(2); backs `pthread_sigqueue(3)`).
    pub fn sys_rt_tgsigqueueinfo(
        &self,
        tgid: usize,
        tid: usize,
        signum: usize,
        info: UserInPtr<SigInfoHead>,
    ) -> SysResult {
        let head = info.read()?;
        info!(
            "rt_tgsigqueueinfo: tgid={}, tid={}, sig={}, si_code={}",
            tgid, tid, signum, head.code
        );
        let tgid = single_task_id(tgid)?;
        let tid = single_task_id(tid)?;
        // The target THREAD, not its thread group: forging an `si_code` at a
        // sibling thread is forging it at somebody else, and asking about the
        // group let that through.
        if !may_queue_siginfo(head.code, tid == self.thread.id()) {
            return Err(LxError::EPERM);
        }
        let process = ROOT_JOB.find_process(tgid).ok_or(LxError::ESRCH)?;
        let thread_obj = process.get_child(tid).map_err(|_| LxError::ESRCH)?;
        let thread: Arc<Thread> = thread_obj.downcast_arc().map_err(|_| LxError::ESRCH)?;
        let signal = Signal::from_syscall_arg(signum)?;
        // `do_send_specific()`, which is `tgkill`'s own path: the credentials
        // of the thread GROUP, since a thread has none of its own.
        self.may_signal_process(&self.kill_context(signal), &process)?;
        // rt_tgsigqueueinfo(tgid, tid, 0) probes and delivers nothing -- but
        // it is refused just the same, so it cannot report the existence of a
        // thread you may not signal.
        if let Some(signal) = signal {
            queue_signal_to_thread(&thread, signal, self.sent_by_me(signal, SignalCode::QUEUE));
        }
        Ok(0)
    }

    /// Synchronously wait for one of the signals in `set` to become pending,
    /// dequeue it, and return its number — *without* invoking its handler.
    ///
    /// The caller is expected (POSIX) to have blocked the signals in `set`; a
    /// blocked signal is still queued to the thread's pending set when sent, and
    /// this call is what consumes it. Returns the signal number on success,
    /// `-EAGAIN` if `timeout` elapses with nothing delivered, and `-EINTR` if an
    /// unblocked signal *outside* `set` interrupts the wait.
    ///
    /// busybox `init` (PID 1) parks here between reaping children; while this
    /// was unimplemented it spun, flooding the log with
    /// `unknown syscall: RT_SIGTIMEDWAIT`.
    pub async fn sys_rt_sigtimedwait(
        &mut self,
        set: UserInPtr<Sigset>,
        mut info: UserOutPtr<SigInfo>,
        timeout: UserInPtr<TimeSpec>,
        sigsetsize: usize,
    ) -> SysResult {
        check_sigsetsize(sigsetsize)?;
        let mut waitset = set.read()?;
        // SIGKILL/SIGSTOP can never be caught or waited for.
        waitset.remove(Signal::SIGKILL);
        waitset.remove(Signal::SIGSTOP);
        // A null `timeout` means wait indefinitely; otherwise resolve the
        // monotonic deadline once, up front.
        let deadline = if timeout.is_null() {
            None
        } else {
            let dur = timeout.read()?.try_into_duration()?;
            // Saturating: adding a `Duration` panics on overflow, and this
            // one comes from userspace.
            Some(kernel_hal::timer::timer_now().saturating_add(dur))
        };
        info!(
            "rt_sigtimedwait: set={:#x}, timeout={:?}, thread={}",
            waitset.val(),
            deadline,
            self.thread.id()
        );
        loop {
            // A waited-for signal already pending? Dequeue and return it. We do
            // not have per-signal queued `siginfo` (pending signals are a plain
            // bitmask), so only `si_signo` is reported — enough for callers like
            // busybox init that follow up with `waitpid`.
            {
                let mut thread = self.thread.lock_linux();
                let ready = Sigset::new(thread.signals.val() & waitset.val());
                if let Some(sig) = ready.find_first_signal() {
                    // With what came with it: who sent it, which child it is
                    // about. `sigwaitinfo(3)` is how a program asks for that.
                    let si = thread.take_siginfo(sig);
                    drop(thread);
                    if !info.is_null() {
                        info.write(si)?;
                    }
                    return Ok(sig as usize);
                }
            }
            // An unblocked signal *outside* `set` interrupts the wait (EINTR).
            linux_object::process::check_signals()?;
            // Timed out with nothing delivered.
            if let Some(end) = deadline {
                if kernel_hal::timer::timer_now() >= end {
                    return Err(LxError::EAGAIN);
                }
            }
            let next = kernel_hal::timer::deadline_after(core::time::Duration::from_millis(10));
            kernel_hal::thread::sleep_until(next).await;
        }
    }

    /// Install a temporary blocked-signal mask for wait syscalls that take a
    /// sigmask (`ppoll`, `pselect6`, `epoll_pwait`).
    ///
    /// Returns `None` when `sigmask` is null (no change). On a normal return the
    /// guard restores the previous mask; on `EINTR` the caller must call
    /// [`TempSigmaskGuard::keep_for_signal`] so the temporary mask stays in
    /// effect until `handle_signal` claims `saved_sigmask` for the frame
    /// (same contract as `rt_sigsuspend`).
    pub(crate) fn install_temp_sigmask(
        &self,
        sigmask: UserInPtr<Sigset>,
        sigsetsize: usize,
    ) -> Result<Option<TempSigmaskGuard>, LxError> {
        if sigmask.is_null() {
            return Ok(None);
        }
        check_sigsetsize(sigsetsize)?;
        let newmask = sigmask.read()?;
        let thread = alloc::sync::Arc::clone(self.thread);
        let old = {
            let mut lt = thread.lock_linux();
            let old = lt.signal_mask();
            // Drops SIGKILL and SIGSTOP, which can never be blocked.
            lt.set_signal_mask(newmask);
            lt.saved_sigmask = Some(old);
            old
        };
        Ok(Some(TempSigmaskGuard {
            thread,
            old,
            restore: true,
        }))
    }
}

/// RAII restore for [`Syscall::install_temp_sigmask`].
///
/// Holds an owned `Arc<Thread>` so it does not borrow `Syscall` across the
/// subsequent `&mut self` wait call.
pub(crate) struct TempSigmaskGuard {
    thread: alloc::sync::Arc<Thread>,
    old: Sigset,
    restore: bool,
}

impl TempSigmaskGuard {
    /// Leave the temporary mask in place after `EINTR` so a pending signal can
    /// still be delivered under it; `saved_sigmask` then restores `old` via
    /// `sigreturn`.
    pub(crate) fn keep_for_signal(&mut self) {
        self.restore = false;
    }
}

impl Drop for TempSigmaskGuard {
    fn drop(&mut self) {
        if !self.restore {
            return;
        }
        let mut thread = self.thread.lock_linux();
        // If a signal already claimed `saved_sigmask` for its frame, leave the
        // mask alone — `sigreturn` will reinstate `old`.
        if thread.saved_sigmask.is_some() {
            thread.set_signal_mask(self.old);
            thread.saved_sigmask = None;
        }
    }
}

/// The signal syscalls had no tests at all, and they are the path by which one
/// program interrupts, stops or ends another: a shell's Ctrl-C, a compositor's
/// signalfd, a runtime's `pthread_kill`, an init's shutdown.
///
/// Everything below is a pure decision lifted out of a syscall body, so it can
/// be exercised without a thread, a process or a job.
#[cfg(test)]
mod signal_tests {
    use super::*;

    /// `pid_t` in a register, the way a caller leaves it there.
    fn reg(value: i32) -> usize {
        value as u32 as usize
    }

    // ---- pid_t is an `int` -------------------------------------------------

    #[test]
    fn the_high_half_of_the_register_is_dropped_the_way_linux_drops_it() {
        // `SYSCALL_DEFINE2(kill, pid_t, pid, int, sig)` casts the register to
        // `int` before anything looks at it, so rubbish in the high half
        // cannot change which process is named.
        assert_eq!(pid_arg(0xdead_beef_0000_0001u64 as isize), 1);
        assert_eq!(pid_arg(0x0000_0001_0000_002au64 as isize), 42);
    }

    // ---- how wide the caller says its sigset_t is --------------------------

    /// The width every syscall that takes a `sigset_t` demands. It is the one
    /// thing that keeps a libc built against a different `_NSIG` from having
    /// eight bytes read out of a four-byte object -- and `signalfd4`, the
    /// eighth caller, was not asking: it named the argument `_sizemask` and
    /// read eight bytes whatever the caller said.
    #[test]
    fn the_only_sigset_width_this_kernel_takes_is_its_own() {
        assert_eq!(check_sigsetsize(8), Ok(()));
        assert_eq!(core::mem::size_of::<Sigset>(), 8);
    }

    #[test]
    fn any_other_width_is_einval() {
        for n in [0usize, 1, 4, 7, 9, 16, 128, usize::MAX] {
            assert_eq!(
                check_sigsetsize(n),
                Err(LxError::EINVAL),
                "sigsetsize={}",
                n
            );
        }
    }

    #[test]
    fn a_broadcast_that_arrived_in_a_32_bit_slot_is_still_a_broadcast() {
        // A caller that keeps its pid in 32 bits puts 0xffff_ffff in the
        // register for `kill(-1, sig)`. Read as 64 bits that is 4294967295, a
        // pid that cannot exist, so the broadcast became an `ESRCH` — and a
        // shutdown that signals every process signalled none.
        assert_eq!(pid_arg(0xffff_ffffu32 as isize), -1);
        assert_eq!(
            kill_target(pid_arg(0xffff_ffffu32 as isize)),
            SendTarget::EveryProcess
        );
    }

    #[test]
    fn a_process_group_sent_to_in_a_32_bit_slot_is_still_that_group() {
        // `kill(-pgid, sig)` is how a shell signals a job.
        assert_eq!(
            kill_target(pid_arg(reg(-42) as isize)),
            SendTarget::EveryProcessInGroupByPID(42)
        );
    }

    // ---- which of the four targets a pid names -----------------------------

    #[test]
    fn a_positive_pid_names_one_process() {
        assert_eq!(kill_target(1), SendTarget::Pid(1));
        assert_eq!(kill_target(4321), SendTarget::Pid(4321));
        assert_eq!(kill_target(i32::MAX), SendTarget::Pid(i32::MAX as KoID));
    }

    #[test]
    fn zero_is_the_callers_own_group_and_minus_one_is_everyone() {
        assert_eq!(kill_target(0), SendTarget::EveryProcessInGroup);
        assert_eq!(kill_target(-1), SendTarget::EveryProcess);
    }

    #[test]
    fn anything_below_minus_one_is_the_group_it_names() {
        assert_eq!(kill_target(-2), SendTarget::EveryProcessInGroupByPID(2));
        assert_eq!(
            kill_target(-4321),
            SendTarget::EveryProcessInGroupByPID(4321)
        );
    }

    #[test]
    fn the_most_negative_pid_has_no_positive_counterpart() {
        // `-p` on `i32::MIN` overflows. The old code negated the full register
        // instead, so `kill(isize::MIN, SIGTERM)` — twelve bytes of syscall
        // from any process — overflowed the negation. `unsigned_abs` is what
        // makes this total, and narrowing to `pid_t` first is what makes
        // `unsigned_abs` enough.
        assert_eq!(
            kill_target(i32::MIN),
            SendTarget::EveryProcessInGroupByPID(2_147_483_648)
        );
    }

    #[test]
    fn every_pid_names_some_target() {
        // No arm of `kill(2)` may be unreachable: the old `match` ended in
        // `_ => unimplemented!()`, which is a panic wearing a different word.
        for pid in [i32::MIN, i32::MIN + 1, -2, -1, 0, 1, 2, i32::MAX] {
            let _ = kill_target(pid);
        }
    }

    // ---- tkill/tgkill take a single task -----------------------------------

    #[test]
    fn a_thread_id_of_zero_is_a_malformed_call_not_a_missing_thread() {
        // tkill(2): "This is only valid for single tasks", and Linux answers
        // EINVAL before it looks anything up. A zero id used to reach the
        // lookup and come back ESRCH, which says the thread died.
        assert_eq!(single_task_id(0), Err(LxError::EINVAL));
    }

    #[test]
    fn a_negative_thread_id_is_refused_rather_than_looked_up() {
        for tid in [-1, -2, -4321, i32::MIN] {
            assert_eq!(
                single_task_id(reg(tid)),
                Err(LxError::EINVAL),
                "tkill({}, ..) must be EINVAL",
                tid
            );
        }
    }

    #[test]
    fn a_thread_id_survives_the_high_half_of_the_register() {
        assert_eq!(single_task_id(0xffff_ffff_0000_002a), Ok(42));
        assert_eq!(single_task_id(0x0000_0001_0000_0001), Ok(1));
    }

    #[test]
    fn a_positive_thread_id_passes_through_unchanged() {
        assert_eq!(single_task_id(1), Ok(1));
        assert_eq!(single_task_id(42), Ok(42));
        assert_eq!(single_task_id(i32::MAX as usize), Ok(i32::MAX as KoID));
    }

    #[test]
    fn the_first_id_that_does_not_fit_in_an_int_is_refused() {
        // 0x8000_0000 is `i32::MIN` once the register is read as the `int` it
        // is, so it is not a huge valid tid: it is a negative one.
        assert_eq!(single_task_id(0x8000_0000), Err(LxError::EINVAL));
    }

    // ---- the init guard, which only one of two syscalls had ----------------

    #[test]
    fn init_is_pid_one() {
        // Asserted against the literal, not against itself: the number is the
        // ABI's, and a test built from the constant moves with it.
        assert_eq!(linux_object::process::INIT_PID, 1);
    }

    #[test]
    fn init_cannot_be_killed_from_another_process() {
        // `kill -9 1` from a root shell removed the supervisor, after which
        // nothing restarts services and nothing shuts the machine down.
        assert_eq!(sigkill_outcome(1234, 1), KillOutcome::IgnoredForInit);
    }

    #[test]
    fn init_can_still_end_itself() {
        // The self-kill arm is checked first, so the guard protects init from
        // everyone else without trapping it inside its own process.
        assert_eq!(sigkill_outcome(1, 1), KillOutcome::Caller);
    }

    #[test]
    fn a_process_that_kills_itself_ends_the_caller() {
        // Not the same as killing a stranger: the current process has to come
        // down through the calling thread.
        assert_eq!(sigkill_outcome(77, 77), KillOutcome::Caller);
    }

    #[test]
    fn an_ordinary_process_is_killed() {
        assert_eq!(sigkill_outcome(77, 1234), KillOutcome::Target);
    }

    // ---- who may forge an si_code ------------------------------------------

    #[test]
    fn si_tkill_is_minus_six() {
        // Also from the ABI, so also asserted against the literal.
        assert_eq!(SI_TKILL, -6);
    }

    #[test]
    fn a_kernel_code_cannot_be_forged_at_another_process() {
        // si_code >= 0 is the kernel's half of the space: SI_USER (0),
        // SI_KERNEL (0x80). rt_sigqueueinfo(2) answers EPERM.
        assert!(!may_queue_siginfo(0, false));
        assert!(!may_queue_siginfo(0x80, false));
        assert!(!may_queue_siginfo(i32::MAX, false));
    }

    #[test]
    fn a_tkill_cannot_be_impersonated_at_another_process() {
        // SI_TKILL is negative but still refused: it would claim the signal
        // came from a tkill, which carries the sender's identity.
        assert!(!may_queue_siginfo(SI_TKILL, false));
    }

    #[test]
    fn si_queue_is_exactly_what_sigqueue_is_for() {
        // SI_QUEUE is -1. Refusing it would break sigqueue(3) itself.
        assert!(may_queue_siginfo(-1, false));
        assert!(may_queue_siginfo(-2, false));
        assert!(may_queue_siginfo(i32::MIN, false));
    }

    #[test]
    fn anything_goes_at_your_own_process() {
        // The rule is about lying to somebody else. A process may stamp any
        // code on a signal it sends itself.
        for code in [0, 0x80, SI_TKILL, -1, i32::MAX, i32::MIN] {
            assert!(may_queue_siginfo(code, true), "si_code {} at self", code);
        }
    }

    // ---- sigaltstack, in Linux's order -------------------------------------

    fn stack(flags: SignalStackFlags, size: usize) -> SignalStack {
        SignalStack {
            sp: 0x1000,
            flags,
            size,
        }
    }

    /// A `stack_t` with a raw `ss_flags`, built the way the syscall gets one.
    ///
    /// `sigaltstack` reads the struct straight out of user memory, so a bit
    /// the kernel has no name for arrives intact -- which is the only way to
    /// produce one, and exactly what the flag check exists to catch.
    /// `SignalStackFlags::from_bits_truncate` would drop it and test nothing.
    fn user_stack(flags: u32, size: usize) -> SignalStack {
        #[repr(C)]
        struct RawStack {
            sp: usize,
            flags: u32,
            size: usize,
        }
        let raw = RawStack {
            sp: 0x1000,
            flags,
            size,
        };
        let ptr: UserInPtr<SignalStack> = UserInPtr::from(&raw as *const RawStack as usize);
        ptr.read().expect("a host address is readable")
    }

    #[test]
    fn a_flag_the_kernel_has_no_name_for_survives_the_read() {
        // Without this the two tests below would be asserting on a value the
        // kernel can never see, and the flag check they cover would be dead.
        assert!(!VALID_SIGSTACK_FLAGS.contains(user_stack(0x4, 8192).flags));
    }

    #[test]
    fn minsigstksz_is_two_kilobytes() {
        assert_eq!(MIN_SIGSTACK_SIZE, 2048);
    }

    #[test]
    fn a_thread_running_on_its_alternate_stack_may_not_change_it() {
        assert_eq!(
            check_sigaltstack(stack(SignalStackFlags::empty(), 8192), true),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn the_refusal_to_change_it_comes_before_anything_else() {
        // do_sigaltstack() answers EPERM before it reads a single field of
        // the new stack. Checking the size first told a thread that may not
        // ask at all that its stack was too small, which is a different bug
        // to go looking for.
        assert_eq!(
            check_sigaltstack(user_stack(0x4, 0), true),
            Err(LxError::EPERM)
        );
    }

    #[test]
    fn an_unknown_flag_is_refused_before_the_size_is_looked_at() {
        // A call that is wrong twice over: Linux rejects the flag (EINVAL)
        // and never reaches the size. Answering ENOMEM sends the caller to
        // grow a stack that was never the problem.
        assert_eq!(
            check_sigaltstack(user_stack(0x4, 0), false),
            Err(LxError::EINVAL)
        );
    }

    #[test]
    fn a_stack_too_small_for_a_signal_frame_is_enomem() {
        assert_eq!(
            check_sigaltstack(stack(SignalStackFlags::empty(), 2047), false),
            Err(LxError::ENOMEM)
        );
        assert_eq!(
            check_sigaltstack(stack(SignalStackFlags::empty(), 0), false),
            Err(LxError::ENOMEM)
        );
    }

    #[test]
    fn a_stack_exactly_the_minimum_is_enough() {
        assert_eq!(
            check_sigaltstack(stack(SignalStackFlags::empty(), MIN_SIGSTACK_SIZE), false),
            Ok(())
        );
    }

    #[test]
    fn disabling_a_stack_needs_no_size_at_all() {
        // `ss_size` is not looked at when the call is turning the alternate
        // stack off, so the usual `{.ss_flags = SS_DISABLE}` with everything
        // else zero has to work.
        assert_eq!(
            check_sigaltstack(stack(SignalStackFlags::DISABLE, 0), false),
            Ok(())
        );
    }

    #[test]
    fn autodisarm_is_accepted_with_a_big_enough_stack() {
        assert_eq!(
            check_sigaltstack(
                stack(SignalStackFlags::AUTODISARM, MIN_SIGSTACK_SIZE),
                false
            ),
            Ok(())
        );
        assert_eq!(
            check_sigaltstack(
                stack(SignalStackFlags::AUTODISARM | SignalStackFlags::DISABLE, 0),
                false
            ),
            Ok(())
        );
    }
}

#[cfg(test)]
mod queued_siginfo_tests {
    //! `tkill`/`tgkill` queue straight onto the target thread; what they
    //! leave for the handler goes with the signal.

    use super::*;
    use linux_object::process::LinuxProcess;
    use linux_object::thread::ThreadExt;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::ROOT_JOB;

    #[test]
    fn a_thread_signal_arrives_with_who_sent_it() {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            43_201,
            "t",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let thread = Thread::create_linux(&proc).unwrap();
        queue_signal_to_thread(
            &thread,
            Signal::SIGUSR1,
            SigInfo::from_user(Signal::SIGUSR1, 31, 1000, SignalCode::TKILL),
        );
        let info = thread.lock_linux().take_siginfo(Signal::SIGUSR1);
        let b = info.as_bytes();
        let word = |at: usize| i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        assert_eq!(
            (info.code, word(16), word(20)),
            (SignalCode::TKILL, 31, 1000)
        );
    }
}
