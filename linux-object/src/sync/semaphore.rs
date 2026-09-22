//! A counting, blocking, semaphore.
//!
//! Same as [std::sync::Semaphore at rust 1.7.0](https://docs.rs/std-semaphore/0.1.0/std_semaphore/)

use super::{Event, EventBus};
use crate::error::LxError;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use kernel_hal::sync::Mutex;

/// How often a blocked `semop` wakes up just to ask whether it should still
/// be waiting. Same reasoning, and the same figure, as the io-multiplex
/// backstop: a ceiling on how late a `kill` can be, not on how fast a
/// `release` is noticed (the event bus still does that immediately).
const SEMOP_INTERRUPT_CHECK_TICK_MS: u64 = 100;

/// A counting, blocking, semaphore.
pub struct Semaphore {
    /// value and removed inner struct
    lock: Arc<Mutex<SemaphoreInner>>,
}

/// Semaphore inner data
struct SemaphoreInner {
    /// can be thought of as a number of resources
    count: isize,
    /// current Semaphore pid
    pid: usize,
    /// is removed
    removed: bool,
    /// Bumped on every change to `count`, so a waiter can tell "nothing has
    /// happened yet" from "it changed and changed back".
    ///
    /// The event bus cannot answer that on its own: its `CAN_ACQUIRE` flag
    /// says the count is *positive*, which is the wrong question for a
    /// `semop` waiting for the count to reach **zero** (semop(2)'s `sem_op ==
    /// 0`). A counter has no such blind spot and cannot race: a change that
    /// lands between the snapshot and the park is still visible afterwards.
    generation: u64,
    /// EventBus of this Semaphore
    eventbus: EventBus,
}

impl SemaphoreInner {
    /// Set the count and leave the derived state -- the "can acquire" signal
    /// and the generation counter -- telling the truth about it.
    ///
    /// Every write to `count` goes through here. A signal left set from an
    /// earlier release wakes every waiter on the bus for a resource that is
    /// not there, and a write that forgets to bump the generation leaves a
    /// `semop` parked on a value that already changed.
    fn store(&mut self, count: isize) {
        self.count = count;
        self.generation = self.generation.wrapping_add(1);
        if count >= 1 {
            self.eventbus.set(Event::SEMAPHORE_CAN_ACQUIRE);
        } else {
            self.eventbus.clear(Event::SEMAPHORE_CAN_ACQUIRE);
        }
    }
}

/// An RAII guard which will release a resource acquired from a semaphore when
/// dropped.
pub struct SemaphoreGuard<'a> {
    sem: &'a Semaphore,
}

impl Semaphore {
    /// Creates a new semaphore with the initial count specified.
    ///
    /// The count specified can be thought of as a number of resources, and a
    /// call to `acquire` or `access` will block until at least one resource is
    /// available. It is valid to initialize a semaphore with a negative count.
    pub fn new(count: isize) -> Semaphore {
        Semaphore {
            lock: Arc::new(Mutex::new(SemaphoreInner {
                count,
                removed: false,
                pid: 0,
                generation: 0,
                eventbus: EventBus::default(),
            })),
        }
    }

    /// Set the semaphore in removed statue
    pub fn remove(&self) {
        let mut inner = self.lock.lock();
        inner.removed = true;
        inner.eventbus.set(Event::SEMAPHORE_REMOVED);
    }

