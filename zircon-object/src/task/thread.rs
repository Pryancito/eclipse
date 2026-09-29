mod thread_state;

pub use self::thread_state::ThreadStateKind;

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{
    AtomicBool, AtomicI8, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use core::{any::Any, future::Future, pin::Pin};

use bitflags::bitflags;
use cfg_if::cfg_if;
use futures::{channel::oneshot::*, future::FutureExt, pin_mut, select_biased};
use kernel_hal::context::UserContext;
use kernel_hal::sync::Mutex;

use self::thread_state::ContextAccessState;
use super::process::EXT_CANARY;
use super::{exception::*, Process, Task};
use crate::object::{KObjectBase, KoID, Signal};
use crate::{define_count_helper, impl_kobject, ZxError, ZxResult};

/// Runnable / computation entity
///
/// ## SYNOPSIS
///
/// TODO
///
/// ## DESCRIPTION
///
/// The thread object is the construct that represents a time-shared CPU execution
/// context. Thread objects live associated to a particular
/// [Process Object](crate::task::Process) which provides the memory and the handles to other
/// objects necessary for I/O and computation.
///
/// ### Lifetime
/// Threads are created by calling [`Thread::create()`], but only start executing
/// when either [`Thread::start()`] or [`Process::start()`] are called. Both syscalls
/// take as an argument the entrypoint of the initial routine to execute.
///
/// The thread passed to [`Process::start()`] should be the first thread to start execution
/// on a process.
///
/// A thread terminates execution:
/// - by calling [`CurrentThread::exit()`]
/// - when the parent process terminates
/// - by calling [`Task::kill()`]
/// - after generating an exception for which there is no handler or the handler
///   decides to terminate the thread.
///
/// Returning from the entrypoint routine does not terminate execution. The last
/// action of the entrypoint should be to call [`CurrentThread::exit()`].
///
/// Closing the last handle to a thread does not terminate execution. In order to
/// forcefully kill a thread for which there is no available handle, use
/// `KernelObject::get_child()` to obtain a handle to the thread. This method is strongly
/// discouraged. Killing a thread that is executing might leave the process in a
/// corrupt state.
///
/// Fuchsia native threads are always *detached*. That is, there is no *join()* operation
/// needed to do a clean termination. However, some runtimes above the kernel, such as
/// C11 or POSIX might require threads to be joined.
///
/// ### Signals
/// Threads provide the following signals:
/// - [`THREAD_TERMINATED`]
/// - [`THREAD_SUSPENDED`]
/// - [`THREAD_RUNNING`]
///
/// When a thread is started [`THREAD_RUNNING`] is asserted. When it is suspended
/// [`THREAD_RUNNING`] is deasserted, and [`THREAD_SUSPENDED`] is asserted. When
/// the thread is resumed [`THREAD_SUSPENDED`] is deasserted and
/// [`THREAD_RUNNING`] is asserted. When a thread terminates both
/// [`THREAD_RUNNING`] and [`THREAD_SUSPENDED`] are deasserted and
/// [`THREAD_TERMINATED`] is asserted.
///
/// Note that signals are OR'd into the state maintained by the
/// `KernelObject::wait_signal()` family of functions thus
/// you may see any combination of requested signals when they return.
///
/// [`Thread::create()`]: Thread::create
/// [`CurrentThread::exit()`]: CurrentThread::exit
/// [`Process::exit()`]: crate::task::Process::exit
/// [`THREAD_TERMINATED`]: crate::object::Signal::THREAD_TERMINATED
/// [`THREAD_SUSPENDED`]: crate::object::Signal::THREAD_SUSPENDED
/// [`THREAD_RUNNING`]: crate::object::Signal::THREAD_RUNNING
/// Linux scheduling policy: time-sharing fair scheduling (a.k.a. `SCHED_OTHER`).
pub const SCHED_NORMAL: u8 = 0;
/// Linux scheduling policy: first-in-first-out real-time scheduling.
pub const SCHED_FIFO: u8 = 1;
/// Linux scheduling policy: round-robin real-time scheduling.
pub const SCHED_RR: u8 = 2;
/// Linux scheduling policy: "batch" fair scheduling (treated like `SCHED_NORMAL`).
pub const SCHED_BATCH: u8 = 3;
/// Linux scheduling policy: very-low-priority background fair scheduling.
pub const SCHED_IDLE: u8 = 5;
/// Linux scheduling policy: sporadic-task deadline scheduling. Accepted by the
/// ABI but not honoured by this scheduler.
pub const SCHED_DEADLINE: u8 = 6;

/// Smallest (highest-priority) nice value.
pub const MIN_NICE: i8 = -20;
/// Largest (lowest-priority) nice value.
pub const MAX_NICE: i8 = 19;
/// Lowest real-time static priority for `SCHED_FIFO` / `SCHED_RR`.
pub const MIN_RT_PRIO: u8 = 1;
/// Highest real-time static priority for `SCHED_FIFO` / `SCHED_RR`.
pub const MAX_RT_PRIO: u8 = 99;

/// CFS load weight of a nice-0 task (`sched_prio_to_weight[20]` in Linux).
const WEIGHT_NICE0: u32 = 1024;

/// Linux's `sched_prio_to_weight[]`: the load weight for nice values -20..=19.
/// Adjacent levels differ by ~25%, i.e. one nice step is roughly 10% of CPU —
/// exactly the design described in
/// `Documentation/scheduler/sched-nice-design.rst`.
const SCHED_PRIO_TO_WEIGHT: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291, // nice -20..=-16
    29154, 23254, 18705, 14949, 11916, // nice -15..=-11
    9548, 7620, 6100, 4904, 3906, //      nice -10..=-6
    3121, 2501, 1991, 1586, 1277, //      nice  -5..=-1
    1024, 820, 655, 526, 423, //          nice   0..=4
    335, 272, 215, 172, 137, //           nice   5..=9
    110, 87, 70, 56, 45, //               nice  10..=14
    36, 29, 23, 18, 15, //                nice  15..=19
];

/// The verdict of one scheduler tick: whether to preempt the running thread,
/// and the slice deadline to carry forward.
///
/// Pulled out of [`Thread::tick_should_preempt`], which reads the clock, so that
/// the four cases can be tested at all: a test cannot move
/// `kernel_hal::timer::timer_now`, and a policy with four cases and no test is
/// a policy nobody can change safely.
///
/// `now` and `end` are monotonic nanoseconds; `slice` is the length
/// [`Thread::timeslice_ns`] gave this thread, never [`u64::MAX`] (a
/// `SCHED_FIFO` thread does not reach here).
fn slice_verdict(now: u64, end: u64, slice: u64) -> (bool, u64) {
    // No slice running: start one. `set_sched` parks a zero here so a new
    // policy or nice takes effect on the next tick.
    if end == 0 {
        return (false, now.saturating_add(slice));
    }
    // A deadline further out than a whole slice cannot be one we set from this
    // clock: the clock moved backwards across a migration between cores with
    // unsynchronised counters. Start again from the clock we can see.
    if end > now.saturating_add(slice) {
        return (false, now.saturating_add(slice));
    }
    // Inside the slice, which is the common case: nothing to do, and the
    // deadline stands.
    if now < end {
        return (false, end);
    }
    // Past the deadline by MORE than a whole slice. The thread cannot have been
    // on a CPU for that span: this runs from the timer interrupt of the thread
    // that is executing, and the scheduler tick is milliseconds, so an expiry
    // is seen at the first tick after it and never a whole slice late. So the
    // deadline is the stale one from before the thread blocked.
    //
    // This is the BACKSTOP, not the mechanism. What a woken thread gets its
    // fresh slice from is [`Thread::sched_note_resumed`], which knows it woke
    // instead of inferring it -- and has to, because the clock cannot see a
    // short wait at all: a thread that runs 15 ms, waits 8 ms on a futex and
    // wakes has a deadline only 3 ms in the past, which is exactly what a late
    // tick looks like, and a media thread whose audio callback fires every
    // 10 ms is that shape every time. This arm covers whatever parks a thread
    // without passing through that call.
    if now - end > slice {
        return (false, now.saturating_add(slice));
    }
    // The slice really did run out.
    (true, now.saturating_add(slice))
}

/// Map a nice value to its CFS load weight.
fn nice_to_weight(nice: i8) -> u32 {
    let clamped = nice.clamp(MIN_NICE, MAX_NICE);
    SCHED_PRIO_TO_WEIGHT[(clamped as i32 + 20) as usize]
}

/// Base timeslice for a nice-0 fair task: 20 ms.
///
/// Wake-up preemption + the yielded-lane fix (see `WakerPage::mark_yielded`)
/// already give the CPU to freshly woken tasks; shortening the slice is not
/// required for interactivity and would raise preemption churn on workloads
/// that were already fine (SMP aggregate, pipe bandwidth, desktop).
const BASE_TIMESLICE_NS: u64 = 20_000_000;
/// Timeslice for `SCHED_RR` tasks: 100 ms, matching Linux's default RR quantum.
const RR_TIMESLICE_NS: u64 = 100_000_000;
/// Never give a runnable task a slice shorter than this.
const MIN_TIMESLICE_NS: u64 = 4_000_000;
/// Cap a fair task's slice so a very negative nice can't monopolise a CPU.
const MAX_TIMESLICE_NS: u64 = 120_000_000;

/// Per-thread Linux-compatible scheduling attributes.
///
/// The kernel core is an async per-CPU executor, not a Linux runqueue, so these
/// attributes do not select *which* runnable task runs next. They are honoured
/// for the *length* of a task's timeslice (see [`Thread::tick_should_preempt`])
/// so that `nice` and the scheduling policy have a real, observable effect on
/// CPU share, and they are reported faithfully through the `sched_*` /
/// `setpriority` syscalls and procfs.
struct SchedAttr {
    /// Scheduling policy (one of the `SCHED_*` constants).
    policy: AtomicU8,
    /// Nice value (-20..=19); only meaningful for the fair policies.
    nice: AtomicI8,
    /// Static real-time priority (1..=99 for FIFO/RR, else 0).
    rt_priority: AtomicU8,
    /// Monotonic time (ns) at which the current slice expires; 0 means
    /// "start a fresh slice on the next tick".
    ///
    /// Deliberately a *deadline*, not a countdown of ticks. The timer interrupt
    /// no longer has a fixed period — it is programmed for the nearest pending
    /// timer deadline (see `kernel_hal::bare::timer`) — so counting interrupts
    /// would make a thread's effective timeslice depend on how much unrelated
    /// timer traffic the machine happens to have. A 20 ms slice has to stay
    /// 20 ms whether that is 5 interrupts or 500.
    slice_end_ns: AtomicU64,
    /// Set while this thread's future is parked: the last poll returned
    /// `Pending`, so the executor gave the CPU to somebody else.
    ///
    /// This is what tells a resumption from a plain poll, and it is the only
    /// exact answer available. The clock cannot give it: a thread that runs
    /// 15 ms, waits 8 ms on a futex and wakes has a deadline only 3 ms in the
    /// past, which is indistinguishable from a late tick -- and a media thread
    /// whose audio callback fires every 10 ms is exactly that shape.
    parked: AtomicBool,
}

impl Default for SchedAttr {
    fn default() -> Self {
        Self {
            policy: AtomicU8::new(SCHED_NORMAL),
            nice: AtomicI8::new(0),
            rt_priority: AtomicU8::new(0),
            slice_end_ns: AtomicU64::new(0),
            parked: AtomicBool::new(false),
        }
    }
}

