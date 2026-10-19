//! Linux Thread

use crate::error::SysResult;
use crate::signal::{SigInfo, Signal, SignalStack, SignalUserContext, Sigset};
use crate::sync::shared_futex;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use kernel_hal::context::{UserContext, UserContextField};
use kernel_hal::sync::{Mutex, MutexGuard};
use kernel_hal::user::{Out, UserInPtr, UserOutPtr, UserPtr};
use zircon_object::object::KernelObject;
use zircon_object::task::{CurrentThread, Process, Thread};
use zircon_object::{ZxError, ZxResult};

/// Thread extension for linux
pub trait ThreadExt {
    /// Create the FIRST thread of a program, which has no creating thread to
    /// take anything from. Everything else goes through
    /// [`create_linux_with`](Self::create_linux_with).
    fn create_linux(proc: &Arc<Process>) -> ZxResult<Arc<Self>>;
    /// Create a thread carrying `linux`, which a creating thread built with
    /// [`LinuxThread::child_of`].
    fn create_linux_with(proc: &Arc<Process>, linux: LinuxThread) -> ZxResult<Arc<Self>>;
    /// lock and get Linux thread
    fn lock_linux(&self) -> LinuxThreadGuard<'_>;
    /// Like [`lock_linux`](Self::lock_linux) but returns `None` instead of
    /// panicking when the extension is not a `Mutex<LinuxThread>`. Use this when
    /// walking another process's threads (signal delivery, enumeration): a
    /// thread observed mid-teardown during SMP churn must be skipped, not bring
    /// down the kernel.
    fn try_lock_linux(&self) -> Option<LinuxThreadGuard<'_>>;
    /// Whether [`LinuxThread::handle_signal`] would deliver something,
    /// answered **without taking the thread's lock**.
    ///
    /// The return to user mode asks this on every syscall of every thread;
    /// see [`LinuxThreadExt`] for what it costs to ask it the other way.
    /// A thread whose ext is not a Linux one (or is being torn down) reads
    /// as "nothing", which is what the old code did with `try_lock_linux`.
    fn has_deliverable_signal(&self) -> bool;
    /// Set pointer to thread ID.
    fn set_tid_address(&self, tidptr: UserOutPtr<i32>);
    /// The word `set_tid_address` (or `CLONE_CHILD_CLEARTID`) registered:
    /// what thread exit zeroes and wakes, null when nothing was registered.
    fn tid_address(&self) -> UserOutPtr<i32>;
    /// Get robust list.
    fn get_robust_list(
        &self,
        _head_ptr: UserOutPtr<UserOutPtr<RobustList>>,
        _len_ptr: UserOutPtr<usize>,
    ) -> SysResult;
    /// Set robust list.
    fn set_robust_list(&self, head: UserInPtr<RobustList>, len: usize);
    /// `execve` from a thread that is not the leader, once `de_thread` has
    /// left it alone in its process: the thread the new image is to run in.
    ///
    /// Linux's `de_thread()` gives the exec'ing thread the leader's pid, so
    /// the new image starts with `gettid() == getpid()` whichever thread
    /// called `execve`. A tid here is the thread object's own id, which
    /// nothing changes, so the new image runs in a new thread made with the
    /// pid as its id, and this thread's Linux state -- mask, pending signals,
    /// name, timer slack, I/O priority -- goes across to it, with its CPU
    /// affinity and scheduling attributes. This thread keeps a fresh state
    /// and is left to exit. `BAD_STATE`, and nothing moved, while any other
    /// thread is still in the list: the old leader, still on its way out,
    /// would be a second thread with the same tid.
    fn successor_as_leader(&self) -> ZxResult<Arc<Thread>>;
}

/// CurrentThread extension for linux
pub trait CurrentThreadExt {
    /// exit linux thread
    fn exit_linux(&self, exit_code: i32);
    /// Release every robust lock this thread still holds, and forget the list
    /// that named them.
    ///
    /// Linux does this for EVERY task on its way out, whatever ends it
    /// (`do_exit` -> `exit_mm` -> `futex_exit_release`), and for a task that
    /// `execve`s (`exec_mm_release` -> `futex_exec_release`). The thread's
    /// own `exit(2)` is only one of those ways: `exit_group`, a fatal signal
    /// and the sibling `execve` kills are the others, and they are how a
    /// PROCESS dies -- which is the death a robust lock exists to survive.
    ///
    /// Must run on the dying thread itself, with its address space still in
    /// place: the list is walked through user pointers. Calling it again is a
    /// no-op, because the list is taken, not read.
    fn release_robust_locks(&self);
}

/// The `Thread::ext` fat pointer's DATA word has been overwritten, and the
/// `downcast_ref` just handed out a `&Mutex<LinuxThread>` built on it.
///
/// Same blind spot as [`ext_data_word_overwritten`][crate::process] on the
/// process side: the `TypeId` a downcast checks lives in the vtable word, so a
/// data word replaced on its own leaves the downcast succeeding and the
/// reference pointing at an address nobody built the object at. Here the very
/// next thing is `.lock()`, which writes -- so the symptom is a page fault at
/// that address plus the mutex's own offset, inside `TicketMutex::lock`, with
/// nothing on screen about the thread or the field.
///
/// Nothing here dereferences the ext.
#[cold]
#[inline(never)]
fn thread_ext_data_word_overwritten(thread: &Thread) -> ! {
    let (data, vtable) = thread.ext_fat();
    let (born_data, born_vtable) = thread.ext_born();
    panic!(
        "Thread::lock_linux(): tid={} pid={} -- the ext DATA word was overwritten and the \
         downcast could not see it (the TypeId lives in the vtable word, which is {}). \
         fat data={:#x} vtable={:#x}, at birth data={:#x} vtable={:#x} -> {}; canaries {}. \
         Refusing to lock a Mutex<LinuxThread> at {:#x}: acquiring it would fault at \
         {:#x}+offset and halt the machine naming the mutex instead of this thread",
        thread.id(),
        thread.proc().id(),
        if vtable == born_vtable {
            "intact"
        } else {
            "also moved"
        },
        data,
        vtable,
        born_data,
        born_vtable,
        thread.ext_drift().describe(),
        match thread.ext_canaries() {
            (true, true) => "both INTACT: a precise write to ext alone",
            (false, true) => "LOW broken: overrun growing upward from below",
            (true, false) => "HIGH broken: overrun growing downward from above",
            (false, false) => "BOTH broken: wide overrun across the field",
        },
        data,
        data,
    )
}

/// The Linux side of a thread: its state behind its own lock, plus the one
/// question the **return to user mode** asks about it, published outside.
///
/// `run_user` calls `handle_signal` on every syscall return of every thread,
/// and the answer is "nothing to deliver" on all but a handful of returns in
/// a program's life. Measured on the host
/// (`process::exit_path_benches::the_thread_lock`), that question cost
/// 11.3 ns, of which the body is 1.6:
///
/// ```text
/// handle_signal_through_the_thread_lock      11.31
/// handle_signal_with_the_lock_already_held    1.58
/// lock_linux_and_drop                        11.05
///   the_downcast_alone                        1.83
///   the_drift_check_alone                     0.63
///   an_uncontended_mutex_lock                 6.53
/// ```
///
/// So nine of those eleven nanoseconds are the *asking*, not the answer, and
/// a thread's lock is uncontended by construction -- this is a fixed toll on
/// every syscall, not a contention problem.
///
/// [`deliverable`][Self::deliverable] is that answer, as a word any CPU can
/// read without taking anything. What makes it safe is not a promise that
/// each mutator refreshes it -- `signals` is a public field with writers in
/// three files -- but **where** the refresh lives: it is the `Drop` of
/// [`LinuxThreadGuard`], the only way to reach the state at all. Every
/// mutation path holds that guard, so republishing happens on the way out
/// whether the mutator remembered or not, and it happens **before** the
/// mutex is released, because fields drop after the `Drop::drop` body. A
/// panic inside a mutator still unwinds through it.
///
/// A reader without the lock can see a value one store old, never one that
/// was never true: the same window Linux has with `TIF_SIGPENDING`, and the
/// one this path already had, since the check happens on the next return to
/// user mode either way.
pub struct LinuxThreadExt {
    /// `signals & !signal_mask`: nonzero means `handle_signal` has
    /// something. Written only by [`LinuxThreadGuard::drop`].
    deliverable: AtomicU64,
    /// The state itself.
    inner: Mutex<LinuxThread>,
}

