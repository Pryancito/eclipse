//! Kernel object basis.
//!
//! # Create new kernel object
//!
//! - Create a new struct.
//! - Make sure it has a field named `base` with type [`KObjectBase`].
//! - Implement [`KernelObject`] trait with [`impl_kobject`] macro.
//!
//! ## Example
//! ```
//! use zircon_object::object::*;
//! extern crate alloc;
//!
//! pub struct SampleObject {
//!    base: KObjectBase,
//! }
//! impl_kobject!(SampleObject);
//! ```
//!
//! # Implement methods for kernel object
//!
//! ## Constructor
//!
//! Each kernel object should have a constructor returns `Arc<Self>`
//! (or a pair of them, e.g. [`Channel`]).
//!
//! Don't return `Self` since it must be created on heap.
//!
//! ### Example
//! ```
//! use zircon_object::object::*;
//! use std::sync::Arc;
//!
//! pub struct SampleObject {
//!     base: KObjectBase,
//! }
//! impl SampleObject {
//!     pub fn new() -> Arc<Self> {
//!         Arc::new(SampleObject {
//!             base: KObjectBase::new(),
//!         })
//!     }
//! }
//! ```
//!
//! ## Interior mutability
//!
//! All kernel objects use the [interior mutability pattern] :
//! each method takes either `&self` or `&Arc<Self>` as the first argument.
//!
//! To handle mutable variable, create another **inner structure**,
//! and put it into the object with a lock wrapped.
//!
//! ### Example
//! ```
//! use zircon_object::object::*;
//! use std::sync::Arc;
//! use kernel_hal::sync::Mutex;
//!
//! pub struct SampleObject {
//!     base: KObjectBase,
//!     inner: Mutex<SampleObjectInner>,
//! }
//! struct SampleObjectInner {
//!     x: usize,
//! }
//!
//! impl SampleObject {
//!     pub fn set_x(&self, x: usize) {
//!         let mut inner = self.inner.lock();
//!         inner.x = x;
//!     }
//! }
//! ```
//!
//! # Downcast trait to concrete type
//!
//! [`KernelObject`] inherit [`downcast_rs::DowncastSync`] trait.
//! You can use `downcast_arc` method to downcast `Arc<dyn KernelObject>` to `Arc<T: KernelObject>`.
//!
//! ## Example
//! ```
//! use zircon_object::object::*;
//! use std::sync::Arc;
//!
//! let object: Arc<dyn KernelObject> = DummyObject::new();
//! let concrete = object.downcast_arc::<DummyObject>().unwrap();
//! ```
//!
//! [`Channel`]: crate::ipc::Channel
//! [`KObjectBase`]: KObjectBase
//! [`KernelObject`]: KernelObject
//! [`impl_kobject`]: impl_kobject
//! [`downcast_rs::DowncastSync`]: downcast_rs::DowncastSync
//! [interior mutability pattern]: https://doc.rust-lang.org/reference/interior-mutability.html

use {
    crate::signal::*,
    alloc::{boxed::Box, string::String, sync::Arc, vec::Vec},
    core::{
        fmt::Debug,
        future::Future,
        pin::Pin,
        sync::atomic::*,
        task::{Context, Poll},
    },
    downcast_rs::{impl_downcast, DowncastSync},
    kernel_hal::sync::{HeldByCurrentCpu, Mutex},
};

pub use {super::*, clock::*, counter::*, handle::*, rights::*, signal::*};

mod clock;
mod counter;
mod handle;
mod rights;
#[allow(hidden_glob_reexports)]
mod signal;

/// Common interface of a kernel object.
///
/// Implemented by [`impl_kobject`] macro.
///
/// [`impl_kobject`]: impl_kobject
pub trait KernelObject: DowncastSync + Debug {
    /// Get object's KoID.
    fn id(&self) -> KoID;
    /// Get the name of the type of the kernel object.
    fn type_name(&self) -> &str;
    /// Get object's name.
    fn name(&self) -> alloc::string::String;
    /// Get object's name without blocking on the object lock (`None` when
    /// it is held); see [`KObjectBase::try_name`].
    ///
    /// The default answers `None` rather than falling back to `name()`: an
    /// implementation that does not go through `impl_kobject!` would
    /// otherwise silently reintroduce the self-deadlock this method exists
    /// to prevent (a diagnostic run from inside a signal callback waiting on
    /// the lock its own CPU holds). Losing a name in a trace is the cheaper
    /// failure; override this to provide one.
    fn try_name(&self) -> Option<alloc::string::String> {
        None
    }
    /// Set object's name.
    fn set_name(&self, name: &str);
    /// Get the signal status.
    fn signal(&self) -> Signal;
    /// Assert `signal`.
    fn signal_set(&self, signal: Signal);
    /// Deassert `signal`.
    fn signal_clear(&self, signal: Signal);
    /// Change signal status: first `clear` then `set` indicated bits.
    ///
    /// All signal callbacks will be called.
    fn signal_change(&self, clear: Signal, set: Signal);
    /// Add `callback` for signal status changes.
    ///
    /// The `callback` is a function of `Fn(Signal) -> bool`.
    /// It returns a bool indicating whether the handle process is over.
    /// If true, the function will never be called again.
    fn add_signal_callback(&self, callback: SignalHandler);
    /// Attempt to find a child of the object with given KoID.
    ///
    /// If the object is a *Process*, the *Threads* it contains may be obtained.
    ///
    /// If the object is a *Job*, its (immediate) child *Jobs* and the *Processes*
    /// it contains may be obtained.
    ///
    /// If the object is a *Resource*, its (immediate) child *Resources* may be obtained.
    fn get_child(&self, _id: KoID) -> ZxResult<Arc<dyn KernelObject>> {
        Err(ZxError::WRONG_TYPE)
    }
    /// Attempt to get the object's peer.
    ///
    /// An object peer is the opposite endpoint of a `Channel`, `Socket`, `Fifo`, or `EventPair`.
    fn peer(&self) -> ZxResult<Arc<dyn KernelObject>> {
        Err(ZxError::NOT_SUPPORTED)
    }
    /// If the object is related to another (such as the other end of a channel, or the parent of
    /// a job), returns the KoID of that object, otherwise returns zero.
    fn related_koid(&self) -> KoID {
        0
    }
    /// Get object's allowed signals.
    fn allowed_signals(&self) -> Signal {
        Signal::USER_ALL
    }
}

