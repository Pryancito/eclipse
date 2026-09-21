//! Linux Thread

use crate::error::SysResult;
use crate::process::ProcessExt;
use crate::signal::{SigInfo, Signal, SignalStack, SignalUserContext, Sigset};
use alloc::string::String;
use alloc::sync::Arc;
use kernel_hal::context::{UserContext, UserContextField};
use kernel_hal::sync::{Mutex, MutexGuard};
use kernel_hal::user::{Out, UserInPtr, UserOutPtr, UserPtr};
use zircon_object::object::KernelObject;
use zircon_object::task::{CurrentThread, Process, Thread};
use zircon_object::ZxResult;

/// Thread extension for linux
pub trait ThreadExt {
    /// create linux thread
    fn create_linux(proc: &Arc<Process>) -> ZxResult<Arc<Self>>;
    /// lock and get Linux thread
    fn lock_linux(&self) -> MutexGuard<'_, LinuxThread>;
    /// Like [`lock_linux`](Self::lock_linux) but returns `None` instead of
    /// panicking when the extension is not a `Mutex<LinuxThread>`. Use this when
    /// walking another process's threads (signal delivery, enumeration): a
    /// thread observed mid-teardown during SMP churn must be skipped, not bring
    /// down the kernel.
    fn try_lock_linux(&self) -> Option<MutexGuard<'_, LinuxThread>>;
    /// Set pointer to thread ID.
    fn set_tid_address(&self, tidptr: UserOutPtr<i32>);
    /// Get robust list.
    fn get_robust_list(
        &self,
        _head_ptr: UserOutPtr<UserOutPtr<RobustList>>,
        _len_ptr: UserOutPtr<usize>,
    ) -> SysResult;
    /// Set robust list.
    fn set_robust_list(&self, head: UserInPtr<RobustList>, len: usize);
}

/// CurrentThread extension for linux
pub trait CurrentThreadExt {
    /// exit linux thread
    fn exit_linux(&self, exit_code: i32);
}

impl ThreadExt for Thread {
    fn create_linux(proc: &Arc<Process>) -> ZxResult<Arc<Self>> {
        let linux_thread = Mutex::new(LinuxThread {
            clear_child_tid: 0.into(),
            signals: Sigset::default(),
            signal_mask: Sigset::default(),
            saved_sigmask: None,
            signal_alternate_stack: SignalStack::default(),
            robust_list: 0.into(),
            robust_list_len: 0,
            handling_signal: None,
            comm: String::new(),
            timerslack_ns: 0,
        });
        // The thread-group leader (the process's first/main thread) must have a
        // TID equal to the process PID, just like Linux. Userspace relies on
        // this: e.g. winit's `is_main_thread()` panics unless gettid()==getpid()
        // on the main thread, and tgkill(getpid(), gettid()) must reach the
        // leader. Without it, every KObject (process, thread, VMO, ...) draws a
        // distinct id from one global counter, so the leader's TID never matched
        // its PID. Subsequent threads (pthread_create) keep getting fresh,
        // unique ids — only the leader reuses the PID, which is allocated to no
        // other object.
        let leader_id = if proc.thread_ids().is_empty() {
            Some(proc.id())
        } else {
            None
        };
        Thread::create_with_ext_id(proc, "", linux_thread, leader_id)
    }