pub struct Thread {
    base: KObjectBase,
    _counter: CountHelper,
    proc: Arc<Process>,
    /// Guard word immediately BEFORE `ext`. See [`EXT_CANARY`].
    canary_lo: u64,
    ext: Box<dyn Any + Send + Sync>,
    /// Guard word immediately AFTER `ext`. See [`EXT_CANARY`].
    canary_hi: u64,
    /// The two words of the `ext` fat pointer as they were at construction.
    /// See [`Thread::record_ext_birth`].
    ext_born: [AtomicUsize; 2],
    inner: Mutex<ThreadInner>,
    exceptionate: Arc<Exceptionate>,
    /// CPU affinity mask: bit `i` set means this thread may run on logical
    /// CPU `i`. Shared with the scheduler (handed to `spawn_with_affinity`)
    /// so `set_affinity` takes effect on a running thread. Defaults to
    /// "all CPUs" (`u64::MAX`).
    affinity: Arc<AtomicU64>,
    /// Linux-compatible scheduling attributes (policy / nice / RT priority).
    sched: SchedAttr,
    /// The syscall this thread is executing, plus one; 0 means "in user code".
    ///
    /// Together with [`ThreadState::Blocked`] this is what makes a hang
    /// legible: knowing a thread is asleep is half the answer, and the other
    /// half is what it went to sleep IN. A plain relaxed store on entry and
    /// exit, with no lock, so it costs the same on the fast path as the
    /// accounting already there.
    current_syscall: AtomicU32,
    /// Nanoseconds this thread has spent executing user code.
    ///
    /// Deliberately **outside** `inner`: it is written once per user-mode exit,
    /// i.e. on every single syscall, page fault and interrupt. Living in the
    /// mutex it cost a full IRQ-disabling spinlock round trip per trap purely to
    /// add a number that nothing else reads under that lock. It is a pure
    /// accumulator (add-only, read-only elsewhere), so a relaxed atomic is both
    /// cheaper and exactly as correct.
    time_ns: AtomicU64,
    /// Nanoseconds this thread has spent actually running kernel-mode code —
    /// i.e. wall-clock time strictly inside a call to `Future::poll` on this
    /// thread's task (see `ThreadSwitchFuture::poll`), minus the portion of
    /// that same call attributed to `time_ns` above. That boundary is the only
    /// place this is unambiguous: a `poll` call is synchronous Rust code that
    /// cannot itself block, so every nanosecond inside it is real CPU time,
    /// and none of the (possibly very long) wall-clock a syscall spends
    /// suspended awaiting an event — which happens *between* `poll` calls —
    /// is ever included. This is `/proc/[pid]/stat`'s `stime`.
    sys_time_ns: AtomicU64,
    /// Logical CPU core this thread last ran on (field 39, `processor`, of
    /// `/proc/[pid]/stat`). Updated on every poll from `ThreadSwitchFuture`.
    last_cpu: AtomicU32,
}

impl_kobject!(Thread
    fn related_koid(&self) -> KoID {
        self.proc.id()
    }
);
define_count_helper!(Thread);

#[derive(Default)]
struct ThreadInner {
    /// Thread context
    ///
    /// It will be taken away when running this thread.
    context: Option<Box<UserContext>>,

    /// Thread context before handling the signal,
    /// (trapframe, siginfo_address, user_signal_ctx_address)
    ///
    /// It only works when executing signal handlers
    context_before: Option<(UserContext, usize, usize)>,

    /// The number of existing `SuspendToken`.
    suspend_count: usize,
    /// The waker of task when suspending.
    waker: Option<Waker>,
    /// A token used to kill blocking thread
    killer: Option<Sender<()>>,
    /// Thread state
    ///
    /// NOTE: This variable will never be `Suspended`. On suspended, the
    /// `suspend_count` is non-zero, and this represents the state before suspended.
    state: ThreadState,
    /// The currently processing exception
    exception: Option<Arc<Exception>>,
    /// Should The ProcessStarting exception generated at start of this thread
    first_thread: bool,
    /// Should The ThreadExiting exception do not block this thread
    killed: bool,
    flags: ThreadFlag,
}

impl ThreadInner {
    fn state(&self) -> ThreadState {
        // Dying > Exception > Suspend > Blocked
        if self.suspend_count == 0
            || self.context.is_none()
            || self.state == ThreadState::BlockedException
            || self.state == ThreadState::Dying
            || self.state == ThreadState::Dead
        {
            self.state
        } else {
            ThreadState::Suspended
        }
    }

    /// Change state and update signal.
    fn change_state(&mut self, state: ThreadState, base: &KObjectBase) {
        self.state = state;
        match self.state() {
            ThreadState::Dead => base.signal_change(
                Signal::THREAD_RUNNING | Signal::THREAD_SUSPENDED,
                Signal::THREAD_TERMINATED,
            ),
            ThreadState::New | ThreadState::Dying => base.signal_clear(
                Signal::THREAD_RUNNING | Signal::THREAD_SUSPENDED | Signal::THREAD_TERMINATED,
            ),
            ThreadState::Suspended => base.signal_change(
                Signal::THREAD_RUNNING | Signal::THREAD_TERMINATED,
                Signal::THREAD_SUSPENDED,
            ),
            _ => base.signal_change(
                Signal::THREAD_TERMINATED | Signal::THREAD_SUSPENDED,
                Signal::THREAD_RUNNING,
            ),
        }
    }

    /// Backup current context
    fn backup_context(&mut self, context: UserContext, siginfo: usize, uctx: usize) {
        self.context_before = Some((context, siginfo, uctx));
    }
}

bitflags! {
    /// Thread flags.
    #[derive(Default)]
    pub struct ThreadFlag: usize {
        /// The thread currently has a VCPU.
        const VCPU = 1 << 3;
    }
}

type ThreadFuture = dyn Future<Output = ()> + Send;
type ThreadFuturePinned = Pin<Box<ThreadFuture>>;

/// The type of a new thread function.
pub type ThreadFn = fn(thread: CurrentThread) -> ThreadFuturePinned;

impl Thread {
    /// Create a new thread.
    pub fn create(proc: &Arc<Process>, name: &str) -> ZxResult<Arc<Self>> {
        Self::create_with_ext(proc, name, ())
    }

    /// Create a new thread with extension info.
    ///
    /// # Example
    /// ```
    /// # use std::sync::Arc;
    /// # use zircon_object::task::*;
    /// # kernel_hal::init();
    /// let job = Job::root();
    /// let proc = Process::create(&job, "proc").unwrap();
    /// // create a thread with extension info
    /// let thread = Thread::create_with_ext(&proc, "thread", job.clone()).unwrap();
    /// // get the extension info
    /// let ext = thread.ext().downcast_ref::<Arc<Job>>().unwrap();
    /// assert!(Arc::ptr_eq(ext, &job));
    /// ```
    pub fn create_with_ext(
        proc: &Arc<Process>,
        name: &str,
        ext: impl Any + Send + Sync,
    ) -> ZxResult<Arc<Self>> {
        Self::create_with_ext_id(proc, name, ext, None)
    }

    /// Create a new thread with extension info and an optional fixed KoID.
    ///
    /// When `id` is `Some`, the thread is created with that exact KoID instead
    /// of a freshly allocated one. This is used to give a process's *leader*
    /// (main) thread a TID equal to the process PID, matching Linux — where the
    /// thread-group leader's TID always equals the TGID. Callers must guarantee
    /// the id is unique among the process's threads (it is, since only the
    /// leader reuses the PID and the PID is allocated to nothing else).
    pub fn create_with_ext_id(
        proc: &Arc<Process>,
        name: &str,
        ext: impl Any + Send + Sync,
        id: Option<KoID>,
    ) -> ZxResult<Arc<Self>> {
        let base = match id {
            Some(id) => KObjectBase::with_id(id, name, Default::default()),
            None => KObjectBase::with_name_pooled(name),
        };
        let thread = Arc::new(Thread {
            base,
            _counter: CountHelper::new(),
            proc: proc.clone(),
            canary_lo: EXT_CANARY,
            ext: Box::new(ext),
            canary_hi: EXT_CANARY,
            ext_born: [AtomicUsize::new(0), AtomicUsize::new(0)],
            exceptionate: Exceptionate::new(ExceptionChannelType::Thread),
            inner: Mutex::new(ThreadInner {
                context: Some(Box::new(UserContext::new())),
                ..Default::default()
            }),
            affinity: Arc::new(AtomicU64::new(u64::MAX)),
            sched: SchedAttr::default(),
            current_syscall: AtomicU32::new(0),
            time_ns: AtomicU64::new(0),
            sys_time_ns: AtomicU64::new(0),
            last_cpu: AtomicU32::new(0),
        });
        thread.record_ext_birth();
        proc.add_thread(thread.clone())?;
        Ok(thread)
    }

    /// Get the process.
    pub fn proc(&self) -> &Arc<Process> {
        &self.proc
    }

    /// Get the extension info.
    pub fn ext(&self) -> &Box<dyn Any + Send + Sync> {
        &self.ext
    }

    /// State of the guards around `ext`, as `(lo_ok, hi_ok)`.
    pub fn ext_canaries(&self) -> (bool, bool) {
        (self.canary_lo == EXT_CANARY, self.canary_hi == EXT_CANARY)
    }

    /// The `ext` fat pointer as it currently reads, as `(data, vtable)`.
    pub fn ext_fat(&self) -> (usize, usize) {
        let fat: [usize; 2] =
            unsafe { core::mem::transmute::<&dyn Any, [usize; 2]>(&*self.ext as &dyn Any) };
        (fat[0], fat[1])
    }

    /// Snapshot the `ext` fat pointer, taken once immediately after the
    /// `Arc<Thread>` is built and before it is added to its process. See
    /// [`Process::record_ext_birth`] — `Thread::ext` fails the same way
    /// (`downcast_arc().unwrap()` in the signal path), so it gets the same
    /// evidence.
    fn record_ext_birth(&self) {
        let (data, vtable) = self.ext_fat();
        self.ext_born[0].store(data, Ordering::Relaxed);
        self.ext_born[1].store(vtable, Ordering::Relaxed);
    }

    /// The snapshot taken by [`Thread::record_ext_birth`], as `(data, vtable)`.
    pub fn ext_born(&self) -> (usize, usize) {
        (
            self.ext_born[0].load(Ordering::Relaxed),
            self.ext_born[1].load(Ordering::Relaxed),
        )
    }

    /// Returns a copy of saved context of current thread, or `Err(ZxError::BAD_STATE)`
    /// if the thread is running.
    pub fn context_cloned(&self) -> ZxResult<UserContext> {
        self.with_context(|ctx| *ctx)
    }

    /// Access saved context of current thread, or `Err(ZxError::BAD_STATE)` if
    /// the thread is running.
    pub fn with_context<T, F>(&self, f: F) -> ZxResult<T>
    where
        F: FnOnce(&mut UserContext) -> T,
    {
        let mut inner = self.inner.lock();
        if let Some(ctx) = inner.context.as_mut() {
            Ok(f(ctx))
        } else {
            Err(ZxError::BAD_STATE)
        }
    }

    /// Backup current user context before calling signal handler
    pub fn backup_context(&self, context: UserContext, siginfo: usize, uctx: usize) {
        let mut inner = self.inner.lock();
        inner.backup_context(context, siginfo, uctx);
    }

    /// Fetch the context backup
    pub fn fetch_backup_context(&self) -> Option<(UserContext, usize, usize)> {
        let mut inner = self.inner.lock();
        inner.context_before.take()
    }

    /// Start execution on the thread.
    pub fn start(self: &Arc<Self>, thread_fn: ThreadFn) -> ZxResult {
        self.inner
            .lock()
            .change_state(ThreadState::Running, &self.base);
        let current = CurrentThread(self.clone());
        let future = thread_fn(current);
        kernel_hal::thread::spawn_with_affinity(
            ThreadSwitchFuture::new(self.clone(), future),
            self.affinity.clone(),
        );
        Ok(())
    }

    /// Get the thread's CPU affinity mask (bit `i` set => may run on CPU `i`).
    pub fn affinity(&self) -> u64 {
        self.affinity.load(Ordering::Relaxed)
    }