impl_downcast!(sync KernelObject);

/// The base struct of a kernel object.
pub struct KObjectBase {
    /// The object's KoID.
    pub id: KoID,
    /// Whether `id` was drawn from the recycling PID pool (processes/threads
    /// only) and must be returned to it on drop. `with_id` and plain-object
    /// constructors never set this, so fixed ids (init=1, shells 101..) and
    /// high monotonic ids are never fed back into the pool.
    pooled: bool,
    inner: Mutex<KObjectBaseInner>,
}

/// Recycling allocator for the ids userspace sees as Linux pids/tids.
///
/// Only `Process` and `Thread` draw from here (via
/// [`KObjectBase::with_name_pooled`]); every other kernel object takes a
/// monotonic id ≥ 2^32 from [`KObjectBase::new_koid`], so object churn (VMOs,
/// handles, channels — hundreds per spawn) no longer inflates the pid space.
/// Ids live in `[FLOOR, CEIL)`; `CEIL` matches the `pid_max` procfs announces,
/// so a pid is always a valid Linux `pid_t` and never crosses the i32 border
/// no matter the uptime.
///
/// An id returns to the pool when its `KObjectBase` drops — and a parent holds
/// an `Arc` of each child until it is reaped, so a pid cannot be reused while
/// a zombie still wears it: Linux's rule, enforced by lifetime instead of
/// bookkeeping.
mod pid_pool {
    use alloc::collections::VecDeque;
    use core::sync::atomic::{AtomicU64, Ordering};
    use lock::Mutex;

    /// First pooled pid. Everything below is reserved for fixed ids
    /// (`with_id`: init=1, per-tty shells 101..106).
    pub(super) const FLOOR: u64 = 1024;
    /// One past the highest pooled pid; equals Linux's PID_MAX_LIMIT and the
    /// value `/proc/sys/kernel/pid_max` reports.
    pub(super) const CEIL: u64 = 4_194_304;

    struct Pool {
        free: VecDeque<u32>,
        next_fresh: u32,
    }

    static POOL: Mutex<Pool> = Mutex::new(Pool {
        free: VecDeque::new(),
        next_fresh: FLOOR as u32,
    });
    static RECYCLED: AtomicU64 = AtomicU64::new(0);

    /// A pooled pid, or `None` when over 4M processes/threads are alive at
    /// once (the caller falls back to a high monotonic id rather than fail).
    pub(super) fn alloc() -> Option<u64> {
        let mut p = POOL.lock();
        if let Some(id) = p.free.pop_front() {
            let n = RECYCLED.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                // One line per boot: proof in the log that recycling is live.
                log::info!("[pid] first recycled pid: {}", id);
            }
            return Some(id as u64);
        }
        if (p.next_fresh as u64) < CEIL {
            let id = p.next_fresh;
            p.next_fresh += 1;
            return Some(id as u64);
        }
        None
    }

    pub(super) fn release(id: u64) {
        debug_assert!((FLOOR..CEIL).contains(&id));
        static RELEASED: AtomicU64 = AtomicU64::new(0);
        let n = RELEASED.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            // One line per boot: the release side of the pool is alive too —
            // its absence under churn means task objects are being leaked
            // (an Arc holder outliving reap), which is a bug to chase, not a
            // pool problem.
            log::info!("[pid] first pid released: {}", id);
        }
        POOL.lock().free.push_back(id as u32);
    }
}

impl Drop for KObjectBase {
    fn drop(&mut self) {
        if self.pooled {
            pid_pool::release(self.id);
        }
    }
}

const MAX_SIGNAL_CALLBACKS: usize = 1024;

/// The mutable part of `KObjectBase`.
#[derive(Default)]
struct KObjectBaseInner {
    name: String,
    signal: Signal,
    signal_callbacks: Vec<SignalHandler>,
}

impl Default for KObjectBase {
    fn default() -> Self {
        KObjectBase {
            id: Self::new_koid(),
            pooled: false,
            inner: Default::default(),
        }
    }
}

impl KObjectBase {
    /// Create a new kernel object base.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a kernel object base with initial `signal`.
    pub fn with_signal(signal: Signal) -> Self {
        KObjectBase::with(Default::default(), signal)
    }

    /// Create a kernel object base with `name`.
    pub fn with_name(name: &str) -> Self {
        KObjectBase::with(name, Default::default())
    }

    /// Create a kernel object base with both signal and name
    pub fn with(name: &str, signal: Signal) -> Self {
        let base = KObjectBase {
            id: Self::new_koid(),
            pooled: false,
            inner: Mutex::new(KObjectBaseInner {
                name: String::from(name),
                signal,
                ..Default::default()
            }),
        };
        watch_name_if_requested(&base.inner.lock().name);
        base
    }

    /// Create a kernel object with a fixed KoID (e.g. Linux PID 1 for init).
    pub fn with_id(id: KoID, name: &str, signal: Signal) -> Self {
        KObjectBase {
            id,
            pooled: false,
            inner: Mutex::new(KObjectBaseInner {
                name: String::from(name),
                signal,
                ..Default::default()
            }),
        }
    }

