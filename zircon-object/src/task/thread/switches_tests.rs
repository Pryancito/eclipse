//! Each poll of a thread that leaves the CPU is a context switch, voluntary
//! when the thread went to wait and nonvoluntary when it left runnable; and a
//! process's count is its threads', the exited ones' included.

extern crate std;

use super::*;
use crate::task::Job;

/// `Pending` the first time, without waking anybody: a wait for an event.
struct WaitsOnce(bool);

impl Future for WaitsOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
}

/// Poll `switch` once inside a task: the host's current thread is a
/// task-local.
fn poll_once(switch: &mut ThreadSwitchFuture) -> Poll<()> {
    async_std::task::block_on(core::future::poll_fn(|cx| {
        Poll::Ready(Pin::new(&mut *switch).poll(cx))
    }))
}

#[test]
fn a_wait_is_a_voluntary_switch_and_a_yield_or_a_preemption_is_not() {
    let proc = Process::create(&Job::root(), "switches").unwrap();
    let thread = Thread::create(&proc, "switches").unwrap();
    let mut switch = ThreadSwitchFuture::new(
        thread.clone(),
        Box::pin(async {
            kernel_hal::thread::yield_now().await;
            kernel_hal::thread::preempt_now().await;
            WaitsOnce(false).await;
        }),
    );

    assert!(poll_once(&mut switch).is_pending());
    assert_eq!(thread.context_switches(), (0, 1), "sched_yield");
    assert!(poll_once(&mut switch).is_pending());
    assert_eq!(thread.context_switches(), (0, 2), "the end of a slice");
    assert!(poll_once(&mut switch).is_pending());
    assert_eq!(thread.context_switches(), (1, 2), "a wait");
    assert!(poll_once(&mut switch).is_ready());
    assert_eq!(
        thread.context_switches(),
        (1, 2),
        "the poll that finishes is no switch"
    );
}

#[test]
fn a_yield_another_poll_left_on_the_cpu_is_not_this_threads() {
    let proc = Process::create(&Job::root(), "switches").unwrap();
    let thread = Thread::create(&proc, "switches").unwrap();
    // A yield polled outside any thread, on this CPU, and never asked about.
    let mut other = Box::pin(kernel_hal::thread::yield_now());
    let waker = futures::task::noop_waker();
    assert!(other
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());

    let mut switch = ThreadSwitchFuture::new(thread.clone(), Box::pin(WaitsOnce(false)));
    assert!(poll_once(&mut switch).is_pending());
    assert_eq!(thread.context_switches(), (1, 0));
}

#[test]
fn a_process_counts_its_threads_switches_and_its_gone_ones() {
    let proc = Process::create(&Job::root(), "switches").unwrap();
    let (running, gone) = (
        Thread::create(&proc, "running").unwrap(),
        Thread::create(&proc, "gone").unwrap(),
    );
    running.note_context_switch(false);
    running.note_context_switch(true);
    for _ in 0..3 {
        gone.note_context_switch(false);
    }
    gone.note_context_switch(true);

    assert_eq!(proc.context_switches(), (4, 2));
    gone.terminate_abandoned();
    assert_eq!(
        proc.context_switches(),
        (4, 2),
        "a thread's switches moved when it left"
    );
    running.terminate_abandoned();
    assert_eq!(proc.context_switches(), (4, 2));
}