    /// Set the thread's CPU affinity mask.
    ///
    /// `mask` must have at least one bit set; an all-zero mask is rejected
    /// because it would make the thread unschedulable. The change is observed
    /// by the scheduler on the thread's next placement or work-stealing
    /// decision (it will migrate off a now-disallowed CPU once it next yields).
    pub fn set_affinity(&self, mask: u64) -> ZxResult {
        if mask == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        self.affinity.store(mask, Ordering::Relaxed);
        Ok(())
    }

    /// The thread's current scheduling policy (one of the `SCHED_*` constants).
    pub fn sched_policy(&self) -> u8 {
        self.sched.policy.load(Ordering::Relaxed)
    }

    /// The thread's nice value (-20..=19).
    pub fn sched_nice(&self) -> i8 {
        self.sched.nice.load(Ordering::Relaxed)
    }

    /// The thread's static real-time priority (1..=99 for FIFO/RR, else 0).
    pub fn sched_rt_priority(&self) -> u8 {
        self.sched.rt_priority.load(Ordering::Relaxed)
    }

    /// Whether the thread runs under a real-time policy (`SCHED_FIFO` /
    /// `SCHED_RR`).
    pub fn sched_is_realtime(&self) -> bool {
        matches!(self.sched_policy(), SCHED_FIFO | SCHED_RR)
    }

    /// Replace the thread's scheduling attributes.
    ///
    /// The `sched_*` / `setpriority` syscalls are responsible for validating the
    /// values against Linux's rules before calling this; it just stores them and
    /// forces the next timer tick to recompute the timeslice so the change takes
    /// effect promptly.
    pub fn set_sched(&self, policy: u8, nice: i8, rt_priority: u8) {
        self.sched.policy.store(policy, Ordering::Relaxed);
        self.sched.nice.store(nice, Ordering::Relaxed);
        self.sched.rt_priority.store(rt_priority, Ordering::Relaxed);
        // Drop the remainder of the old slice; the next tick starts a fresh one
        // from the new policy/nice (see `tick_should_preempt`).
        self.sched.slice_end_ns.store(0, Ordering::Relaxed);
    }

    /// Note that the executor has parked this thread: its future answered
    /// `Pending` and somebody else has the CPU now.
    ///
    /// Paired with [`Thread::sched_note_resumed`], which is what makes the
    /// timeslice measure time spent running instead of time on the wall clock.
    pub fn sched_note_parked(&self) {
        self.sched.parked.store(true, Ordering::Relaxed);
    }

    /// Note that the executor is polling this thread again, and give it a fresh
    /// slice if the previous poll had parked it.
    ///
    /// A thread that blocked keeps the deadline it had before it went to sleep,
    /// so coming back looked exactly like running out of time: on its FIRST
    /// timer tick after waking, [`Thread::tick_should_preempt`] answered true
    /// and the thread lost the CPU after running for microseconds. Every
    /// blocking `read`, every `nanosleep`, every futex wait paid for it -- the
    /// opposite of the wake-up preemption in the run loop, which exists to HAND
    /// the CPU to a thread that just woke.
    ///
    /// The park flag is the exact answer where the clock can only guess: it is
    /// set only by a poll that really answered `Pending`, so a future that was
    /// ready straight away never looks like a wake and cannot help itself to a
    /// free slice.
    pub fn sched_note_resumed(&self) {
        if self.sched.parked.swap(false, Ordering::Relaxed) {
            self.sched.slice_end_ns.store(0, Ordering::Relaxed);
        }
    }

    /// Whether the executor has this thread parked: its last poll answered
    /// `Pending`, so the next one is a resumption and owes it a fresh slice.
    pub fn sched_is_parked(&self) -> bool {
        self.sched.parked.load(Ordering::Relaxed)
    }

    /// Length of this thread's timeslice in nanoseconds.
    ///
    /// `SCHED_FIFO` returns [`u64::MAX`] (it is never time-sliced); `SCHED_RR`
    /// returns a fixed 100 ms quantum; `SCHED_IDLE` the minimum; and the fair
    /// policies scale [`BASE_TIMESLICE_NS`] by the nice→weight ratio so each
    /// nice step is worth roughly 10% more/less CPU.
    fn timeslice_ns(&self) -> u64 {
        match self.sched_policy() {
            SCHED_FIFO => u64::MAX,
            SCHED_RR => RR_TIMESLICE_NS,
            SCHED_IDLE => MIN_TIMESLICE_NS,
            _ => {
                let w = nice_to_weight(self.sched_nice()) as u64;
                let t = (BASE_TIMESLICE_NS * w + WEIGHT_NICE0 as u64 / 2) / WEIGHT_NICE0 as u64;
                t.clamp(MIN_TIMESLICE_NS, MAX_TIMESLICE_NS)
            }
        }
    }

    /// Report whether the running thread's timeslice has elapsed and it should
    /// be preempted.
    ///
    /// Called from the timer-interrupt path of the user-trap handler for the
    /// thread currently executing on this CPU. A `SCHED_FIFO` thread is never
    /// preempted here (it yields the CPU only by blocking or calling
    /// `sched_yield`); every other policy is preempted once its nice/policy
    /// derived slice is exhausted. The deadline lives in the thread (not the
    /// CPU), so it survives the executor migrating the thread between cores.
    ///
    /// Compares against the clock rather than counting interrupts. The timer is
    /// programmed for the nearest pending deadline, so the interrupt period
    /// varies from microseconds to the full 4 ms scheduler tick; a tick counter
    /// would have made a thread's real timeslice shrink in proportion to
    /// unrelated timer traffic — a `nanosleep`-heavy neighbour would silently
    /// cut everyone else's slice, and the extra preemptions cost far more than
    /// they bought.
    pub fn tick_should_preempt(&self) -> bool {
        let slice = self.timeslice_ns();
        if slice == u64::MAX {
            return false;
        }
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        let end = self.sched.slice_end_ns.load(Ordering::Relaxed);
        let (preempt, next) = slice_verdict(now, end, slice);
        self.sched.slice_end_ns.store(next, Ordering::Relaxed);
        preempt
    }

    /// Setup the instruction and stack pointer, then tart execution on the thread
    pub fn start_with_entry(
        self: &Arc<Self>,
        entry: usize,
        stack: usize,
        arg1: usize,
        arg2: usize,
        thread_fn: ThreadFn,
    ) -> ZxResult {
        self.with_context(|ctx| ctx.setup_uspace(entry, stack, &[arg1, arg2, 0]))?;
        self.start(thread_fn)
    }

    /// Stop the thread. Internal implementation of `exit` and `kill`.
    ///
    /// The thread do not terminate immediately when stopped. It is just made dying.
    /// It will terminate after some cleanups (when `terminate` are called **explicitly** by upper layer).
    fn stop(&self, killed: bool) {
        // The wake and the oneshot `send` both run code this module does not
        // own -- the executor's waker vtable, and whatever the woken task does
        // next -- so they happen with `inner` released. `Waiter::wake` in
        // `signal/futex.rs` already keeps that discipline ("The waker is
        // invoked after releasing the waiter lock"); this path, which is the
        // one `Process::exit` drives for every thread it kills, did not: it
        // woke the task with this thread's own spin lock still held, taken with
        // interrupts off. A woken task that reaches back for `inner` -- reading
        // its state, or terminating -- is a CPU that does not come back.
        let (waker, killer) = {
            let mut inner = self.inner.lock();
            if inner.state == ThreadState::Dead {
                return;
            }
            if killed {
                inner.killed = true;
            }
            if inner.state == ThreadState::Dying {
                // Already dying: only the killer, and only for a kill.
                (None, if killed { inner.killer.take() } else { None })
            } else {
                inner.change_state(ThreadState::Dying, &self.base);
                // For a blocking thread, the killer is what gets it out.
                (inner.waker.take(), inner.killer.take())
            }
        };
        if let Some(waker) = waker {
            waker.wake_by_ref();
        }
        if let Some(killer) = killer {
            // It's ok to ignore the error since the other end could be closed
            killer.send(()).ok();
        }
    }

    /// Read one aspect of thread state.
    pub fn read_state(&self, kind: ThreadStateKind, buf: &mut [u8]) -> ZxResult<usize> {
        let inner = self.inner.lock();
        let state = inner.state();
        if state != ThreadState::BlockedException && state != ThreadState::Suspended {
            if inner.exception.is_some() {
                return Err(ZxError::NOT_SUPPORTED);
            }
            return Err(ZxError::BAD_STATE);
        }
        let context = inner.context.as_ref().ok_or(ZxError::BAD_STATE)?;
        context.read_state(kind, buf)
    }

    /// Write one aspect of thread state.
    pub fn write_state(&self, kind: ThreadStateKind, buf: &[u8]) -> ZxResult {
        let mut inner = self.inner.lock();
        let state = inner.state();
        if state != ThreadState::BlockedException && state != ThreadState::Suspended {
            if inner.exception.is_some() {
                return Err(ZxError::NOT_SUPPORTED);
            }
            return Err(ZxError::BAD_STATE);
        }
        let context = inner.context.as_mut().ok_or(ZxError::BAD_STATE)?;
        context.write_state(kind, buf)
    }

    /// Get the thread's information.
    pub fn get_thread_info(&self) -> ThreadInfo {
        let inner = self.inner.lock();
        ThreadInfo {
            state: inner.state() as u32,
            wait_exception_channel_type: inner
                .exception
                .as_ref()
                .map_or(0, |exception| exception.current_channel_type() as u32),
            cpu_affinity_mask: {
                let mut m = [0u64; 8];
                m[0] = self.affinity.load(Ordering::Relaxed);
                m
            },
        }
    }

    /// Get the thread's exception report.
    pub fn get_thread_exception_info(&self) -> ZxResult<ExceptionReport> {
        let inner = self.inner.lock();
        if inner.state() != ThreadState::BlockedException {
            return Err(ZxError::BAD_STATE);
        }
        let report = inner.exception.as_ref().ok_or(ZxError::BAD_STATE)?.report();
        Ok(report)
    }

    /// Get the thread's version 1 exception report.
    pub fn get_thread_exception_info_v1(&self) -> ZxResult<ExceptionReportV1> {
        Ok(self.get_thread_exception_info()?.as_v1())
    }

    /// Record the syscall this thread is entering, or `None` on the way out.
    pub fn set_current_syscall(&self, num: Option<u32>) {
        self.current_syscall.store(
            num.map_or(0, |n| n.wrapping_add(1)),
            core::sync::atomic::Ordering::Relaxed,
        );
    }

    /// The syscall this thread is currently executing, if any.
    pub fn current_syscall(&self) -> Option<u32> {
        match self
            .current_syscall
            .load(core::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            n => Some(n - 1),
        }
    }

    /// Get the thread state.
    pub fn state(&self) -> ThreadState {
        self.inner.lock().state()
    }

    /// Mark this thread blocked in a syscall, or running again.
    ///
    /// [`ThreadState::Blocked`] existed but nothing ever set it, so every live
    /// thread looked `Running` — and `/proc/<pid>/status`, which is the first
    /// field any hang investigation reads, reported `R (running)` for a task
    /// asleep in `read`. During one such investigation that turned two
    /// processes quietly deadlocked on IPC into "both spinning in userspace",
    /// which is a different bug entirely.
    ///
    /// Signal-wise this is a no-op by construction: `change_state` maps
    /// `Blocked` and `Running` to the same `THREAD_RUNNING` transition, which
    /// is also what Zircon does — a thread blocked in a syscall is still
    /// running as far as `zx_object_wait` is concerned, and only
    /// `ZX_INFO_THREAD` tells the two apart. So this writes the field directly
    /// rather than going through `change_state`, which would re-publish a
    /// signal set that is already current — a second lock and a walk of the
    /// object's waiters on every block and unblock, for no observable effect.
    ///
    /// This is the GENERIC marker, and it defers to everything more specific.
    /// `blocking_run` already records why a thread is waiting — `BlockedFutex`,
    /// `BlockedChannel`, `BlockedPort` — and asserts on return that nobody
    /// moved the state under it, so overwriting one of those both loses
    /// information and trips that assertion (it panicked all four CPUs at
    /// once). Hence: only `Running` becomes `Blocked`, and only the generic
    /// `Blocked` goes back to `Running`. Teardown wins for the same reason —
    /// `Dying`/`Dead`/`New`/`Suspended` are all left exactly as they are, so
    /// an unblock racing a kill can never resurrect a thread into `Running`
    /// and strand it in the run loop.
    ///
    /// Returns whether it actually changed anything, so the caller knows
    /// whether it owes an unmark.
    pub fn set_blocked(&self, blocked: bool) -> bool {
        let mut inner = self.inner.lock();
        let (from, to) = if blocked {
            (ThreadState::Running, ThreadState::Blocked)
        } else {
            (ThreadState::Blocked, ThreadState::Running)
        };
        if inner.state != from {
            return false;
        }
        inner.state = to;
        true
    }