    /// Create a kernel object base whose id is a RECYCLING Linux pid/tid.
    ///
    /// Only `Process` and `Thread` use this: their ids are the pids/tids
    /// userspace sees, so they must stay small (< `pid_max`) and be reused
    /// after death — Linux semantics. Every other object keeps a monotonic
    /// id ≥ 2^32 that can never collide with a pid.
    pub fn with_name_pooled(name: &str) -> Self {
        if let Some(id) = pid_pool::alloc() {
            let base = KObjectBase {
                id,
                pooled: true,
                inner: Mutex::new(KObjectBaseInner {
                    name: String::from(name),
                    ..Default::default()
                }),
            };
            watch_name_if_requested(&base.inner.lock().name);
            return base;
        }
        // Over 4M live processes/threads — practically unreachable, but a
        // process with a big (non-pid_t) id beats failing the spawn.
        log::error!("[pid] pool exhausted (>4M live tasks); falling back to a high monotonic id");
        Self::with_name(name)
    }

    /// Generate a new KoID.
    ///
    /// Starts at 2^32: ids of plain kernel objects (VMOs, handles, channels,
    /// timers — allocated by the hundreds per process spawn) live strictly
    /// above the pid space, so object churn can never push a value that
    /// userspace treats as a `pid_t` past any limit. At 2^32..2^64 there is
    /// headroom for millions of years of allocation.
    fn new_koid() -> KoID {
        static KOID: AtomicU64 = AtomicU64::new(1 << 32);
        KOID.fetch_add(1, Ordering::SeqCst)
    }

    /// Get object's name.
    ///
    /// Never blocks on a lock this CPU already holds. `signal_change` keeps
    /// `inner` locked while it invokes the callbacks, so a callback — or
    /// anything it reaches, such as file teardown on `PROCESS_TERMINATED` —
    /// that asks this same object for its name would spin forever with
    /// interrupts off. The detector named it with holder and waiter on the
    /// same line:
    ///
    /// ```text
    /// cpu=3 at zircon-object/src/object/mod.rs:387
    /// HOLDER cpu=3 at zircon-object/src/object/mod.rs:387
    /// ```
    ///
    /// [`Self::try_name`] exists for callers that can handle the absence, and
    /// the known call sites use it. This is the backstop for the ones that
    /// cannot: a placeholder in a diagnostic is a blemish, a wedged CPU is the
    /// machine.
    pub fn name(&self) -> String {
        if self.inner.held_by_current_cpu() {
            return String::from("<name: object lock held by this CPU>");
        }
        self.inner.lock().name.clone()
    }

    /// Get object's name without waiting for the object lock: `None` when
    /// it is held. For diagnostics that may run INSIDE a signal callback:
    /// `signal_change` keeps `inner` locked while it invokes the callbacks,
    /// so a callback (or anything it calls, such as file teardown on
    /// PROCESS_TERMINATED) that asks this same object for its `name()`
    /// spins on a lock its own CPU holds. Observed as the process-exit path
    /// deadlocking in the `[signal] SIGHUP` trace of a pty master dropped by
    /// the exiting process's own termination callback.
    pub fn try_name(&self) -> Option<String> {
        self.inner.try_lock().map(|inner| inner.name.clone())
    }

    /// Set object's name.
    pub fn set_name(&self, name: &str) {
        let mut inner = self.inner.lock();
        inner.name = String::from(name);
        watch_name_if_requested(&inner.name);
    }

    /// Get the signal status.
    pub fn signal(&self) -> Signal {
        self.inner.lock().signal
    }

    /// Change signal status: first `clear` then `set` indicated bits.
    ///
    /// All signal callbacks will be called.
    pub fn signal_change(&self, clear: Signal, set: Signal) {
        let mut inner = self.inner.lock();
        let old_signal = inner.signal;
        inner.signal.remove(clear);
        inner.signal.insert(set);
        let new_signal = inner.signal;
        if new_signal == old_signal {
            return;
        }
        // Zircon visits the most recently registered observers first.
        inner.signal_callbacks.reverse();
        inner.signal_callbacks.retain(|f| !f(new_signal));
        inner.signal_callbacks.reverse();
    }

    /// Assert `signal`.
    pub fn signal_set(&self, signal: Signal) {
        self.signal_change(Signal::empty(), signal);
    }

    /// Deassert `signal`.
    pub fn signal_clear(&self, signal: Signal) {
        self.signal_change(signal, Signal::empty());
    }

    /// Add `callback` for signal status changes.
    ///
    /// The `callback` is a function of `Fn(Signal) -> bool`.
    /// It returns a bool indicating whether the handle process is over.
    /// If true, the function will never be called again.
    pub fn add_signal_callback(&self, callback: SignalHandler) {
        let mut inner = self.inner.lock();
        // Check the callback immediately, in case that a signal arrives just before the call of
        // `add_signal_callback` (since lock is acquired inside it) and the callback is not triggered
        // in time.
        if !callback(inner.signal) {
            if inner.signal_callbacks.len() >= MAX_SIGNAL_CALLBACKS {
                // Full — but most of it is probably dead. A callback is only
                // ever retired when it is *called*, which happens on a signal
                // change, and an object can go its whole life without one
                // while waits come and go on it: `wait_signal_many` leaves a
                // callback behind on every target that did not fire, a
                // cancelled `zx_object_wait_async` leaves one behind, and a
                // dropped `wait_signal` leaves one behind. Silently refusing
                // the new one wedges whoever registered it — for a
                // `wait_signal` permanently, since its future has already
                // latched "registered" and will never ask again — so make
                // room first.
                //
                // Evaluating the list at the current signal is exactly what
                // the last `signal_change` did, and no callback in the tree
                // acts twice for the same signal, so a live one answers
                // `false` again and only the dead drop out: a dropped
                // waiter, a finished wait, a cancelled observer, a closed
                // handle, a dead port.
                let signal = inner.signal;
                inner.signal_callbacks.retain(|f| !f(signal));
                if inner.signal_callbacks.len() >= MAX_SIGNAL_CALLBACKS {
                    warn!(
                        "[kobject] \"{}\" holds {} live signal callbacks; refusing one more — \
                         whoever registered it will never be woken",
                        inner.name, MAX_SIGNAL_CALLBACKS
                    );
                    return;
                }
            }
            inner.signal_callbacks.push(callback);
        }
    }
}

