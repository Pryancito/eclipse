use crate::{object::*, task::Thread};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::*;
use core::task::{Context, Poll, Waker};
use kernel_hal::sync::Mutex;

/// A primitive for creating userspace synchronization tools.
///
/// ## SYNOPSIS
/// A **futex** is a Fast Userspace muTEX. It is a low level
/// synchronization primitive which is a building block for higher level
/// APIs such as `pthread_mutex_t` and `pthread_cond_t`.
/// Futexes are designed to not enter the kernel or allocate kernel
/// resources in the uncontested case.
pub struct Futex {
    base: KObjectBase,
    value: &'static AtomicI32,
    inner: Mutex<FutexInner>,
}

impl_kobject!(Futex);

#[derive(Default)]
struct FutexInner {
    waiter_queue: VecDeque<Arc<Waiter>>,
    /// NOTE: use `set_owner`
    owner: Option<Arc<Thread>>,
}

impl Futex {
    /// Create a new Futex.
    ///
    /// The parameter `value` is the reference to
    /// an userspace `AtomicI32`. This reference is the
    /// information used in kernel to track what futex given threads are
    /// waiting on. For the Zircon-style wait/wake/requeue API the kernel never
    /// modifies `*value`: it is up to userspace code to correctly atomically
    /// modify this value across threads in order to build mutexes and so on.
    /// The Linux PI-futex helpers ([`load`](Futex::load),
    /// [`compare_exchange`](Futex::compare_exchange),
    /// [`store_by_waiters`](Futex::store_by_waiters)) are the exception: there
    /// the kernel drives the lock word, as `FUTEX_LOCK_PI`/`UNLOCK_PI` require.
    pub fn new(value: &'static AtomicI32) -> Arc<Self> {
        Arc::new(Futex {
            base: KObjectBase::default(),
            value,
            inner: Mutex::new(FutexInner::default()),
        })
    }

    /// Wait on a futex.
    ///
    /// This atomically verifies that `value_ptr` still contains the value `current_value`
    /// and sleeps until the futex is made available by a call to [`wake`].
    ///
    /// See [`wait_with_owner`] for advanced usage and more details.
    ///
    /// [`wait_with_owner`]: Futex::wait_with_owner
    /// [`wake`]: Futex::wake
    pub fn wait(self: &Arc<Self>, current_value: i32) -> impl Future<Output = ZxResult> {
        self.wait_with_owner(current_value, None, None)
    }

    /// Wake some number of threads waiting on a futex.
    ///
    /// It wakes at most `wake_count` of the waiters that are waiting on this futex.
    /// Return the number of waiters that were woken up.
    ///
    /// # Ownership
    ///
    /// The owner of the futex is set to nothing, regardless of the wake count.
    pub fn wake(&self, wake_count: usize) -> usize {
        // Drain up to `wake_count` waiters in a single critical section,
        // clear the owner in the same lock, then deliver wakeups after the
        // lock is released. `Waiter::wake` takes the waiter lock, so holding
        // futex.inner -> waiter.inner here while poll/Drop take
        // waiter.inner -> futex.inner would be a lock-order inversion
        // (deadlock under SMP). This collapses N lock acquires (one per
        // waker) into one for the common case.
        if wake_count == 0 {
            self.inner.lock().set_owner(None);
            return 0;
        }
        // Wake-one fast path (the canonical pthread_mutex_unlock case): one
        // lock acquire, zero allocations.
        if wake_count == 1 {
            let waiter = {
                let mut inner = self.inner.lock();
                let w = inner.waiter_queue.pop_front();
                inner.set_owner(None);
                w
            };
            return match waiter {
                Some(w) if w.wake() => 1,
                Some(_) => {
                    // Tombstoned (cancelled / timed out); try the next live waiter.
                    loop {
                        let w = self.inner.lock().waiter_queue.pop_front();
                        match w {
                            Some(w) if w.wake() => break 1,
                            Some(_) => continue,
                            None => break 0,
                        }
                    }
                }
                None => 0,
            };
        }
        // Wake-many (broadcast / condvar): drain the batch under one lock,
        // wake outside it. Trades N lock acquires for one Vec allocation.
        let batch: alloc::vec::Vec<Arc<Waiter>> = {
            let mut inner = self.inner.lock();
            let take = wake_count.min(inner.waiter_queue.len());
            let mut v = alloc::vec::Vec::with_capacity(take);
            for _ in 0..take {
                if let Some(w) = inner.waiter_queue.pop_front() {
                    v.push(w);
                }
            }
            inner.set_owner(None);
            v
        };
        let mut woken = 0;
        for waiter in batch {
            // Tombstoned waiters (timed-out / cancelled) return false and
            // must not consume the wake count.
            if waiter.wake() {
                woken += 1;
            }
        }
        // Tombstone top-up: rare path where some popped waiters were already
        // cancelled. Keep semantics: we must wake exactly up to `wake_count`
        // live waiters when available.
        while woken < wake_count {
            let waiter = self.inner.lock().waiter_queue.pop_front();
            match waiter {
                Some(waiter) => {
                    if waiter.wake() {
                        woken += 1;
                    }
                }
                None => break,
            }
        }
        woken
    }

    /// Fast comparison against the futex's current value, without taking the
    /// queue lock or allocating a waiter.
    ///
    /// Used by `FUTEX_WAIT` to short-circuit `EAGAIN` when userspace already
    /// lost the cmpxchg race — the canonical sysbench / pthread mutex hot
    /// path. A `false` result is authoritative; a `true` result must still
    /// be re-checked under the queue lock by the slow path.
    pub fn value_eq(&self, expected: i32) -> bool {
        // `Acquire` suffices: the producer (FUTEX_WAKE side) Release-stores
        // the new value in userspace before issuing the wake; we only need
        // happens-before with that store, not full SeqCst.
        self.value.load(Ordering::Acquire) == expected
    }

    // ------ Kernel-side word updates (Linux PI futexes) ------
    //
    // Linux's priority-inheritance futex ops (FUTEX_LOCK_PI / UNLOCK_PI /
    // TRYLOCK_PI) differ from every other futex command in that the KERNEL
    // owns the lock word's transitions: it writes the owner TID into the
    // word on acquisition, sets FUTEX_WAITERS while a thread blocks, and
    // clears the word on release. The three helpers below are the only
    // places this object writes the user word; the plain wait/wake API above
    // never does.

    /// Load the current value of the futex word.
    pub fn load(&self) -> i32 {
        self.value.load(Ordering::SeqCst)
    }