    fn lock_linux(&self) -> MutexGuard<'_, LinuxThread> {
        // See Process::linux(): a failed downcast means a non-Linux thread
        // leaked into a Linux-only path or the ext Box was corrupted. Identify
        // the thread/process so the panic names the culprit.
        self.ext()
            .downcast_ref::<Mutex<LinuxThread>>()
            .unwrap_or_else(|| {
                // Same evidence as Process::linux(): the fat pointer now, the
                // fat pointer at construction, and the guards either side of
                // the field. Which words moved says whether this is an 8-byte
                // store, a whole-fat-pointer assignment, or no write at all.
                let (data, vtable) = self.ext_fat();
                let (born_data, born_vtable) = self.ext_born();
                let vt = zircon_object::task::vtable_info(vtable);
                // See Process::linux(): `ext` is immutable, so a downcast that
                // fails and then immediately succeeds saw an inconsistent read,
                // not a different type. Carry on with the value we can now see
                // is correct rather than killing the kernel, and log it.
                if let Some(m) = self.ext().downcast_ref::<Mutex<LinuxThread>>() {
                    error!(
                        "[ext-glitch] Thread::lock_linux(): tid={} pid={} name={:?} downcast \
                         failed then SUCCEEDED on retry -- ext read inconsistently. \
                         fat data={:#x} vtable={:#x}, at birth data={:#x} vtable={:#x}",
                        self.id(),
                        self.proc().id(),
                        self.proc().name(),
                        data,
                        vtable,
                        born_data,
                        born_vtable,
                    );
                    return m;
                }
                panic!(
                    "Thread::lock_linux(): tid={} proc pid={} name={:?} has no \
                     LinuxThread ext (ext fat pointer: data={:#x} vtable={:#x} \
                     -> {:x?} (drop, size, align), Mutex<LinuxThread> would be \
                     size={} align={}; \
                     at birth: data={:#x} vtable={:#x} -> {}; canaries {}) -- \
                     non-Linux thread in a Linux path, or corrupted ext",
                    self.id(),
                    self.proc().id(),
                    self.proc().name(),
                    data,
                    vtable,
                    vt,
                    core::mem::size_of::<Mutex<LinuxThread>>(),
                    core::mem::align_of::<Mutex<LinuxThread>>(),
                    born_data,
                    born_vtable,
                    match (born_data == data, born_vtable == vtable) {
                        (true, true) => "UNCHANGED: the ext was never a LinuxThread",
                        (true, false) => "VTABLE ONLY: one 8-byte store, data untouched",
                        (false, true) => "DATA ONLY: one 8-byte store, vtable untouched",
                        (false, false) => "BOTH words replaced",
                    },
                    match self.ext_canaries() {
                        (true, true) => "both INTACT: a precise write to ext alone",
                        (false, true) => "LOW broken: overrun growing upward from below",
                        (true, false) => "HIGH broken: overrun growing downward from above",
                        (false, false) => "BOTH broken: wide overrun across the field",
                    },
                )
            })
            .lock()
    }

    fn try_lock_linux(&self) -> Option<MutexGuard<'_, LinuxThread>> {
        Some(self.ext().downcast_ref::<Mutex<LinuxThread>>()?.lock())
    }

    /// Set pointer to thread ID.
    fn set_tid_address(&self, tidptr: UserPtr<i32, Out>) {
        self.lock_linux().clear_child_tid = tidptr;
    }

    fn get_robust_list(
        &self,
        mut head_ptr: UserOutPtr<UserOutPtr<RobustList>>,
        mut len_ptr: UserOutPtr<usize>,
    ) -> SysResult {
        let linux = self.lock_linux();
        let head: UserOutPtr<RobustList> = linux.robust_list.as_addr().into();
        let len = linux.robust_list_len;
        drop(linux);
        head_ptr.write(head)?;
        len_ptr.write(len)?;
        Ok(0)
    }

    fn set_robust_list(&self, head: UserInPtr<RobustList>, len: usize) {
        self.lock_linux().robust_list = head;
        self.lock_linux().robust_list_len = len;
    }
}