impl dyn KernelObject {
    /// Asynchronous wait for one of `signal`.
    pub fn wait_signal(self: &Arc<Self>, signal: Signal) -> impl Future<Output = Signal> {
        #[must_use = "wait_signal does nothing unless polled/`await`-ed"]
        struct SignalFuture {
            object: Arc<dyn KernelObject>,
            signal: Signal,
            waiter: Arc<SignalWaiter>,
        }

        struct SignalWaiter {
            registered: AtomicBool,
            waker: Mutex<Option<core::task::Waker>>,
        }

        impl Future for SignalFuture {
            type Output = Signal;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let current_signal = self.object.signal();
                if !(current_signal & self.signal).is_empty() {
                    return Poll::Ready(current_signal);
                }
                *self.waiter.waker.lock() = Some(cx.waker().clone());
                if !self.waiter.registered.swap(true, Ordering::AcqRel) {
                    self.object.add_signal_callback(Box::new({
                        let signal = self.signal;
                        let waiter = Arc::downgrade(&self.waiter);
                        move |s| {
                            let Some(waiter) = waiter.upgrade() else {
                                return true;
                            };
                            if (s & signal).is_empty() {
                                return false;
                            }
                            // A competing waiter/cancel may consume the signal
                            // before the next poll. Allow that poll to rearm.
                            waiter.registered.store(false, Ordering::Release);
                            if let Some(waker) = waiter.waker.lock().take() {
                                waker.wake();
                            }
                            true
                        }
                    }));
                }
                Poll::Pending
            }
        }

        SignalFuture {
            object: self.clone(),
            signal,
            waiter: Arc::new(SignalWaiter {
                registered: AtomicBool::new(false),
                waker: Mutex::new(None),
            }),
        }
    }

    /// Once one of the `signal` asserted, push a packet with `key` into the `port`,
    ///
    /// It's used to implement `sys_object_wait_async`.
    #[allow(unsafe_code)]
    pub fn send_signal_to_port_async(self: &Arc<Self>, signal: Signal, port: &Arc<Port>, key: u64) {
        port.wait_async(self, (0, 0), key, signal, WaitAsyncOptions::empty(), None);
    }
}

/// Asynchronous wait signal for multiple objects.
pub fn wait_signal_many(
    targets: &[(Arc<dyn KernelObject>, Signal)],
) -> impl Future<Output = Vec<Signal>> {
    /// Shared between the future and the callbacks it leaves on its targets.
    ///
    /// Those callbacks outlive the poll that registered them and, on every
    /// target whose signal never arrives, the whole wait: a callback is only
    /// retired when it is called, and an object may have no further signal
    /// change. So neither the waker nor the answer to "am I still wanted"
    /// can live inside a callback. A waker cloned once is stale as soon as
    /// the executor hands the future a different one, and a finished wait
    /// whose callbacks keep waking a task that is gone goes on filling the
    /// object's callback list until it reaches `MAX_SIGNAL_CALLBACKS` and
    /// the object stops accepting waits from anyone.
    struct ManyWaiter {
        waker: Mutex<Option<core::task::Waker>>,
        /// Set when the wait is over but the future is still alive, so the
        /// callbacks left on the targets retire themselves. A wait that was
        /// simply dropped needs no flag: the `Weak` they hold stops
        /// upgrading the moment the future goes.
        done: AtomicBool,
    }

    #[must_use = "wait_signal_many does nothing unless polled/`await`-ed"]
    struct SignalManyFuture {
        targets: Vec<(Arc<dyn KernelObject>, Signal)>,
        waiter: Arc<ManyWaiter>,
        registered: bool,
    }

    impl SignalManyFuture {
        fn happened(&self, current_signals: &[Signal]) -> bool {
            self.targets
                .iter()
                .zip(current_signals)
                .any(|(&(_, desired), &current)| !(current & desired).is_empty())
        }
    }

    impl Future for SignalManyFuture {
        type Output = Vec<Signal>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let current_signals: Vec<_> =
                self.targets.iter().map(|(obj, _)| obj.signal()).collect();
            if self.happened(&current_signals) {
                self.waiter.done.store(true, Ordering::Release);
                return Poll::Ready(current_signals);
            }
            // On every poll, not only the first: a later poll may carry a
            // different waker (a `select!` arm re-armed, a task moved), and
            // the callbacks below are the only thing that will ever wake
            // this future again.
            *self.waiter.waker.lock() = Some(cx.waker().clone());
            if !self.registered {
                for (object, signal) in self.targets.iter() {
                    object.add_signal_callback(Box::new({
                        let signal = *signal;
                        let waiter = Arc::downgrade(&self.waiter);
                        move |s| {
                            let Some(waiter) = waiter.upgrade() else {
                                return true;
                            };
                            if waiter.done.load(Ordering::Acquire) {
                                return true;
                            }
                            if (s & signal).is_empty() {
                                return false;
                            }
                            if let Some(waker) = waiter.waker.lock().take() {
                                waker.wake();
                            }
                            // Stay registered: the signal may be consumed
                            // again before the poll this just asked for, and
                            // this is the only callback the wait has on this
                            // target — retiring it here would leave the
                            // target mute for the rest of the wait.
                            false
                        }
                    }));
                }
                self.registered = true;
            }
            Poll::Pending
        }
    }

    SignalManyFuture {
        targets: Vec::from(targets),
        waiter: Arc::new(ManyWaiter {
            waker: Mutex::new(None),
            done: AtomicBool::new(false),
        }),
        registered: false,
    }
}