    /// Acquires a resource of this semaphore, blocking the current thread until
    /// it can do so.
    ///
    /// This method will block until the internal count of the semaphore is at
    /// least 1.
    pub async fn acquire(&self) -> Result<(), LxError> {
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct SemaphoreFuture {
            inner: Arc<Mutex<SemaphoreInner>>,
            sub_id: Option<u64>,
            /// Backstop slot: the event bus fires for `release` and for
            /// `IPC_RMID`, and for nothing that happens to the WAITER. Without
            /// a tick nothing re-polls this, so nothing checks for a signal,
            /// and `semop(-1)` on a semaphore nobody ever releases was a wait
            /// that `kill -9` could not end either.
            timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
        }

        impl Drop for SemaphoreFuture {
            fn drop(&mut self) {
                if let Some(id) = self.sub_id.take() {
                    self.inner.lock().eventbus.unsubscribe(id);
                }
                kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
            }
        }

        impl Future for SemaphoreFuture {
            type Output = Result<(), LxError>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                let this = self.as_mut().get_mut();
                {
                    let mut inner = this.inner.lock();
                    if inner.removed {
                        if let Some(id) = this.sub_id.take() {
                            inner.eventbus.unsubscribe(id);
                        }
                        return Poll::Ready(Err(LxError::EIDRM));
                    }
                    if inner.count >= 1 {
                        let count = inner.count - 1;
                        inner.store(count);
                        if let Some(id) = this.sub_id.take() {
                            inner.eventbus.unsubscribe(id);
                        }
                        return Poll::Ready(Ok(()));
                    }
                    if this.sub_id.is_none() {
                        let waker = cx.waker().clone();
                        this.sub_id = inner.eventbus.subscribe(Box::new(move |_| {
                            waker.wake_by_ref();
                            true
                        }));
                    }
                }

                // Nothing to take. Before parking again, ask whether this
                // thread is still supposed to be waiting -- the bus reports
                // only what the SEMAPHORE does, so this is the only thing here
                // that can answer a signal or a kill. semop(2) lists EINTR
                // among its errors for exactly this.
                if let Err(err) = crate::process::check_signals() {
                    if let Some(id) = this.sub_id.take() {
                        this.inner.lock().eventbus.unsubscribe(id);
                    }
                    kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
                    return Poll::Ready(Err(err));
                }
                let deadline = kernel_hal::timer::deadline_after(Duration::from_millis(
                    SEMOP_INTERRUPT_CHECK_TICK_MS,
                ));
                kernel_hal::timer_waker::ensure_timer_waker(&mut this.timer, deadline, cx);
                Poll::Pending
            }
        }