impl CurrentThreadExt for CurrentThread {
    /// Exit current thread for Linux.
    fn exit_linux(&self, _exit_code: i32) {
        let mut linux_thread = self.lock_linux();
        let clear_child_tid = &mut linux_thread.clear_child_tid;
        // perform futex wake 1
        // ref: http://man7.org/linux/man-pages/man2/set_tid_address.2.html
        if !clear_child_tid.is_null() {
            info!("exit: do futex {:?} wake 1", clear_child_tid);
            #[cfg(target_os = "none")]
            {
                let vaddr = clear_child_tid.as_addr();
                let vmar = self.proc().vmar();
                if vmar.contains(vaddr) {
                    // The page may be lazily allocated or CoW (mapped
                    // read-only after fork): fault it in writable first.
                    // Skipping the clear+wake here would leave pthread_join
                    // (and musl's __tl_sync) waiting forever.
                    let writable = matches!(
                        vmar.get_vaddr_flags(vaddr),
                        Ok(flags) if flags.contains(kernel_hal::MMUFlags::WRITE)
                    );
                    let mapped = writable
                        || vmar
                            .handle_page_fault(
                                vaddr,
                                kernel_hal::MMUFlags::WRITE | kernel_hal::MMUFlags::USER,
                            )
                            .is_ok();
                    if mapped && clear_child_tid.write(0).is_ok() {
                        if let Some(futex) = self.proc().linux().get_futex(vaddr) {
                            futex.wake(1);
                        }
                    }
                }
            }
            #[cfg(not(target_os = "none"))]
            {
                clear_child_tid.write(0).unwrap();
                let uaddr = clear_child_tid.as_addr();
                if let Some(futex) = self.proc().linux().get_futex(uaddr) {
                    futex.wake(1);
                }
            }
        }
        self.exit();
    }
}

/// robust_list
#[derive(Default)]
pub struct RobustList {
    /// head
    pub head: usize,
    /// off
    pub off: isize,
    /// pending
    pub pending: usize,
}

/// Linux specific thread information.
pub struct LinuxThread {
    /// Kernel performs futex wake when thread exits.
    /// Ref: <http://man7.org/linux/man-pages/man2/set_tid_address.2.html>
    clear_child_tid: UserOutPtr<i32>,
    /// Linux signals
    pub signals: Sigset,
    /// Signal mask.
    ///
    /// Private on purpose: SIGKILL and SIGSTOP must never be in it, and while
    /// this was a public field that rule lived at four separate call sites --
    /// `sigprocmask`, `sigsuspend`, `ppoll`/`pselect`'s temporary mask and
    /// `sigreturn`'s restored one. Two of them applied it and two did not, so
    /// a process could block the two signals it is never allowed to block and
    /// stop answering `SIGSTOP` for good. Go through [`Self::set_signal_mask`]
    /// and friends, which cannot forget.
    signal_mask: Sigset,
    /// Signal mask to restore once the currently-awaited signal handler
    /// returns. Set by `rt_sigsuspend` so that the original mask is restored
    /// after the temporarily-unblocked signal is delivered.
    pub saved_sigmask: Option<Sigset>,
    /// signal alternate stack
    pub signal_alternate_stack: SignalStack,
    /// robust_list
    robust_list: UserInPtr<RobustList>,
    robust_list_len: usize,
    /// handling signals
    pub handling_signal: Option<u32>,
    /// Thread name (`prctl(PR_SET_NAME)` / `/proc/<pid>/comm`), at most
    /// [`TASK_COMM_LEN`]` - 1` bytes. Empty = never set: readers fall back to
    /// the executable's basename, so a fresh thread reports its program name.
    pub comm: String,
    /// Timer slack in nanoseconds (`prctl(PR_SET_TIMERSLACK)`). `0` = never
    /// set → reads as the Linux default of 50 µs. Recorded and read back;
    /// timers here do not apply slack coalescing.
    pub timerslack_ns: u64,
}

/// Size of the kernel's per-task `comm` buffer, including the trailing NUL
/// (`TASK_COMM_LEN` in `include/linux/sched.h`): names are truncated to 15
/// bytes.
pub const TASK_COMM_LEN: usize = 16;

fn unmodified_check(siginfo: &SigInfo, user_ctx: &SignalUserContext) -> usize {
    let mut check = 0usize;
    let default_info = SigInfo::default();
    let mut default_ctx = SignalUserContext::default();
    default_ctx.context.set_pc(user_ctx.context.get_pc());
    check |= (*siginfo != default_info) as usize;
    check |= ((user_ctx.flags != default_ctx.flags) as usize) << 1;
    check |= ((user_ctx.link != default_ctx.link) as usize) << 2;
    check |= ((user_ctx.stack != default_ctx.stack) as usize) << 3;
    check |= ((user_ctx._pad != default_ctx._pad) as usize) << 4;
    check |= ((user_ctx.context != default_ctx.context) as usize) << 5;
    #[cfg(target_arch = "x86_64")]
    {
        check |= ((user_ctx.fpregs_mem != default_ctx.fpregs_mem) as usize) << 6;
    }
    check
}