    /// Compare-and-swap the futex word: `current -> new`. On failure the
    /// value actually found is returned in `Err`.
    pub fn compare_exchange(&self, current: i32, new: i32) -> Result<i32, i32> {
        self.value
            .compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
    }

    /// Store `with_waiters` into the futex word if at least one waiter is
    /// queued, `without_waiters` otherwise, and report which one happened.
    ///
    /// The queue check and the store happen under the queue lock, so they are
    /// atomic with respect to a waiter's check-and-enqueue in
    /// [`wait`](Futex::wait): either the waiter is already queued (and the
    /// caller wakes it), or its enqueue-time value check sees the stored
    /// value and fails with `BAD_STATE` (so it retries). Without this, an
    /// unlocker could observe an empty queue, a locker enqueue against the
    /// still-owned word, and the unlocker then clear the word — a lost
    /// wakeup. This is `FUTEX_UNLOCK_PI`'s release step.
    ///
    /// Queued waiters that were already cancelled (tombstoned) still count:
    /// the worst case is a stale `with_waiters` value that costs the next
    /// locker one extra kernel round trip, never a lost wakeup.
    pub fn store_by_waiters(&self, with_waiters: i32, without_waiters: i32) -> bool {
        let inner = self.inner.lock();
        let has_waiters = !inner.waiter_queue.is_empty();
        self.value.store(
            if has_waiters {
                with_waiters
            } else {
                without_waiters
            },
            Ordering::SeqCst,
        );
        has_waiters
    }

    // ------ Advanced APIs on Zircon ------

    /// Get the owner of the futex.
    pub fn owner(&self) -> Option<Arc<Thread>> {
        self.inner.lock().owner.clone()
    }

    /// Whether nobody waits on this futex and nobody owns it, so its
    /// process may forget it (see [`FutexTable`]).
    pub fn is_idle(&self) -> bool {
        let inner = self.inner.lock();
        inner.waiter_queue.is_empty() && inner.owner.is_none()
    }

