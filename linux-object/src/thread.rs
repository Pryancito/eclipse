//! Linux Thread

use crate::error::SysResult;
use crate::process::ProcessExt;
use crate::signal::{SigInfo, Signal, SignalStack, SignalUserContext, Sigset};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
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

/// Release every robust lock a dying thread still holds.
trait RobustExit {
    fn release_robust_locks(&self);
}

impl RobustExit for CurrentThread {
    fn release_robust_locks(&self) {
        let (head_addr, len) = {
            let linux = self.lock_linux();
            (linux.robust_list.as_addr(), linux.robust_list_len)
        };
        // A length other than the struct's own is what `set_robust_list`
        // refuses, so it can only be the never-registered zero.
        if head_addr == 0 || len != core::mem::size_of::<RobustList>() {
            return;
        }
        // Every read here is a `get_user` on the dying thread's own address
        // space, in kernel mode. The list head lives in the thread's TLS and
        // the entries in mutexes it just held, so the pages are normally
        // resident -- but "normally" is not enough on a path that runs at
        // every thread exit, and this kernel does not recover from a fault
        // taken on a raw access (see the `clear_child_tid` code below, which
        // faults its page in for the same reason). So: inside the process's
        // own mappings, and faulted in first.
        let vmar = self.proc().vmar();
        let held = walk_robust_list(head_addr, |addr| {
            if !addr.is_multiple_of(core::mem::align_of::<usize>()) {
                return None;
            }
            #[cfg(target_os = "none")]
            {
                if !vmar.contains(addr) {
                    return None;
                }
                let present = vmar.get_vaddr_flags(addr).is_ok();
                if !present
                    && vmar
                        .handle_page_fault(addr, kernel_hal::MMUFlags::USER)
                        .is_err()
                {
                    return None;
                }
            }
            let word: UserInPtr<usize> = addr.into();
            word.read().ok()
        });
        drop(vmar);
        if held.is_empty() {
            return;
        }
        let tid = self.id() as i32;
        let proc = self.proc();
        for lock in held {
            let Some(futex) = proc.linux().get_futex(lock.addr) else {
                continue;
            };
            // Retry on a lost race, as Linux's `handle_futex_death` does: the
            // word can still change under us while this thread is dying.
            loop {
                let uval = futex.load();
                match robust_death_action(uval, tid, lock.pi) {
                    RobustDeath::NotOurs => break,
                    RobustDeath::Died { new_value, wake } => {
                        if futex.compare_exchange(uval, new_value).is_err() {
                            continue;
                        }
                        if wake {
                            futex.wake(1);
                        }
                        break;
                    }
                }
            }
        }
    }
}

impl CurrentThreadExt for CurrentThread {
    /// Exit current thread for Linux.
    fn exit_linux(&self, _exit_code: i32) {
        // Linux's `mm_release` does this first, before the `clear_child_tid`
        // wake below: release every robust lock this thread still holds. The
        // list was registered by `set_robust_list` -- which both glibc and
        // musl call for every thread they create -- and was stored and read
        // by nobody, so a thread that died holding a robust or process-shared
        // mutex left every waiter blocked in the kernel for the life of the
        // process, with no way for userspace to break it.
        self.release_robust_locks();
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
                // Linux's `mm_release` is `if (!put_user(0, tidptr)) do_futex(...)`:
                // a tid address userspace got wrong costs that thread its exit
                // wake and nothing else. This `unwrap` made it a kernel panic
                // instead, reachable from any thread exit -- musl's `start()`
                // aborts a thread it could not finish creating with
                // `set_tid_address(&args->control); for(;;) exit(0);`, and when
                // that pointer is garbage the whole kernel came down on the
                // libos build. Three of the libc suite's pthread cases died
                // here rather than in the test.
                if clear_child_tid.write(0).is_ok() {
                    let uaddr = clear_child_tid.as_addr();
                    if let Some(futex) = self.proc().linux().get_futex(uaddr) {
                        futex.wake(1);
                    }
                }
            }
        }
        self.exit();
    }
}

/// Linux's `struct robust_list_head`: the head of the list of locks a thread
/// holds, so the kernel can release them if the thread dies holding one.
///
/// `repr(C)` because userspace writes this: the three words are read back out
/// of the process's own memory at thread exit, by offset.
#[derive(Default)]
#[repr(C)]
pub struct RobustList {
    /// First entry, or the address of this head itself when the list is empty.
    /// Bit 0 is the PI flag, not part of the address.
    pub head: usize,
    /// Signed distance from a list node to the futex word it guards. Negative
    /// in both glibc and musl -- the lock word sits *before* the list node
    /// inside `pthread_mutex_t`.
    pub off: isize,
    /// The entry a thread is part-way through adding or removing. Handled last
    /// and exactly once, because it may or may not be on the list yet.
    pub pending: usize,
}