    /// Add the parameter to the time this thread has run on cpu.
    ///
    /// Called on every return from user mode, so it stays off `inner`'s lock —
    /// see [`Thread::time_ns`].
    pub fn time_add(&self, time: u128) {
        self.time_ns.fetch_add(time as u64, Ordering::Relaxed);
    }

    /// Get the time this thread has run on cpu.
    pub fn get_time(&self) -> u64 {
        self.time_ns.load(Ordering::Relaxed)
    }

    /// Add the parameter to the kernel-mode time this thread has run on cpu.
    ///
    /// Called from `ThreadSwitchFuture::poll`, the one place that can measure
    /// it unambiguously — see [`Thread::sys_time_ns`]. Public like its
    /// user-mode twin [`Thread::time_add`]: the pair is what `terminate`
    /// credits to the process, and `/proc/<pid>/stat`'s field 15 is the only
    /// reader of the kernel-mode half, in another crate.
    pub fn sys_time_add(&self, time: u64) {
        self.sys_time_ns.fetch_add(time, Ordering::Relaxed);
    }

    /// Get the kernel-mode time this thread has run on cpu.
    pub fn get_sys_time(&self) -> u64 {
        self.sys_time_ns.load(Ordering::Relaxed)
    }

    /// Set the logical CPU core this thread last ran on.
    pub(crate) fn set_last_cpu(&self, cpu: u32) {
        self.last_cpu.store(cpu, Ordering::Relaxed);
    }

    /// Get the logical CPU core this thread last ran on.
    pub fn last_cpu(&self) -> u32 {
        self.last_cpu.load(Ordering::Relaxed)
    }

    /// Get scheduler runtime statistics for this thread.
    pub fn get_runtime_info(&self) -> TaskRuntimeInfo {
        let runtime = self.get_time();
        TaskRuntimeInfo {
            cpu_time: runtime,
            // zCore does not track run-queue latency separately yet.  Report
            // elapsed scheduled time so callers can still observe progress.
            queue_time: runtime,
            page_fault_time: 0,
            lock_contention_time: 0,
        }
    }

    /// Set this thread as the first thread of a process.
    pub(super) fn set_first_thread(&self) {
        self.inner.lock().first_thread = true;
    }

    /// Whether this thread is the first thread of a process.
    pub fn is_first_thread(&self) -> bool {
        self.inner.lock().first_thread
    }

    /// Get the thread's flags.
    pub fn flags(&self) -> ThreadFlag {
        self.inner.lock().flags
    }

    /// Apply `f` to the thread's flags.
    pub fn update_flags(&self, f: impl FnOnce(&mut ThreadFlag)) {
        f(&mut self.inner.lock().flags)
    }

    /// Terminate the current running thread.
    ///
    /// Nothing here touches the process with this thread's lock held. It used
    /// to: `remove_thread` takes the process's `inner`, so this path ran
    /// thread -> process while `Process::exit` runs process -> thread (it held
    /// its own `inner` across `thread.kill()`). Two orders of the same pair of
    /// spin locks taken with interrupts off, and a process exiting on one CPU
    /// while one of its threads terminates on another is two CPUs that never
    /// come back. The process side let go too, so neither order exists now.
    fn terminate(&self) {
        self.exceptionate.shutdown();
        self.inner
            .lock()
            .change_state(ThreadState::Dead, &self.base);
        // Credit this thread's CPU time to the process before the thread
        // disappears from its list, so process-level accounting
        // (getrusage/times, the parent's wait4 rusage) keeps it.
        let proc = self.proc();
        proc.dead_threads_time_add(self.get_time());
        proc.dead_threads_sys_time_add(self.get_sys_time());
        proc.remove_thread(self.base.id);
    }

    /// Terminate a thread whose coroutine was abandoned and can never run again.
    ///
    /// Normally a thread leaves its process's thread list through
    /// `CurrentThread::drop`, on the way out of `run_user`. When the kernel
    /// contains a fault by abandoning a coroutine mid-poll (`zcore::oops`) that
    /// `CurrentThread` is leaked along with the rest of the aborted call chain,
    /// so its `Drop` never runs — and without this the thread would sit in its
    /// process's list forever, keeping an exited process from ever reaching
    /// `terminate()` and releasing its address space.
    ///
    /// Safe to call at most once per thread, in place of that lost `Drop`.
    pub fn terminate_abandoned(&self) {
        self.terminate();
    }

    /// Install `waker` as this thread's wake-up, the way a suspended thread's
    /// own future does from its `poll`.
    ///
    /// Only for tests: they cannot poll that future by hand, and without a door
    /// like this there is no way to ask whether `stop` wakes with `inner` held.
    #[cfg(test)]
    pub fn set_waker_for_test(&self, waker: core::task::Waker) {
        self.inner.lock().waker = Some(waker);
    }
}

impl Task for Thread {
    fn kill(&self) {
        self.stop(true)
    }

    fn suspend(&self) {
        let mut inner = self.inner.lock();
        inner.suspend_count += 1;
        let state = inner.state;
        inner.change_state(state, &self.base);
    }

    fn resume(&self) {
        let mut inner = self.inner.lock();
        // A resume with no suspend behind it used to be an `assert_ne!`, which
        // is a kernel panic. It is reachable: `Process::suspend` counts on
        // every thread the process holds at that moment and `Process::resume`
        // on every thread it holds now, so a thread created in between is
        // resumed having never been suspended -- and `SuspendToken::create`
        // takes any `Arc<dyn Task>`, so that pairing is not this crate's to
        // forbid. There is nothing to undo, and the threads that WERE suspended
        // still get their resume from the same loop.
        if inner.suspend_count == 0 {
            // `debug!` and not `warn!`: the comment above says this is reached
            // in ordinary operation, and a warning on an ordinary path is
            // noise that buries the warnings that mean something.
            debug!("thread {} resumed with no suspend behind it", self.base.id);
            return;
        }
        inner.suspend_count -= 1;
        let waker = if inner.suspend_count == 0 {
            let state = inner.state;
            inner.change_state(state, &self.base);
            inner.waker.take()
        } else {
            None
        };
        // Out of the lock before the wake, for the reason in `stop`.
        drop(inner);
        if let Some(waker) = waker {
            waker.wake_by_ref();
        }
    }

    fn exceptionate(&self) -> Arc<Exceptionate> {
        self.exceptionate.clone()
    }

    fn debug_exceptionate(&self) -> Arc<Exceptionate> {
        panic!("thread do not have debug exceptionate");
    }
}

/// A handle to current thread.
///
/// This is a wrapper of [`Thread`] that provides additional methods for the thread runner.
/// It can only be obtained from the argument of `thread_fn` in a new thread started by [`Thread::start`].
///
/// It will terminate current thread on drop.
///
/// [`Thread`]: crate::task::Thread
/// [`Thread::start`]: crate::task::Thread::start
pub struct CurrentThread(Arc<Thread>);

impl CurrentThread {
    /// Returns the inner structure `Arc<Thread>`.
    pub fn inner(&self) -> Arc<Thread> {
        self.0.clone()
    }

    /// Exit the current thread.
    ///
    /// The thread do not terminate immediately when exited. It is just made dying.
    /// It will terminate after some cleanups on this struct drop.
    pub fn exit(&self) {
        self.stop(false);
    }

