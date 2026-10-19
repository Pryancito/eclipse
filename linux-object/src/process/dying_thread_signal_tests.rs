//! A process-directed signal never lands on a thread on its way out.
//!
//! `send_signal_to_process` queues a signal on the first thread that does not
//! block it, and a dying thread blocks nothing and takes nothing again: the
//! signal was queued there and lost. A SIGTERM sent while a worker was in
//! `pthread_exit` did not end the process. Linux's `wants_signal()` skips a
//! thread with `PF_EXITING` for this. And an `execve` from a non-leader
//! thread now leaves exactly that shape for a moment: the old thread on its
//! way out, first in the list, and the new leader behind it.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::Task;

fn a_process(pid: KoID) -> (Arc<Process>, Arc<Thread>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "two",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let first = Thread::create_linux(&proc).unwrap();
    let second = Thread::create_linux(&proc).unwrap();
    (proc, first, second)
}

#[test]
fn a_signal_goes_past_a_dying_thread_to_a_live_one() {
    let (proc, first, second) = a_process(48_201);
    first.kill();

    send_signal_to_process(proc.id() as usize, LinuxSignal::SIGTERM).unwrap();

    assert!(
        !first.lock_linux().signals.contains(LinuxSignal::SIGTERM),
        "queued on a thread that will never take it"
    );
    assert!(second.lock_linux().signals.contains(LinuxSignal::SIGTERM));
}

/// The ordinary case is left as it was: the first thread that can take it.
#[test]
fn with_nobody_dying_the_first_thread_takes_it() {
    let (proc, first, second) = a_process(48_202);

    send_signal_to_process(proc.id() as usize, LinuxSignal::SIGTERM).unwrap();

    assert!(first.lock_linux().signals.contains(LinuxSignal::SIGTERM));
    assert!(!second.lock_linux().signals.contains(LinuxSignal::SIGTERM));
}