/// Macro to auto implement `KernelObject` trait.
#[macro_export]
macro_rules! impl_kobject {
    ($class:ident $( $fn:tt )*) => {
        impl $crate::object::KernelObject for $class {
            fn id(&self) -> KoID {
                self.base.id
            }
            fn type_name(&self) -> &str {
                stringify!($class)
            }
            fn name(&self) -> alloc::string::String {
                self.base.name()
            }
            fn try_name(&self) -> Option<alloc::string::String> {
                self.base.try_name()
            }
            fn set_name(&self, name: &str){
                self.base.set_name(name)
            }
            fn signal(&self) -> Signal {
                self.base.signal()
            }
            fn signal_set(&self, signal: Signal) {
                self.base.signal_set(signal);
            }
            fn signal_clear(&self, signal: Signal) {
                self.base.signal_clear(signal);
            }
            fn signal_change(&self, clear: Signal, set: Signal) {
                self.base.signal_change(clear, set);
            }
            fn add_signal_callback(&self, callback: $crate::object::SignalHandler) {
                self.base.add_signal_callback(callback);
            }
            $( $fn )*
        }
        impl core::fmt::Debug for $class {
            fn fmt(
                &self,
                f: &mut core::fmt::Formatter<'_>,
            ) -> core::result::Result<(), core::fmt::Error> {
                use $crate::object::KernelObject;
                f.debug_tuple(&stringify!($class))
                    .field(&self.id())
                    .field(&self.name())
                    .finish()
            }
        }
    };
}

/// Define a pair of kcounter (create, destroy),
/// and a helper struct `CountHelper` which increases the counter on construction and drop.
#[macro_export]
macro_rules! define_count_helper {
    ($class:ident) => {
        struct CountHelper(());
        impl CountHelper {
            fn new() -> Self {
                $crate::kcounter!(CREATE_COUNT, concat!(stringify!($class), ".create"));
                CREATE_COUNT.add(1);
                CountHelper(())
            }
        }
        impl Drop for CountHelper {
            fn drop(&mut self) {
                $crate::kcounter!(DESTROY_COUNT, concat!(stringify!($class), ".destroy"));
                DESTROY_COUNT.add(1);
            }
        }
    };
}

/// The type of kernel object ID.
pub type KoID = u64;

/// The type of kernel object signal handler.
pub type SignalHandler = Box<dyn Fn(Signal) -> bool + Send>;

/// Empty kernel object. Just for test.
pub struct DummyObject {
    base: KObjectBase,
}

impl_kobject!(DummyObject);