    /// Wait until the thread is ready to run (not suspended),
    /// and then take away its context to run the thread.
    pub fn wait_for_run(&self) -> impl Future<Output = Box<UserContext>> {
        #[must_use = "wait_for_run does nothing unless polled/`await`-ed"]
        struct RunnableChecker {
            thread: Arc<Thread>,
        }
        impl Future for RunnableChecker {
            type Output = Box<UserContext>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                let mut inner = self.thread.inner.lock();
                if inner.state() != ThreadState::Suspended {
                    // resume:  return the context token from thread object
                    // There is no need to call change_state here
                    // since take away the context of a non-suspended thread won't change it's state
                    Poll::Ready(inner.context.take().unwrap())
                } else {
                    // suspend: put waker into the thread object
                    inner.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
        RunnableChecker {
            thread: self.0.clone(),
        }
    }

    /// The thread ends running and takes back the context.
    pub fn put_context(&self, context: Box<UserContext>) {
        let mut inner = self.inner.lock();
        inner.context = Some(context);
        // Re-running `change_state` with the state it already has is not a
        // no-op in general: `ThreadInner::state()` reports `Suspended` only once
        // the context is back (a suspended thread is one that has parked its
        // context), so handing the context back is exactly the moment a pending
        // `zx_task_suspend` becomes observable and the object signals must flip.
        //
        // It *is* a no-op for a plain running thread, which is the case on every
        // syscall, page fault and interrupt: nothing suspended us, the reported
        // state is `Running` before and after, and the signals already say
        // THREAD_RUNNING. Skipping it there drops a `KObjectBase` lock
        // acquire/release from the hottest path in the kernel.
        if inner.suspend_count == 0 && inner.state == ThreadState::Running {
            return;
        }
        let state = inner.state;
        inner.change_state(state, &self.base);
    }

    /// Run async future and change state while blocking.
    pub async fn blocking_run<F, T, FT>(
        &self,
        future: F,
        state: ThreadState,
        deadline: Duration,
        cancel_token: Option<Receiver<()>>,
    ) -> ZxResult<T>
    where
        F: Future<Output = FT> + Unpin,
        FT: IntoResult<T>,
    {
        let (old_state, killed) = {
            let mut inner = self.inner.lock();
            if inner.state() == ThreadState::Dying {
                return Err(ZxError::STOP);
            }
            let (sender, receiver) = channel();
            inner.killer = Some(sender);
            let old_state = inner.state;
            inner.change_state(state, &self.base);
            (old_state, receiver)
        };
        let ret = if let Some(cancel_token) = cancel_token {
            select_biased! {
                ret = future.fuse() => ret.into_result(),
                _ = killed.fuse() => Err(ZxError::STOP),
                _ = kernel_hal::thread::sleep_until(deadline).fuse() => Err(ZxError::TIMED_OUT),
                _ = cancel_token.fuse() => Err(ZxError::CANCELED),
            }
        } else {
            select_biased! {
                ret = future.fuse() => ret.into_result(),
                _ = killed.fuse() => Err(ZxError::STOP),
                _ = kernel_hal::thread::sleep_until(deadline).fuse() => Err(ZxError::TIMED_OUT),
            }
        };
        let mut inner = self.inner.lock();
        inner.killer = None;
        if inner.state() == ThreadState::Dying {
            return ret;
        }
        assert_eq!(inner.state, state);
        inner.change_state(old_state, &self.base);
        ret
    }

    /// Create an exception on this thread and wait for the handling.
    pub async fn handle_exception(&self, type_: ExceptionType) {
        let exception = {
            let mut inner = self.inner.lock();
            let cx = if !type_.is_synth() {
                inner.context.as_ref().map(|cx| cx.as_ref())
            } else {
                None
            };
            if !type_.is_synth() {
                error!(
                    "User mode exception: {:?} {:#x?}",
                    type_,
                    cx.expect("Architectural exception should has context")
                );
            }
            let exception = Exception::new(&self.0, type_, cx);
            inner.exception = Some(exception.clone());
            exception
        };
        if type_ == ExceptionType::ThreadExiting {
            let handled = exception.send_to(&self.0.proc().debug_exceptionate());
            if let Ok(future) = handled {
                self.dying_run(future).await.ok();
            }
        } else {
            let future = exception.handle();
            pin_mut!(future);
            self.blocking_run(
                future,
                ThreadState::BlockedException,
                Duration::from_nanos(u64::MAX),
                None,
            )
            .await
            .ok();
        }
        self.inner.lock().exception = None;
    }

    /// Run a blocking task when the thread is exited itself and dying.
    ///
    /// The task will stop running if and once the thread is killed.
    async fn dying_run<F, T, FT>(&self, future: F) -> ZxResult<T>
    where
        F: Future<Output = FT> + Unpin,
        FT: IntoResult<T>,
    {
        let killed = {
            let mut inner = self.inner.lock();
            if inner.killed {
                return Err(ZxError::STOP);
            }
            let (sender, receiver) = channel::<()>();
            inner.killer = Some(sender);
            receiver
        };
        select_biased! {
            ret = future.fuse() => ret.into_result(),
            _ = killed.fuse() => Err(ZxError::STOP),
        }
    }
}

impl core::ops::Deref for CurrentThread {
    type Target = Arc<Thread>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for CurrentThread {
    fn drop(&mut self) {
        // Drop runs while ThreadSwitchFuture may still have the process CR3
        // active; switch back before terminate() can unmap the process VM.
        #[cfg(target_os = "none")]
        kernel_hal::vm::activate_kernel_paging();
        self.terminate();
    }
}

/// `into_result` returns `Self` if the type parameter is already a `ZxResult`,
/// otherwise wraps the value in an `Ok`.
///
/// Used to implement `Thread::blocking_run`, which takes a future whose `Output` may
/// or may not be a `ZxResult`.
pub trait IntoResult<T> {
    /// Performs the conversion.
    fn into_result(self) -> ZxResult<T>;
}

impl<T> IntoResult<T> for T {
    fn into_result(self) -> ZxResult<T> {
        Ok(self)
    }
}

impl<T> IntoResult<T> for ZxResult<T> {
    fn into_result(self) -> ZxResult<T> {
        self
    }
}

/// The thread state.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum ThreadState {
    /// The thread has been created but it has not started running yet.
    #[default]
    New = 0,
    /// The thread is running user code normally.
    Running = 1,
    /// Stopped due to `zx_task_suspend()`.
    Suspended = 2,
    /// In a syscall or handling an exception.
    Blocked = 3,
    /// The thread is in the process of being terminated, but it has not been stopped yet.
    Dying = 4,
    /// The thread has stopped running.
    Dead = 5,
    /// The thread is stopped in an exception.
    BlockedException = 0x103,
    /// The thread is stopped in `zx_nanosleep()`.
    BlockedSleeping = 0x203,
    /// The thread is stopped in `zx_futex_wait()`.
    BlockedFutex = 0x303,
    /// The thread is stopped in `zx_port_wait()`.
    BlockedPort = 0x403,
    /// The thread is stopped in `zx_channel_call()`.
    BlockedChannel = 0x503,
    /// The thread is stopped in `zx_object_wait_one()`.
    BlockedWaitOne = 0x603,
    /// The thread is stopped in `zx_object_wait_many()`.
    BlockedWaitMany = 0x703,
    /// The thread is stopped in `zx_interrupt_wait()`.
    BlockedInterrupt = 0x803,
    /// Pager.
    BlockedPager = 0x903,
}

/// The thread information.
#[repr(C)]
pub struct ThreadInfo {
    state: u32,
    wait_exception_channel_type: u32,
    cpu_affinity_mask: [u64; 8],
}

/// Number of threads currently inside the executor's `poll` — i.e. actually
/// occupying a CPU right now. It is bracketed around [`ThreadSwitchFuture::poll`]
/// (the single point every scheduled thread is polled through).
///
/// This is the instantaneous run count the load-average sampler wants: a thread
/// parked on a `Poll::Pending` future is *not* inside `poll`, so it does not
/// count. That is exactly correct for a Linux-style load average — idle/blocked
/// tasks (a shell in `read`, `top` in `nanosleep`, a daemon waiting on the
/// network) must not inflate the average, even though their `Process` is still
/// `Status::Running` (alive). The previous accounting counted every live
/// process as runnable, so the load average climbed toward the live-process
/// count on a fully idle box.
static RUNNING_THREADS: AtomicUsize = AtomicUsize::new(0);

/// RAII guard that marks the calling thread as running for the span of one
/// `poll`, decrementing again even if the inner future panics.
struct RunningGuard;

impl RunningGuard {
    #[inline]
    fn new() -> Self {
        RUNNING_THREADS.fetch_add(1, Ordering::Relaxed);
        RunningGuard
    }
}

impl Drop for RunningGuard {
    #[inline]
    fn drop(&mut self) {
        RUNNING_THREADS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Number of threads currently executing on a CPU (being polled right now).
///
/// A thread blocked on a pending future is not counted. The sampler that reads
/// this is itself running inside a `poll`, so it should subtract its own
/// contribution to recover the count of *other* runnable threads.
pub fn running_thread_count() -> usize {
    RUNNING_THREADS.load(Ordering::Relaxed)
}

/// Runtime accounting returned by the current `ZX_INFO_TASK_RUNTIME` topic.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TaskRuntimeInfo {
    cpu_time: u64,
    queue_time: u64,
    page_fault_time: u64,
    lock_contention_time: u64,
}

/// Runtime accounting returned by `ZX_INFO_TASK_RUNTIME_V1`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TaskRuntimeInfoV1 {
    cpu_time: u64,
    queue_time: u64,
}

impl From<TaskRuntimeInfo> for TaskRuntimeInfoV1 {
    fn from(info: TaskRuntimeInfo) -> Self {
        Self {
            cpu_time: info.cpu_time,
            queue_time: info.queue_time,
        }
    }
}

struct ThreadSwitchFuture {
    thread: Arc<Thread>,
    // Plain spin mutex (NOT `lock::Mutex`): this guard is held across the inner
    // future's `poll`, which legitimately re-enables interrupts (syscalls run
    // with IRQs on) and can return `Pending` with interrupts still enabled.
    // `lock::Mutex` would `push_off`/`pop_off` around that span, and dropping
    // the guard with interrupts on trips the `pop_off: intr_on` assertion (a
    // kernel panic under heavy mutex/yield workloads, e.g. sysbench `threads`).
    // The future is only ever polled by one executor at a time (task borrow
    // bit), so this mutex is never actually contended.
    future: spin::Mutex<ThreadFuturePinned>,
}

impl ThreadSwitchFuture {
    pub fn new(thread: Arc<Thread>, future: ThreadFuturePinned) -> Self {
        Self {
            future: spin::Mutex::new(future),
            thread,
        }
    }
}

impl Future for ThreadSwitchFuture {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        cfg_if! {
            if #[cfg(all(target_os = "none", target_arch = "aarch64"))] {
                use kernel_hal::arch::config::USER_TABLE_FLAG;
                kernel_hal::vm::activate_paging(self.thread.proc().vmar().table_phys() | USER_TABLE_FLAG);
            } else {
                kernel_hal::vm::activate_paging(self.thread.proc().vmar().table_phys());
            }
        }
        // Resuming real work on this CPU: undo any tickless-idle tick stretch so
        // preemption/HID polling run at full rate again. Cheap no-op (one
        // per-CPU flag read) unless this is the first poll after an idle stretch.
        kernel_hal::timer::timer_idle_exit();
        kernel_hal::thread::set_current_thread(Some(self.thread.clone()));
        let cpu = kernel_hal::cpu::cpu_id() as usize;
        self.thread.set_last_cpu(cpu as u32);
        // CPU-time accounting anchor. The span of one `poll` call is the only
        // wall-clock measurement that can never include time spent blocked:
        // `poll` is synchronous Rust code, so it either runs to completion or
        // returns `Pending` — it cannot itself await, unlike the syscall (or
        // trap handler) it drives, which may suspend for an arbitrarily long
        // time *between* poll calls waiting on a real event. Wrapping a whole
        // syscall in wall-clock instead (including any blocking wait inside
        // it) previously mismeasured a thread parked in poll/epoll/futex as
        // continuously busy — every idle process reading ~1s of `stime` per
        // elapsed second, and the system-wide total following it to ~90% sys
        // with `idle` squeezed out to match.
        //
        // `user_before`/`get_time()` recover how much of this span `run_user`
        // already attributed as user-mode time (via `Thread::time_add`, timed
        // separately and more precisely around `enter_uspace`); the remainder
        // is genuine kernel-mode CPU time — syscall dispatch and trap
        // handling, up to the point the future actually yields — which is
        // `/proc/[pid]/stat`'s `stime`.
        let poll_start = kernel_hal::timer::timer_now();
        let user_before = self.thread.get_time();
        let ret = {
            // Count this thread as running only while it is actually being
            // polled; the guard drops (decrementing) as soon as the poll
            // returns, so a thread that parks on `Pending` stops counting.
            let _running = RunningGuard::new();
            self.future.lock().as_mut().poll(cx)
        };
        let poll_ns = kernel_hal::timer::timer_now()
            .checked_sub(poll_start)
            .unwrap_or_default()
            .as_nanos() as u64;
        let user_ns = self.thread.get_time().saturating_sub(user_before);
        let sys_ns = poll_ns.saturating_sub(user_ns);
        if sys_ns > 0 {
            self.thread.sys_time_add(sys_ns);
            kernel_hal::kstats::note_sys_time(cpu, sys_ns);
        }
        if user_ns > 0 {
            kernel_hal::kstats::note_user_time(cpu, user_ns);
        }
        kernel_hal::thread::set_current_thread(None);
        // Lazy-TLB: keep the process CR3 active after the poll instead of
        // reloading the kernel CR3 every time. A CR3 reload flushes the TLB,
        // which is very expensive (especially under emulation), and doing it
        // on every poll dominated the cost of syscall-heavy / yield-heavy
        // workloads (e.g. sysbench `threads`, ~2 reloads per `sched_yield`).
        // The next poll's `activate_paging` is a no-op when it is the same
        // address space, so consecutive polls of one process cost zero CR3
        // writes. Safety: the kernel mappings are shared into every process
        // page table, so kernel code runs fine under the user CR3; the kernel
        // CR3 is restored (a) before a CPU goes idle / steals work — see the
        // executor idle callback in `zCore::utils::wait_for_exit` — and
        // (b) on thread teardown in `CurrentThread::drop`, both of which run
        // before any process page table can be freed.
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::*;
    use crate::task::*;
    use kernel_hal::timer::timer_now;

    /// A waker that answers one question: was the thread's own lock free when
    /// it was woken?
    ///
    /// `Thread::stop` woke the task with `inner` still held -- a spin lock taken
    /// with interrupts off -- so a woken task that reached back for `inner`
    /// (reading its state, or terminating) was a CPU that did not come back.
    /// `Waiter::wake` in `signal/futex.rs` already invokes its waker after
    /// letting the lock go; this path did not.
    mod lock_probe {
        use super::*;
        use alloc::sync::Arc as ProbeArc;
        use core::sync::atomic::{AtomicBool, Ordering};
        use core::task::{RawWaker, RawWakerVTable, Waker};

        pub struct Probe {
            pub woken: AtomicBool,
            pub lock_was_free: AtomicBool,
            pub thread: Mutex<Option<ProbeArc<Thread>>>,
        }

        impl Probe {
            pub fn new(thread: &ProbeArc<Thread>) -> ProbeArc<Self> {
                ProbeArc::new(Probe {
                    woken: AtomicBool::new(false),
                    lock_was_free: AtomicBool::new(false),
                    thread: Mutex::new(Some(thread.clone())),
                })
            }
        }

        unsafe fn clone_raw(data: *const ()) -> RawWaker {
            ProbeArc::increment_strong_count(data as *const Probe);
            RawWaker::new(data, &VTABLE)
        }

        unsafe fn wake_raw(data: *const ()) {
            wake_by_ref_raw(data);
            drop_raw(data);
        }

        unsafe fn wake_by_ref_raw(data: *const ()) {
            let probe = &*(data as *const Probe);
            probe.woken.store(true, Ordering::SeqCst);
            // The thread is reached through a second handle, exactly as a woken
            // task would reach it, and `try_lock` answers without wedging the
            // test when the answer is "held".
            let thread = probe.thread.lock().clone();
            if let Some(thread) = thread {
                let free = thread.inner.try_lock().is_some();
                probe.lock_was_free.store(free, Ordering::SeqCst);
            }
        }

        unsafe fn drop_raw(data: *const ()) {
            ProbeArc::decrement_strong_count(data as *const Probe);
        }

        static VTABLE: RawWakerVTable =
            RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw);

        pub fn waker_of(probe: &ProbeArc<Probe>) -> Waker {
            let data = ProbeArc::into_raw(probe.clone()) as *const ();
            unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
        }
    }