        let future = SemaphoreFuture {
            inner: self.lock.clone(),
            sub_id: None,
            timer: None,
        };
        future.await
    }

    /// Release a resource from this semaphore.
    ///
    /// This will increment the number of resources in this semaphore by 1 and
    /// will notify any pending waiters in `acquire` or `access` if necessary.
    pub fn release(&self) {
        let mut inner = self.lock.lock();
        let count = inner.count.saturating_add(1);
        inner.store(count);
    }

    /// Acquires a resource of this semaphore, returning an RAII guard to
    /// release the semaphore when dropped.
    ///
    /// This function is semantically equivalent to an `acquire` followed by a
    /// `release` when the guard returned is dropped.
    pub async fn access(&self) -> Result<SemaphoreGuard<'_>, LxError> {
        self.acquire().await?;
        Ok(SemaphoreGuard { sem: self })
    }

    /// Get the current count
    pub fn get(&self) -> isize {
        self.lock.lock().count
    }

    /// Get the current eventbus callback length
    pub fn get_ncnt(&self) -> usize {
        self.lock.lock().eventbus.get_callback_len()
    }

    /// Get the current pid
    pub fn get_pid(&self) -> usize {
        self.lock.lock().pid
    }

    /// Set the current pid
    pub fn set_pid(&self, pid: usize) {
        self.lock.lock().pid = pid;
    }

    /// Add `delta` to the count, in one step, and leave the "can acquire"
    /// signal telling the truth afterwards.
    ///
    /// This is what a `SEM_UNDO` record needs at process exit: the adjustment
    /// it has accumulated is any integer, not a count of `release()` calls, and
    /// it may well be negative. Doing it as `set(get() + delta)` would be two
    /// separate lock acquisitions with a window in between.
    pub fn adjust(&self, delta: isize) {
        let mut inner = self.lock.lock();
        let count = inner.count.saturating_add(delta);
        inner.store(count);
    }

    /// Set the current count.
    ///
    /// Used by `semctl(SETVAL/SETALL)` and by the `semop` apply. It CLEARS
    /// the "can acquire" signal when the new value is not positive -- setting
    /// a semaphore back to 0 used to leave the flag from whenever it was last
    /// positive, and every waiter on the bus then woke for a resource that
    /// was not there and went straight back to sleep.
    pub fn set(&self, value: isize) {
        let mut inner = self.lock.lock();
        inner.store(value);
    }

    /// The value, and the generation it belongs to, read together.
    ///
    /// A `semop` plans against a snapshot of the WHOLE set and then applies
    /// it, so it needs to know the snapshot is still current; reading the
    /// value and the generation in two steps would not tell it that.
    pub fn get_versioned(&self) -> (isize, u64) {
        let inner = self.lock.lock();
        (inner.count, inner.generation)
    }

    /// Whether this semaphore has been `IPC_RMID`-ed.
    pub fn is_removed(&self) -> bool {
        self.lock.lock().removed
    }

    /// Park until this semaphore's value may have changed, WITHOUT taking
    /// anything from it.
    ///
    /// This is what a blocked `semop` waits on. It cannot use
    /// [`acquire`](Self::acquire), which both waits and decrements: semop(2)
    /// applies the whole operation array atomically, so a blocked caller must
    /// hold *nothing* while it waits and re-plan from scratch when it wakes.
    ///
    /// `since` is the generation the caller planned against. Returns as soon
    /// as the current one differs, which makes the "it changed while I was
    /// getting here" race a no-op rather than a missed wakeup.
    pub async fn wait_for_change(&self, since: u64) -> Result<(), LxError> {
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct ChangeFuture {
            inner: Arc<Mutex<SemaphoreInner>>,
            since: u64,
            sub_id: Option<u64>,
            timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
        }

        impl Drop for ChangeFuture {
            fn drop(&mut self) {
                if let Some(id) = self.sub_id.take() {
                    self.inner.lock().eventbus.unsubscribe(id);
                }
                kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
            }
        }

        impl ChangeFuture {
            fn done(&mut self, out: Result<(), LxError>) -> Poll<Result<(), LxError>> {
                if let Some(id) = self.sub_id.take() {
                    self.inner.lock().eventbus.unsubscribe(id);
                }
                kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
                Poll::Ready(out)
            }
        }

        impl Future for ChangeFuture {
            type Output = Result<(), LxError>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                let this = self.as_mut().get_mut();
                {
                    let mut inner = this.inner.lock();
                    if inner.removed {
                        drop(inner);
                        return this.done(Err(LxError::EIDRM));
                    }
                    if inner.generation != this.since {
                        drop(inner);
                        return this.done(Ok(()));
                    }
                    if this.sub_id.is_none() {
                        let waker = cx.waker().clone();
                        this.sub_id = inner.eventbus.subscribe(Box::new(move |_| {
                            waker.wake_by_ref();
                            true
                        }));
                    }
                }
                // Same reason as `acquire`: the bus reports only what the
                // SEMAPHORE does, so without this a `semop` blocked on a
                // semaphore nobody releases could not be killed either.
                if let Err(err) = crate::process::check_signals() {
                    return this.done(Err(err));
                }
                // The backstop also covers the one thing the bus cannot
                // report: `CAN_ACQUIRE` says the count went POSITIVE, and a
                // `semop` with `sem_op == 0` is waiting for it to reach ZERO.
                let deadline = kernel_hal::timer::deadline_after(Duration::from_millis(
                    SEMOP_INTERRUPT_CHECK_TICK_MS,
                ));
                kernel_hal::timer_waker::ensure_timer_waker(&mut this.timer, deadline, cx);
                Poll::Pending
            }
        }

        ChangeFuture {
            inner: self.lock.clone(),
            since,
            sub_id: None,
            timer: None,
        }
        .await
    }
}

impl Drop for SemaphoreGuard<'_> {
    fn drop(&mut self) {
        self.sem.release();
    }
}

