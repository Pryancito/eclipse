//! A wait whose future completes in the same instant its deadline fires.
//!
//! `blocking_run` races the future against the timer with `select_biased!`,
//! which polls the future first and the timer second. On SMP a `FUTEX_WAKE`
//! on another CPU can land between those two polls: `Waiter::wake` marks the
//! waiter woken and counts it, then the timer is found expired and the wait
//! ends `TIMED_OUT`. The wake is gone: the waker was told one thread got it,
//! and that thread reports that nobody came. Linux answers 0 there
//! (`futex_wait` returns success when `futex_unqueue` finds it already
//! woken), so a `pthread_mutex_unlock` racing a `pthread_mutex_timedlock`
//! timeout never strands the other waiters behind a free lock.
//!
//! The tests below put the wake exactly in that window: a wrapper future
//! polls the futex wait, and when it is still pending, wakes the futex itself
//! before handing back `Pending`, with a deadline already past.

use super::*;
use crate::signal::Futex;
use crate::task::Job;
use core::sync::atomic::AtomicI32;

/// Polls `inner`; the first time it answers `Pending`, runs `between` (the
/// other CPU) before answering `Pending` too.
struct WakeBetweenPolls<F, W> {
    inner: Pin<Box<F>>,
    between: Option<W>,
}

impl<F: Future, W: FnOnce() + Unpin> Future for WakeBetweenPolls<F, W> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        match self.inner.as_mut().poll(cx) {
            Poll::Ready(out) => Poll::Ready(out),
            Poll::Pending => {
                if let Some(between) = self.between.take() {
                    between();
                }
                Poll::Pending
            }
        }
    }
}

fn futex_word() -> &'static AtomicI32 {
    Box::leak(Box::new(AtomicI32::new(0)))
}

#[async_std::test]
async fn a_wake_that_lands_as_the_deadline_fires_is_not_lost() {
    let proc = Process::create(&Job::root(), "proc").expect("a process");
    let thread = CurrentThread(Thread::create(&proc, "thread").expect("a thread"));
    let futex = Futex::new(futex_word());
    let woken = Arc::new(AtomicUsize::new(0));
    let future = WakeBetweenPolls {
        inner: Box::pin(futex.wait(0)),
        between: Some({
            let (futex, woken) = (futex.clone(), woken.clone());
            move || woken.store(futex.wake(1), Ordering::SeqCst)
        }),
    };
    // Already past: the timer is ready on the very first poll.
    let result: ZxResult = thread
        .blocking_run(future, ThreadState::BlockedFutex, Duration::ZERO, None)
        .await;
    assert_eq!(
        woken.load(Ordering::SeqCst),
        1,
        "the wake counted the waiter"
    );
    assert_eq!(
        result,
        Ok(()),
        "the wake counted this waiter as woken, so the wait must say it was: \
         TIMED_OUT here is a wake nobody received"
    );
    assert_eq!(thread.state(), ThreadState::New);
}

#[async_std::test]
async fn a_deadline_with_nobody_waking_still_times_out() {
    let proc = Process::create(&Job::root(), "proc").expect("a process");
    let thread = CurrentThread(Thread::create(&proc, "thread").expect("a thread"));
    let futex = Futex::new(futex_word());
    let result: ZxResult = thread
        .blocking_run(
            futex.wait(0),
            ThreadState::BlockedFutex,
            Duration::ZERO,
            None,
        )
        .await;
    assert_eq!(result, Err(ZxError::TIMED_OUT));
    assert!(futex.is_idle(), "the timed-out waiter left the queue");
    assert_eq!(futex.wake(1), 0, "and a later wake finds nobody");
}
