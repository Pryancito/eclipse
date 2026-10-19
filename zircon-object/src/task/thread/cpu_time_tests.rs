//! The CPU time a process reports is its threads': the ones running and the
//! ones gone, user and kernel, and none of it twice or never.
//!
//! The poll that finishes a thread is the one that took it off its process's
//! list (`CurrentThread::drop` runs inside it), so what that poll measured in
//! the kernel used to be credited to a thread nobody added up any more: a
//! process whose life was one poll, the `fork`/`exec`/`exit` of most of what
//! a shell runs, had used no kernel time at all.

extern crate std;

use super::*;
use crate::task::Job;

#[test]
fn a_process_counts_its_running_threads_and_its_gone_ones() {
    let proc = Process::create(&Job::root(), "cpu").unwrap();
    let (running, gone) = (
        Thread::create(&proc, "running").unwrap(),
        Thread::create(&proc, "gone").unwrap(),
    );
    running.time_add(5_000);
    running.sys_time_add(1_000);
    gone.time_add(7_000);
    gone.sys_time_add(2_000);

    assert_eq!(proc.cpu_time_ns(), (12_000, 3_000));
    gone.terminate_abandoned();
    assert_eq!(
        proc.cpu_time_ns(),
        (12_000, 3_000),
        "a thread's time moved when it left"
    );
    running.terminate_abandoned();
    assert_eq!(proc.cpu_time_ns(), (12_000, 3_000));
}

#[test]
fn the_poll_that_ends_a_thread_counts_for_its_process() {
    let proc = Process::create(&Job::root(), "cpu").unwrap();
    let thread = Thread::create(&proc, "last").unwrap();
    let current = CurrentThread(thread.clone());
    let mut switch = ThreadSwitchFuture::new(
        thread,
        Box::pin(async move {
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(2) {}
            drop(current);
        }),
    );

    // One poll, inside a task: the host's current thread is a task-local.
    let polled = async_std::task::block_on(core::future::poll_fn(|cx| {
        Poll::Ready(Pin::new(&mut switch).poll(cx))
    }));

    assert!(polled.is_ready());
    let (_, kernel) = proc.cpu_time_ns();
    assert!(
        kernel >= 2_000_000,
        "the last poll's {} ns of kernel time went to a thread that had left",
        kernel
    );
}