impl Deref for SemaphoreGuard<'_> {
    type Target = Semaphore;

    fn deref(&self) -> &Self::Target {
        self.sem
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LxError;

    #[test]
    fn adjust_moves_the_count_either_way_in_one_step() {
        // What a `SEM_UNDO` record needs at process exit: an adjustment of any
        // sign, applied under a single lock.
        let sem = Semaphore::new(5);
        sem.adjust(3);
        assert_eq!(sem.get(), 8);
        sem.adjust(-6);
        assert_eq!(sem.get(), 2);
        sem.adjust(0);
        assert_eq!(sem.get(), 2);
    }

    #[test]
    fn adjust_saturates_instead_of_overflowing() {
        let sem = Semaphore::new(isize::MAX);
        sem.adjust(10);
        assert_eq!(sem.get(), isize::MAX);
        let sem = Semaphore::new(isize::MIN);
        sem.adjust(-10);
        assert_eq!(sem.get(), isize::MIN);
    }

    #[test]
    fn adjust_down_past_zero_clears_the_can_acquire_signal() {
        // The count is allowed to go negative, and while it is there nobody
        // can acquire. A signal left set from an earlier release wakes every
        // waiter on the bus for a resource that is not there.
        let sem = Semaphore::new(0);
        sem.adjust(2);
        assert!(sem
            .lock
            .lock()
            .eventbus
            .events()
            .contains(Event::SEMAPHORE_CAN_ACQUIRE));
        sem.adjust(-5);
        assert_eq!(sem.get(), -3);
        assert!(!sem
            .lock
            .lock()
            .eventbus
            .events()
            .contains(Event::SEMAPHORE_CAN_ACQUIRE));
    }

    #[async_std::test]
    async fn adjust_wakes_a_waiter_once_the_count_is_positive_again() {
        let sem = Arc::new(Semaphore::new(1));
        sem.adjust(-3);
        assert_eq!(sem.get(), -2);
        let waiter = {
            let sem = Arc::clone(&sem);
            async_std::task::spawn(async move { sem.acquire().await })
        };
        async_std::task::yield_now().await;
        sem.adjust(3);
        waiter.await.unwrap();
        assert_eq!(sem.get(), 0);
    }

    #[async_std::test]
    async fn acquire_decrements_count() {
        let sem = Semaphore::new(2);
        sem.acquire().await.unwrap();
        assert_eq!(sem.get(), 1);
        sem.acquire().await.unwrap();
        assert_eq!(sem.get(), 0);
    }

    #[async_std::test]
    async fn release_wakes_waiter() {
        let sem = Arc::new(Semaphore::new(0));
        let waiter = {
            let sem = Arc::clone(&sem);
            async_std::task::spawn(async move { sem.acquire().await })
        };
        async_std::task::yield_now().await;
        sem.release();
        waiter.await.unwrap();
        assert_eq!(sem.get(), 0);
    }

    #[async_std::test]
    async fn remove_causes_eidrm_on_acquire() {
        let sem = Arc::new(Semaphore::new(0));
        let waiter = {
            let sem = Arc::clone(&sem);
            async_std::task::spawn(async move { sem.acquire().await })
        };
        async_std::task::yield_now().await;
        sem.remove();
        assert!(matches!(waiter.await, Err(LxError::EIDRM)));
    }

    #[test]
    fn guard_releases_on_drop() {
        let sem = Semaphore::new(0);
        {
            sem.set(1);
            let _guard = async_std::task::block_on(sem.access()).unwrap();
            assert_eq!(sem.get(), 0);
        }
        assert_eq!(sem.get(), 1);
    }
}

#[cfg(test)]
mod semop_primitive_tests {
    //! What a `semop` needs from a single semaphore, beyond "take one".
    //!
    //! `semop(2)` plans over the whole set and then applies it, so it needs
    //! to write an exact value (not "one more" or "one less"), to know
    //! whether the snapshot it planned against is still current, and to park
    //! until something changes without taking anything.

    use super::*;
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn can_acquire(sem: &Semaphore) -> bool {
        sem.lock
            .lock()
            .eventbus
            .events()
            .contains(Event::SEMAPHORE_CAN_ACQUIRE)
    }