    /// Wait on a futex.
    ///
    /// This atomically verifies that `value_ptr` still contains the value `current_value`
    /// and sleeps until the futex is made available by a call to [`wake`].
    ///
    /// # SPURIOUS WAKEUPS
    ///
    /// This implementation currently does not generate spurious wakeups.
    ///
    /// # Ownership
    ///
    /// A successful call results in the owner of the futex being set to the
    /// thread referenced by the `new_owner`, or to nothing if it is `None`.
    ///
    /// # Errors
    ///
    /// - `INVALID_ARGS`: One of the following is true
    ///   - `new_owner` is currently a member of the waiters for this.
    ///   - `new_owner` has not been started yet.
    /// - `BAD_STATE`: `current_value` does not match the value at `value_ptr`.
    /// - `TIMED_OUT`: The thread was not woken before deadline passed.
    ///
    /// [`wake`]: Futex::wake
    pub fn wait_with_owner(
        self: &Arc<Self>,
        current_value: i32,
        thread: Option<Arc<Thread>>,
        new_owner: Option<Arc<Thread>>,
    ) -> impl Future<Output = ZxResult> {
        #[must_use = "wait does nothing unless polled/`await`-ed"]
        struct FutexFuture {
            waiter: Arc<Waiter>,
            current_value: i32,
            new_owner: Option<Arc<Thread>>,
        }
        impl Future for FutexFuture {
            type Output = ZxResult;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut inner = self.waiter.inner.lock();
                // check wakeup
                if inner.woken {
                    // set new owner on success
                    inner.futex.inner.lock().set_owner(self.new_owner.clone());
                    return Poll::Ready(Ok(()));
                }
                // first time?
                if inner.waker.is_none() {
                    let futex = inner.futex.clone();
                    let mut futex_inner = futex.inner.lock();
                    // Check the value while holding the futex queue lock: a
                    // concurrent FUTEX_WAKE pops waiters under the same lock,
                    // so the check-and-enqueue is atomic with respect to it.
                    // Checking the value before taking the lock allowed a wake
                    // to slip in between the check and the enqueue, leaving
                    // the waiter asleep forever (lost wakeup), which hung
                    // pthread barriers/condvars (e.g. sysbench startup).
                    let value = futex.value.load(Ordering::SeqCst);
                    if value != self.current_value {
                        return Poll::Ready(Err(ZxError::BAD_STATE));
                    }
                    // check new owner
                    if !futex_inner.is_valid_new_owner(&self.new_owner) {
                        return Poll::Ready(Err(ZxError::INVALID_ARGS));
                    }
                    inner.waker.replace(cx.waker().clone());
                    futex_inner.waiter_queue.push_back(self.waiter.clone());
                } else if inner
                    .waker
                    .as_ref()
                    .is_some_and(|waker| !waker.will_wake(cx.waker()))
                {
                    // Already queued, and re-polled with a DIFFERENT waker.
                    // `Future::poll` only promises to wake the waker of the
                    // MOST RECENT poll, so keeping the first one is a lost
                    // wakeup: the waiter then sleeps to its deadline, or for
                    // good when it has none.
                    inner.waker = Some(cx.waker().clone());
                }
                Poll::Pending
            }
        }
        // The FutexFuture will be dropped when the thread is no longer waiting
        // if we wake without be woken, remove myself from the waiter_queue
        impl Drop for FutexFuture {
            fn drop(&mut self) {
                let mut inner = self.waiter.inner.lock();
                if !inner.woken {
                    // Tombstone the waiter: if a concurrent wake/requeue
                    // already popped it from the queue (so the search below
                    // misses), its wake() must report "already consumed".
                    inner.woken = true;
                    let futex = inner.futex.clone();
                    let queue = &mut futex.inner.lock().waiter_queue;
                    if let Some(pos) = queue.iter().position(|x| Arc::ptr_eq(x, &self.waiter)) {
                        // Nobody cares about the order of queue, so just remove faster
                        queue.swap_remove_back(pos);
                    }
                }
            }
        }
        FutexFuture {
            waiter: Arc::new(Waiter {
                thread,
                inner: Mutex::new(WaiterInner {
                    waker: None,
                    woken: false,
                    futex: self.clone(),
                }),
            }),
            current_value,
            new_owner,
        }
    }

    /// Wake exactly one thread from the futex wait queue.
    ///
    /// If there is at least one thread to wake, the owner of the futex will
    /// be set to the thread which was woken. Otherwise, the futex will have
    /// no owner.
    ///
    /// # Ownership
    ///
    /// If there is at least one thread to wake, the owner of the futex will be
    /// set to the thread which was woken. Otherwise, the futex will have no owner.
    pub fn wake_single_owner(&self) {
        // Pop the waiter and set the new owner under one lock acquire
        // (pre-compute the next owner from the popped waiter's thread).
        // The actual wake happens after the lock is released to preserve the
        // futex.inner -> waiter.inner ordering ban.
        //
        // A popped waiter may be a tombstone: cancelled or timed out, yet
        // still queued because the `FutexFuture` that owned it searched its
        // previous queue after a `requeue` had already moved it (see the
        // retarget comment there). A tombstone is not "a thread to wake", so
        // it may neither become the owner nor strand the live waiter behind
        // it -- keep going, exactly as `wake` does.
        loop {
            let waiter = {
                let mut inner = self.inner.lock();
                let w = inner.waiter_queue.pop_front();
                let new_owner = w.as_ref().and_then(|w| w.thread.clone());
                inner.set_owner(new_owner);
                w
            };
            match waiter {
                // Woken for real: the owner installed above is its thread.
                Some(waiter) if waiter.wake() => return,
                // Tombstoned: owns nothing and wakes nothing. Next.
                Some(_) => continue,
                // Queue drained: the lock above left the futex unowned.
                None => return,
            }
        }
    }

    /// Requeuing is a generalization of waking.
    ///
    /// First, verifies that the value in `current_value` matches the value of the futex,
    /// and if not reports `ZxError::BAD_STATE`. After waking `wake_count` threads,
    /// `requeue_count` threads are moved from the original futex's wait queue to the
    /// wait queue corresponding to another `requeue_futex`.
    ///
    /// This requeueing behavior may be used to avoid thundering herds on wake.
    ///
    /// Returns how many waiters were woken plus how many were moved, which is
    /// the number `FUTEX_REQUEUE`/`FUTEX_CMP_REQUEUE` answer with on Linux.
    ///
    /// # Ownership
    ///
    /// The owner of this futex is set to nothing, regardless of the wake count.
    /// The owner of the `requeue_futex` is set to the thread `new_requeue_owner`.
    ///
    /// # Errors
    ///
    /// - `BAD_STATE`: `check_value` was asked for and `current_value` does not
    ///   match the value of this futex (Linux answers `EAGAIN`).
    /// - `INVALID_ARGS`: `new_requeue_owner` is one of the waiters, so it would
    ///   come out owning a futex it is itself blocked on.
    pub fn requeue(
        &self,
        current_value: i32,
        wake_count: usize,
        requeue_count: usize,
        requeue_futex: &Arc<Futex>,
        new_requeue_owner: Option<Arc<Thread>>,
        check_value: bool,
    ) -> ZxResult<usize> {
        // Locks are taken in address order below so that two concurrent
        // requeues with swapped futexes cannot deadlock (ABBA) -- and first
        // of all the degenerate case where both are the SAME futex, which no
        // ordering can save.
        let this = self as *const Futex;
        let that = Arc::as_ptr(requeue_futex);
        if this == that {
            // A futex requeued onto itself. Zircon rejects it one layer up
            // (`zx_futex_requeue` answers INVALID_ARGS when `value_ptr` and
            // `requeue_ptr` are the same futex), so this is the Linux side:
            // there it is legal, and since moving waiters from a queue to
            // itself changes nothing, all that is left is the wake half.
            //
            // What Linux does NOT skip is the comparison. `futex_requeue`
            // (kernel/futex/requeue.c) reads the word under the bucket lock
            // before touching a single waiter and answers EAGAIN when it no
            // longer matches, whether or not the two addresses are equal --
            // `double_lock_hb` exists precisely so the same-bucket case goes
            // through the identical path. Returning `Ok` and waking anyway,
            // which is what this branch used to do, tells a condvar
            // broadcast it won a race it had lost and wakes the sleepers it
            // was supposed to leave alone.
            {
                // Under the queue lock, for the same reason the two-futex
                // path below checks there: it must not race a waiter's
                // check-and-enqueue. `wake` takes the lock again afterwards;
                // a waiter that slips in between is merely woken too, which
                // the API allows, whereas one skipped would be lost.
                let _queue = self.inner.lock();
                if check_value && self.value.load(Ordering::SeqCst) != current_value {
                    return Err(ZxError::BAD_STATE);
                }
            }
            return Ok(self.wake(wake_count));
        }
        let mut to_wake = alloc::vec::Vec::new();
        let mut to_requeue = alloc::vec::Vec::new();
        {
            // Hold BOTH queue locks while moving waiters: if a waiter is
            // popped from this queue but not yet visible on the target one,
            // a concurrent FUTEX_WAKE on the target (e.g. a mutex unlock
            // racing musl's condvar unlock_requeue) finds an empty queue and
            // the wakeup is lost — threads then stall until a timeout.
            let (mut inner, mut new_inner);
            if (this as usize) <= (that as usize) {
                inner = self.inner.lock();
                new_inner = requeue_futex.inner.lock();
            } else {
                new_inner = requeue_futex.inner.lock();
                inner = self.inner.lock();
            }
            if check_value {
                // check value (under the queue lock, like FUTEX_WAIT does)
                if self.value.load(Ordering::SeqCst) != current_value {
                    return Err(ZxError::BAD_STATE);
                }
            }
            // A thread may not own a futex it is itself blocked on: that is
            // what `wait_with_owner` already refuses through the very same
            // helper, and `zx_futex_requeue` documents the rule for
            // `new_requeue_owner` too. Asked of BOTH queues -- a waiter of
            // this futex is one requeue away from waiting on the other one --
            // and before anything moves, so a rejected call leaves every
            // waiter exactly where it was.
            if !inner.is_valid_new_owner(&new_requeue_owner)
                || !new_inner.is_valid_new_owner(&new_requeue_owner)
            {
                return Err(ZxError::INVALID_ARGS);
            }
            for _ in 0..wake_count {
                if let Some(waiter) = inner.waiter_queue.pop_front() {
                    to_wake.push(waiter);
                } else {
                    break;
                }
            }
            let requeue_count = requeue_count.min(inner.waiter_queue.len());
            for waiter in inner.waiter_queue.drain(..requeue_count) {
                new_inner.waiter_queue.push_back(waiter.clone());
                to_requeue.push(waiter);
            }
            inner.set_owner(None);
            new_inner.set_owner(new_requeue_owner);
        }
        // Retarget waiters after releasing the queue locks (`reset_futex`
        // takes the waiter lock; taking it under futex.inner would invert
        // the poll/Drop lock order). A waiter cancelled in this window
        // searches its old queue, misses, and stays tombstoned on the new
        // queue, where wake() skips it without consuming a count.
        let requeued = to_requeue.len();
        for waiter in to_requeue {
            waiter.reset_futex(requeue_futex.clone());
        }
        // Deliver wakeups last, with no futex lock held. A tombstone among
        // them wakes nobody and so counts for nobody.
        let mut woken = 0;
        for waiter in to_wake {
            if waiter.wake() {
                woken += 1;
            }
        }
        Ok(woken + requeued)
    }
}

