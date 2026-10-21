//! A thread that is killed leaves the wait it is parked in.
//!
//! `Thread::stop` wakes a `blocking_run` and nothing else, and every
//! interruptible wait of this crate parks on `SIGNAL_WAKE` or, for
//! `waitpid`, on the process's `SIGCHLD` bit. A kill set neither, so:
//!
//! - `execve` from a thread that is not the first waits in `de_thread` for
//!   each sibling to die, and a sibling in `pause` never did. The program
//!   hung in its `execve` for good (signal-probe, section `wait`, on Linux
//!   and on this kernel alike since then).
//! - `exit_group`, or a fatal signal, with a thread in `nanosleep`, a
//!   blocking `read` or `waitpid`: the process is reaped (its status is out
//!   at once), but its last thread never goes, so `Process::terminate` never
//!   runs, and its address space, its job entry and its GPU buffers stay
//!   for as long as the machine does.
//!
//! The waits run on a host thread of their own with a watchdog, so a wait
//! the kill does not end fails one test instead of hanging the suite.
extern crate std;

use super::*;
use crate::thread::ThreadExt;
use core::time::Duration;
use rcore_fs_ramfs::RamFS;
use std::sync::mpsc;
use zircon_object::task::Task;

/// A process with `threads` Linux threads.
fn a_process(pid: KoID, threads: usize) -> (Arc<Process>, Vec<Arc<Thread>>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "sleeper",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let threads = (0..threads)
        .map(|_| Thread::create_linux(&proc).unwrap())
        .collect();
    (proc, threads)
}

/// `make(thread)` awaited as `thread` (the current thread is what
/// `check_signals` asks about), on a host thread of its own. The receiver
/// gets the result once the wait ends.
fn parked<T, F, M>(thread: &Arc<Thread>, make: M) -> mpsc::Receiver<T>
where
    T: Send + 'static,
    F: core::future::Future<Output = T>,
    M: FnOnce(Arc<Thread>) -> F + Send + 'static,
{
    let thread = thread.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = async_std::task::block_on(async move {
            kernel_hal::thread::set_current_thread(Some(thread.clone()));
            make(thread).await
        });
        let _ = tx.send(r);
    });
    // Long enough for the wait to be parked, not merely started.
    std::thread::sleep(Duration::from_millis(50));
    rx
}

/// Generous: a kill that wakes the wait ends it in microseconds.
const WATCHDOG: Duration = Duration::from_secs(3);

/// The sibling in `pause` that `execve`'s `de_thread` kills.
#[test]
fn a_kill_ends_a_pause() {
    let (_proc, threads) = a_process(47_101, 1);
    let pause = parked(&threads[0], |t| async move { wait_for_signal(&t).await });
    assert!(
        pause.try_recv().is_err(),
        "nothing should end a pause before the kill"
    );
    threads[0].kill();
    assert_eq!(
        pause.recv_timeout(WATCHDOG),
        Ok(LxError::EINTR),
        "a killed thread stayed in pause, and the execve that killed it \
         waits for it to die"
    );
}

/// `nanosleep`, with its end far beyond the watchdog.
#[test]
fn a_kill_ends_a_sleep() {
    let (_proc, threads) = a_process(47_102, 1);
    let end = kernel_hal::timer::timer_now() + Duration::from_secs(3600);
    let sleep = parked(&threads[0], move |t| async move {
        interruptible_sleep_until(&t, end).await
    });
    threads[0].kill();
    assert_eq!(sleep.recv_timeout(WATCHDOG), Ok(Err(LxError::EINTR)));
}

/// A blocking `read` of a pipe: a wait that only a writer would end.
#[test]
fn a_kill_ends_a_wait_that_nothing_else_would() {
    let (_proc, threads) = a_process(47_103, 1);
    let read = parked(&threads[0], |t| async move {
        interruptible_on(&t, core::future::pending::<()>()).await
    });
    threads[0].kill();
    assert_eq!(read.recv_timeout(WATCHDOG), Ok(Err(LxError::EINTR)));
}