/// Linux's `ROBUST_LIST_LIMIT`. The list lives in user memory and is walked at
/// every thread exit, so a program that builds a circular one -- by accident
/// or on purpose -- would otherwise keep a CPU in the kernel forever.
pub const ROBUST_LIST_LIMIT: usize = 2048;

/// One lock a dying thread still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RobustFutex {
    /// The futex word itself, already offset from the list node.
    pub addr: usize,
    /// Whether the entry was tagged as priority-inheriting (bit 0 of the
    /// pointer that named it).
    pub pi: bool,
}

/// Walk the robust list a dying thread registered, newest lock first.
///
/// `read_word` reads one machine word of the thread's memory, or returns
/// `None` when that address cannot be read -- which ends the walk, exactly as
/// a failed `get_user` ends Linux's. The list is userspace's, so every part of
/// it is untrusted: the length is capped, the terminator is the head's own
/// address, and the offset is applied as the signed quantity it is declared to
/// be.
pub fn walk_robust_list(
    head_addr: usize,
    mut read_word: impl FnMut(usize) -> Option<usize>,
) -> Vec<RobustFutex> {
    let mut found = Vec::new();
    if head_addr == 0 {
        return found;
    }
    // Bit 0 of every list pointer is the PI flag; the address is the rest.
    let split = |word: usize| (word & !1, word & 1 == 1);
    let (mut entry, mut pi) = match read_word(head_addr) {
        Some(word) => split(word),
        None => return found,
    };
    let offset = match read_word(head_addr + core::mem::size_of::<usize>()) {
        Some(word) => word as isize,
        None => return found,
    };
    let (pending, pending_pi) = match read_word(head_addr + 2 * core::mem::size_of::<usize>()) {
        Some(word) => split(word),
        None => return found,
    };
    // The list is circular by construction: it ends by pointing back at the
    // head. An entry of 0 is an empty list that was never linked up.
    let mut limit = ROBUST_LIST_LIMIT;
    while entry != head_addr && entry != 0 && limit > 0 {
        let next = read_word(entry);
        // The pending entry may already be linked in; it is handled once, at
        // the end, so skip it here rather than releasing the same lock twice.
        if entry != pending {
            found.push(RobustFutex {
                addr: entry.wrapping_add(offset as usize),
                pi,
            });
        }
        match next {
            Some(word) => {
                let (next_entry, next_pi) = split(word);
                entry = next_entry;
                pi = next_pi;
            }
            None => break,
        }
        limit -= 1;
    }
    if pending != 0 {
        found.push(RobustFutex {
            addr: pending.wrapping_add(offset as usize),
            pi: pending_pi,
        });
    }
    found
}

/// What to do with one futex word a dying thread may be holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobustDeath {
    /// The word does not name this thread as the owner: it was released
    /// already, or the list entry is stale. Leave it alone.
    NotOurs,
    /// Mark the owner as dead, and wake a waiter if the word says one is
    /// waiting and the lock is not priority-inheriting.
    Died {
        /// The value to store: the waiters bit as found, plus the death mark.
        new_value: i32,
        /// Whether a `FUTEX_WAKE` has to follow.
        wake: bool,
    },
}