#[allow(unsafe_code)]
impl LinuxThread {
    /// Restore the information after the signal handler returns
    pub fn restore_after_handle_signal(
        &mut self,
        ctx: &mut UserContext,
        old_ctx: &UserContext,
        siginfo_ptr: usize,
        uctx_ptr: usize,
    ) {
        let siginfo = unsafe { &*(siginfo_ptr as *const SigInfo) };
        let user_ctx = unsafe { &*(uctx_ptr as *const SignalUserContext) };
        let check = unmodified_check(siginfo, user_ctx);
        if check != 0 {
            error!("unsupported signal fields : {:b}", check);
            trace!("uctx = {:x?}", *user_ctx);
            // Be tolerant: userland may legally modify parts of ucontext/siginfo.
            // We restore the saved context and only honor the restored PC/mask below.
        }
        *ctx = *old_ctx;
        ctx.set_field(UserContextField::InstrPointer, user_ctx.context.get_pc());
        // The ucontext is userland's to modify between the handler running
        // and `sigreturn`, so this mask is untrusted input like any other:
        // without the filter a handler could return with SIGKILL blocked.
        self.set_signal_mask(Sigset::new(user_ctx.sig_mask.val()));
        self.handling_signal = None;
    }

    /// The signals this thread currently has blocked.
    pub fn signal_mask(&self) -> Sigset {
        self.signal_mask
    }

    /// Replace the blocked-signal mask, dropping the two signals that can
    /// never be blocked. `sigprocmask(2)` is explicit that the attempt is
    /// *ignored*, not refused, so this returns nothing and fails at nothing.
    pub fn set_signal_mask(&mut self, mask: Sigset) {
        self.signal_mask = mask.blockable();
    }

    /// `SIG_BLOCK`: add `set` to what is blocked.
    pub fn block_signals(&mut self, set: &Sigset) {
        let mut new = self.signal_mask;
        new.insert_set(set);
        self.set_signal_mask(new);
    }

    /// `SIG_UNBLOCK`: take `set` out of what is blocked.
    ///
    /// The filter in the setter can never fire on this path -- taking signals
    /// out of a mask cannot put SIGKILL into it -- so going through the setter
    /// here is for the day the invariant is maintained by something else.
    pub fn unblock_signals(&mut self, set: &Sigset) {
        let mut new = self.signal_mask;
        new.remove_set(set);
        self.set_signal_mask(new);
    }

    /// Get signal info
    pub fn get_signal_info(&self) -> (Sigset, Sigset, Option<u32>) {
        (self.signals, self.signal_mask, self.handling_signal)
    }

    /// Address registered via `set_tid_address`/`CLONE_CHILD_CLEARTID`, for
    /// `prctl(PR_GET_TID_ADDRESS)`. `0` when never set.
    pub fn tid_address(&self) -> usize {
        self.clear_child_tid.as_addr()
    }

    /// Handle signal
    pub fn handle_signal(&mut self) -> Option<(Signal, Sigset)> {
        if self.handling_signal.is_none() {
            let signal = self
                .signals
                .mask_with(&self.signal_mask)
                .find_first_signal();
            if let Some(signal) = signal {
                self.handling_signal = Some(signal as u32);
                self.signals.remove(signal);
                // If a `rt_sigsuspend` (or similar) saved a mask to restore once
                // the handler returns, hand that mask to the signal frame so it
                // is reinstated on `sigreturn`. Otherwise keep the current mask.
                let restore_mask = self.saved_sigmask.take().unwrap_or(self.signal_mask);
                return Some((signal, restore_mask));
            }
        }
        None
    }
}

#[cfg(test)]
mod signal_delivery_tests {
    //! `handle_signal` is the whole of signal delivery: every Ctrl-C, every
    //! `kill`, every SIGCHLD a shell waits on comes out of this one function.
    //! It needs no process and no scheduler -- it reads two bitmaps and writes
    //! three fields -- so the rules it enforces can be pinned exactly.

    use super::*;
    use core::convert::TryFrom;