impl DummyObject {
    /// Create a new `DummyObject`.
    pub fn new() -> Arc<Self> {
        Arc::new(DummyObject {
            base: KObjectBase::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::sync::Barrier;
    use std::time::Duration;

    /// A waker that only counts. The waits in this module are measured by
    /// hand rather than `await`-ed: a test that `await`s the wake it is
    /// checking for hangs when the wake goes missing, which is the one
    /// answer a test must never give.
    struct Counter(AtomicUsize);

    impl Counter {
        fn new() -> Arc<Self> {
            Arc::new(Counter(AtomicUsize::new(0)))
        }

        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl futures::task::ArcWake for Counter {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// How many signal callbacks the object is still holding.
    fn callbacks(object: &DummyObject) -> usize {
        object.base.inner.lock().signal_callbacks.len()
    }

    fn two_dummies() -> (Arc<DummyObject>, Arc<DummyObject>) {
        (DummyObject::new(), DummyObject::new())
    }

    fn targets(a: &Arc<DummyObject>, b: &Arc<DummyObject>) -> [(Arc<dyn KernelObject>, Signal); 2] {
        [
            (a.clone() as Arc<dyn KernelObject>, Signal::READABLE),
            (b.clone() as Arc<dyn KernelObject>, Signal::WRITABLE),
        ]
    }

    #[test]
    fn a_name_asked_for_while_the_object_lock_is_held_does_not_wait_for_it() {
        let base = KObjectBase::with_name("lockholder");
        let held = base.inner.lock();
        // `signal_change` keeps `inner` locked while it runs the callbacks,
        // so a callback that asks this object for its name arrives here with
        // the lock already held by its own CPU. `try_name` is the answer for
        // the callers that can do without one.
        assert_eq!(base.try_name(), None);
        drop(held);
        assert_eq!(base.try_name().as_deref(), Some("lockholder"));
        assert_eq!(base.name(), "lockholder");
        // `name()`'s own backstop — the `<name: ...>` placeholder for when
        // `held_by_current_cpu()` says this CPU is the holder — cannot be
        // reached from here: a hosted build has no "this CPU", so
        // `kernel_sync::HeldByCurrentCpu` is a constant `false` there and
        // asking for the name under the guard above spins forever instead
        // of answering. That half is covered on bare metal only.
    }

    #[test]
    fn the_default_try_name_gives_up_rather_than_reach_for_the_lock() {
        struct Handrolled(KObjectBase);

        impl core::fmt::Debug for Handrolled {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("Handrolled")
            }
        }

        impl KernelObject for Handrolled {
            fn id(&self) -> KoID {
                self.0.id
            }
            fn type_name(&self) -> &str {
                "Handrolled"
            }
            fn name(&self) -> String {
                self.0.name()
            }
            fn set_name(&self, name: &str) {
                self.0.set_name(name)
            }
            fn signal(&self) -> Signal {
                self.0.signal()
            }
            fn signal_set(&self, signal: Signal) {
                self.0.signal_set(signal)
            }
            fn signal_clear(&self, signal: Signal) {
                self.0.signal_clear(signal)
            }
            fn signal_change(&self, clear: Signal, set: Signal) {
                self.0.signal_change(clear, set)
            }
            fn add_signal_callback(&self, callback: SignalHandler) {
                self.0.add_signal_callback(callback)
            }
        }

        let object = Handrolled(KObjectBase::with_name("handrolled"));
        assert_eq!(object.name(), "handrolled");
        // Not the name: an object that did not go through `impl_kobject!`
        // has not said its `name()` is safe to call from inside a signal
        // callback, and falling back to it would put the self-deadlock
        // `try_name` exists to avoid right back on that path.
        assert_eq!(object.try_name(), None);
    }

    #[test]
    fn a_signal_change_that_nets_to_nothing_calls_nobody() {
        let object = DummyObject::new();
        let calls = Arc::new(AtomicUsize::new(0));
        object.signal_set(Signal::READABLE);
        object.add_signal_callback(Box::new({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                false
            }
        }));
        // Registering evaluates it once, against the signal as it stands.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        object.signal_set(Signal::READABLE);
        object.signal_clear(Signal::WRITABLE);
        // Cleared and set again in the one call: the bit never left, so
        // there is no edge to report.
        object.signal_change(Signal::READABLE, Signal::READABLE);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        object.signal_set(Signal::WRITABLE);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_signal_change_clears_before_it_sets() {
        let object = DummyObject::new();
        object.signal_set(Signal::READABLE | Signal::USER_SIGNAL_0);
        // WRITABLE is named on both sides of the same call: the set wins,
        // and a bit named on neither side is left alone.
        object.signal_change(Signal::READABLE | Signal::WRITABLE, Signal::WRITABLE);
        assert_eq!(object.signal(), Signal::WRITABLE | Signal::USER_SIGNAL_0);
    }

    #[test]
    fn callbacks_are_visited_newest_first_and_keep_their_order() {
        let object = DummyObject::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..3 {
            object.add_signal_callback(Box::new({
                let order = order.clone();
                move |_| {
                    order.lock().push(i);
                    false
                }
            }));
        }
        order.lock().clear();
        object.signal_set(Signal::READABLE);
        // Zircon visits the most recently registered observer first.
        assert_eq!(*order.lock(), [2, 1, 0]);
        order.lock().clear();
        // And the list is put back the way it was, so the next change is not
        // served in the opposite order.
        object.signal_set(Signal::WRITABLE);
        assert_eq!(*order.lock(), [2, 1, 0]);
    }

    #[test]
    fn a_callback_satisfied_at_once_is_never_stored() {
        let object = DummyObject::new();
        object.signal_set(Signal::READABLE);
        let calls = Arc::new(AtomicUsize::new(0));
        object.add_signal_callback(Box::new({
            let calls = calls.clone();
            move |s| {
                calls.fetch_add(1, Ordering::SeqCst);
                s.contains(Signal::READABLE)
            }
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(callbacks(&object), 0);
        object.signal_set(Signal::WRITABLE);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_wait_that_is_dropped_retires_its_callback() {
        let object = DummyObject::new();
        let waiter = object.clone() as Arc<dyn KernelObject>;
        let counter = Counter::new();
        let waker = futures::task::waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let future = waiter.wait_signal(Signal::READABLE);
            futures::pin_mut!(future);
            assert!(future.as_mut().poll(&mut cx).is_pending());
            assert_eq!(callbacks(&object), 1);
        }
        object.signal_set(Signal::READABLE);
        assert_eq!(counter.count(), 0);
        assert_eq!(callbacks(&object), 0);
    }

    /// A callback is retired only when it is *called*, and an object can go
    /// a long time without a signal change while waits come and go on it.
    /// Refusing a new one because the list is full of waits that are long
    /// over wedges whoever asked, and for a `wait_signal` it wedges them for
    /// good: the future has already latched "registered" and never asks
    /// again, even once the list drains.
    #[test]
    fn a_full_callback_list_makes_room_by_dropping_the_dead() {
        let object = DummyObject::new();
        let waiter = object.clone() as Arc<dyn KernelObject>;
        let counter = Counter::new();
        let waker = futures::task::waker(counter.clone());
        let mut cx = Context::from_waker(&waker);

        for _ in 0..MAX_SIGNAL_CALLBACKS {
            let future = waiter.wait_signal(Signal::READABLE);
            futures::pin_mut!(future);
            assert!(future.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(callbacks(&object), MAX_SIGNAL_CALLBACKS);
        assert_eq!(counter.count(), 0);

        let future = waiter.wait_signal(Signal::READABLE);
        futures::pin_mut!(future);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            callbacks(&object),
            1,
            "the thousand dead ones should have gone, and the new one stayed"
        );
        object.signal_set(Signal::READABLE);
        assert_eq!(
            counter.count(),
            1,
            "the wait past the cap was dropped on the floor"
        );
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Signal::READABLE));
    }

    #[test]
    fn a_wait_wakes_the_waker_of_its_latest_poll() {
        let object = DummyObject::new();
        let waiter = object.clone() as Arc<dyn KernelObject>;
        let stale = Counter::new();
        let stale_waker = futures::task::waker(stale.clone());
        let fresh = Counter::new();
        let fresh_waker = futures::task::waker(fresh.clone());

        let future = waiter.wait_signal(Signal::READABLE);
        futures::pin_mut!(future);
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&stale_waker))
            .is_pending());
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&fresh_waker))
            .is_pending());
        assert_eq!(callbacks(&object), 1);

        object.signal_set(Signal::READABLE);
        assert_eq!(stale.count(), 0, "woke the waker of an earlier poll");
        assert_eq!(fresh.count(), 1);
    }