    #[test]
    fn killing_a_thread_wakes_it_with_its_own_lock_let_go() {
        use core::sync::atomic::Ordering;

        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        let probe = lock_probe::Probe::new(&thread);
        thread.set_waker_for_test(lock_probe::waker_of(&probe));

        thread.kill();

        assert!(
            probe.woken.load(Ordering::SeqCst),
            "killing a thread has to wake whatever it was parked in"
        );
        assert!(
            probe.lock_was_free.load(Ordering::SeqCst),
            "the wake ran with the thread's own lock still held: a woken task \
             that reaches back for it is a CPU that never comes back"
        );
    }

    #[test]
    fn resuming_a_thread_wakes_it_with_its_own_lock_let_go() {
        use core::sync::atomic::Ordering;

        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        thread.suspend();
        let probe = lock_probe::Probe::new(&thread);
        thread.set_waker_for_test(lock_probe::waker_of(&probe));

        thread.resume();

        assert!(
            probe.woken.load(Ordering::SeqCst),
            "resuming a suspended thread has to wake it"
        );
        assert!(
            probe.lock_was_free.load(Ordering::SeqCst),
            "the resume's wake ran with the thread's own lock still held"
        );
    }

    #[test]
    fn create() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");
        assert_eq!(thread.flags(), ThreadFlag::empty());

        assert_eq!(thread.related_koid(), proc.id());
        let child = proc.get_child(thread.id()).unwrap().downcast_arc().unwrap();
        assert!(Arc::ptr_eq(&child, &thread));
    }

    #[async_std::test]
    async fn start() {
        kernel_hal::init();
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");
        let thread1 = Thread::create(&proc, "thread1").expect("failed to create thread");

        // function for new thread
        async fn new_thread(thread: CurrentThread) {
            let cx = thread.wait_for_run().await;
            assert_eq!(cx.general().rip, 1);
            assert_eq!(cx.general().rsp, 4);
            assert_eq!(cx.general().rdi, 3);
            assert_eq!(cx.general().rsi, 2);
            async_std::task::sleep(Duration::from_millis(10)).await;
            thread.put_context(cx);
        }

        // start a new thread
        let handle = Handle::new(proc.clone(), Rights::DEFAULT_PROCESS);
        proc.start(&thread, 1, 4, Some(handle.clone()), 2, |thread| {
            Box::pin(new_thread(thread))
        })
        .expect("failed to start thread");

        // check info and state
        let info = proc.get_info();
        assert!(info.started && !info.has_exited && info.return_code == 0);
        assert_eq!(proc.status(), Status::Running);
        assert_eq!(thread.state(), ThreadState::Running);

        // start again should fail
        assert_eq!(
            proc.start(&thread, 1, 4, Some(handle.clone()), 2, |thread| Box::pin(
                new_thread(thread)
            )),
            Err(ZxError::BAD_STATE)
        );

        // start another thread should fail
        assert_eq!(
            proc.start(&thread1, 1, 4, Some(handle.clone()), 2, |thread| Box::pin(
                new_thread(thread)
            )),
            Err(ZxError::BAD_STATE)
        );

        // wait 100ms for the new thread to exit
        async_std::task::sleep(core::time::Duration::from_millis(100)).await;

        // no other references to `Thread`
        assert_eq!(Arc::strong_count(&thread), 1);
        assert_eq!(thread.state(), ThreadState::Dead);
    }

    #[async_std::test]
    async fn blocking_run() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");
        let thread = CurrentThread(thread);

        let handle = Handle::new(proc.clone(), Rights::DEFAULT_PROCESS);
        let handle_value = proc.add_handle(handle);
        let object = proc
            .get_dyn_object_with_rights(handle_value, Rights::WAIT)
            .unwrap();

        let cancel_token = proc.get_cancel_token(handle_value).unwrap();
        let future = object.wait_signal(Signal::READABLE);
        let deadline = timer_now() + Duration::from_millis(20);
        let result = thread
            .blocking_run(
                future,
                ThreadState::BlockedWaitOne,
                deadline.into(),
                Some(cancel_token),
            )
            .await;
        assert_eq!(result.err(), Some(ZxError::TIMED_OUT));

        let cancel_token = proc.get_cancel_token(handle_value).unwrap();
        let future = object.wait_signal(Signal::READABLE);
        // Two seconds, where this used to be twenty milliseconds. What is being
        // tested is that closing the handle cancels a wait parked on it; the
        // deadline is only a backstop, so that a cancel which never arrives
        // fails the test instead of hanging it. Sized at 20 ms it was a race
        // against the 10 ms sleep below -- and the sleep is an `async_std` task,
        // so it resumes when the executor gets a core, which at 32 test threads
        // took longer than the deadline about one run in thirty: `TIMED_OUT`
        // came back instead of `CANCELED`.
        let deadline = timer_now() + Duration::from_secs(2);
        async_std::task::spawn({
            let proc = proc.clone();
            async move {
                // Long enough that `blocking_run` has parked on the signal
                // before the handle goes away, and far from the deadline.
                async_std::task::sleep(Duration::from_millis(10)).await;
                proc.remove_handle(handle_value).unwrap();
            }
        });
        let result = thread
            .blocking_run(
                future,
                ThreadState::BlockedWaitOne,
                deadline.into(),
                Some(cancel_token),
            )
            .await;
        assert_eq!(result.err(), Some(ZxError::CANCELED));
    }

    #[test]
    fn info() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        let info = thread.get_thread_info();
        assert!(info.state == thread.state() as u32 && info.wait_exception_channel_type == 0);
        assert_eq!(
            thread.get_thread_exception_info().err(),
            Some(ZxError::BAD_STATE)
        );
    }

    #[test]
    fn read_write_state() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        const SIZE: usize = core::mem::size_of::<kernel_hal::context::GeneralRegs>();
        let mut buf = [0; 10];
        assert_eq!(
            thread.read_state(ThreadStateKind::General, &mut buf).err(),
            Some(ZxError::BAD_STATE)
        );
        assert_eq!(
            thread.write_state(ThreadStateKind::General, &buf).err(),
            Some(ZxError::BAD_STATE)
        );

        thread.suspend();

        assert_eq!(
            thread.read_state(ThreadStateKind::General, &mut buf).err(),
            Some(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(
            thread.write_state(ThreadStateKind::General, &buf).err(),
            Some(ZxError::BUFFER_TOO_SMALL)
        );

        let mut buf = [0; SIZE];
        assert!(thread
            .read_state(ThreadStateKind::General, &mut buf)
            .is_ok());
        assert!(thread.write_state(ThreadStateKind::General, &buf).is_ok());
        // TODO
    }

    #[test]
    fn ext() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        let _ext = thread.ext();
        // TODO
    }

    #[async_std::test]
    async fn wait_for_run() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        assert_eq!(thread.state(), ThreadState::New);

        thread.start(|thread| Box::pin(new_thread(thread))).unwrap();
        async fn new_thread(thread: CurrentThread) {
            assert_eq!(thread.state(), ThreadState::Running);

            // without suspend
            let context = thread.wait_for_run().await;
            thread.put_context(context);

            // with suspend
            thread.suspend();
            thread.suspend();
            assert_eq!(thread.state(), ThreadState::Suspended);
            async_std::task::spawn({
                let thread = (*thread).clone();
                async move {
                    async_std::task::sleep(Duration::from_millis(10)).await;
                    thread.resume();
                    async_std::task::sleep(Duration::from_millis(10)).await;
                    thread.resume();
                }
            });
            let time = timer_now();
            let _context = thread.wait_for_run().await;
            assert!(timer_now() - time >= Duration::from_millis(20));
        }
        let thread: Arc<dyn KernelObject> = thread;
        thread.wait_signal(Signal::THREAD_TERMINATED).await;
    }

    #[test]
    fn time() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        assert_eq!(thread.get_time(), 0);
        thread.time_add(10);
        assert_eq!(thread.get_time(), 10);
    }
    /// `Process::suspend` counts on the threads the process holds at that
    /// moment and `Process::resume` on the threads it holds now, so a thread
    /// created in between is resumed having never been suspended. That used to
    /// be an `assert_ne!` in `Thread::resume`, that is a kernel panic, and
    /// `SuspendToken::create` takes any `Arc<dyn Task>`, process handles
    /// included.
    #[test]
    fn a_thread_born_while_the_process_was_suspended_does_not_panic_on_resume() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let first = Thread::create(&proc, "first").expect("failed to create thread");

        let task: Arc<dyn Task> = proc.clone();
        let token = crate::task::SuspendToken::create(&task);
        let late = Thread::create(&proc, "late").expect("failed to create thread");

        // Dropping the token resumes every thread the process has NOW, and one
        // of them was never suspended.
        drop(token);

        // The one that was suspended came back, so a matched pair still
        // balances, and a bare resume on the other is still nothing to do.
        first.suspend();
        first.resume();
        late.resume();
    }
}

#[cfg(test)]
mod sched_tests {
    use super::*;
    use crate::task::*;

    fn a_thread() -> Arc<Thread> {
        let root = Job::root();
        let proc = Process::create(&root, "proc").expect("un proceso");
        Thread::create(&proc, "thread").expect("un hilo")
    }

    /// `sched_prio_to_weight[]` as Linux ships it (`kernel/sched/core.c`).
    /// Copied out on purpose rather than computed: a typo in a table is exactly
    /// what a table test is for, and a wrong entry is a thread that quietly
    /// gets the CPU share of a different nice level.
    const LINUX_WEIGHTS: [u32; 40] = [
        88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100,
        4904, 3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172,
        137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
    ];

    #[test]
    fn the_weight_table_is_the_one_linux_ships() {
        assert_eq!(
            SCHED_PRIO_TO_WEIGHT, LINUX_WEIGHTS,
            "la tabla de pesos no es la de Linux"
        );
        for (i, w) in LINUX_WEIGHTS.iter().enumerate() {
            let nice = i as i32 - 20;
            assert_eq!(
                nice_to_weight(nice as i8),
                *w,
                "el peso de nice {} no es el de Linux",
                nice
            );
        }
        assert_eq!(nice_to_weight(0), WEIGHT_NICE0, "nice 0 no pesa 1024");
        // Fuera del rango se recorta, no se sale de la tabla.
        assert_eq!(
            nice_to_weight(-100),
            nice_to_weight(MIN_NICE),
            "nice muy bajo no se recorta"
        );
        assert_eq!(
            nice_to_weight(100),
            nice_to_weight(MAX_NICE),
            "nice muy alto no se recorta"
        );
    }