    /// A one-signal set, for the mask helpers.
    fn one(sig: Signal) -> Sigset {
        let mut s = Sigset::empty();
        s.insert(sig);
        s
    }

    /// A thread with nothing pending and nothing blocked, which is how one
    /// starts life.
    fn thread() -> LinuxThread {
        LinuxThread {
            clear_child_tid: 0.into(),
            signals: Sigset::default(),
            signal_mask: Sigset::default(),
            saved_sigmask: None,
            signal_alternate_stack: SignalStack::default(),
            robust_list: 0.into(),
            robust_list_len: 0,
            handling_signal: None,
            comm: String::new(),
            timerslack_ns: 0,
        }
    }

    #[test]
    fn nothing_pending_delivers_nothing() {
        let mut t = thread();
        assert!(t.handle_signal().is_none());
        assert!(t.handling_signal.is_none());
    }

    #[test]
    fn a_pending_signal_is_taken_and_stops_being_pending() {
        // Taking it must clear it: left pending, the same signal is delivered
        // again on the next check, and the handler runs for ever.
        let mut t = thread();
        t.signals.insert(Signal::SIGINT);
        let (sig, mask) = t.handle_signal().expect("SIGINT was pending");
        assert_eq!(sig, Signal::SIGINT);
        assert!(
            mask.is_empty(),
            "nothing was blocked, so nothing is restored"
        );
        assert!(!t.signals.contains(Signal::SIGINT), "it is still pending");
        assert_eq!(t.handling_signal, Some(Signal::SIGINT as u32));
    }

    #[test]
    fn the_lowest_numbered_pending_signal_goes_first() {
        let mut t = thread();
        t.signals.insert(Signal::SIGWINCH);
        t.signals.insert(Signal::SIGTERM);
        t.signals.insert(Signal::SIGUSR1);
        let (sig, _) = t.handle_signal().unwrap();
        assert_eq!(
            sig,
            Signal::SIGUSR1,
            "SIGUSR1 is 10, the lowest of the three"
        );
        // The other two are untouched and will be taken in turn.
        assert!(t.signals.contains(Signal::SIGTERM));
        assert!(t.signals.contains(Signal::SIGWINCH));
    }

    #[test]
    fn a_blocked_signal_stays_pending_instead_of_being_delivered() {
        // This is what `sigprocmask` buys: the signal is not lost, it waits.
        // Delivering it anyway defeats every critical section userspace has;
        // dropping it loses the signal for good.
        let mut t = thread();
        t.signals.insert(Signal::SIGINT);
        t.block_signals(&one(Signal::SIGINT));
        assert!(
            t.handle_signal().is_none(),
            "a blocked signal was delivered"
        );
        assert!(
            t.signals.contains(Signal::SIGINT),
            "it was dropped, not held"
        );

        // Unblocking releases it, still pending, with no second `kill` needed.
        t.unblock_signals(&one(Signal::SIGINT));
        let (sig, _) = t.handle_signal().expect("unblocking must release it");
        assert_eq!(sig, Signal::SIGINT);
    }

    #[test]
    fn a_blocked_signal_does_not_hide_an_unblocked_one_behind_it() {
        // The blocked signal has the lower number, so a mask applied *after*
        // picking the first pending signal would return SIGINT and deliver
        // something the process explicitly blocked.
        let mut t = thread();
        t.signals.insert(Signal::SIGINT);
        t.signals.insert(Signal::SIGTERM);
        t.block_signals(&one(Signal::SIGINT));
        let (sig, _) = t.handle_signal().unwrap();
        assert_eq!(sig, Signal::SIGTERM, "the blocked SIGINT was delivered");
        assert!(
            t.signals.contains(Signal::SIGINT),
            "and it stopped being pending"
        );
    }

    #[test]
    fn nothing_new_is_delivered_while_a_handler_is_running() {
        // Re-entering the handler would build a second signal frame on a stack
        // that already holds one, on top of the first handler's locals.
        let mut t = thread();
        t.signals.insert(Signal::SIGUSR1);
        assert!(t.handle_signal().is_some());
        t.signals.insert(Signal::SIGUSR2);
        assert!(
            t.handle_signal().is_none(),
            "a second signal was delivered on top of the running handler"
        );
        assert!(
            t.signals.contains(Signal::SIGUSR2),
            "and it was consumed doing it"
        );
    }