impl LinuxThreadExt {
    /// Wraps `linux`, publishing what it starts out with.
    ///
    /// No production path creates a thread with a signal already pending --
    /// [`LinuxThread::child_of`] carries the mask over and leaves `signals`
    /// empty, as `clone(2)` does -- so storing a plain zero here would
    /// behave identically, and a mutation that does exactly that passes all
    /// 3012 tests: it is an **equivalent** mutant, and no unit test can
    /// catch it. It derives the word from the state anyway, because the day
    /// a new flag makes a child inherit a pending signal, a hardcoded zero
    /// publishes a lie and this does not.
    pub fn new(linux: LinuxThread) -> Self {
        let deliverable = AtomicU64::new(linux.deliverable_signals());
        Self {
            deliverable,
            inner: Mutex::new(linux),
        }
    }

    /// What the return to user mode reads, taking no lock.
    pub fn deliverable(&self) -> u64 {
        self.deliverable.load(Ordering::Acquire)
    }
}

/// A locked [`LinuxThread`] that republishes
/// [`LinuxThreadExt::deliverable`] when it is dropped.
///
/// Derefs to `LinuxThread`, so it is used exactly like the `MutexGuard` it
/// replaced. The point of the type is its `Drop`: see [`LinuxThreadExt`].
pub struct LinuxThreadGuard<'a> {
    guard: MutexGuard<'a, LinuxThread>,
    ext: &'a LinuxThreadExt,
}

impl core::ops::Deref for LinuxThreadGuard<'_> {
    type Target = LinuxThread;
    fn deref(&self) -> &LinuxThread {
        &self.guard
    }
}

impl core::ops::DerefMut for LinuxThreadGuard<'_> {
    fn deref_mut(&mut self) -> &mut LinuxThread {
        &mut self.guard
    }
}

impl Drop for LinuxThreadGuard<'_> {
    fn drop(&mut self) {
        // Still holding the lock: `self.guard` is a field, and fields are
        // dropped after this body runs. A reader that sees this store has
        // seen a value that was true at the moment the lock was released.
        self.ext
            .deliverable
            .store(self.guard.deliverable_signals(), Ordering::Release);
    }
}

impl ThreadExt for Thread {
    fn create_linux(proc: &Arc<Process>) -> ZxResult<Arc<Self>> {
        Self::create_linux_with(proc, LinuxThread::initial())
    }

    fn successor_as_leader(&self) -> ZxResult<Arc<Thread>> {
        let proc = self.proc();
        if proc.thread_ids() != [self.id()] {
            return Err(ZxError::BAD_STATE);
        }
        let leader = Thread::create_with_ext_id(
            proc,
            &self.name(),
            LinuxThreadExt::new(LinuxThread::initial()),
            Some(proc.id()),
        )?;
        core::mem::swap(&mut *self.lock_linux(), &mut *leader.lock_linux());
        // A mask with every CPU this one may use is one the setter takes.
        let _ = leader.set_affinity(self.affinity());
        leader.set_sched(
            self.sched_policy(),
            self.sched_nice(),
            self.sched_rt_priority(),
        );
        Ok(leader)
    }