impl FutexInner {
    fn is_valid_new_owner(&self, new_owner: &Option<Arc<Thread>>) -> bool {
        // TODO: check whether the thread has been started yet
        if let Some(new_owner) = &new_owner {
            if self
                .waiter_queue
                .iter()
                .filter_map(|waiter| waiter.thread.as_ref())
                .any(|thread| Arc::ptr_eq(thread, new_owner))
            {
                return false;
            }
        }
        true
    }

    fn set_owner(&mut self, owner: Option<Arc<Thread>>) {
        // TODO: change the priority of owner thread
        self.owner = owner;
    }
}

/// The futexes of one process, keyed by the address of their word.
///
/// A futex is only worth remembering while someone waits on it, owns it, or
/// holds it: userspace names a futex by any aligned word it likes (a mutex
/// on the stack, one in a freed block, or every word of the address space in
/// a loop), and a table that kept every one for the life of the process was
/// kernel heap that only exit gave back. Inserting past the threshold sweeps
/// the entries nobody references and nobody waits on; the threshold then
/// doubles from what survived, so a sweep costs amortized constant time.
#[derive(Default)]
pub struct FutexTable {
    map: hashbrown::HashMap<usize, Arc<Futex>>,
    sweep_at: usize,
}

/// The table sweeps no earlier than this many entries.
const FUTEX_TABLE_MIN_SWEEP: usize = 64;

impl FutexTable {
    /// The futex of the word at `addr`, made with `create` if the table has
    /// none.
    pub fn get_or_create(
        &mut self,
        addr: usize,
        create: impl FnOnce() -> Arc<Futex>,
    ) -> Arc<Futex> {
        if let Some(futex) = self.map.get(&addr) {
            return futex.clone();
        }
        if self.map.len() >= self.sweep_at.max(FUTEX_TABLE_MIN_SWEEP) {
            self.map
                .retain(|_, futex| Arc::strong_count(futex) > 1 || !futex.is_idle());
            self.sweep_at = (self.map.len() * 2).max(FUTEX_TABLE_MIN_SWEEP);
        }
        let futex = create();
        self.map.insert(addr, futex.clone());
        futex
    }

    /// Forget every futex.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// How many futexes the table remembers right now.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the table remembers no futex.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

struct Waiter {
    /// The thread waiting on the futex.
    thread: Option<Arc<Thread>>,
    inner: Mutex<WaiterInner>,
}

struct WaiterInner {
    /// The waker of waiting future. `None` indicates first poll.
    waker: Option<Waker>,
    woken: bool,
    futex: Arc<Futex>,
}

impl Waiter {
    /// Wake up the waiting thread.
    ///
    /// Returns `false` if the waiter was already woken or cancelled (its
    /// future dropped, e.g. on timeout), in which case it must not consume
    /// a wake count. The waker is invoked after releasing the waiter lock.
    fn wake(&self) -> bool {
        let waker = {
            let mut inner = self.inner.lock();
            if inner.woken {
                return false;
            }
            inner.woken = true;
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
            true
        } else {
            false
        }
    }

    /// Reset futex on requeue.
    fn reset_futex(&self, futex: Arc<Futex>) {
        self.inner.lock().futex = futex;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Job, Process};
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::time::Duration;
    use futures::task::{waker, ArcWake};

    /// A futex over a freshly leaked word, so every test owns its own value
    /// instead of sharing one `static` with the next one.
    fn futex_with(value: i32) -> Arc<Futex> {
        Futex::new(Box::leak(Box::new(AtomicI32::new(value))))
    }

    /// A waker that only counts, so a test can ask "was THIS waiter woken?"
    /// rather than "did somebody wake".
    struct Counter(AtomicUsize);

    impl Counter {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl ArcWake for Counter {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A waiter blocked on `futex`, queued by polling its future once by hand.
    ///
    /// Driving the future instead of spawning a task is what makes these
    /// tests exact: the queue is in a known state at every line, there is no
    /// sleep to tune, and dropping the returned future is a cancellation with
    /// no race in it. Hold on to the future -- dropping it cancels the wait.
    #[allow(clippy::type_complexity)]
    fn queue_waiter(
        futex: &Arc<Futex>,
        value: i32,
        thread: Option<Arc<Thread>>,
    ) -> (Pin<Box<dyn Future<Output = ZxResult>>>, Arc<Counter>) {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let mut future: Pin<Box<dyn Future<Output = ZxResult>>> =
            Box::pin(futex.wait_with_owner(value, thread, None));
        let waker = waker(counter.clone());
        assert_eq!(
            future.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending,
            "the waiter should be on the queue now"
        );
        (future, counter)
    }

    /// A cancelled waiter still sitting on a queue.
    ///
    /// This state is reachable for real: `requeue` retargets waiters AFTER
    /// releasing the queue locks, so one cancelled inside that window looks
    /// for itself on the queue it has just left, misses, and stays behind on
    /// the new one -- which is exactly what the comment there says. Building
    /// it by hand is the same state without the race.
    fn enqueue_tombstone(futex: &Arc<Futex>, thread: Option<Arc<Thread>>) {
        let waiter = Arc::new(Waiter {
            thread,
            inner: Mutex::new(WaiterInner {
                waker: None,
                woken: true,
                futex: futex.clone(),
            }),
        });
        futex.inner.lock().waiter_queue.push_back(waiter);
    }

    /// Two threads of one throwaway process, to hang futex ownership on.
    fn two_threads() -> (Arc<Thread>, Arc<Thread>) {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        (
            Thread::create(&proc, "one").expect("failed to create thread"),
            Thread::create(&proc, "two").expect("failed to create thread"),
        )
    }