/// Linux's `handle_futex_death`, without the memory access.
///
/// Userspace cannot do this itself: the whole point is that the owner is gone.
/// The next thread to take the lock sees `FUTEX_OWNER_DIED` and gets
/// `EOWNERDEAD` from `pthread_mutex_lock`, its chance to repair whatever the
/// dead owner left half-done. Without the mark, every waiter stays blocked in
/// the kernel for as long as the process lives.
pub fn robust_death_action(uval: i32, tid: i32, pi: bool) -> RobustDeath {
    const FUTEX_WAITERS: i32 = 0x8000_0000_u32 as i32;
    const FUTEX_OWNER_DIED: i32 = 0x4000_0000;
    const FUTEX_TID_MASK: i32 = 0x3fff_ffff;
    if uval & FUTEX_TID_MASK != tid & FUTEX_TID_MASK {
        return RobustDeath::NotOurs;
    }
    RobustDeath::Died {
        new_value: (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED,
        // A PI lock's waiters are woken by the PI unlock path, which hands the
        // lock over rather than racing for it; waking them here as well would
        // put a second thread into a queue that has already picked a winner.
        wake: !pi && uval & FUTEX_WAITERS != 0,
    }
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

#[cfg(test)]
mod robust_list_tests {
    //! The robust list is how a thread that dies holding a lock stops being
    //! everyone else's problem: the kernel walks it, marks each lock's owner
    //! dead and wakes a waiter, and the next thread to take the lock gets
    //! `EOWNERDEAD` instead of blocking forever. Both glibc and musl register
    //! one for every thread they create, so this runs at every thread exit in
    //! the system -- on a list that lives in user memory and can say anything.

    use super::*;
    use alloc::collections::BTreeMap;

    const HEAD: usize = 0x7000_0000;
    const W: usize = core::mem::size_of::<usize>();

    /// The head's three words plus whatever entries a test lays out.
    fn memory(next: usize, offset: isize, pending: usize) -> BTreeMap<usize, usize> {
        BTreeMap::from([
            (HEAD, next),
            (HEAD + W, offset as usize),
            (HEAD + 2 * W, pending),
        ])
    }

    fn walk(mem: &BTreeMap<usize, usize>) -> Vec<RobustFutex> {
        walk_robust_list(HEAD, |addr| mem.get(&addr).copied())
    }

    #[test]
    fn an_empty_list_points_back_at_its_own_head() {
        // This is how both libcs initialise it, and it is what almost every
        // thread exit sees. Treating the head as an entry would hand the
        // futex code an address inside the list head itself.
        assert!(walk(&memory(HEAD, -16, 0)).is_empty());
    }

    #[test]
    fn a_list_that_was_never_linked_up_is_empty() {
        assert!(walk(&memory(0, -16, 0)).is_empty());
        // And a thread that never called `set_robust_list` at all.
        assert!(walk_robust_list(0, |_| panic!("must not read anything")).is_empty());
    }

    #[test]
    fn the_futex_word_is_found_by_a_signed_offset_from_the_node() {
        // `futex_offset` is a `long`, and in both glibc and musl it is
        // NEGATIVE: the lock word sits before the list node inside
        // `pthread_mutex_t`. Read as unsigned it becomes a huge number, the
        // address wraps, and the kernel marks a word nowhere near the mutex.
        let node = HEAD + 0x1000;
        let mut mem = memory(node, -16, 0);
        mem.insert(node, HEAD);
        let held = walk(&mem);
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].addr, node - 16, "the word is before the node");
    }

    #[test]
    fn a_positive_offset_works_too() {
        let node = HEAD + 0x1000;
        let mut mem = memory(node, 24, 0);
        mem.insert(node, HEAD);
        assert_eq!(walk(&mem)[0].addr, node + 24);
    }

    #[test]
    fn every_entry_of_a_chain_is_visited_in_order() {
        // Newest lock first: a thread pushes each lock onto the head as it
        // takes it, so the walk releases in reverse order of acquisition.
        let (a, b, c) = (HEAD + 0x1000, HEAD + 0x2000, HEAD + 0x3000);
        let mut mem = memory(a, -8, 0);
        mem.insert(a, b);
        mem.insert(b, c);
        mem.insert(c, HEAD);
        let held = walk(&mem);
        assert_eq!(
            held.iter().map(|f| f.addr).collect::<Vec<_>>(),
            vec![a - 8, b - 8, c - 8]
        );
    }

    #[test]
    fn the_low_bit_of_a_pointer_is_the_pi_flag_and_not_an_address() {
        // Each list pointer carries the PI flag in bit 0. Leaving it in the
        // address would misalign every entry by one byte -- and `get_futex`
        // refuses an unaligned word, so every robust lock would be skipped
        // in silence.
        let node = HEAD + 0x1000;
        let mut mem = memory(node | 1, -8, 0);
        mem.insert(node, HEAD);
        let held = walk(&mem);
        assert_eq!(held.len(), 1);
        assert_eq!(
            held[0].addr,
            node - 8,
            "the flag is not part of the address"
        );
        assert!(held[0].pi, "and it is not lost either");
    }

    #[test]
    fn each_entry_keeps_its_own_pi_flag() {
        let (a, b) = (HEAD + 0x1000, HEAD + 0x2000);
        let mut mem = memory(a, 0, 0);
        mem.insert(a, b | 1);
        mem.insert(b, HEAD);
        let held = walk(&mem);
        assert!(!held[0].pi, "the first entry was named by the head");
        assert!(held[1].pi, "the second by the first entry's own pointer");
    }

    #[test]
    fn the_pending_entry_is_handled_last_and_only_once() {
        // `list_op_pending` is the lock a thread was part-way through adding
        // or removing when it died, so it may or may not be on the list.
        // Releasing it twice would mark a word the thread no longer owns.
        let (a, b) = (HEAD + 0x1000, HEAD + 0x2000);
        let mut mem = memory(a, 0, b);
        mem.insert(a, b);
        mem.insert(b, HEAD);
        let held = walk(&mem);
        assert_eq!(
            held.iter().map(|f| f.addr).collect::<Vec<_>>(),
            vec![a, b],
            "b appears once, at the end"
        );
    }

    #[test]
    fn a_pending_entry_that_never_made_it_onto_the_list_is_still_released() {
        // The other half of the same race: the thread died before linking it.
        // This is the entry the mechanism exists for -- a lock taken and not
        // yet recorded is exactly what no one else can clean up.
        let pending = HEAD + 0x5000;
        let held = walk(&memory(HEAD, -8, pending));
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].addr, pending - 8);
    }

    #[test]
    fn a_pending_entry_carries_its_own_pi_flag() {
        let pending = HEAD + 0x5000;
        let held = walk(&memory(HEAD, 0, pending | 1));
        assert_eq!(held[0].addr, pending);
        assert!(held[0].pi);
    }

    #[test]
    fn a_circular_list_does_not_keep_the_cpu_forever() {
        // The list is user memory and the kernel walks it at every thread
        // exit. Two entries pointing at each other is all it takes, and a
        // process can build one deliberately. Linux caps the walk at
        // ROBUST_LIST_LIMIT for this reason and so do we.
        let (a, b) = (HEAD + 0x1000, HEAD + 0x2000);
        let mut mem = memory(a, 0, 0);
        mem.insert(a, b);
        mem.insert(b, a);
        assert_eq!(walk(&mem).len(), ROBUST_LIST_LIMIT);
    }

    #[test]
    fn the_limit_is_the_one_linux_uses() {
        // A number, not a constant compared to itself: userspace can rely on
        // a list of this length being walked completely.
        assert_eq!(ROBUST_LIST_LIMIT, 2048);
    }

    #[test]
    fn a_self_referential_entry_stops_at_the_limit_too() {
        let a = HEAD + 0x1000;
        let mut mem = memory(a, 0, 0);
        mem.insert(a, a);
        assert_eq!(walk(&mem).len(), ROBUST_LIST_LIMIT);
    }

    #[test]
    fn an_unreadable_word_ends_the_walk_where_it_is() {
        // Every read is a `get_user` on a dying thread's address space, and
        // the page may already be gone. Linux stops; it does not guess, and
        // it does not give up on the entries it already has.
        let (a, b) = (HEAD + 0x1000, HEAD + 0x2000);
        let mut mem = memory(a, 0, 0);
        mem.insert(a, b);
        // `b` is deliberately absent: reading its `next` fails.
        let held = walk(&mem);
        assert_eq!(
            held.iter().map(|f| f.addr).collect::<Vec<_>>(),
            vec![a, b],
            "both known entries are released before the walk stops"
        );
    }

    #[test]
    fn a_broken_list_does_not_cost_the_pending_entry_its_release() {
        // Linux returns outright when a `next` read fails, pending entry and
        // all (`if (rc) return;` in `exit_robust_list`). We go on to the
        // pending one instead, on purpose: its address came from a read of
        // the head that *did* succeed, a failed read somewhere down the list
        // says nothing about it, and it is the entry most likely to be held
        // and unrecorded -- which is the whole case this mechanism exists
        // for. Marking a word this thread owns costs nothing if it is wrong.
        let (a, b, pending) = (HEAD + 0x1000, HEAD + 0x2000, HEAD + 0x5000);
        let mut mem = memory(a, 0, pending);
        mem.insert(a, b);
        // `b`'s own `next` is unreadable, so the walk stops there.
        let held = walk(&mem);
        assert_eq!(
            held.iter().map(|f| f.addr).collect::<Vec<_>>(),
            vec![a, b, pending]
        );
    }

    #[test]
    fn an_unreadable_head_releases_nothing() {
        // Each of the head's three words is a separate read, and any of them
        // can fail. None of them may be guessed: an offset read as zero would
        // mark the list node instead of the lock.
        for missing in [HEAD, HEAD + W, HEAD + 2 * W] {
            let mut mem = memory(HEAD + 0x1000, -8, 0);
            mem.insert(HEAD + 0x1000, HEAD);
            mem.remove(&missing);
            assert!(
                walk(&mem).is_empty(),
                "a head with {:#x} unreadable must release nothing",
                missing
            );
        }
    }

    // ---- what to write into each word -----------------------------------

    const WAITERS: i32 = 0x8000_0000_u32 as i32;
    const OWNER_DIED: i32 = 0x4000_0000;

    #[test]
    fn a_lock_owned_by_someone_else_is_left_alone() {
        // A stale list entry, or a lock this thread released without
        // unlinking. Marking it would tell its real owner's waiters that the
        // owner is dead while it is still running.
        assert_eq!(robust_death_action(99, 7, false), RobustDeath::NotOurs);
        assert_eq!(robust_death_action(0, 7, false), RobustDeath::NotOurs);
    }

    #[test]
    fn a_lock_we_hold_is_marked_dead_and_keeps_its_waiters_bit() {
        // The waiters bit has to survive: it is what tells the next unlock
        // that it must come into the kernel at all.
        assert_eq!(
            robust_death_action(7 | WAITERS, 7, false),
            RobustDeath::Died {
                new_value: WAITERS | OWNER_DIED,
                wake: true
            }
        );
        // Uncontended: marked, but there is nobody to wake.
        assert_eq!(
            robust_death_action(7, 7, false),
            RobustDeath::Died {
                new_value: OWNER_DIED,
                wake: false
            }
        );
    }

    #[test]
    fn the_owner_is_read_out_of_the_low_thirty_bits_only() {
        // The top two bits of the word are flags, not part of the tid.
        // Comparing the whole word would leave a contended lock -- the only
        // kind anyone is waiting on -- looking like someone else's.
        assert_eq!(
            robust_death_action(7 | WAITERS | OWNER_DIED, 7, false),
            RobustDeath::Died {
                new_value: WAITERS | OWNER_DIED,
                wake: true
            },
            "a lock already marked dead is marked again, waiters woken again"
        );
    }

    #[test]
    fn the_highest_possible_tid_is_still_recognised() {
        // The tid mask is 30 bits. A thread whose id has bit 30 or 31 set
        // would otherwise never match its own locks.
        let tid = 0x3fff_ffff;
        assert!(matches!(
            robust_death_action(tid, tid, false),
            RobustDeath::Died { .. }
        ));
    }

    #[test]
    fn a_priority_inheriting_lock_is_marked_but_not_woken_here() {
        // The PI unlock path hands the lock to a chosen waiter rather than
        // letting them race. A wake from here would put a second thread into
        // a queue that has already picked its winner.
        assert_eq!(
            robust_death_action(7 | WAITERS, 7, true),
            RobustDeath::Died {
                new_value: WAITERS | OWNER_DIED,
                wake: false
            }
        );
    }

    #[test]
    fn the_three_lock_word_bits_are_the_ones_the_libcs_use() {
        // These are the uAPI: glibc and musl write and read the same word.
        assert_eq!(OWNER_DIED, 1 << 30);
        assert_eq!(WAITERS, 1i32 << 31);
        // And the mask is everything below them.
        assert_eq!(
            robust_death_action(0x3fff_ffff, 0x3fff_ffff, false),
            RobustDeath::Died {
                new_value: OWNER_DIED,
                wake: false
            }
        );
    }

    #[test]
    fn the_head_is_the_three_words_userspace_writes() {
        // `struct robust_list_head` is filled in by the libc and read back by
        // offset: a field out of place reads the offset as the head pointer.
        assert_eq!(core::mem::size_of::<RobustList>(), 3 * W);
        let head = RobustList {
            head: 0x1111_1111_1111_1111,
            off: -16,
            pending: 0x3333_3333_3333_3333,
        };
        let bytes: [u8; 24] = unsafe { core::mem::transmute(head) };
        let word = |at: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[at..at + 8]);
            usize::from_ne_bytes(w)
        };
        assert_eq!(word(0), 0x1111_1111_1111_1111);
        assert_eq!(word(8) as isize, -16);
        assert_eq!(word(16), 0x3333_3333_3333_3333);
    }
}