    /// A waker that does nothing. These tests drive `wait_for_change` by hand
    /// -- one poll at a time -- rather than `block_on`, ON PURPOSE: a mutation
    /// that stops the wait from ever resolving would make `block_on` hang
    /// forever, and a hang is not a detected failure. A bounded poll turns
    /// that same mutation into a clean assertion failure on the next line.
    fn noop_waker() -> Waker {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        unsafe fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    /// Setting a semaphore back to a non-positive value must CLEAR the "can
    /// acquire" signal. It used to only ever set it, so a semaphore that had
    /// been positive kept the flag forever: every waiter on the bus woke for
    /// a resource that was not there, found nothing, and parked again --
    /// several times a second, for as long as the set existed.
    #[test]
    fn setting_a_semaphore_back_to_zero_clears_the_can_acquire_signal() {
        let sem = Semaphore::new(0);
        sem.set(4);
        assert!(can_acquire(&sem));
        sem.set(0);
        assert!(
            !can_acquire(&sem),
            "a semaphore at zero must not advertise that it can be acquired"
        );
        sem.set(-2);
        assert!(!can_acquire(&sem));
    }

    /// Every write to the value moves the generation on, whichever way it
    /// came in. A `semop` parked on a stale generation is a `semop` that
    /// missed its wakeup.
    #[test]
    fn every_kind_of_write_moves_the_generation_on() {
        let sem = Semaphore::new(0);
        let (_, start) = sem.get_versioned();
        let mut seen = start;
        for step in 0..4 {
            match step {
                0 => sem.release(),
                1 => sem.adjust(-3),
                2 => sem.set(7),
                _ => async_std::task::block_on(sem.acquire()).unwrap(),
            }
            let (_, now) = sem.get_versioned();
            assert_ne!(now, seen, "write {} left the generation where it was", step);
            seen = now;
        }
    }

    /// The value and its generation come back from one read. Two reads could
    /// straddle a change and hand the caller a value that belongs to one
    /// generation and a stamp that belongs to another -- which is exactly the
    /// combination that makes a planned-then-applied `semop` write a value
    /// nobody agreed to.
    #[test]
    fn the_value_and_its_generation_come_from_the_same_read() {
        let sem = Semaphore::new(3);
        let (value, generation) = sem.get_versioned();
        assert_eq!(value, 3);
        sem.set(3);
        let (same_value, later) = sem.get_versioned();
        assert_eq!(same_value, 3);
        assert_ne!(
            later, generation,
            "a write of the same value still happened, and a waiter must re-plan"
        );
    }

    /// A wait that starts on a generation that has ALREADY moved returns at
    /// once. This is the race the counter exists for: between the plan and
    /// the park, another process can release the very unit being waited for,
    /// and a wait that only listened for future events would sleep through
    /// it.
    #[test]
    fn a_wait_on_a_stale_generation_returns_immediately() {
        let sem = Semaphore::new(0);
        let (_, generation) = sem.get_versioned();
        sem.release();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(sem.wait_for_change(generation));
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "a change that already happened must be seen on the very first poll"
        );
    }

    /// And a removed set answers `EIDRM` rather than waiting, `IPC_RMID`
    /// being the other way a blocked `semop` ends.
    #[test]
    fn a_wait_on_a_removed_semaphore_is_eidrm() {
        let sem = Semaphore::new(0);
        let (_, generation) = sem.get_versioned();
        sem.remove();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(sem.wait_for_change(generation));
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Err(LxError::EIDRM))
        ));
    }

    /// The wait parks while nothing has changed, resolves once something does,
    /// and takes nothing when it resolves. `acquire` both waits and
    /// decrements, which a blocked `semop` must never do: it has to hold
    /// nothing at all while it waits, or the array it is halfway through stops
    /// being atomic.
    #[test]
    fn waiting_for_a_change_parks_then_resolves_and_takes_nothing() {
        let sem = Semaphore::new(0);
        let (_, generation) = sem.get_versioned();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(sem.wait_for_change(generation));

        // Nothing has changed yet, so it parks.
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "a wait on the current generation must park"
        );

        // A release moves the generation on. The next poll must resolve...
        sem.release();
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "a change while parked must wake the wait on the next poll"
        );
        // ...without having taken the unit the release added.
        assert_eq!(
            sem.get(),
            1,
            "the released unit must still be there for whoever re-plans first"
        );
    }
}