    #[async_std::test]
    async fn wait() {
        static VALUE: AtomicI32 = AtomicI32::new(1);
        let futex = Futex::new(&VALUE);

        let count = futex.wake(1);
        assert_eq!(count, 0);

        // inconsistent value should fail.
        assert_eq!(futex.wait(0).await, Err(ZxError::BAD_STATE));

        // spawn a new task to wake me up.
        {
            let futex = futex.clone();
            async_std::task::spawn(async move {
                async_std::task::sleep(Duration::from_millis(10)).await;
                VALUE.store(2, Ordering::SeqCst);
                let count = futex.wake(1);
                assert_eq!(count, 1);
            });
        }
        // wait for wake.
        futex.wait(1).await.unwrap();
        assert_eq!(VALUE.load(Ordering::SeqCst), 2);
        assert_eq!(futex.wake(1), 0);
    }

    #[async_std::test]
    async fn requeue() {
        static VALUE: AtomicI32 = AtomicI32::new(1);
        let futex = Futex::new(&VALUE);
        static REQUEUE_VALUE: AtomicI32 = AtomicI32::new(100);
        let requeue_futex = Futex::new(&REQUEUE_VALUE);

        let count = futex.wake(1);
        assert_eq!(count, 0);

        // inconsistent value should fail.
        assert_eq!(futex.wait(0).await, Err(ZxError::BAD_STATE));

        // spawn a new task to wait
        {
            let futex = futex.clone();
            async_std::task::spawn(async move {
                futex.wait(1).await.unwrap();
            });
        }
        // spawn a new task to requeue.
        {
            let futex = futex.clone();
            async_std::task::spawn(async move {
                async_std::task::sleep(Duration::from_millis(10)).await;
                VALUE.store(2, Ordering::SeqCst);

                let waiters = futex.inner.lock().waiter_queue.clone();
                assert_eq!(waiters.len(), 2);

                // inconsistent value should fail.
                assert_eq!(
                    futex.requeue(1, 1, 1, &requeue_futex, None, true),
                    Err(ZxError::BAD_STATE)
                );
                assert!(futex.requeue(2, 1, 1, &requeue_futex, None, true).is_ok());
                // 1 waiter waken, 1 waiter moved into `requeue_futex`.
                assert_eq!(futex.inner.lock().waiter_queue.len(), 0);
                assert_eq!(requeue_futex.inner.lock().waiter_queue.len(), 1);
                assert!(Arc::ptr_eq(
                    &requeue_futex.inner.lock().waiter_queue[0],
                    &waiters[1]
                ));
                // wake the requeued waiter.
                assert_eq!(requeue_futex.wake(1), 1);
            });
        }
        // wait for wake.
        futex.wait(1).await.unwrap();
        assert_eq!(VALUE.load(Ordering::SeqCst), 2);
    }

    #[async_std::test]
    async fn owner() {
        let root_job = Job::root();
        let proc = Process::create(&root_job, "proc").expect("failed to create process");
        let thread = Thread::create(&proc, "thread").expect("failed to create thread");

        static VALUE: AtomicI32 = AtomicI32::new(1);
        let futex = proc.get_futex(&VALUE);
        assert!(futex.owner().is_none());
        futex.inner.lock().set_owner(Some(thread.clone()));

        {
            let futex = futex.clone();
            let thread = thread.clone();
            async_std::task::spawn(async move {
                futex
                    .wait_with_owner(1, Some(thread.clone()), Some(thread))
                    .await
                    .unwrap();
            });
        }
        async_std::task::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            futex
                .wait_with_owner(1, Some(thread.clone()), Some(thread.clone()))
                .await
                .unwrap_err(),
            ZxError::INVALID_ARGS
        );

        futex.inner.lock().set_owner(None);
        futex.wake_single_owner();
        assert!(Arc::ptr_eq(&futex.owner().unwrap(), &thread));
        assert_eq!(futex.wake(1), 0);
    }

    /// The Linux PI-futex word helpers: CAS acquisition, and the unlock-side
    /// store that must agree with the wait queue under the queue lock.
    #[async_std::test]
    async fn pi_word_helpers() {
        const WAITERS: i32 = 0x8000_0000_u32 as i32;
        static VALUE: AtomicI32 = AtomicI32::new(0);
        let futex = Futex::new(&VALUE);

        // Uncontended acquire: 0 -> tid, exactly what musl's
        // pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT) probe needs.
        assert_eq!(futex.load(), 0);
        assert_eq!(futex.compare_exchange(0, 7), Ok(0));
        assert_eq!(futex.load(), 7);
        // A stale expectation fails and reports the real value.
        assert_eq!(futex.compare_exchange(0, 9), Err(7));

        // Unlock with nobody queued clears the word entirely.
        assert!(!futex.store_by_waiters(WAITERS, 0));
        assert_eq!(futex.load(), 0);

        // A blocked locker: owner 7 again, waiter publishes FUTEX_WAITERS and
        // sleeps on the contended value.
        assert_eq!(futex.compare_exchange(0, 7), Ok(0));
        assert_eq!(futex.compare_exchange(7, 7 | WAITERS), Ok(7));
        let waiter = {
            let futex = futex.clone();
            async_std::task::spawn(async move { futex.wait(7 | WAITERS).await })
        };
        async_std::task::sleep(Duration::from_millis(10)).await;
        // Unlock with a waiter queued: the word keeps FUTEX_WAITERS with owner
        // 0 (so the woken locker's CAS takes it and later unlocks still enter
        // the kernel), and the caller is told to wake somebody.
        assert!(futex.store_by_waiters(WAITERS, 0));
        assert_eq!(futex.load(), WAITERS);
        assert_eq!(futex.wake(1), 1);
        waiter.await.unwrap();
        // The woken locker takes the free word, preserving the flag bit.
        assert_eq!(futex.compare_exchange(WAITERS, 8 | WAITERS), Ok(WAITERS));
        assert_eq!(futex.load() & 0x3fff_ffff, 8);
    }
    /// `zx_futex_wake` clears the owner "regardless of the wake count", zero
    /// included -- and a zero wake must not disturb the queue.
    #[test]
    fn a_wake_of_zero_clears_the_owner_and_touches_nobody() {
        let (thread, _) = two_threads();
        let futex = futex_with(1);
        let (_waiter, counter) = queue_waiter(&futex, 1, None);
        futex.inner.lock().set_owner(Some(thread));

        assert_eq!(futex.wake(0), 0);
        assert!(futex.owner().is_none());
        assert_eq!(counter.count(), 0);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 1);
    }