    #[test]
    fn a_saved_mask_is_handed_back_once_and_then_forgotten() {
        // `sigsuspend` installs a temporary mask and leaves the old one here
        // to be reinstated when the handler returns. Handing it back twice
        // would restore a stale mask over whatever the process set since.
        let mut t = thread();
        let mut original = Sigset::empty();
        original.insert(Signal::SIGCHLD);
        t.saved_sigmask = Some(original);
        t.signals.insert(Signal::SIGINT);

        let (_, restore) = t.handle_signal().unwrap();
        assert!(
            restore.contains(Signal::SIGCHLD),
            "the mask to reinstate is the one sigsuspend saved, not the temporary one"
        );
        assert!(t.saved_sigmask.is_none(), "the saved mask was not taken");

        // Next time round there is nothing saved, so the current mask is what
        // the frame carries.
        t.handling_signal = None;
        t.block_signals(&one(Signal::SIGWINCH));
        t.signals.insert(Signal::SIGTERM);
        let (_, restore) = t.handle_signal().unwrap();
        assert!(restore.contains(Signal::SIGWINCH));
        assert!(
            !restore.contains(Signal::SIGCHLD),
            "the stale mask came back"
        );
    }

    #[test]
    fn get_signal_info_reports_what_delivery_just_did() {
        // `/proc/<pid>/status` reads SigPnd/SigBlk from here, and it is the
        // only window onto this state from outside.
        let mut t = thread();
        t.signals.insert(Signal::SIGTERM);
        t.block_signals(&one(Signal::SIGWINCH));
        let (pending, blocked, handling) = t.get_signal_info();
        assert!(pending.contains(Signal::SIGTERM));
        assert!(blocked.contains(Signal::SIGWINCH));
        assert!(handling.is_none());

        t.handle_signal().unwrap();
        let (pending, _, handling) = t.get_signal_info();
        assert!(
            !pending.contains(Signal::SIGTERM),
            "still reported as pending"
        );
        assert_eq!(handling, Some(Signal::SIGTERM as u32));
    }

    #[test]
    fn the_two_unblockable_signals_never_enter_the_mask() {
        // Every route userspace has to this field goes through these three
        // helpers, and none of them may let SIGKILL or SIGSTOP in: a thread
        // that blocks SIGSTOP can no longer be stopped, and `handle_signal`
        // below would mask the signal out for ever.
        let mut wanted = Sigset::empty();
        for sig in [Signal::SIGKILL, Signal::SIGSTOP, Signal::SIGINT] {
            wanted.insert(sig);
        }

        // SIG_SETMASK.
        let mut t = thread();
        t.set_signal_mask(wanted);
        assert!(!t.signal_mask().contains(Signal::SIGKILL));
        assert!(!t.signal_mask().contains(Signal::SIGSTOP));
        assert!(
            t.signal_mask().contains(Signal::SIGINT),
            "SIGINT was dropped too"
        );

        // SIG_BLOCK, which adds to what is already there.
        let mut t = thread();
        t.block_signals(&one(Signal::SIGCHLD));
        t.block_signals(&wanted);
        assert!(!t.signal_mask().contains(Signal::SIGKILL));
        assert!(!t.signal_mask().contains(Signal::SIGSTOP));
        assert!(
            t.signal_mask().contains(Signal::SIGCHLD),
            "the earlier block was lost"
        );
        assert!(t.signal_mask().contains(Signal::SIGINT));

        // And the whole point: a SIGSTOP sent to a thread that tried to block
        // it is still delivered.
        let mut t = thread();
        t.set_signal_mask(wanted);
        t.signals.insert(Signal::SIGSTOP);
        let (sig, _) = t
            .handle_signal()
            .expect("SIGSTOP was blocked, so the thread can never be stopped");
        assert_eq!(sig, Signal::SIGSTOP);
    }