    fn create_linux_with(proc: &Arc<Process>, linux: LinuxThread) -> ZxResult<Arc<Self>> {
        let linux_thread = LinuxThreadExt::new(linux);
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

    fn lock_linux(&self) -> LinuxThreadGuard<'_> {
        // See Process::linux(): a failed downcast means a non-Linux thread
        // leaked into a Linux-only path or the ext Box was corrupted. Identify
        // the thread/process so the panic names the culprit.
        let m = self
            .ext()
            .downcast_ref::<LinuxThreadExt>()
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
                if let Some(m) = self.ext().downcast_ref::<LinuxThreadExt>() {
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
                    core::mem::size_of::<LinuxThreadExt>(),
                    core::mem::align_of::<LinuxThreadExt>(),
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
            });
        // The downcast vouches for the vtable word only -- see
        // `thread_ext_data_word_overwritten`. Ask about the data word BEFORE
        // `.lock()` writes through it.
        if self.ext_drift().data_moved() {
            thread_ext_data_word_overwritten(self);
        }
        LinuxThreadGuard {
            guard: m.inner.lock(),
            ext: m,
        }
    }

    fn has_deliverable_signal(&self) -> bool {
        self.ext()
            .downcast_ref::<LinuxThreadExt>()
            .is_some_and(|m| m.deliverable() != 0)
    }

    fn try_lock_linux(&self) -> Option<LinuxThreadGuard<'_>> {
        let m = self.ext().downcast_ref::<LinuxThreadExt>()?;
        // Not a `None` case: the ext IS a Mutex<LinuxThread>, its address is
        // what moved, and answering "no Linux thread" would hide the write.
        if self.ext_drift().data_moved() {
            thread_ext_data_word_overwritten(self);
        }
        Some(LinuxThreadGuard {
            guard: m.inner.lock(),
            ext: m,
        })
    }

    /// Set pointer to thread ID.
    fn set_tid_address(&self, tidptr: UserPtr<i32, Out>) {
        self.lock_linux().clear_child_tid = tidptr;
    }

    fn tid_address(&self) -> UserPtr<i32, Out> {
        UserPtr::from(self.lock_linux().clear_child_tid.as_addr())
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

/// [`CurrentThreadExt::release_robust_locks`], for a `Thread` a host test can
/// build: the walk only needs the thread's id, its list and its process.
fn release_robust_locks_of(thread: &Thread) {
    // Taken, not read: Linux's `futex_cleanup` clears `tsk->robust_list`
    // once it has walked it, so the walk happens once per thread however
    // many of the death paths reach here. `exit(2)` releases first and then
    // falls out of the run loop, which releases again.
    let (head_addr, len) = {
        // `try_`: a thread with no Linux extension holds no robust locks.
        let Some(mut linux) = thread.try_lock_linux() else {
            return;
        };
        let taken = (linux.robust_list.as_addr(), linux.robust_list_len);
        linux.robust_list = 0.into();
        linux.robust_list_len = 0;
        taken
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
    let vmar = thread.proc().vmar();
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
    let tid = thread.id() as i32;
    let proc = thread.proc();
    for lock in held {
        // Linux's `handle_futex_death` wakes with shared flags, so this is
        // the queue a waiter in another process sleeps on when the lock is
        // a process-shared one in shared memory. The per-process table it
        // used to wake is a queue nobody in that process was on.
        let Some(futex) = shared_futex::futex_for_shared_op(proc, lock.addr) else {
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

impl CurrentThreadExt for CurrentThread {
    fn release_robust_locks(&self) {
        release_robust_locks_of(self);
    }

    /// Exit current thread for Linux.
    fn exit_linux(&self, exit_code: i32) {
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
                        // `mm_release` wakes with shared flags too: see
                        // `futex_for_shared_op`.
                        if let Some(futex) = shared_futex::futex_for_shared_op(self.proc(), vaddr) {
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
                    if let Some(futex) = shared_futex::futex_for_shared_op(self.proc(), uaddr) {
                        futex.wake(1);
                    }
                }
            }
        }
        drop(linux_thread);
        // `exit(2)` from the LAST thread of a process is the process's exit,
        // and its code is the process's code. Publish the hint first: two
        // concurrent `_exit` calls can both see `thread_count() == 2` and
        // neither win the `last_thread_of` race, so without the hint
        // `remove_thread` → `terminate` invents `Exited(0)`.
        // (`exit_group` sets the status itself, and a status already set is
        // kept, so this changes nothing for the ordinary path.)
        self.proc().hint_exit_code(exit_code as i64);
        if last_thread_of(self.proc().thread_count()) {
            self.proc().exit(exit_code as i64);
        }
        self.exit();
    }
}

/// Whether the thread now exiting is the last one its process has, counting
/// itself: the one whose exit code becomes the PROCESS's exit code.
///
/// A separate function because the count is read without the process lock and
/// the off-by-one is the whole of the decision: `thread_count()` still counts
/// the caller, so "last" is 1, not 0.
pub fn last_thread_of(thread_count: usize) -> bool {
    thread_count <= 1
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

/// The three fields of a robust futex word, from `linux/futex.h`: the top bit
/// says a thread is waiting, the next one that the owner died without
/// releasing it, and the thirty below them hold the owner's tid.
///
/// They are the uAPI. glibc's `__pthread_mutex_lock` and musl's equivalent
/// write and read the same word, so a number out of step here does not break
/// a test, it breaks every robust mutex in userspace.
const FUTEX_WAITERS: i32 = 0x8000_0000_u32 as i32;
const FUTEX_OWNER_DIED: i32 = 0x4000_0000;
const FUTEX_TID_MASK: i32 = 0x3fff_ffff;

/// Linux's `handle_futex_death`, without the memory access.
///
/// Userspace cannot do this itself: the whole point is that the owner is gone.
/// The next thread to take the lock sees `FUTEX_OWNER_DIED` and gets
/// `EOWNERDEAD` from `pthread_mutex_lock`, its chance to repair whatever the
/// dead owner left half-done. Without the mark, every waiter stays blocked in
/// the kernel for as long as the process lives.
pub fn robust_death_action(uval: i32, tid: i32, pi: bool) -> RobustDeath {
    // The word's low thirty bits against the RAW tid, as `handle_futex_death`
    // compares them. Masking the tid as well would make every pair of tids
    // 0x4000_0000 apart alias, and these tids are kernel object ids -- a
    // counter with none of the `PID_MAX_LIMIT` cap that keeps a Linux tid
    // inside the mask. The thread that aliased would mark a lock its owner
    // still holds as owner-dead, and the waiters would get `EOWNERDEAD` from
    // a lock nobody abandoned.
    if uval & FUTEX_TID_MASK != tid {
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
    /// The signals this thread is parked in `rt_sigtimedwait(2)` for, empty
    /// otherwise. They are BLOCKED (POSIX has the caller block them, and the
    /// wait is what consumes them), yet a process-directed signal must land
    /// on this thread and not on whichever thread comes first: Linux keeps
    /// such a signal in the shared pending set, where any thread's
    /// `sigwait` dequeues it, and unblocks the set for the length of the
    /// wait so `wants_signal()` picks the waiter. See [`Self::wants_signal`].
    pub sigwait: Sigset,
    /// signal alternate stack
    pub signal_alternate_stack: SignalStack,
    /// robust_list
    robust_list: UserInPtr<RobustList>,
    robust_list_len: usize,
    /// The signal most recently handed to a handler, for `/proc` and for
    /// `sigreturn`'s own bookkeeping. INFORMATIONAL: it gates nothing.
    ///
    /// It used to gate [`Self::handle_signal`], which delivered nothing at
    /// all while it was set -- and it is set from handler entry until the
    /// `sigreturn`. A thread inside a handler therefore took no other signal,
    /// however long the handler ran, while `check_signals_of` went on
    /// reporting that second signal as deliverable and answering `EINTR` to
    /// every blocking syscall. The signal was never delivered, so the `EINTR`
    /// repeated for ever: a livelock with the process making no progress and
    /// burning a core. Measured on a hung Firefox (12-oct): **3.056.442 of
    /// 3.130.607 `epoll_pwait` calls, 97.6 %, returned `EINTR`**, with the
    /// same storm in `futex` behind it.
    ///
    /// What stops a signal re-entering its own handler is the MASK, as in
    /// Linux: `handler_mask` adds `sa_mask` and the signal itself (unless
    /// `SA_NODEFER`) on entry, and `sigreturn` puts the old mask back. That
    /// is already done (see `handle_signal` in `loader/src/linux.rs`), which
    /// is why the gate was redundant for its stated purpose and fatal for
    /// every other signal.
    pub handling_signal: Option<u32>,
    /// Thread name (`prctl(PR_SET_NAME)` / `/proc/<pid>/comm`), at most
    /// [`TASK_COMM_LEN`]` - 1` bytes. Empty = never set: readers fall back to
    /// the executable's basename, so a fresh thread reports its program name.
    pub comm: String,
    /// Timer slack in nanoseconds (`prctl(PR_SET_TIMERSLACK)`). `0` = never
    /// set → reads as the Linux default of 50 µs. Recorded and read back;
    /// timers here do not apply slack coalescing.
    pub timerslack_ns: u64,
    /// I/O priority (`ioprio_set(2)`), as the `(class << 13) | level` word
    /// userspace passes; `0` (`IOPRIO_CLASS_NONE`) = never set → reads as the
    /// best-effort level derived from the nice value, as Linux's
    /// `get_task_ioprio` does. Recorded and read back: there is no I/O
    /// scheduler here to act on it, but `ionice` must see what it set.
    pub ioprio: u16,
    /// The `siginfo_t` that goes with each pending signal in `signals`, when
    /// the sender left one: who sent a `kill`, which child a `SIGCHLD` is
    /// about and how it ended. `signals` is a bitmap, so a signal is pending
    /// once and, as for Linux's non-realtime signals, the FIRST sender's
    /// information is the one kept. Read and cleared by [`Self::take_siginfo`].
    pending_info: BTreeMap<u8, SigInfo>,
    /// The `siginfo_t` handed to the handler now running, so `sigreturn` can
    /// tell what the handler changed from what the kernel wrote.
    handling_info: SigInfo,
}

/// Size of the kernel's per-task `comm` buffer, including the trailing NUL
/// (`TASK_COMM_LEN` in `include/linux/sched.h`): names are truncated to 15
/// bytes.
pub const TASK_COMM_LEN: usize = 16;

/// `IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0)`: the io priority `ionice -c3`
/// sets, for fixtures that must not sit at the never-set default.
#[cfg(test)]
const IDLE_IOPRIO: u16 = 3 << 13;

fn unmodified_check(siginfo: &SigInfo, delivered: &SigInfo, user_ctx: &SignalUserContext) -> usize {
    let mut check = 0usize;
    let mut default_ctx = SignalUserContext::default();
    default_ctx.context.set_pc(user_ctx.context.get_pc());
    check |= (*siginfo != *delivered) as usize;
    check |= ((user_ctx.flags != default_ctx.flags) as usize) << 1;
    check |= ((user_ctx.link != default_ctx.link) as usize) << 2;
    // `uc_stack` is not compared: the kernel fills it (`__save_altstack`)
    // and the handler may change it, which `sigreturn` honours.
    check |= ((user_ctx._pad != default_ctx._pad) as usize) << 4;
    // `uc_mcontext` is not compared either, for the same reason as `uc_stack`:
    // the kernel fills it with the interrupted registers and `sigreturn`
    // honours whatever the handler left there. It used to be compared against
    // a struct holding only the program counter, so every ordinary signal
    // logged "unsupported signal fields" at `error!` -- and once the registers
    // were actually filled in, every single one would have.
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
        let check = unmodified_check(siginfo, &self.handling_info, user_ctx);
        // This gate decides one log line and nothing else -- see the
        // tolerance note below -- so the mutation that turns it into
        // `check == 0` is an equivalent one for any test that does not
        // capture the log, and it survives on purpose. What the tests pin is
        // the WORD: `unmodified_check`'s bits, one per field, since the bit
        // positions are the whole of what a reader of that line can use.
        if check != 0 {
            error!("unsupported signal fields : {:b}", check);
            trace!("uctx = {:x?}", *user_ctx);
            // Be tolerant: userland may legally modify parts of ucontext/siginfo.
            // We restore the saved context and only honor the restored PC/mask below.
        }
        // `restore_altstack()`: judged from the handler's own stack pointer,
        // the one it is making the `sigreturn` call with.
        let handler_sp = ctx.get_field(UserContextField::StackPointer);
        self.signal_alternate_stack
            .restore_from_frame(user_ctx.stack, handler_sp);
        *ctx = *old_ctx;
        // Everything the frame carries, not just the program counter: a
        // handler that rewrote a register in its `ucontext` asked for the
        // interrupted code to resume with it (`MachineContext::restore_into`).
        user_ctx.context.restore_into(ctx);
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

    /// Whether a signal aimed at the whole process should be queued to THIS
    /// thread: it has the signal unblocked, or it is parked in
    /// `rt_sigtimedwait` waiting for exactly that signal (`wants_signal()`,
    /// which sees the wait as an unblocked window). Without the second half
    /// a program that blocks a signal in every thread and dedicates one
    /// thread to `sigwait(3)` -- the pattern of dbus-daemon, of glib's
    /// `g_unix_signal_source` helpers, of every Java and Python runtime --
    /// never had its signal reach the waiter: it was queued to the first
    /// thread, where it stayed blocked for good, and the waiter timed out.
    pub fn wants_signal(&self, signal: Signal) -> bool {
        !self.signal_mask.contains(signal) || self.sigwait.contains(signal)
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

    /// The very first thread of a program: nothing carried over, because
    /// there is no creating thread to carry it from.
    pub fn initial() -> LinuxThread {
        LinuxThread {
            clear_child_tid: 0.into(),
            signals: Sigset::default(),
            signal_mask: Sigset::default(),
            saved_sigmask: None,
            sigwait: Sigset::default(),
            signal_alternate_stack: SignalStack::default(),
            robust_list: 0.into(),
            robust_list_len: 0,
            handling_signal: None,
            comm: String::new(),
            timerslack_ns: 0,
            ioprio: 0,
            pending_info: BTreeMap::new(),
            handling_info: SigInfo::default(),
        }
    }

    /// What the main thread of a `fork`ed (or `vfork`ed) child takes from the
    /// thread that forked it. The process half is
    /// [`LinuxProcessInner::forked_child`](crate::process::LinuxProcess).
    pub fn forked_child(&self) -> LinuxThread {
        // Linux's test is `(clone_flags & (CLONE_VM|CLONE_VFORK)) ==
        // CLONE_VM`, and a `vfork` sets BOTH bits, so it lands here with the
        // plain `fork` rather than with the threads.
        self.child_of(false)
    }

    /// What a `pthread_create`d thread takes from the thread that created it.
    pub fn new_thread(&self) -> LinuxThread {
        self.child_of(true)
    }

    /// The one decision behind [`Self::forked_child`] and
    /// [`Self::new_thread`]: what a newly created thread takes from the
    /// thread creating it. `shares_address_space` is Linux's
    /// `(clone_flags & (CLONE_VM|CLONE_VFORK)) == CLONE_VM`.
    ///
    /// Private, and reached through the two named constructors above, because
    /// a bare `true`/`false` at a call site is the kind of thing that gets
    /// written backwards and reviewed straight past.
    ///
    /// The fields are named ONE BY ONE with no `..`, so a field added to
    /// [`LinuxThread`] stops compiling here until someone says which side it
    /// falls on. The mirror of this one is [`Self::reset_for_exec`].
    fn child_of(&self, shares_address_space: bool) -> LinuxThread {
        LinuxThread {
            // --- carried over from the creating thread ------------------
            //
            // sigprocmask(2): "A child created via fork(2) inherits a copy
            // of its parent's signal mask", and pthread_create(3) says the
            // same of a new thread. This is what makes the standard race-free
            // idiom work -- block SIGCHLD, fork, and the child is already
            // protected before its first instruction -- and what stops a
            // library thread from waking up able to take signals its creator
            // had deliberately shut out.
            signal_mask: self.signal_mask,
            // prctl(2): "The timer slack value is inherited by children
            // created via fork(2)"; `copy_process` copies it to a new thread
            // along with the rest of the task struct.
            timerslack_ns: self.timerslack_ns,
            // `copy_io()`: without CLONE_IO the new task gets an io_context
            // of its own with `ioc->ioprio` copied from the creator's, so a
            // child of `ionice -c3 make` is idle-class too; with CLONE_IO it
            // shares the creator's, which reads the same.
            ioprio: self.ioprio,
            // sigaltstack(2): "A child created via fork(2) inherits a copy of
            // its parent's alternate signal stack settings" -- but a THREAD
            // must not, or two threads take their signal frames to the same
            // few pages and the second one lands on top of the first. That is
            // exactly the rule in `copy_process`:
            //     /* sigaltstack should be cleared when sharing the same VM */
            //     if ((clone_flags & (CLONE_VM|CLONE_VFORK)) == CLONE_VM)
            //             sas_ss_reset(p);
            signal_alternate_stack: if shares_address_space {
                SignalStack::default()
            } else {
                self.signal_alternate_stack
            },

            // --- fresh on purpose ---------------------------------------
            //
            // fork(2): "The child's set of pending signals is initially
            // empty"; `init_sigpending` does the same for a new thread. A
            // signal is delivered once, to whoever it was aimed at.
            signals: Sigset::default(),
            // `copy_process`: `p->robust_list = NULL`. The list names locks
            // THIS thread holds, and the new one holds none -- inheriting it
            // would have the child release, on its own death, locks its
            // parent is still using.
            robust_list: 0.into(),
            robust_list_len: 0,
            // `p->clear_child_tid = (clone_flags & CLONE_CHILD_CLEARTID) ?
            // child_tidptr : NULL`. The clone path installs it afterwards
            // when the flag is there; a plain fork leaves it null.
            clear_child_tid: 0.into(),
            // A `fork` from inside a handler does leave the child inside it,
            // but what keeps a second signal out of a running handler is the
            // MASK above -- sigaction(2) blocks the handler's own signal for
            // its duration -- and the child does inherit that. This latch
            // belongs to a `sigreturn` frame, and the child has its own.
            handling_signal: None,
            saved_sigmask: None,
            sigwait: Sigset::default(),
            // Pending signals are fresh (above), and so is what went with them.
            pending_info: BTreeMap::new(),
            handling_info: SigInfo::default(),
            // A new thread reports the program's name until it sets one of
            // its own; empty is how a reader knows to fall back to the
            // executable's basename.
            comm: String::new(),
        }
    }

    /// What `execve` must make this thread forget.
    ///
    /// Every field reset here either names an address in the image `execve`
    /// is about to destroy, or is handler state whose handler is going with
    /// it. Linux does the same in `begin_new_exec()` (`sas_ss_reset(me)`,
    /// `me->robust_list = NULL`) and in `exec_mm_release()` -> `mm_release()`
    /// (`tsk->clear_child_tid = NULL`).
    ///
    /// The fields are named ONE BY ONE with no `..`, so a field added to
    /// [`LinuxThread`] stops compiling here until someone says whether
    /// `execve` keeps it. The process half is
    /// [`LinuxProcess::reset_for_exec`](crate::process::LinuxProcess::reset_for_exec).
    pub fn reset_for_exec(&mut self) {
        let Self {
            clear_child_tid,
            signals: _,
            signal_mask: _,
            saved_sigmask,
            // A thread inside `execve` is not inside `rt_sigtimedwait`.
            sigwait: _,
            signal_alternate_stack,
            robust_list,
            robust_list_len,
            handling_signal,
            comm,
            timerslack_ns: _,
            // The io_context is not part of the image: `ionice -c3 cmd` is
            // an `ioprio_set` followed by an `execve`, and it must stick.
            ioprio: _,
            // Pending signals survive `execve`, and what went with them too.
            pending_info: _,
            handling_info: _,
        } = self;

        // `mm_release()`: the address the kernel writes a 0 into, and
        // futex-wakes, when this thread dies. It points into the image being
        // replaced, so past the exec it names whatever the NEW image happens
        // to have put at that address.
        *clear_child_tid = 0.into();

        // `begin_new_exec()`. Same story and worse: this one is the head of a
        // chain of user addresses the kernel walks AND compare-exchanges at
        // thread exit (see [`walk_robust_list`]). A thread that has just
        // replaced its address space holds no locks in it.
        *robust_list = 0.into();
        *robust_list_len = 0;

        // `sas_ss_reset(me)` in `begin_new_exec()`, which is exactly what
        // `SignalStack::default()` is: sp and size zero, `SS_DISABLE` set.
        // Left stale, the next `SA_ONSTACK` signal puts its frame at an
        // address that belonged to the old image.
        *signal_alternate_stack = SignalStack::default();

        // A thread may `execve` from inside a signal handler. The `sigreturn`
        // that would clear these is in the image that just went away, so
        // without this the new program starts life mid-handler:
        // `handle_signal` delivers NOTHING while `handling_signal` is set --
        // Ctrl-C included, for good -- and a stale `saved_sigmask` would be
        // installed as the new program's mask by the first handler to return.
        *handling_signal = None;
        *saved_sigmask = None;

        // execve(2) resets the name to the new image's basename: empty means
        // "never set", which is how readers fall back to it.
        comm.clear();

        // Kept on purpose, because execve(2) says so:
        // - `signals`: the set of pending signals is preserved.
        // - `signal_mask`: the signal mask is preserved.
        // - `timerslack_ns`: prctl(2) puts PR_SET_TIMERSLACK among the
        //   settings that survive both fork and exec, and `begin_new_exec()`
        //   never touches it.
    }

    /// Handle signal
    /// Make `signal` pending, with what the sender knows about it.
    ///
    /// A signal already pending keeps the information it came with, as Linux
    /// does for the non-realtime signals: the second `kill` is not queued and
    /// the handler learns about the first.
    pub fn queue_signal(&mut self, signal: Signal, info: Option<SigInfo>) {
        self.signals.insert(signal);
        if let Some(info) = info {
            self.pending_info.entry(signal as u8).or_insert(info);
        }
    }

    /// Take `signal` out of the pending set together with its `siginfo_t`,
    /// which is [`SigInfo::bare`] when the sender left none. What is
    /// returned is also what `sigreturn` will compare the handler's frame
    /// against.
    pub fn take_siginfo(&mut self, signal: Signal) -> SigInfo {
        self.signals.remove(signal);
        let info = self
            .pending_info
            .remove(&(signal as u8))
            .unwrap_or_else(|| SigInfo::bare(signal));
        self.handling_info = info;
        info
    }

    /// The signals this thread would take **right now**: pending and not
    /// blocked. Zero means [`Self::handle_signal`] would answer `None`.
    ///
    /// This is the one question the return to user mode asks, and the whole
    /// of what [`LinuxThreadExt::deliverable`] publishes. It is derived from
    /// the same two fields `handle_signal` consults, and from nothing else,
    /// so the two cannot drift apart: a signal that becomes deliverable
    /// without this word changing would have to be deliverable without
    /// `handle_signal` seeing it either.
    pub fn deliverable_signals(&self) -> u64 {
        self.signals.mask_with(&self.signal_mask).val()
    }

    pub fn handle_signal(&mut self) -> Option<(Signal, Sigset)> {
        // The mask decides, and nothing else: a signal a handler blocked is
        // not in `mask_with`, and the signal being handled blocks itself
        // through `handler_mask`. There is NO "one handler at a time" gate --
        // see [`Self::handling_signal`] for the livelock that was.
        let signal = self
            .signals
            .mask_with(&self.signal_mask)
            .find_first_signal()?;
        self.handling_signal = Some(signal as u32);
        self.signals.remove(signal);
        // If a `rt_sigsuspend` (or similar) saved a mask to restore once
        // the handler returns, hand that mask to the signal frame so it
        // is reinstated on `sigreturn`. Otherwise keep the current mask --
        // which, for a handler entered from inside another handler, is the
        // OUTER handler's mask, so the inner `sigreturn` lands back in it.
        let restore_mask = self.saved_sigmask.take().unwrap_or(self.signal_mask);
        Some((signal, restore_mask))
    }

    /// The signal [`Self::handle_signal`] returned was taken with NO handler
    /// to run -- ignored, a stop, a default `SIGCONT` -- and `restore_mask`
    /// is the mask it handed out with it.
    ///
    /// `handle_signal` claims `saved_sigmask` for the frame whatever the
    /// signal turns out to be, and with no frame there is no `sigreturn` to
    /// put it back: the temporary mask of an `rt_sigsuspend`, a `ppoll` or a
    /// `pselect6` stayed the thread's mask for good. Linux puts it back the
    /// moment it knows no handler will run (`restore_saved_sigmask()`). With
    /// nothing saved, `restore_mask` is the mask already in place, and this
    /// changes nothing.
    pub fn taken_without_handler(&mut self, restore_mask: Sigset) {
        self.handling_signal = None;
        self.set_signal_mask(restore_mask);
    }
}

#[cfg(test)]
mod deliverable_mirror_tests;
#[cfg(test)]
mod robust_exit_tests;
#[cfg(test)]
mod successor_tests;

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
            sigwait: Sigset::default(),
            signal_alternate_stack: SignalStack::default(),
            robust_list: 0.into(),
            robust_list_len: 0,
            handling_signal: None,
            comm: String::new(),
            timerslack_ns: 0,
            ioprio: IDLE_IOPRIO,
            pending_info: BTreeMap::new(),
            handling_info: SigInfo::default(),
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
    fn a_second_signal_is_delivered_while_another_handler_runs() {
        // THE LIVELOCK. Delivery used to stop dead for the whole length of a
        // handler, while `check_signals_of` went on answering `EINTR` to
        // every blocking syscall for the signal it refused to deliver: the
        // syscall returned `EINTR`, userspace retried, and nothing ever
        // changed. 97.6 % of a hung Firefox's `epoll_pwait` calls.
        let mut t = thread();
        t.signals.insert(Signal::SIGUSR1);
        let (first, _) = t.handle_signal().expect("SIGUSR1 was pending");
        assert_eq!(first, Signal::SIGUSR1);
        // What the loader does on handler entry (`handler_mask`): block the
        // signal being handled.
        t.block_signals(&one(Signal::SIGUSR1));

        t.signals.insert(Signal::SIGTERM);
        let (second, _) = t
            .handle_signal()
            .expect("a signal that arrives during a handler has to be delivered, as in Linux");
        assert_eq!(second, Signal::SIGTERM);
        assert!(
            !t.signals.contains(Signal::SIGTERM),
            "left pending, every blocking syscall keeps answering EINTR for it and the \
             thread never gets past it"
        );
    }

    #[test]
    fn the_handled_signal_is_not_redelivered_because_its_own_mask_blocks_it() {
        // The other half of the one above: with no gate, re-entry is stopped
        // by the mask `handler_mask` installed, exactly as Linux stops it.
        let mut t = thread();
        t.signals.insert(Signal::SIGUSR1);
        t.handle_signal().expect("SIGUSR1 was pending");
        t.block_signals(&one(Signal::SIGUSR1));

        t.signals.insert(Signal::SIGUSR1);
        assert!(
            t.handle_signal().is_none(),
            "the handler's own mask is what keeps the signal out of its own handler"
        );
        assert!(
            t.signals.contains(Signal::SIGUSR1),
            "and it waits there for the sigreturn, it is not lost"
        );
    }

    #[test]
    fn a_nested_delivery_restores_the_outer_handlers_mask() {
        // The inner frame carries the mask in force when it was built, which
        // is the outer handler's -- so the inner `sigreturn` lands back
        // inside the outer handler with its `sa_mask` still up, not with the
        // mask from before any signal arrived.
        let mut t = thread();
        t.signals.insert(Signal::SIGUSR1);
        let (_, outer_restore) = t.handle_signal().unwrap();
        assert!(
            outer_restore.is_empty(),
            "nothing was blocked before the first signal"
        );
        t.block_signals(&one(Signal::SIGUSR1));

        t.signals.insert(Signal::SIGTERM);
        let (_, inner_restore) = t.handle_signal().unwrap();
        assert!(
            inner_restore.contains(Signal::SIGUSR1),
            "the inner sigreturn would drop the outer handler's sa_mask"
        );
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
    fn frames_cannot_pile_up_because_each_handler_blocks_its_own_signal() {
        // This replaces a test that asserted the opposite -- that NOTHING is
        // delivered while a handler runs -- whose stated reason was that a
        // second frame would be built on a stack that already holds one.
        // That reason is real and Linux answers it the same way this does:
        // not by refusing every signal, but by blocking each one inside its
        // own handler, so a given signal can add at most one frame and the
        // nesting is bounded by the number of distinct unblocked signals.
        // Refusing every signal instead cost a livelock -- see
        // [`LinuxThread::handling_signal`].
        let mut t = thread();
        t.signals.insert(Signal::SIGUSR1);
        assert!(t.handle_signal().is_some());
        t.block_signals(&one(Signal::SIGUSR1));
        t.signals.insert(Signal::SIGUSR2);
        assert!(t.handle_signal().is_some(), "SIGUSR2 nests, as in Linux");
        t.block_signals(&one(Signal::SIGUSR2));

        // Both arrive again while both handlers are on the stack.
        t.signals.insert(Signal::SIGUSR1);
        t.signals.insert(Signal::SIGUSR2);
        assert!(
            t.handle_signal().is_none(),
            "a handler's own signal got back into it and the frames pile up"
        );
        assert!(t.signals.contains(Signal::SIGUSR1), "it waits, not lost");
        assert!(t.signals.contains(Signal::SIGUSR2), "it waits, not lost");
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
        // the frame carries. (No reset of `handling_signal` here: it gates
        // nothing -- see the field.)
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
    fn a_signal_with_no_handler_puts_the_saved_mask_back_at_once() {
        // `sigsuspend(&temp)`, and the signal that ends it is a stop: no
        // frame, so no `sigreturn` to reinstate the mask `handle_signal`
        // took. Left alone, `temp` -- the mask the program wanted for the
        // length of one call -- would be its mask from then on.
        let mut t = thread();
        let mut original = Sigset::empty();
        original.insert(Signal::SIGUSR1);
        t.saved_sigmask = Some(original);
        t.signals.insert(Signal::SIGSTOP);

        let (sig, restore) = t.handle_signal().unwrap();
        assert_eq!(sig, Signal::SIGSTOP);
        t.taken_without_handler(restore);
        assert_eq!(
            t.signal_mask().val(),
            original.val(),
            "the temporary mask stayed"
        );
        assert!(t.saved_sigmask.is_none());
        assert_eq!(t.handling_signal, None);
    }

    #[test]
    fn with_nothing_saved_it_leaves_the_mask_as_it_is() {
        let mut t = thread();
        t.block_signals(&one(Signal::SIGWINCH));
        t.signals.insert(Signal::SIGCONT);
        let before = t.signal_mask();
        let (_, restore) = t.handle_signal().unwrap();
        t.taken_without_handler(restore);
        assert_eq!(t.signal_mask().val(), before.val());
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
    fn sigreturn_reinstalls_the_alternate_stack_the_frame_carries() {
        // The frame's `uc_stack` is what `sigreturn` installs back: for an
        // `SS_AUTODISARM` stack, disarmed for the length of the handler,
        // this is the only way it ever comes back. It never did.
        use crate::signal::{SignalStack, SignalStackFlags};
        let alt = SignalStack {
            sp: 0x7000_0000,
            flags: SignalStackFlags::AUTODISARM,
            size: 0x4000,
        };
        let mut t = thread();
        t.handling_signal = Some(Signal::SIGUSR1 as u32);
        t.signal_alternate_stack = alt;
        let uc_stack = t.signal_alternate_stack.take_for_frame();
        assert!(!t.signal_alternate_stack.usable_from(0x7fff_0000));

        let info = SigInfo::default();
        let mut uctx = SignalUserContext::default();
        uctx.stack = uc_stack;
        let old_ctx = UserContext::default();
        let mut ctx = UserContext::default();
        // The handler returns from the alternate stack.
        ctx.set_field(UserContextField::StackPointer, alt.sp + alt.size / 2);
        t.restore_after_handle_signal(
            &mut ctx,
            &old_ctx,
            &info as *const SigInfo as usize,
            &uctx as *const SignalUserContext as usize,
        );
        assert_eq!(
            t.signal_alternate_stack, alt,
            "sigreturn did not put the auto-disarmed stack back"
        );
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn sigreturn_resumes_on_the_stack_the_handler_chose() {
        use crate::signal::MachineContext;
        // A handler is allowed to rewrite the registers in its `ucontext`, and
        // `sigreturn` is where that takes effect. Only the PC was honoured, so
        // the one mechanism built entirely out of this -- Go's asynchronous
        // preemption, which pushes a return address, moves `rsp` down over it
        // and points `rip` at `asyncPreempt` -- got the new entry point on the
        // old stack, and `asyncPreempt` returned to whatever word the
        // goroutine was holding at its stack pointer.
        let mut t = thread();
        t.handling_signal = Some(Signal::SIGURG as u32);

        let mut old_ctx = UserContext::default();
        old_ctx.general_mut().rsp = 0xc000_040000;
        old_ctx.general_mut().r14 = 0xaaaa;

        let info = SigInfo::default();
        let mut uctx = SignalUserContext {
            context: MachineContext::from_context(&mut old_ctx.clone()),
            ..Default::default()
        };
        uctx.context.rsp = 0xc000_03fff8;
        uctx.context.rip = 0x4812_00;

        let mut ctx = UserContext::default();
        t.restore_after_handle_signal(
            &mut ctx,
            &old_ctx,
            &info as *const SigInfo as usize,
            &uctx as *const SignalUserContext as usize,
        );

        assert_eq!(
            ctx.general().rsp,
            0xc000_03fff8,
            "sigreturn put the old stack pointer back under the handler's new pc"
        );
        assert_eq!(ctx.general().rip, 0x4812_00);
        assert_eq!(
            ctx.general().r14,
            0xaaaa,
            "a register the handler left alone must come back as it was"
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
            "sigreturn left the thread marked as mid-handler"
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
        // Against the kernel's own constants, which is the whole point: this
        // module keeps its own copies to build the expectations below from,
        // and a test that compares its literal with its literal passes
        // whatever the kernel says. The constants lived inside
        // `robust_death_action` until the mutation run showed that nothing
        // named their numbers.
        assert_eq!(super::FUTEX_OWNER_DIED, 1 << 30);
        assert_eq!(super::FUTEX_WAITERS, 1i32 << 31);
        assert_eq!(super::FUTEX_TID_MASK, 0x3fff_ffff);
        assert_eq!(OWNER_DIED, super::FUTEX_OWNER_DIED);
        assert_eq!(WAITERS, super::FUTEX_WAITERS);
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
    fn a_tid_too_wide_for_the_mask_claims_nothing() {
        // The comparison is the word's low thirty bits against the WHOLE tid,
        // as `handle_futex_death` does it. Masking the tid as well would
        // alias every pair of tids 0x4000_0000 apart, and these tids are
        // kernel object ids: a counter, with none of the `PID_MAX_LIMIT` cap
        // that keeps a Linux tid inside the mask. The thread that aliased
        // would mark a lock its owner still holds as owner-dead, and the
        // waiters would get EOWNERDEAD from a lock nobody abandoned.
        for wide in [7 | OWNER_DIED, 7 | WAITERS, 7 | WAITERS | OWNER_DIED] {
            assert_eq!(
                robust_death_action(7, wide, false),
                RobustDeath::NotOurs,
                "tid {:#x} claimed the lock of tid 7",
                wide
            );
        }
        // A tid that does fit is untouched: the mask is there to hide the two
        // flag bits the libc wrote into the word, nothing else.
        assert!(matches!(
            robust_death_action(7 | WAITERS, 7, false),
            RobustDeath::Died { .. }
        ));
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

#[cfg(test)]
mod exec_reset_tests {
    //! What a thread must forget when its process calls `execve`, and what it
    //! must not.
    //!
    //! Every field [`LinuxThread::reset_for_exec`] clears holds an address in
    //! the image `execve` destroys, or handler state whose handler goes with
    //! it. So each of these is a way the kernel would otherwise reach into
    //! the NEW program with a number that described the old one.

    use super::*;
    use crate::signal::SignalStackFlags;

    /// A thread with a NON-DEFAULT value in every single field. A fixture
    /// that is already empty where the test asserts emptiness asserts
    /// nothing, so this one leaves nothing at its default.
    fn a_thread_in_full_swing() -> LinuxThread {
        let mut signals = Sigset::empty();
        signals.insert(Signal::SIGUSR2);
        let mut signal_mask = Sigset::empty();
        signal_mask.insert(Signal::SIGPIPE);
        let mut saved = Sigset::empty();
        saved.insert(Signal::SIGWINCH);
        LinuxThread {
            clear_child_tid: 0x7fff_0000.into(),
            signals,
            signal_mask,
            saved_sigmask: Some(saved),
            signal_alternate_stack: SignalStack {
                sp: 0x5000_0000,
                flags: SignalStackFlags::AUTODISARM,
                size: 0x4000,
            },
            robust_list: 0x6000_0000.into(),
            robust_list_len: core::mem::size_of::<RobustList>(),
            handling_signal: Some(Signal::SIGUSR2 as u32),
            comm: String::from("programa-viejo"),
            sigwait: Sigset::default(),
            timerslack_ns: 1_234_567,
            ioprio: IDLE_IOPRIO,
            pending_info: BTreeMap::new(),
            handling_info: SigInfo::default(),
        }
    }

    #[test]
    fn a_thread_that_execs_no_longer_has_an_alternate_signal_stack() {
        // `sas_ss_reset(me)` in `begin_new_exec()`. That address named the
        // image `execve` destroyed; the next `SA_ONSTACK` signal would put
        // its frame there, in the middle of whatever the new program mapped.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        let alt = t.signal_alternate_stack;
        // Spelled out rather than compared against `SignalStack::default()`:
        // a test that checks a value against the very constant it is meant to
        // pin moves with it and checks nothing.
        assert_eq!(alt.sp, 0);
        assert_eq!(alt.size, 0);
        assert!(alt.flags.contains(SignalStackFlags::DISABLE));
        // And what that means where it is read: no frame goes there.
        assert!(!alt.usable_from(0x1000));
    }

    #[test]
    fn a_thread_that_execs_holds_no_robust_locks() {
        // `me->robust_list = NULL` in `begin_new_exec()`. This head is the
        // one the kernel WALKS and compare-exchanges when the thread dies
        // (see `walk_robust_list`), so a stale one has it writing
        // FUTEX_OWNER_DIED into words of the new program that happen to
        // carry this thread's tid.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        assert_eq!(t.robust_list.as_addr(), 0);
        assert_eq!(t.robust_list_len, 0);
        assert!(walk_robust_list(t.robust_list.as_addr(), |_| {
            panic!("a thread that has just exec'd must read no user memory")
        })
        .is_empty());
    }

    #[test]
    fn a_thread_that_execs_forgets_the_set_tid_address_word() {
        // `mm_release()` sets `tsk->clear_child_tid = NULL`. Kept, the kernel
        // writes a zero and futex-wakes at that address when the thread dies
        // -- an address that is now somewhere inside the new program.
        let mut t = a_thread_in_full_swing();
        assert_ne!(t.tid_address(), 0, "the fixture must have one to forget");
        t.reset_for_exec();
        assert_eq!(t.tid_address(), 0);
    }

    #[test]
    fn a_thread_that_execs_from_inside_a_handler_can_still_be_interrupted() {
        // The `sigreturn` that clears `handling_signal` is in the image
        // `execve` just destroyed, so it is never coming. `handle_signal`
        // delivers NOTHING while that field is set, so without this reset the
        // new program can never be interrupted again -- Ctrl-C included, for
        // the rest of its life, whatever it does.
        let mut t = a_thread_in_full_swing();
        assert!(
            t.handling_signal.is_some(),
            "the fixture must exec from inside a handler"
        );
        t.reset_for_exec();
        t.signals.insert(Signal::SIGINT);
        assert_eq!(
            t.handle_signal().map(|(sig, _)| sig),
            Some(Signal::SIGINT),
            "the new program never got its signal"
        );
    }

    #[test]
    fn a_thread_that_execs_does_not_restore_the_old_programs_mask() {
        // `saved_sigmask` is the mask an `rt_sigsuspend` asked to have
        // reinstated once its handler returned. That handler went with the
        // image, so the mask must not survive to be installed behind the new
        // program's back by the first handler IT runs.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        t.signals.insert(Signal::SIGINT);
        let (_, restore) = t.handle_signal().expect("SIGINT is deliverable");
        assert!(
            !restore.contains(Signal::SIGWINCH),
            "the frame carried the old program's suspend mask"
        );
        assert_eq!(restore.val(), t.signal_mask().val());
    }

    #[test]
    fn a_thread_that_execs_answers_to_the_new_programs_name() {
        // execve(2) resets the name to the new image's basename, and empty is
        // how a reader knows to fall back to it; a stale `prctl(PR_SET_NAME)`
        // override would have /proc/<pid>/comm naming the program that is
        // gone.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        assert!(t.comm.is_empty());
    }

    #[test]
    fn signals_that_were_pending_are_still_pending_after_the_exec() {
        // execve(2): the set of pending signals is preserved. A SIGTERM sent
        // to a shell between its fork and its exec must still kill the
        // program the shell went on to run.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        assert!(t.signals.contains(Signal::SIGUSR2));
    }

    #[test]
    fn the_blocked_signal_mask_survives_the_exec() {
        // execve(2): the signal mask is preserved. This is what a program
        // that blocks a signal and then execs is relying on -- and it is the
        // documented way to hand a child a signal already blocked.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        assert!(t.signal_mask().contains(Signal::SIGPIPE));
    }

    #[test]
    fn the_timer_slack_survives_the_exec() {
        // prctl(2) lists PR_SET_TIMERSLACK among the settings that survive
        // both fork and exec; `begin_new_exec()` never touches it.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        assert_eq!(t.timerslack_ns, 1_234_567);
    }

    #[test]
    fn the_io_priority_survives_the_exec() {
        // `ionice -c3 cmd` is `ioprio_set(IOPRIO_WHO_PROCESS, 0, idle)` and
        // then `execve(cmd)`: the io_context is the task's, not the image's,
        // and `begin_new_exec()` never touches it.
        let mut t = a_thread_in_full_swing();
        t.reset_for_exec();
        assert_eq!(t.ioprio, IDLE_IOPRIO);
    }
}

#[cfg(test)]
mod clone_inheritance_tests {
    //! What a new thread takes from the thread that created it, and what it
    //! must start life without. Three paths land here — `fork`, `vfork` and
    //! `pthread_create` — and they differ in exactly one thing, which is
    //! whether they share the address space.

    use super::*;
    use crate::signal::SignalStackFlags;

    /// The creating thread, with a NON-DEFAULT value in every field: a
    /// fixture that is already empty where a test asserts emptiness asserts
    /// nothing.
    fn the_creating_thread() -> LinuxThread {
        let mut signals = Sigset::empty();
        signals.insert(Signal::SIGUSR2);
        let mut signal_mask = Sigset::empty();
        signal_mask.insert(Signal::SIGPIPE);
        let mut saved = Sigset::empty();
        saved.insert(Signal::SIGWINCH);
        LinuxThread {
            clear_child_tid: 0x7fff_0000.into(),
            signals,
            signal_mask,
            saved_sigmask: Some(saved),
            signal_alternate_stack: SignalStack {
                sp: 0x5000_0000,
                flags: SignalStackFlags::empty(),
                size: 0x4000,
            },
            robust_list: 0x6000_0000.into(),
            robust_list_len: core::mem::size_of::<RobustList>(),
            handling_signal: Some(Signal::SIGUSR2 as u32),
            comm: String::from("el-que-crea"),
            sigwait: Sigset::default(),
            timerslack_ns: 1_234_567,
            ioprio: IDLE_IOPRIO,
            pending_info: BTreeMap::new(),
            handling_info: SigInfo::default(),
        }
    }

    #[test]
    fn a_forked_child_starts_with_its_parents_signal_mask() {
        // sigprocmask(2): "A child created via fork(2) inherits a copy of its
        // parent's signal mask." It is what the standard race-free idiom
        // rests on -- block the signal, fork, and the child is covered before
        // its first instruction, rather than from whenever it gets around to
        // blocking it itself.
        let parent = the_creating_thread();
        let mut child = parent.forked_child();
        assert!(child.signal_mask().contains(Signal::SIGPIPE));
        // And what that means where it is read: the blocked signal waits
        // instead of being delivered the moment the child runs.
        child.signals.insert(Signal::SIGPIPE);
        assert!(
            child.handle_signal().is_none(),
            "the child took a signal its parent had blocked"
        );
    }

    #[test]
    fn a_new_thread_starts_with_its_creators_signal_mask() {
        // pthread_create(3): "The new thread inherits a copy of the creating
        // thread's signal mask." A library that blocks a signal before
        // spawning its worker is relying on the worker never seeing it.
        let mut thread = the_creating_thread().new_thread();
        assert!(thread.signal_mask().contains(Signal::SIGPIPE));
        thread.signals.insert(Signal::SIGPIPE);
        assert!(thread.handle_signal().is_none());
    }

    #[test]
    fn what_the_parent_did_not_block_still_reaches_the_child() {
        // The other half of the same decision: inheriting the mask must not
        // turn into inheriting a block on everything.
        let mut child = the_creating_thread().forked_child();
        child.signals.insert(Signal::SIGINT);
        assert_eq!(child.handle_signal().map(|(s, _)| s), Some(Signal::SIGINT));
    }

    #[test]
    fn a_forked_child_keeps_the_alternate_signal_stack() {
        // sigaltstack(2): "A child created via fork(2) inherits a copy of its
        // parent's alternate signal stack settings." A child that lost it
        // would take its stack-overflow handler on the stack that just
        // overflowed.
        let child = the_creating_thread().forked_child();
        assert_eq!(child.signal_alternate_stack.sp, 0x5000_0000);
        assert_eq!(child.signal_alternate_stack.size, 0x4000);
        assert!(child.signal_alternate_stack.usable_from(0x1000));
    }

    #[test]
    fn a_new_thread_gets_no_alternate_signal_stack_of_its_own() {
        // `copy_process`: "sigaltstack should be cleared when sharing the
        // same VM", `if ((clone_flags & (CLONE_VM|CLONE_VFORK)) == CLONE_VM)
        // sas_ss_reset(p);`. Two threads sharing one alternate stack put
        // their signal frames on top of each other.
        let thread = the_creating_thread().new_thread();
        assert_eq!(thread.signal_alternate_stack.sp, 0);
        assert_eq!(thread.signal_alternate_stack.size, 0);
        assert!(thread
            .signal_alternate_stack
            .flags
            .contains(SignalStackFlags::DISABLE));
        assert!(!thread.signal_alternate_stack.usable_from(0x1000));
    }

    #[test]
    fn the_timer_slack_is_inherited_by_both() {
        // prctl(2): "The timer slack value is inherited by children created
        // via fork(2)", and `copy_process` carries it into a new thread with
        // the rest of the task struct.
        assert_eq!(
            the_creating_thread().forked_child().timerslack_ns,
            1_234_567
        );
        assert_eq!(the_creating_thread().new_thread().timerslack_ns, 1_234_567);
    }

    #[test]
    fn nothing_is_pending_for_a_thread_that_has_just_been_created() {
        // fork(2): "The child's set of pending signals is initially empty",
        // and `init_sigpending` does the same for a thread. A signal is
        // delivered once, to whoever it was aimed at -- copying the set would
        // have a SIGTERM aimed at a shell kill every child it forked next.
        let parent = the_creating_thread();
        assert!(
            parent.signals.contains(Signal::SIGUSR2),
            "the fixture must have something pending to not hand down"
        );
        assert!(parent.forked_child().signals.is_empty());
        assert!(parent.new_thread().signals.is_empty());
    }

    #[test]
    fn a_new_thread_holds_no_robust_locks() {
        // `copy_process`: `p->robust_list = NULL`. The list names locks the
        // CREATING thread holds; handing it down would have the child
        // release, on its own death, locks its parent is still inside.
        let child = the_creating_thread().new_thread();
        assert_eq!(child.robust_list.as_addr(), 0);
        assert_eq!(child.robust_list_len, 0);
    }

    #[test]
    fn a_new_thread_has_no_tid_address_until_clone_installs_one() {
        // `p->clear_child_tid = (clone_flags & CLONE_CHILD_CLEARTID) ?
        // child_tidptr : NULL`. Inherited, the child would zero and
        // futex-wake its PARENT's tid word when it died, and musl points that
        // word at its global `__thread_list_lock`.
        let child = the_creating_thread().new_thread();
        assert_eq!(child.tid_address(), 0);
    }

    #[test]
    fn a_child_forked_from_inside_a_handler_is_not_stuck_in_one() {
        // `handling_signal` belongs to a `sigreturn` frame and the child has
        // its own. Left set, the child could never be delivered a signal
        // again; what actually keeps a second one out of a running handler is
        // the inherited mask, not this latch.
        let parent = the_creating_thread();
        assert!(parent.handling_signal.is_some(), "forked mid-handler");
        let mut child = parent.forked_child();
        assert!(child.saved_sigmask.is_none());
        child.signals.insert(Signal::SIGINT);
        assert!(child.handle_signal().is_some());
    }

    #[test]
    fn a_new_thread_answers_to_the_programs_name() {
        // Empty is how a reader knows to fall back to the executable's
        // basename, which is what a thread with no `prctl(PR_SET_NAME)` of
        // its own reports.
        assert!(the_creating_thread().new_thread().comm.is_empty());
    }

    #[test]
    fn a_forked_child_and_a_new_thread_keep_the_io_priority() {
        // `copy_io()`: the new task's io_context takes `ioc->ioprio` from
        // the creator's (or shares it, with CLONE_IO). `ionice -c3 make`
        // would otherwise idle only the `make` and none of the compilers.
        assert_eq!(the_creating_thread().forked_child().ioprio, IDLE_IOPRIO);
        assert_eq!(the_creating_thread().new_thread().ioprio, IDLE_IOPRIO);
    }

    #[test]
    fn the_first_thread_of_a_program_carries_nothing() {
        // There is no creating thread to carry anything from: this is the
        // loader's path, and it must not quietly become some other thread's
        // child.
        let first = LinuxThread::initial();
        assert!(first.signals.is_empty());
        assert!(first.signal_mask().is_empty());
        assert!(first.saved_sigmask.is_none());
        assert!(!first.signal_alternate_stack.usable_from(0x1000));
        assert_eq!(first.robust_list.as_addr(), 0);
        assert_eq!(first.robust_list_len, 0);
        assert_eq!(first.tid_address(), 0);
        assert!(first.handling_signal.is_none());
        assert!(first.comm.is_empty());
        assert_eq!(first.timerslack_ns, 0);
    }
}

#[cfg(test)]
mod last_thread_tests {
    //! Whose exit code becomes the process's.

    use super::last_thread_of;

    /// `thread_count()` still counts the thread that is exiting, so "the last
    /// one" is one, not zero. Off by one here and either every `exit(2)`
    /// takes the whole process down with it, or none of them sets its code.
    #[test]
    fn the_last_thread_is_the_one_that_is_still_counted() {
        assert!(last_thread_of(1));
        assert!(!last_thread_of(2));
        assert!(!last_thread_of(64));
    }

    /// A count of zero can only mean the caller is already off the list;
    /// either way there is nobody left, so the code is still this thread's.
    #[test]
    fn nobody_left_is_also_the_last_one() {
        assert!(last_thread_of(0));
    }
}

#[cfg(test)]
mod siginfo_tests {
    //! What a pending signal carries besides its number. The pending set was
    //! a bitmap and nothing else, so every handler was handed a zeroed
    //! `siginfo_t`: `si_pid` 0 for a `kill`, `si_pid` 0 and `si_status` 0
    //! for a `SIGCHLD`.

    use super::*;
    use crate::signal::SignalCode;

    fn word(info: &SigInfo, at: usize) -> i32 {
        let b = info.as_bytes();
        i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn a_signal_comes_out_with_what_it_was_queued_with() {
        let mut t = LinuxThread::initial();
        t.queue_signal(
            Signal::SIGUSR1,
            Some(SigInfo::from_user(
                Signal::SIGUSR1,
                77,
                1000,
                SignalCode::USER,
            )),
        );
        assert!(t.signals.contains(Signal::SIGUSR1));
        let info = t.take_siginfo(Signal::SIGUSR1);
        assert!(!t.signals.contains(Signal::SIGUSR1), "taking it clears it");
        assert_eq!(word(&info, 0), Signal::SIGUSR1 as i32, "si_signo");
        assert_eq!(word(&info, 16), 77, "si_pid");
        assert_eq!(word(&info, 20), 1000, "si_uid");
        assert_eq!(info.code, SignalCode::USER);
    }

    #[test]
    fn the_first_sender_of_a_pending_signal_is_the_one_the_handler_learns_about() {
        // Non-realtime semantics: a signal already pending is not queued
        // again, so the second `kill` changes nothing about the first.
        let mut t = LinuxThread::initial();
        t.queue_signal(
            Signal::SIGTERM,
            Some(SigInfo::from_user(Signal::SIGTERM, 11, 0, SignalCode::USER)),
        );
        t.queue_signal(
            Signal::SIGTERM,
            Some(SigInfo::from_user(Signal::SIGTERM, 22, 0, SignalCode::USER)),
        );
        assert_eq!(word(&t.take_siginfo(Signal::SIGTERM), 16), 11);
    }

    #[test]
    fn a_signal_queued_without_information_is_bare_but_numbered() {
        let mut t = LinuxThread::initial();
        t.queue_signal(Signal::SIGHUP, None);
        let info = t.take_siginfo(Signal::SIGHUP);
        assert_eq!(info.signo, Signal::SIGHUP as i32);
        assert_eq!(word(&info, 16), 0, "si_pid of nobody");
        assert_eq!(info.code, SignalCode::USER);
    }

    #[test]
    fn taking_leaves_nothing_behind_for_the_next_time() {
        let mut t = LinuxThread::initial();
        t.queue_signal(
            Signal::SIGUSR2,
            Some(SigInfo::from_user(Signal::SIGUSR2, 5, 0, SignalCode::USER)),
        );
        t.take_siginfo(Signal::SIGUSR2);
        t.queue_signal(Signal::SIGUSR2, None);
        assert_eq!(word(&t.take_siginfo(Signal::SIGUSR2), 16), 0);
    }

    #[test]
    fn sigreturn_compares_the_frame_against_what_was_delivered_not_against_zeros() {
        // Before, a frame carrying real information looked "modified" to
        // `restore_after_handle_signal` and was logged as such on every
        // return. The check compares against what the kernel wrote.
        let mut t = LinuxThread::initial();
        let sent = SigInfo::from_user(Signal::SIGUSR1, 9, 9, SignalCode::USER);
        t.queue_signal(Signal::SIGUSR1, Some(sent));
        let delivered = t.take_siginfo(Signal::SIGUSR1);
        let uctx = SignalUserContext::default();
        assert_eq!(unmodified_check(&delivered, &t.handling_info, &uctx) & 1, 0);
        let mut doctored = delivered;
        doctored.errno = 3;
        assert_eq!(unmodified_check(&doctored, &t.handling_info, &uctx) & 1, 1);
    }

    /// And one bit per field, in the positions the log line names.
    ///
    /// `error!("unsupported signal fields : {:b}")` is the whole output of
    /// this function -- it gates nothing, the comment next to it says so --
    /// so the bit positions ARE the contract: whoever reads `1000` in a log
    /// has to be able to look up which field moved. Only bit 0 was tested;
    /// the other four mutants (each `!=` and the `check != 0` gate) survived,
    /// so every bit of that word could have shifted without a test noticing.
    ///
    /// `uc_stack` and `uc_mcontext` are deliberately NOT compared (the kernel
    /// fills them and `sigreturn` honours what the handler left), so bits 3
    /// and 5 do not exist, and this asserts that too: a frame that changes
    /// only those two is still "unmodified", which is the regression that
    /// used to log on every single signal.
    #[test]
    fn each_ucontext_field_the_check_watches_has_its_own_bit() {
        let t = LinuxThread::initial();
        let delivered = t.handling_info;
        let clean = SignalUserContext::default();
        assert_eq!(
            unmodified_check(&delivered, &t.handling_info, &clean),
            0,
            "an untouched frame was reported as modified"
        );

        let ctx = SignalUserContext {
            flags: 1,
            ..Default::default()
        };
        assert_eq!(unmodified_check(&delivered, &t.handling_info, &ctx), 1 << 1);

        let ctx = SignalUserContext {
            link: 0x1000,
            ..Default::default()
        };
        assert_eq!(unmodified_check(&delivered, &t.handling_info, &ctx), 1 << 2);

        let mut pad = [0u64; 15];
        pad[14] = 7;
        let ctx = SignalUserContext {
            _pad: pad,
            ..Default::default()
        };
        assert_eq!(unmodified_check(&delivered, &t.handling_info, &ctx), 1 << 4);

        // The two the kernel fills: no bit, and no log.
        let mut ctx = SignalUserContext {
            stack: SignalStack {
                sp: 0x4000,
                size: 0x2000,
                ..Default::default()
            },
            ..Default::default()
        };
        ctx.context.set_pc(0x5000);
        assert_eq!(
            unmodified_check(&delivered, &t.handling_info, &ctx),
            0,
            "the fields sigreturn honours were counted as unsupported"
        );
    }

    /// The FPU save area, which is the x86_64 frame's own tail.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn a_changed_fpu_save_area_gets_the_top_bit() {
        let t = LinuxThread::initial();
        let ctx = SignalUserContext {
            fpregs_mem: crate::signal::FpregsMem::scribbled(),
            ..Default::default()
        };
        assert_eq!(
            unmodified_check(&t.handling_info, &t.handling_info, &ctx),
            1 << 6
        );
    }
}