    /// A wake takes exactly the batch it was asked for and leaves the rest.
    #[test]
    fn a_wake_takes_exactly_the_batch_it_was_asked_for() {
        let futex = futex_with(1);
        let waiters: Vec<_> = (0..4).map(|_| queue_waiter(&futex, 1, None)).collect();
        let (owner, _) = two_threads();
        futex.inner.lock().set_owner(Some(owner));

        assert_eq!(futex.wake(2), 2);
        // "The owner of the futex is set to nothing, regardless of the wake
        // count" -- on this path as much as on the other two.
        assert!(futex.owner().is_none());
        assert_eq!(waiters[0].1.count(), 1);
        assert_eq!(waiters[1].1.count(), 1);
        assert_eq!(waiters[2].1.count(), 0);
        assert_eq!(waiters[3].1.count(), 0);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 2);
    }

    /// The condvar broadcast: `INT_MAX` waiters asked for, however many are
    /// there woken, once each, and the queue left empty.
    #[test]
    fn waking_more_than_are_queued_drains_the_queue_once() {
        let futex = futex_with(1);
        let waiters: Vec<_> = (0..3).map(|_| queue_waiter(&futex, 1, None)).collect();

        assert_eq!(futex.wake(i32::MAX as usize), 3);
        for (_, counter) in &waiters {
            assert_eq!(counter.count(), 1);
        }
        assert_eq!(futex.wake(i32::MAX as usize), 0);
        for (_, counter) in &waiters {
            assert_eq!(counter.count(), 1);
        }
    }

    /// Cancelling a wait (the timed-out `FUTEX_WAIT`) takes the waiter off
    /// the queue, so it neither gets woken nor eats somebody else's wake.
    #[test]
    fn a_cancelled_wait_leaves_the_queue() {
        let futex = futex_with(1);
        let (waiter, counter) = queue_waiter(&futex, 1, None);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 1);