    #[test]
    fn sigreturn_cannot_smuggle_a_blocked_sigkill_back_in() {
        // The mask `sigreturn` installs comes out of a ucontext that the
        // signal handler had every opportunity to rewrite, so it is untrusted
        // input and gets the same filter as `sigprocmask`.
        let mut t = thread();
        let mut doctored = Sigset::empty();
        doctored.insert(Signal::SIGKILL);
        doctored.insert(Signal::SIGSTOP);
        doctored.insert(Signal::SIGUSR1);
        t.set_signal_mask(doctored);
        assert!(!t.signal_mask().contains(Signal::SIGKILL));
        assert!(!t.signal_mask().contains(Signal::SIGSTOP));
        assert!(t.signal_mask().contains(Signal::SIGUSR1));
    }

    #[test]
    fn unblocking_a_signal_that_was_not_blocked_does_not_block_it() {
        // `sigprocmask(SIG_UNBLOCK, set)` where `set` is wider than what the
        // thread actually blocks is ordinary and must be a no-op for the
        // extra signals, not their inverse.
        let mut t = thread();
        t.block_signals(&one(Signal::SIGINT));
        let mut wide = Sigset::empty();
        wide.insert(Signal::SIGINT);
        wide.insert(Signal::SIGTERM);
        t.unblock_signals(&wide);
        assert!(!t.signal_mask().contains(Signal::SIGINT));
        assert!(
            !t.signal_mask().contains(Signal::SIGTERM),
            "unblocking SIGTERM blocked it"
        );
    }

    #[test]
    fn sigreturn_restores_the_pc_and_filters_the_mask_it_is_handed() {
        // `restore_after_handle_signal` is the kernel side of `sigreturn`, and
        // everything it reads -- the PC to resume at, the mask to reinstate --
        // comes out of a `ucontext` sitting on the user stack that the handler
        // had every opportunity to rewrite before returning. So it is
        // untrusted input, and in particular the mask gets the same filter as
        // `sigprocmask`: otherwise a handler returns with SIGKILL blocked and
        // the process can no longer be stopped.
        let mut t = thread();
        t.handling_signal = Some(Signal::SIGUSR1 as u32);

        let info = SigInfo::default();
        let mut uctx = SignalUserContext::default();
        const RESUME_AT: usize = 0x4000_1234;
        uctx.context.set_pc(RESUME_AT);
        let mut doctored = Sigset::empty();
        doctored.insert(Signal::SIGKILL);
        doctored.insert(Signal::SIGSTOP);
        doctored.insert(Signal::SIGUSR2);
        uctx.sig_mask = doctored;

        let mut old_ctx = UserContext::default();
        old_ctx.set_field(UserContextField::InstrPointer, 0xBAD0_0000);
        let mut ctx = UserContext::default();

        t.restore_after_handle_signal(
            &mut ctx,
            &old_ctx,
            &info as *const SigInfo as usize,
            &uctx as *const SignalUserContext as usize,
        );

        assert_eq!(
            ctx.get_field(UserContextField::InstrPointer),
            RESUME_AT,
            "the thread did not resume where the ucontext said"
        );
        assert!(
            !t.signal_mask().contains(Signal::SIGKILL),
            "sigreturn let a handler block SIGKILL"
        );
        assert!(
            !t.signal_mask().contains(Signal::SIGSTOP),
            "sigreturn let a handler block SIGSTOP"
        );
        assert!(
            t.signal_mask().contains(Signal::SIGUSR2),
            "the rest of the restored mask was thrown away"
        );
        assert!(
            t.handling_signal.is_none(),
            "the handler is still marked as running, so no further signal is delivered"
        );
    }

    #[test]
    fn every_signal_can_be_delivered() {
        // The real-time signals go up to 64, which is the last bit of the
        // word: a loop bound that stopped at 63 would make SIGRT64
        // undeliverable and `find_first_signal` would have to invent one.
        for n in 1..=64u8 {
            let sig = Signal::try_from(n).unwrap();
            let mut t = thread();
            t.signals.insert(sig);
            let (got, _) = t
                .handle_signal()
                .unwrap_or_else(|| panic!("{:?} was never delivered", sig));
            assert_eq!(got, sig);
        }
    }
}