/// `waitpid` on a child that is alive and stays so. It parks on the
/// process's `SIGCHLD` bit, not on the thread.
#[test]
fn a_kill_ends_a_waitpid() {
    let (parent, threads) = a_process(47_104, 1);
    let _child = Process::fork_from(&parent).unwrap();
    let parent2 = parent.clone();
    let wait = parked(&threads[0], move |_t| async move {
        wait_child_any(&parent2, false, true).await.map(|_| ())
    });
    assert!(
        wait.try_recv().is_err(),
        "the waitpid ended with its child still alive"
    );
    threads[0].kill();
    assert_eq!(wait.recv_timeout(WATCHDOG), Ok(Err(LxError::EINTR)));
}

/// `exit_group` with every other thread asleep: each one leaves its wait,
/// which is what lets the last of them go and the process be torn down.
#[test]
fn an_exit_ends_the_wait_of_every_thread() {
    let (proc, threads) = a_process(47_105, 3);
    let waits: Vec<_> = threads
        .iter()
        .map(|t| parked(t, |t| async move { wait_for_signal(&t).await }))
        .collect();
    proc.exit(0);
    for (i, wait) in waits.iter().enumerate() {
        assert_eq!(
            wait.recv_timeout(WATCHDOG),
            Ok(LxError::EINTR),
            "thread {} slept through exit_group",
            i
        );
    }
}

/// The kill wakes the thread it kills. A sibling that goes on living goes
/// on waiting, as it would through any other signal that is not its own.
#[test]
fn a_sibling_that_is_not_killed_waits_on() {
    let (_proc, threads) = a_process(47_106, 2);
    let killed = parked(&threads[0], |t| async move { wait_for_signal(&t).await });
    let left = parked(&threads[1], |t| async move { wait_for_signal(&t).await });
    threads[0].kill();
    assert_eq!(killed.recv_timeout(WATCHDOG), Ok(LxError::EINTR));
    assert!(
        left.recv_timeout(Duration::from_millis(200)).is_err(),
        "a sibling nobody killed left its pause"
    );
    threads[1].kill();
    assert_eq!(left.recv_timeout(WATCHDOG), Ok(LxError::EINTR));
}

/// A thread of a Ctrl-Z'd process is parked until the process is continued,
/// and `kill -9` ends the process without continuing it. The thread stayed
/// parked: the process was reaped, but it never let go of its memory or of
/// its robust futexes, and a waiter on one of those timed out.
#[test]
fn a_kill_ends_a_job_control_stop() {
    let (proc, threads) = a_process(47_131, 1);
    proc.linux().job_stop(&proc, LinuxSignal::SIGSTOP as u8);
    let stop = parked(&threads[0], |t| async move {
        wait_while_job_stopped(t.proc()).await
    });
    assert!(
        stop.try_recv().is_err(),
        "nothing should end a stop before the kill"
    );
    proc.exit(exit_code_killed_by(LinuxSignal::SIGKILL as u8));
    assert_eq!(
        stop.recv_timeout(WATCHDOG),
        Ok(()),
        "the thread of the killed process is still parked in its stop"
    );
}

/// A thread of a process with no Linux side has no Linux waits, and its
/// user signals are not this crate's to set.
#[test]
fn a_thread_with_no_linux_side_is_left_alone() {
    // A Linux process first, so that the hook is in place whatever ran
    // before this test.
    let (_linux, _threads) = a_process(47_107, 0);
    let proc = Process::create(&ROOT_JOB, "zircon").unwrap();
    let thread = Thread::create(&proc, "zircon").unwrap();
    thread.kill();
    assert!(!thread.signal().contains(SIGNAL_WAKE));
    let proc_obj: Arc<dyn KernelObject> = proc.clone();
    assert!(!proc_obj.signal().contains(Signal::SIGCHLD));
    assert!(!proc_obj.signal().contains(JOB_CONTINUE_SIGNAL));
}