    #[test]
    fn a_nice_zero_fair_thread_gets_the_base_slice() {
        let t = a_thread();
        assert_eq!(
            t.sched_policy(),
            SCHED_NORMAL,
            "un hilo nuevo no nace SCHED_NORMAL"
        );
        assert_eq!(t.sched_nice(), 0, "un hilo nuevo no nace con nice 0");
        assert_eq!(
            t.sched_rt_priority(),
            0,
            "un hilo nuevo nace con prioridad de tiempo real"
        );
        assert!(!t.sched_is_realtime());
        assert_eq!(
            t.timeslice_ns(),
            BASE_TIMESLICE_NS,
            "nice 0 no da el slice base"
        );
    }

    /// One nice step is ~25% of load weight, i.e. ~10% of CPU: that is the whole
    /// point of `nice`, and a slice that does not move with it makes `nice` a
    /// number that is reported and ignored.
    #[test]
    fn one_nice_step_moves_the_slice_by_about_a_quarter() {
        let t = a_thread();
        for nice in -4..=4i8 {
            t.set_sched(SCHED_NORMAL, nice, 0);
            let lo = t.timeslice_ns();
            t.set_sched(SCHED_NORMAL, nice - 1, 0);
            let hi = t.timeslice_ns();
            assert!(
                hi > lo,
                "nice {} da un slice de {} ns y nice {} otro de {} ns: no baja al subir nice",
                nice - 1,
                hi,
                nice,
                lo
            );
            // 88/64 = 1.375 y 72/64 = 1.125 acotan el 25% de la tabla con holgura.
            assert!(
                hi * 64 > lo * 72 && hi * 64 < lo * 88,
                "de nice {} a nice {} el slice pasa de {} a {} ns, que no es el ~25% de la tabla",
                nice,
                nice - 1,
                lo,
                hi
            );
        }
    }

    #[test]
    fn the_slice_is_clamped_at_both_ends() {
        let t = a_thread();
        t.set_sched(SCHED_NORMAL, MIN_NICE, 0);
        assert_eq!(
            t.timeslice_ns(),
            MAX_TIMESLICE_NS,
            "nice -20 no se recorta por arriba"
        );
        t.set_sched(SCHED_NORMAL, MAX_NICE, 0);
        assert_eq!(
            t.timeslice_ns(),
            MIN_TIMESLICE_NS,
            "nice 19 no se recorta por abajo"
        );
        assert!(MIN_TIMESLICE_NS < BASE_TIMESLICE_NS && BASE_TIMESLICE_NS < MAX_TIMESLICE_NS);
    }

    #[test]
    fn every_policy_gets_the_slice_it_is_promised() {
        let t = a_thread();
        t.set_sched(SCHED_FIFO, 0, 50);
        assert_eq!(t.timeslice_ns(), u64::MAX, "un FIFO se reparte el tiempo");
        assert!(t.sched_is_realtime());
        assert!(
            !t.tick_should_preempt(),
            "un FIFO se desaloja por fin de slice"
        );

        t.set_sched(SCHED_RR, 0, 50);
        assert_eq!(
            t.timeslice_ns(),
            RR_TIMESLICE_NS,
            "un RR no da el cuanto de 100 ms"
        );
        assert!(t.sched_is_realtime());

        t.set_sched(SCHED_IDLE, 0, 0);
        assert_eq!(
            t.timeslice_ns(),
            MIN_TIMESLICE_NS,
            "un IDLE no da el slice mas corto"
        );
        assert!(!t.sched_is_realtime());

        // BATCH y DEADLINE se planifican como NORMAL: el nice cuenta.
        for policy in [SCHED_BATCH, SCHED_DEADLINE] {
            t.set_sched(policy, 0, 0);
            assert_eq!(
                t.timeslice_ns(),
                BASE_TIMESLICE_NS,
                "la politica {} no se planifica como NORMAL",
                policy
            );
            assert!(
                !t.sched_is_realtime(),
                "la politica {} sale como tiempo real",
                policy
            );
        }
    }

    #[test]
    fn changing_the_policy_drops_the_slice_that_was_running() {
        let t = a_thread();
        t.sched.slice_end_ns.store(1_234_567, Ordering::Relaxed);
        t.set_sched(SCHED_NORMAL, 5, 0);
        assert_eq!(
            t.sched.slice_end_ns.load(Ordering::Relaxed),
            0,
            "cambiar la politica deja corriendo el slice de la anterior"
        );
        assert_eq!(t.sched_nice(), 5);
    }

    const SLICE: u64 = 20_000_000;

    #[test]
    fn the_first_tick_of_a_thread_starts_its_slice_instead_of_ending_it() {
        let now = 1_000_000_000;
        assert_eq!(
            slice_verdict(now, 0, SLICE),
            (false, now + SLICE),
            "el primer tick de un hilo lo desaloja"
        );
    }

    #[test]
    fn a_tick_inside_the_slice_leaves_the_deadline_where_it_was() {
        let now = 1_000_000_000;
        let end = now + 5_000_000;
        assert_eq!(
            slice_verdict(now, end, SLICE),
            (false, end),
            "un tick a mitad de slice mueve la fecha limite o desaloja"
        );
    }

    #[test]
    fn a_tick_past_the_deadline_preempts_and_starts_the_next_slice() {
        let now = 1_000_000_000;
        // Un tick de planificador (4 ms) despues de vencer: es como se ve de verdad.
        let end = now - 4_000_000;
        assert_eq!(
            slice_verdict(now, end, SLICE),
            (true, now + SLICE),
            "un slice vencido no desaloja"
        );
        // Y justo en la fecha limite, tambien.
        assert_eq!(slice_verdict(now, now, SLICE), (true, now + SLICE));
    }

    /// Where the line is. A tick that arrives a whole slice late is still a
    /// thread that was running -- a CPU busy with interrupts delays the
    /// scheduler tick -- so it is preempted. One nanosecond further back is a
    /// span no running thread can have gone unticked, so it slept.
    #[test]
    fn a_tick_a_whole_slice_late_is_still_the_slice_running_out() {
        let now = 1_000_000_000;
        assert_eq!(
            slice_verdict(now, now - SLICE, SLICE),
            (true, now + SLICE),
            "un tick que llega un slice tarde se toma por un hilo que despierta"
        );
        assert_eq!(
            slice_verdict(now, now - SLICE - 1, SLICE),
            (false, now + SLICE),
            "un nanosegundo mas atras todavia se toma por un slice agotado"
        );
    }

    /// The bug. A thread that blocked keeps the deadline it had before it went
    /// to sleep, so waking up looked exactly like running out of time.
    #[test]
    fn a_thread_that_slept_longer_than_its_slice_is_not_preempted_the_moment_it_wakes() {
        let now = 2_000_000_000;
        for slept in [SLICE + 1, 100_000_000, 1_000_000_000, now] {
            let end = now - slept;
            assert_eq!(
                slice_verdict(now, end, SLICE),
                (false, now + SLICE),
                "un hilo que durmio {} ns con un slice de {} ns se desaloja al despertar",
                slept,
                SLICE
            );
        }
    }

    #[test]
    fn a_deadline_further_out_than_a_whole_slice_is_not_believed() {
        let now = 1_000_000_000;
        let end = now + SLICE + 1;
        assert_eq!(
            slice_verdict(now, end, SLICE),
            (false, now + SLICE),
            "una fecha limite de un reloj que fue hacia atras se cree"
        );
        // Justo un slice por delante si es creible: es lo que acabamos de poner.
        let end = now + SLICE;
        assert_eq!(slice_verdict(now, end, SLICE), (false, end));
    }

    #[test]
    fn a_deadline_at_the_end_of_the_clock_does_not_wrap_around() {
        let now = u64::MAX - 1;
        let (preempt, next) = slice_verdict(now, now - 1, SLICE);
        assert!(preempt, "un slice vencido al final del reloj no desaloja");
        assert_eq!(
            next,
            u64::MAX,
            "la fecha limite da la vuelta en vez de saturar"
        );
        assert_eq!(
            slice_verdict(u64::MAX, 0, SLICE),
            (false, u64::MAX),
            "el primer slice al final del reloj da la vuelta"
        );
    }

    /// The fix the clock cannot do on its own: a wait SHORTER than a slice.
    ///
    /// A media thread runs 15 ms, waits 8 ms for its audio callback and wakes.
    /// Its deadline is 3 ms in the past, which is exactly what a late tick looks
    /// like, so `slice_verdict` preempts it -- correctly, on the evidence it
    /// has. The park flag is the evidence it does not have.
    #[test]
    fn a_wait_shorter_than_a_slice_still_gets_a_fresh_slice_when_the_thread_says_it_parked() {
        let t = a_thread();
        let slice = t.timeslice_ns();
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        // Corrio 15 ms, esperó 8 ms: la fecha limite quedo 3 ms atras.
        let end = now.saturating_sub(3_000_000);
        t.sched.slice_end_ns.store(end, Ordering::Relaxed);
        // Sin el flag, el reloj no puede saberlo y lo desaloja.
        assert!(
            slice_verdict(now, end, slice).0,
            "el reloj deberia ver esto como un slice agotado; es lo unico que puede ver"
        );
        // Con el flag, el hilo lo dice.
        t.sched_note_parked();
        t.sched_note_resumed();
        assert_eq!(
            t.sched.slice_end_ns.load(Ordering::Relaxed),
            0,
            "despertar de una espera corta no da slice nuevo"
        );
        assert!(
            !t.tick_should_preempt(),
            "y el primer tick tras despertar sigue quitandole la CPU"
        );
    }

    #[test]
    fn a_poll_that_never_parked_does_not_help_itself_to_a_fresh_slice() {
        let t = a_thread();
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        let end = now.saturating_sub(4_000_000);
        t.sched.slice_end_ns.store(end, Ordering::Relaxed);
        // Un future que estaba listo a la primera no aparca, asi que resumed()
        // no tiene nada que perdonar.
        t.sched_note_resumed();
        assert_eq!(
            t.sched.slice_end_ns.load(Ordering::Relaxed),
            end,
            "un poll que no aparco se regala un slice"
        );
        assert!(
            t.tick_should_preempt(),
            "y ademas se queda la CPU con el slice agotado"
        );
    }

    #[test]
    fn a_park_is_forgiven_once_and_not_twice() {
        let t = a_thread();
        t.sched_note_parked();
        t.sched_note_resumed();
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        let end = now.saturating_sub(4_000_000);
        t.sched.slice_end_ns.store(end, Ordering::Relaxed);
        // El segundo resumed() sin un parked() por delante no hace nada: si lo
        // hiciera, un hilo que nunca se bloquea no se desalojaria jamas.
        t.sched_note_resumed();
        assert_eq!(
            t.sched.slice_end_ns.load(Ordering::Relaxed),
            end,
            "un solo aparcado perdona dos despertares"
        );
    }

    /// The whole thing through the thread, with the clock it really uses.
    #[test]
    fn a_thread_that_just_woke_keeps_the_cpu_and_one_that_ran_out_does_not() {
        let t = a_thread();
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        // Lo que deja un hilo que se bloqueo un segundo.
        t.sched
            .slice_end_ns
            .store(now.saturating_sub(1_000_000_000), Ordering::Relaxed);
        assert!(
            !t.tick_should_preempt(),
            "un hilo que acaba de despertar pierde la CPU en su primer tick"
        );
        assert!(
            t.sched.slice_end_ns.load(Ordering::Relaxed) > now,
            "y no se le ha dado un slice nuevo"
        );
        // Lo que deja un hilo que si agoto su slice: vencio hace un tick.
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        t.sched
            .slice_end_ns
            .store(now.saturating_sub(4_000_000), Ordering::Relaxed);
        assert!(
            t.tick_should_preempt(),
            "un hilo que agoto su slice no lo suelta"
        );
    }
}