        drop(waiter);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 0);
        assert_eq!(futex.wake(1), 0);
        assert_eq!(counter.count(), 0);
    }

    /// Wake-one over a tombstone: it must not report "one woken" for a waiter
    /// that is not there any more, nor leave the live one asleep behind it.
    #[test]
    fn a_tombstone_does_not_consume_a_wake_of_one() {
        let futex = futex_with(1);
        enqueue_tombstone(&futex, None);
        let (_waiter, counter) = queue_waiter(&futex, 1, None);

        assert_eq!(futex.wake(1), 1);
        assert_eq!(counter.count(), 1);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 0);

        // And a queue of nothing but tombstones is an empty queue: it wakes
        // nobody and says so, rather than reporting the one it discarded.
        enqueue_tombstone(&futex, None);
        enqueue_tombstone(&futex, None);
        assert_eq!(futex.wake(1), 0);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 0);
    }

    /// The same for the batch path, where the top-up loop has to go back to
    /// the queue for as many live waiters as the tombstones displaced.
    #[test]
    fn a_tombstone_does_not_consume_a_wake_in_a_batch() {
        let futex = futex_with(1);
        enqueue_tombstone(&futex, None);
        let (_first, first) = queue_waiter(&futex, 1, None);
        enqueue_tombstone(&futex, None);
        let (_second, second) = queue_waiter(&futex, 1, None);

        assert_eq!(futex.wake(2), 2);
        assert_eq!(first.count(), 1);
        assert_eq!(second.count(), 1);
    }

    /// `zx_futex_wake_single_owner` says the owner becomes "the thread which
    /// was woken". A tombstone at the head of the queue is nobody's thread:
    /// handing it the futex left the live waiter asleep AND named a cancelled
    /// thread as owner of a lock it was no longer waiting for.
    #[test]
    fn wake_single_owner_skips_a_tombstone_and_owns_the_thread_it_woke() {
        let (gone, live) = two_threads();
        let futex = futex_with(1);
        enqueue_tombstone(&futex, Some(gone.clone()));
        let (_waiter, counter) = queue_waiter(&futex, 1, Some(live.clone()));

        futex.wake_single_owner();
        assert_eq!(counter.count(), 1, "the live waiter is the one to wake");
        let owner = futex.owner().expect("the woken thread owns the futex");
        assert!(Arc::ptr_eq(&owner, &live));
        assert!(!Arc::ptr_eq(&owner, &gone));
    }

    /// "Otherwise, the futex will have no owner": a queue of nothing but
    /// tombstones is an empty queue.
    #[test]
    fn wake_single_owner_over_an_empty_queue_leaves_no_owner() {
        let (gone, previous) = two_threads();
        let futex = futex_with(1);
        futex.inner.lock().set_owner(Some(previous));
        enqueue_tombstone(&futex, Some(gone));

        futex.wake_single_owner();
        assert!(futex.owner().is_none());

        futex.wake_single_owner();
        assert!(futex.owner().is_none());
    }

    /// A futex requeued onto ITSELF. Zircon rejects it one layer up, so this
    /// is the Linux path, where it is legal -- `FUTEX_CMP_REQUEUE` with
    /// `uaddr1 == uaddr2`. Linux still reads the word first and answers
    /// EAGAIN when it moved; this branch used to return success and wake the
    /// sleepers anyway, telling a broadcast it had won a race it lost.
    #[test]
    fn requeueing_a_futex_onto_itself_still_checks_the_value() {
        let futex = futex_with(1);
        let (_waiter, counter) = queue_waiter(&futex, 1, None);

        assert_eq!(
            futex.requeue(99, 1, 1, &futex, None, true),
            Err(ZxError::BAD_STATE)
        );
        assert_eq!(counter.count(), 0, "nobody may be woken on a mismatch");
        assert_eq!(futex.inner.lock().waiter_queue.len(), 1);

        // With the value it was told, the same call is an ordinary wake.
        assert_eq!(futex.requeue(1, 1, 1, &futex, None, true), Ok(1));
        assert_eq!(counter.count(), 1);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 0);
    }

    /// `FUTEX_REQUEUE` (no `CMP_`) passes no value to compare, and then the
    /// self-requeue is a plain wake whatever the word says.
    #[test]
    fn requeueing_a_futex_onto_itself_unchecked_is_a_plain_wake() {
        let futex = futex_with(1);
        let waiters: Vec<_> = (0..3).map(|_| queue_waiter(&futex, 1, None)).collect();

        assert_eq!(futex.requeue(99, 2, 1, &futex, None, false), Ok(2));
        assert_eq!(waiters[0].1.count(), 1);
        assert_eq!(waiters[1].1.count(), 1);
        assert_eq!(waiters[2].1.count(), 0);
        assert_eq!(futex.inner.lock().waiter_queue.len(), 1);
    }

    /// Linux answers a requeue with how many it woke plus how many it moved.
    /// Answering zero told every caller that the broadcast reached nobody.
    #[test]
    fn requeue_answers_how_many_it_woke_and_how_many_it_moved() {
        let source = futex_with(1);
        let target = futex_with(2);
        let waiters: Vec<_> = (0..4).map(|_| queue_waiter(&source, 1, None)).collect();
        let (owner, next_owner) = two_threads();
        source.inner.lock().set_owner(Some(owner));

        assert_eq!(
            source.requeue(1, 1, 2, &target, Some(next_owner.clone()), true),
            Ok(3)
        );
        assert_eq!(waiters[0].1.count(), 1, "the first is woken");
        assert_eq!(source.inner.lock().waiter_queue.len(), 1);
        assert_eq!(target.inner.lock().waiter_queue.len(), 2);
        // "The owner of this futex is set to nothing, regardless of the wake
        // count. The owner of the `requeue_futex` is set to the thread
        // `new_requeue_owner`."
        assert!(source.owner().is_none());
        assert!(Arc::ptr_eq(&target.owner().unwrap(), &next_owner));

        // The two that moved are woken by the target now, not by the source.
        assert_eq!(source.wake(2), 1);
        assert_eq!(waiters[1].1.count(), 0);
        assert_eq!(target.wake(2), 2);
        assert_eq!(waiters[1].1.count(), 1);
        assert_eq!(waiters[2].1.count(), 1);
    }

    /// A waiter that was moved belongs to the queue it was moved TO, so
    /// cancelling it has to take it off that one. Leaving it on the old
    /// futex's queue would strand a tombstone on the new one for good.
    #[test]
    fn a_cancelled_wait_leaves_the_queue_it_was_moved_to() {
        let source = futex_with(1);
        let target = futex_with(2);
        let (waiter, counter) = queue_waiter(&source, 1, None);

        assert_eq!(source.requeue(1, 0, 1, &target, None, true), Ok(1));
        assert_eq!(target.inner.lock().waiter_queue.len(), 1);

        drop(waiter);
        assert_eq!(target.inner.lock().waiter_queue.len(), 0);
        assert_eq!(target.wake(1), 0);
        assert_eq!(counter.count(), 0);
    }

    /// Asking to move more than are queued moves what there is.
    #[test]
    fn requeue_moves_no_more_than_are_queued() {
        let source = futex_with(1);
        let target = futex_with(2);
        let _waiters: Vec<_> = (0..2).map(|_| queue_waiter(&source, 1, None)).collect();

        assert_eq!(
            source.requeue(1, 0, i32::MAX as usize, &target, None, true),
            Ok(2)
        );
        assert_eq!(source.inner.lock().waiter_queue.len(), 0);
        assert_eq!(target.inner.lock().waiter_queue.len(), 2);
    }

    /// A thread may not come out owning a futex it is itself blocked on.
    /// `wait_with_owner` has always refused it; the requeue side set the new
    /// owner without ever asking.
    #[test]
    fn a_waiter_cannot_be_made_the_owner_of_the_futex_it_waits_on() {
        let (blocked, _) = two_threads();
        let source = futex_with(1);
        let target = futex_with(2);
        let (_on_target, on_target) = queue_waiter(&target, 2, Some(blocked.clone()));
        let (_moving, moving) = queue_waiter(&source, 1, None);

        assert_eq!(
            source.requeue(1, 0, 1, &target, Some(blocked.clone()), true),
            Err(ZxError::INVALID_ARGS)
        );
        // And a rejected call leaves every waiter where it was.
        assert_eq!(source.inner.lock().waiter_queue.len(), 1);
        assert_eq!(target.inner.lock().waiter_queue.len(), 1);
        assert!(target.owner().is_none());
        assert_eq!(on_target.count(), 0);
        assert_eq!(moving.count(), 0);

        // Waiting on the SOURCE and on nothing else is the same answer: it is
        // one requeue away from waiting on the target.
        let (waiting, elsewhere) = two_threads();
        let other_source = futex_with(1);
        let other_target = futex_with(2);
        let (_on_source, _) = queue_waiter(&other_source, 1, Some(waiting.clone()));
        assert_eq!(
            other_source.requeue(1, 0, 1, &other_target, Some(waiting), true),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(other_source.inner.lock().waiter_queue.len(), 1);
        assert_eq!(other_target.inner.lock().waiter_queue.len(), 0);
        assert!(other_target.owner().is_none());

        // Any other thread is a fine owner, and then the move happens.
        assert_eq!(
            other_source.requeue(1, 0, 1, &other_target, Some(elsewhere.clone()), true),
            Ok(1)
        );
        assert_eq!(other_target.inner.lock().waiter_queue.len(), 1);
        assert!(Arc::ptr_eq(&other_target.owner().unwrap(), &elsewhere));
    }

    /// The two-futex path compares the value too, and a call it refuses must
    /// not have moved or woken anybody on the way to refusing.
    #[test]
    fn a_requeue_whose_value_moved_leaves_both_queues_alone() {
        let source = futex_with(1);
        let target = futex_with(2);
        let waiters: Vec<_> = (0..2).map(|_| queue_waiter(&source, 1, None)).collect();
        let (owner, _) = two_threads();
        source.inner.lock().set_owner(Some(owner.clone()));

        assert_eq!(
            source.requeue(99, 1, 1, &target, None, true),
            Err(ZxError::BAD_STATE)
        );
        assert_eq!(waiters[0].1.count(), 0);
        assert_eq!(waiters[1].1.count(), 0);
        assert_eq!(source.inner.lock().waiter_queue.len(), 2);
        assert_eq!(target.inner.lock().waiter_queue.len(), 0);
        assert!(Arc::ptr_eq(&source.owner().unwrap(), &owner));
    }

    /// A thread already waiting on the futex cannot be named its owner by a
    /// second waiter: it would come out owning a lock it is itself blocked on.
    /// The test that has covered this since always AWAITS the answer, so with
    /// the check gone it waits for ever instead of failing; polling by hand
    /// turns that into a failure with a name on it.
    #[test]
    fn a_waiter_cannot_be_named_the_owner_by_a_second_wait() {
        let (blocked, other) = two_threads();
        let futex = futex_with(1);
        let (_first, _) = queue_waiter(&futex, 1, Some(blocked.clone()));

        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = waker(counter);
        let mut second = Box::pin(futex.wait_with_owner(1, Some(other.clone()), Some(blocked)));
        assert_eq!(
            second.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Err(ZxError::INVALID_ARGS))
        );
        // Refused before queueing: the second waiter never joined.
        assert_eq!(futex.inner.lock().waiter_queue.len(), 1);

        // Somebody who is not on the queue is a fine owner, and then the wait
        // goes through.
        let mut third = Box::pin(futex.wait_with_owner(1, None, Some(other)));
        assert_eq!(
            third.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        assert_eq!(futex.inner.lock().waiter_queue.len(), 2);
    }

    /// `Future::poll` only promises to wake the waker of the LAST poll. A
    /// waiter that keeps the first one wakes a task that is no longer
    /// listening, and the one that is sleeps on.
    #[test]
    fn a_re_poll_with_a_new_waker_wakes_the_new_one() {
        let futex = futex_with(1);
        let stale = Arc::new(Counter(AtomicUsize::new(0)));
        let fresh = Arc::new(Counter(AtomicUsize::new(0)));
        let (stale_waker, fresh_waker) = (waker(stale.clone()), waker(fresh.clone()));
        let mut future = Box::pin(futex.wait(1));

        assert_eq!(
            future.as_mut().poll(&mut Context::from_waker(&stale_waker)),
            Poll::Pending
        );
        assert_eq!(
            future.as_mut().poll(&mut Context::from_waker(&fresh_waker)),
            Poll::Pending
        );
        // Still one waiter, not two.
        assert_eq!(futex.inner.lock().waiter_queue.len(), 1);

        assert_eq!(futex.wake(1), 1);
        assert_eq!(stale.count(), 0, "the stale waker wakes nobody useful");
        assert_eq!(fresh.count(), 1);
        assert_eq!(
            future.as_mut().poll(&mut Context::from_waker(&fresh_waker)),
            Poll::Ready(Ok(()))
        );
    }

    /// `FUTEX_UNLOCK_PI`'s release step counts a tombstone as a waiter on
    /// purpose: the worst case is one extra kernel round trip for the next
    /// locker, while missing a real waiter would be a lost wakeup.
    #[test]
    fn store_by_waiters_counts_a_tombstone() {
        const WAITERS: i32 = 0x8000_0000_u32 as i32;
        let futex = futex_with(7);

        assert!(!futex.store_by_waiters(WAITERS, 0));
        assert_eq!(futex.load(), 0);

        enqueue_tombstone(&futex, None);
        assert!(futex.store_by_waiters(WAITERS, 0));
        assert_eq!(futex.load(), WAITERS);
    }

    /// The `FUTEX_WAIT` fast path must agree with the check the enqueue does
    /// under the queue lock, in both directions.
    #[test]
    fn value_eq_agrees_with_the_queued_check() {
        let futex = futex_with(1);
        assert!(futex.value_eq(1));
        assert!(!futex.value_eq(2));

        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = waker(counter);
        let mut mismatch = Box::pin(futex.wait(2));
        assert_eq!(
            mismatch.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Err(ZxError::BAD_STATE))
        );
        assert_eq!(futex.inner.lock().waiter_queue.len(), 0);
    }

    /// Every distinct word a process ever named used to stay in its table
    /// until exit. Ten thousand idle futexes, each dropped as soon as it was
    /// made, leave a table no bigger than one sweep's worth.
    #[test]
    fn a_table_forgets_the_futexes_nobody_holds_waits_on_or_owns() {
        let mut table = FutexTable::default();
        let words: &'static [AtomicI32] =
            Box::leak((0..10_000).map(|_| AtomicI32::new(0)).collect());
        for word in words {
            drop(table.get_or_create(word as *const _ as usize, || Futex::new(word)));
        }
        assert!(
            table.len() <= FUTEX_TABLE_MIN_SWEEP,
            "{} idle futexes are still remembered",
            table.len()
        );
        let first = table.get_or_create(words[0].as_ptr() as usize, || Futex::new(&words[0]));
        let again = table.get_or_create(words[0].as_ptr() as usize, || Futex::new(&words[0]));
        assert!(
            Arc::ptr_eq(&first, &again),
            "a futex someone holds is the one the next call gets"
        );
    }

    /// A sweep keeps the futexes that still matter: one a caller holds, one
    /// with a waiter on its queue, and one a thread owns with nobody
    /// waiting. An idle one is replaced by a fresh object after the sweep.
    #[test]
    fn a_sweep_keeps_the_held_the_waited_on_and_the_owned() {
        let proc = Process::create(&Job::root(), "futex-table").unwrap();
        let thread = Thread::create(&proc, "owner").unwrap();
        let mut table = FutexTable::default();
        let word = |v| -> &'static AtomicI32 { Box::leak(Box::new(AtomicI32::new(v))) };
        let key = |w: &'static AtomicI32| w as *const _ as usize;

        let held_word = word(0);
        let held = table.get_or_create(key(held_word), || Futex::new(held_word));

        let waited_word = word(0);
        let waited_id = {
            let futex = table.get_or_create(key(waited_word), || Futex::new(waited_word));
            futex.id()
        };
        let (_waiting, _) = queue_waiter(
            &table.get_or_create(key(waited_word), || unreachable!()),
            0,
            None,
        );

        let owned_word = word(0);
        let owned_id = {
            let futex = table.get_or_create(key(owned_word), || Futex::new(owned_word));
            futex.inner.lock().set_owner(Some(thread.clone()));
            futex.id()
        };

        let idle_word = word(0);
        let idle_id = table
            .get_or_create(key(idle_word), || Futex::new(idle_word))
            .id();

        let filler: &'static [AtomicI32] = Box::leak(
            (0..10 * FUTEX_TABLE_MIN_SWEEP)
                .map(|_| AtomicI32::new(0))
                .collect(),
        );
        for w in filler {
            drop(table.get_or_create(key(w), || Futex::new(w)));
        }

        assert!(Arc::ptr_eq(
            &held,
            &table.get_or_create(key(held_word), || unreachable!())
        ));
        assert_eq!(
            table
                .get_or_create(key(waited_word), || unreachable!())
                .id(),
            waited_id,
            "a futex with a waiter keeps its queue"
        );
        assert_eq!(
            table.get_or_create(key(owned_word), || unreachable!()).id(),
            owned_id,
            "a futex a thread owns keeps its owner"
        );
        assert_ne!(
            table
                .get_or_create(key(idle_word), || Futex::new(idle_word))
                .id(),
            idle_id,
            "the idle one was swept"
        );
    }
}