    #[test]
    fn a_many_wait_wakes_the_waker_of_its_latest_poll() {
        let (a, b) = two_dummies();
        let targets = targets(&a, &b);
        let stale = Counter::new();
        let stale_waker = futures::task::waker(stale.clone());
        let fresh = Counter::new();
        let fresh_waker = futures::task::waker(fresh.clone());

        let future = wait_signal_many(&targets);
        futures::pin_mut!(future);
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&stale_waker))
            .is_pending());
        // A later poll carries the waker the executor holds now.
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&fresh_waker))
            .is_pending());

        assert_eq!(
            (callbacks(&a), callbacks(&b)),
            (1, 1),
            "the second poll registered a second callback"
        );

        b.signal_set(Signal::WRITABLE);
        assert_eq!(stale.count(), 0, "woke the waker of an earlier poll");
        assert_eq!(fresh.count(), 1);
        assert_eq!(
            future.as_mut().poll(&mut Context::from_waker(&fresh_waker)),
            Poll::Ready(vec![Signal::empty(), Signal::WRITABLE])
        );
    }

    #[test]
    fn a_many_wait_rearms_when_the_signal_is_consumed_before_the_poll() {
        let (a, b) = two_dummies();
        let targets = targets(&a, &b);
        let counter = Counter::new();
        let waker = futures::task::waker(counter.clone());
        let mut cx = Context::from_waker(&waker);

        let future = wait_signal_many(&targets);
        futures::pin_mut!(future);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        a.signal_set(Signal::READABLE);
        assert_eq!(counter.count(), 1);
        // Taken by a competing waiter before this one got to look.
        a.signal_clear(Signal::READABLE);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        a.signal_set(Signal::READABLE);
        assert_eq!(
            counter.count(),
            2,
            "the target went mute after waking the wait once"
        );
    }

    #[test]
    fn a_finished_many_wait_leaves_nothing_on_the_targets_that_did_not_fire() {
        let (a, b) = two_dummies();
        let targets = targets(&a, &b);
        let counter = Counter::new();
        let waker = futures::task::waker(counter.clone());
        let mut cx = Context::from_waker(&waker);

        let future = wait_signal_many(&targets);
        futures::pin_mut!(future);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        a.signal_set(Signal::READABLE);
        assert_eq!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(vec![Signal::READABLE, Signal::empty()])
        );
        assert_eq!(counter.count(), 1);

        // `b` never fired. Its callback must not outlive the wait, waking a
        // task that has moved on and taking a slot on `b` for good.
        b.signal_set(Signal::WRITABLE);
        assert_eq!(counter.count(), 1, "woke a wait that was already over");
        assert_eq!(callbacks(&b), 0);
    }

    #[test]
    fn a_many_wait_that_is_dropped_retires_its_callbacks() {
        let (a, b) = two_dummies();
        let targets = targets(&a, &b);
        let counter = Counter::new();
        let waker = futures::task::waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let future = wait_signal_many(&targets);
            futures::pin_mut!(future);
            assert!(future.as_mut().poll(&mut cx).is_pending());
            assert_eq!((callbacks(&a), callbacks(&b)), (1, 1));
        }
        a.signal_set(Signal::READABLE);
        b.signal_set(Signal::WRITABLE);
        assert_eq!(counter.count(), 0);
        assert_eq!((callbacks(&a), callbacks(&b)), (0, 0));
    }

    #[test]
    fn a_many_wait_already_satisfied_registers_nothing() {
        let (a, b) = two_dummies();
        let targets = targets(&a, &b);
        let counter = Counter::new();
        let waker = futures::task::waker(counter.clone());
        let mut cx = Context::from_waker(&waker);

        a.signal_set(Signal::READABLE);
        let future = wait_signal_many(&targets);
        futures::pin_mut!(future);
        assert_eq!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(vec![Signal::READABLE, Signal::empty()])
        );
        assert_eq!((callbacks(&a), callbacks(&b)), (0, 0));
    }

    #[test]
    fn a_pid_can_never_collide_with_a_plain_object_id() {
        let task = KObjectBase::with_name_pooled("task");
        assert!(
            (pid_pool::FLOOR..pid_pool::CEIL).contains(&task.id),
            "a pid must be a valid pid_t and stay under pid_max, got {}",
            task.id
        );
        for base in [
            KObjectBase::new(),
            KObjectBase::with_name("vmo"),
            KObjectBase::with_signal(Signal::WRITABLE),
            KObjectBase::with("channel", Signal::READABLE),
        ] {
            assert!(
                base.id >= 1 << 32,
                "object churn must live above the pid space, got {}",
                base.id
            );
        }
    }

    #[test]
    fn a_fixed_id_is_never_fed_back_into_the_pid_pool() {
        // init and the per-terminal shells get their ids by hand. Handing
        // one back to the pool would give PID 1 to an ordinary task later;
        // the pool's range assertion turns that into a panic right here.
        for (id, name) in [(1u64, "init"), (101, "shell")] {
            let base = KObjectBase::with_id(id, name, Signal::empty());
            assert_eq!(base.id, id);
            assert_eq!(base.name(), name);
            drop(base);
        }
    }

    #[test]
    fn a_name_watch_refuses_a_prefix_it_cannot_pack() {
        // The prefix is packed into 8 bytes and read back up to its first
        // NUL, so an empty one would match every object and an embedded NUL
        // would silently shorten the match to whatever came before it.
        assert!(!watch_process_name(""));
        assert!(!watch_process_name("ni\0ente"));
        assert!(watch_process_name("zzprobe"));
        unwatch_process_name();
    }

    #[test]
    fn wait_rearms_after_signal_is_consumed() {
        use futures::task::{waker, ArcWake};
        struct WakeCounter(AtomicUsize);
        impl ArcWake for WakeCounter {
            fn wake_by_ref(arc: &Arc<Self>) {
                arc.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = waker(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let object = DummyObject::new() as Arc<dyn KernelObject>;
        let future = object.wait_signal(Signal::READABLE);
        futures::pin_mut!(future);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        object.signal_set(Signal::READABLE);
        object.signal_clear(Signal::READABLE);
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        object.signal_set(Signal::READABLE);
        assert_eq!(counter.0.load(Ordering::Relaxed), 2);
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Signal::READABLE));
    }

    #[async_std::test]
    async fn wait() {
        let object = DummyObject::new();
        let barrier = Arc::new(Barrier::new(2));
        async_std::task::spawn({
            let object = object.clone();
            let barrier = barrier.clone();
            async move {
                async_std::task::sleep(Duration::from_millis(20)).await;

                // Assert an irrelevant signal to test the `false` branch of the callback for `READABLE`.
                object.signal_set(Signal::USER_SIGNAL_0);
                object.signal_clear(Signal::USER_SIGNAL_0);
                object.signal_set(Signal::READABLE);
                barrier.wait().await;

                object.signal_set(Signal::WRITABLE);
            }
        });
        let object: Arc<dyn KernelObject> = object;

        let signal = object.wait_signal(Signal::READABLE).await;
        assert_eq!(signal, Signal::READABLE);
        barrier.wait().await;

        let signal = object.wait_signal(Signal::WRITABLE).await;
        assert_eq!(signal, Signal::READABLE | Signal::WRITABLE);
    }

    #[async_std::test]
    async fn wait_many() {
        let objs = [DummyObject::new(), DummyObject::new()];
        let barrier = Arc::new(Barrier::new(2));
        async_std::task::spawn({
            let objs = objs.clone();
            let barrier = barrier.clone();
            async move {
                async_std::task::sleep(Duration::from_millis(20)).await;

                objs[0].signal_set(Signal::READABLE);
                barrier.wait().await;

                objs[1].signal_set(Signal::WRITABLE);
            }
        });
        let obj0: Arc<dyn KernelObject> = objs[0].clone();
        let obj1: Arc<dyn KernelObject> = objs[1].clone();

        let signals = wait_signal_many(&[
            (obj0.clone(), Signal::READABLE),
            (obj1.clone(), Signal::READABLE),
        ])
        .await;
        assert_eq!(signals, [Signal::READABLE, Signal::empty()]);
        barrier.wait().await;

        let signals = wait_signal_many(&[
            (obj0.clone(), Signal::WRITABLE),
            (obj1.clone(), Signal::WRITABLE),
        ])
        .await;
        assert_eq!(signals, [Signal::READABLE, Signal::WRITABLE]);
    }

    #[test]
    fn test_trait_with_dummy() {
        let dummy = DummyObject::new();
        assert_eq!(dummy.name(), String::from(""));
        dummy.set_name("test");
        assert_eq!(dummy.name(), String::from("test"));
        dummy.signal_set(Signal::WRITABLE);
        assert_eq!(dummy.signal(), Signal::WRITABLE);
        dummy.signal_change(Signal::WRITABLE, Signal::READABLE);
        assert_eq!(dummy.signal(), Signal::READABLE);

        assert_eq!(dummy.get_child(0).unwrap_err(), ZxError::WRONG_TYPE);
        assert_eq!(dummy.peer().unwrap_err(), ZxError::NOT_SUPPORTED);
        assert_eq!(dummy.related_koid(), 0);
        assert_eq!(dummy.allowed_signals(), Signal::USER_ALL);

        assert_eq!(
            format!("{:?}", dummy),
            format!("DummyObject({}, \"test\")", dummy.id())
        );
    }
}