/// The thread's own state machine, its signals, and the accounting a dying
/// thread hands back to its process.
///
/// The 28 tests above are almost all about how the scheduler hands out CPU
/// slices. Everything else a `Thread` is — what state it reports while it is
/// suspended, which signals each transition publishes, the generic "blocked in
/// a syscall" marker, its CPU affinity, its two time counters and whether it
/// is the first thread of its process — had no test at all, and mutating it
/// left 16 of 26 mutants alive.
#[cfg(test)]
mod state_and_accounting_tests {
    use super::*;
    use crate::task::*;

    fn a_thread() -> Arc<Thread> {
        let root = Job::root();
        let proc = Process::create(&root, "proc").expect("process");
        Thread::create(&proc, "thread").expect("thread")
    }

    /// Suspension does not overwrite what the thread was actually doing.
    ///
    /// The comment on the gate says the order is `Dying > Exception > Suspend >
    /// Blocked`: a thread stopped in an exception reports the exception even
    /// while a suspend token is held over it, because that is what the debugger
    /// on the other end of the exception channel needs to see. Only a thread
    /// with nothing more specific to say reports `Suspended`.
    #[test]
    fn a_suspended_thread_still_reports_what_stopped_it() {
        let t = a_thread();
        {
            let mut inner = t.inner.lock();
            inner.suspend_count = 1;
            inner.state = ThreadState::BlockedException;
        }
        assert_eq!(
            t.state(),
            ThreadState::BlockedException,
            "a suspend hid the exception the thread is stopped in"
        );

        t.inner.lock().state = ThreadState::BlockedChannel;
        assert_eq!(
            t.state(),
            ThreadState::Suspended,
            "a plain blocked thread under a suspend reports the suspend"
        );

        t.inner.lock().state = ThreadState::Dying;
        assert_eq!(
            t.state(),
            ThreadState::Dying,
            "a dying thread outranks the suspend over it"
        );

        t.inner.lock().suspend_count = 0;
        assert_eq!(t.state(), ThreadState::Dying);
    }

    /// A thread with no context saved is not suspended, whatever the count
    /// says: there is nothing parked to hold.
    #[test]
    fn a_thread_with_no_context_is_never_reported_suspended() {
        let t = a_thread();
        let mut inner = t.inner.lock();
        inner.suspend_count = 3;
        inner.state = ThreadState::Running;
        inner.context = None;
        assert_eq!(inner.state(), ThreadState::Running);
    }

    /// Each transition publishes the signal set `zx_object_wait_one` is
    /// waiting on, and clears the ones that no longer hold. Leaving a stale
    /// bit behind is a waiter that wakes on a thread state that is over.
    #[test]
    fn every_transition_leaves_exactly_the_signals_that_still_hold() {
        let t = a_thread();

        t.inner.lock().change_state(ThreadState::Running, &t.base);
        let s = t.base.signal();
        assert!(s.contains(Signal::THREAD_RUNNING));
        assert!(!s.contains(Signal::THREAD_SUSPENDED));
        assert!(!s.contains(Signal::THREAD_TERMINATED));

        t.suspend();
        let s = t.base.signal();
        assert!(
            s.contains(Signal::THREAD_SUSPENDED),
            "a suspended thread has to publish THREAD_SUSPENDED"
        );
        assert!(
            !s.contains(Signal::THREAD_RUNNING),
            "and stop publishing THREAD_RUNNING"
        );

        t.resume();
        let s = t.base.signal();
        assert!(s.contains(Signal::THREAD_RUNNING));
        assert!(
            !s.contains(Signal::THREAD_SUSPENDED),
            "a resumed thread still looked suspended to anyone waiting"
        );

        t.inner.lock().change_state(ThreadState::Dead, &t.base);
        let s = t.base.signal();
        assert!(s.contains(Signal::THREAD_TERMINATED));
        assert!(!s.contains(Signal::THREAD_RUNNING));
        assert!(!s.contains(Signal::THREAD_SUSPENDED));
    }

    /// `New` and `Dying` publish nothing at all — and that includes taking
    /// `THREAD_TERMINATED` back down. A thread that reached `Dead` and is then
    /// moved to `Dying` (a kill racing teardown) would otherwise stay
    /// terminated for every waiter while it is still being taken apart.
    #[test]
    fn a_dying_thread_publishes_none_of_the_three_signals() {
        let t = a_thread();
        t.inner.lock().change_state(ThreadState::Dead, &t.base);
        assert!(t.base.signal().contains(Signal::THREAD_TERMINATED));

        t.inner.lock().change_state(ThreadState::Dying, &t.base);
        let s = t.base.signal();
        assert!(
            !s.contains(Signal::THREAD_TERMINATED),
            "a dying thread was still publishing THREAD_TERMINATED"
        );
        assert!(!s.contains(Signal::THREAD_RUNNING));
        assert!(!s.contains(Signal::THREAD_SUSPENDED));
    }

    /// Only a `Running` thread becomes `Blocked`, and only the generic
    /// `Blocked` goes back to `Running`.
    ///
    /// This is the marker `/proc/<pid>/status` reads. It defers to everything
    /// more specific on purpose: `blocking_run` records *why* a thread waits
    /// and asserts on return that nobody moved the state under it, so
    /// overwriting `BlockedFutex` both loses that reason and trips the
    /// assertion. Teardown wins for the same reason — an unblock racing a kill
    /// must not resurrect a dying thread into `Running`.
    #[test]
    fn the_generic_blocked_marker_defers_to_every_more_specific_state() {
        let t = a_thread();
        t.inner.lock().state = ThreadState::Running;
        assert!(t.set_blocked(true), "a running thread blocks");
        assert_eq!(t.inner.lock().state, ThreadState::Blocked);

        assert!(
            !t.set_blocked(true),
            "blocking an already blocked thread changes nothing"
        );

        assert!(t.set_blocked(false), "and the generic block comes back");
        assert_eq!(t.inner.lock().state, ThreadState::Running);

        for specific in [
            ThreadState::BlockedFutex,
            ThreadState::BlockedChannel,
            ThreadState::BlockedPort,
            ThreadState::Dying,
            ThreadState::Dead,
            ThreadState::New,
            ThreadState::Suspended,
        ] {
            t.inner.lock().state = specific;
            assert!(
                !t.set_blocked(true),
                "{:?} was overwritten by the generic blocked marker",
                specific
            );
            assert!(
                !t.set_blocked(false),
                "{:?} was resurrected into Running by an unblock",
                specific
            );
            assert_eq!(t.inner.lock().state, specific, "{:?} moved", specific);
        }
    }

    /// An all-zero affinity mask would make the thread unschedulable, so it is
    /// refused; every other mask is stored as given.
    #[test]
    fn a_thread_may_not_be_pinned_to_no_cpu_at_all() {
        let t = a_thread();
        let before = t.affinity();

        assert_eq!(t.set_affinity(0), Err(ZxError::INVALID_ARGS));
        assert_eq!(t.affinity(), before, "the refused mask was stored anyway");

        assert!(t.set_affinity(0b1010).is_ok());
        assert_eq!(t.affinity(), 0b1010);
        assert!(t.set_affinity(u64::MAX).is_ok());
        assert_eq!(t.affinity(), u64::MAX);
        assert!(t.set_affinity(1).is_ok());
        assert_eq!(t.affinity(), 1);
    }

    /// `ZX_INFO_THREAD` publishes the mask in the FIRST word of the eight it
    /// carries, which is where `zx_object_get_info` and `sched_getaffinity`
    /// read CPUs 0..63. Putting it anywhere else reports a thread pinned to no
    /// CPU at all.
    #[test]
    fn the_affinity_mask_is_published_in_the_first_word_of_the_info() {
        let t = a_thread();
        t.set_affinity(0b1101).unwrap();
        let info = t.get_thread_info();
        assert_eq!(info.cpu_affinity_mask[0], 0b1101);
        assert!(
            info.cpu_affinity_mask[1..].iter().all(|w| *w == 0),
            "the mask leaked into a word nobody reads"
        );
        assert_eq!(info.state, ThreadState::New as u32);
        assert_eq!(
            info.wait_exception_channel_type, 0,
            "a thread in no exception waits on no channel"
        );
    }

    /// The two time counters are separate: user time and kernel time, each
    /// accumulating rather than replacing. `/proc/<pid>/stat` fields 14 and 15
    /// are these two, and `times(2)` adds them up.
    #[test]
    fn the_two_clocks_of_a_thread_accumulate_and_do_not_mix() {
        let t = a_thread();
        assert_eq!((t.get_time(), t.get_sys_time()), (0, 0));

        t.time_add(700);
        t.time_add(300);
        assert_eq!(t.get_time(), 1000, "user time replaced instead of adding");
        assert_eq!(t.get_sys_time(), 0, "user time landed in the kernel clock");

        t.sys_time_add(40);
        t.sys_time_add(2);
        assert_eq!(
            t.get_sys_time(),
            42,
            "kernel time replaced instead of adding"
        );
        assert_eq!(t.get_time(), 1000, "kernel time landed in the user clock");
    }

    /// Runtime info reports the CPU time twice, as cpu and queue time, because
    /// zCore does not measure run-queue latency separately yet. Reporting zero
    /// for the second is a thread that looks like it never waited.
    #[test]
    fn runtime_info_carries_the_cpu_time_in_both_of_the_fields_it_fills() {
        let t = a_thread();
        t.time_add(5_000);
        let info = t.get_runtime_info();
        assert_eq!(info.cpu_time, 5_000);
        assert_eq!(info.queue_time, 5_000);
        assert_eq!(info.page_fault_time, 0);
        assert_eq!(info.lock_contention_time, 0);
    }

    /// A thread that exits hands both of its clocks to its process before it
    /// leaves the thread list, so `getrusage`, `times` and the parent's
    /// `wait4` still see the time it burned.
    #[test]
    fn a_dying_thread_hands_both_its_clocks_to_its_process() {
        let root = Job::root();
        let proc = Process::create(&root, "proc").expect("process");
        let t = Thread::create(&proc, "thread").expect("thread");
        t.time_add(9_000);
        t.sys_time_add(1_500);

        assert_eq!(
            (proc.dead_threads_time(), proc.dead_threads_sys_time()),
            (0, 0)
        );
        t.terminate_abandoned();

        assert_eq!(
            proc.dead_threads_time(),
            9_000,
            "the thread's user time died with it"
        );
        assert_eq!(
            proc.dead_threads_sys_time(),
            1_500,
            "the thread's kernel time died with it"
        );
    }

    /// Only the thread a process was started with is its first thread; every
    /// later one is not. `Process::exit` and the initial-thread rules hang off
    /// this.
    #[test]
    fn only_the_thread_a_process_started_with_is_its_first() {
        let root = Job::root();
        let proc = Process::create(&root, "proc").expect("process");
        let first = Thread::create(&proc, "first").expect("thread");
        let second = Thread::create(&proc, "second").expect("thread");

        assert!(!first.is_first_thread(), "no thread is first before start");
        first.set_first_thread();
        assert!(first.is_first_thread());
        assert!(
            !second.is_first_thread(),
            "a later thread came out as the first one"
        );
    }

    /// The flags a thread carries are its own, and `update_flags` is the only
    /// way they move.
    #[test]
    fn a_threads_flags_are_what_was_last_written_into_them() {
        let t = a_thread();
        assert_eq!(t.flags(), ThreadFlag::empty());

        t.update_flags(|f| f.insert(ThreadFlag::VCPU));
        assert_eq!(
            t.flags(),
            ThreadFlag::VCPU,
            "the flag went in but the reader does not see it"
        );

        t.update_flags(|f| f.remove(ThreadFlag::VCPU));
        assert_eq!(t.flags(), ThreadFlag::empty());
    }
}
