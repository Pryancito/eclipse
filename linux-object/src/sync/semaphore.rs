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
    /// EventBus of this Semaphore
    eventbus: EventBus,
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
                        inner.count -= 1;
                        if inner.count < 1 {
                            inner.eventbus.clear(Event::SEMAPHORE_CAN_ACQUIRE);
                        }
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
        inner.count += 1;
        if inner.count >= 1 {
            inner.eventbus.set(Event::SEMAPHORE_CAN_ACQUIRE);
        }
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
        inner.count = inner.count.saturating_add(delta);
        if inner.count >= 1 {
            inner.eventbus.set(Event::SEMAPHORE_CAN_ACQUIRE);
        } else {
            inner.eventbus.clear(Event::SEMAPHORE_CAN_ACQUIRE);
        }
    }

    /// Set the current count
    pub fn set(&self, value: isize) {
        let mut inner = self.lock.lock();
        inner.count = value;
        if inner.count >= 1 {
            inner.eventbus.set(Event::SEMAPHORE_CAN_ACQUIRE);
        }
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