// ── [diag] Wild-write hunt: watch a process name's buffer ────────────────────
//
// A page-fault report that names the running process as `"l\u{fffd}"` says the
// name `String`'s heap buffer was overwritten: names are UTF-8 by construction
// and are written once, then only read. That makes the buffer an ideal target
// for a hardware write-watch — silent under normal operation, so the first
// trap it takes belongs to the corruptor described in
// `docs/README-crash-repro.md`, with its RIP in the trap frame.
//
// Arming is opt-in and one-shot: call `watch_process_name("lunarbar")`, and
// the next kernel object whose name starts with that prefix gets DR0 pointed
// at its name buffer on every CPU.

/// Requested name prefix, packed little-endian into 8 bytes (0 = no request).
/// An atomic rather than a `Mutex<String>`: this is read on the object-naming
/// path, which already holds the object lock and must not allocate.
static WATCH_NAME: AtomicU64 = AtomicU64::new(0);

/// Ask for the next kernel object whose name starts with `prefix` to have its
/// name buffer covered by a hardware write watchpoint. The prefix is capped at
/// 8 bytes (enough for a comm-style name); returns false if it is empty or
/// contains a NUL, which would truncate the packing.
pub fn watch_process_name(prefix: &str) -> bool {
    let b = prefix.as_bytes();
    if b.is_empty() || b.contains(&0) {
        return false;
    }
    let mut packed = [0u8; 8];
    let n = b.len().min(8);
    packed[..n].copy_from_slice(&b[..n]);
    WATCH_NAME.store(u64::from_le_bytes(packed), Ordering::Relaxed);
    true
}

/// Stop looking for a name to watch, and disarm any watchpoint already set.
pub fn unwatch_process_name() {
    WATCH_NAME.store(0, Ordering::Relaxed);
    kernel_hal::watchpoint::clear_watch();
}

/// If `name` matches the requested prefix, point the watchpoint at its buffer
/// and consume the request (one-shot: re-arming on every later object would
/// keep moving the watch off the buffer we want observed).
fn watch_name_if_requested(name: &str) {
    let packed = WATCH_NAME.load(Ordering::Relaxed);
    if packed == 0 {
        return;
    }
    let bytes = packed.to_le_bytes();
    let len = bytes.iter().position(|&c| c == 0).unwrap_or(8);
    if name.len() < len || name.as_bytes()[..len] != bytes[..len] {
        return;
    }
    // Watch an 8-byte window, so a String shorter than that is skipped: its
    // window would overlap neighbouring allocations and make hits ambiguous.
    // x86 ignores a misaligned watchpoint rather than reporting an error, so
    // an unaligned buffer is skipped too — the next matching object is likely
    // to be better placed.
    let addr = name.as_ptr() as usize;
    if name.len() < 8 || !addr.is_multiple_of(8) {
        return;
    }
    if kernel_hal::watchpoint::watch_write(addr, 8) {
        WATCH_NAME.store(0, Ordering::Relaxed);
        warn!(
            "[watchpoint] armed on 8B at {:#x} — name buffer of \"{}\"",
            addr, name
        );
    }
}
