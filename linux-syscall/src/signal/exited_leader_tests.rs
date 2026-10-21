//! A leader that has called `exit(2)` while another thread of its process
//! runs on is still a task on Linux 6.18: `tgkill(pid, pid, sig)` finds it
//! and answers 0, and the signal, `SIGKILL` included, does nothing. Here it
//! was `ESRCH`, as if the pid named nobody. A signal at the process still
//! goes to the threads it has.
//!
//! Fixed pids live in 45_36x here.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{exited_leader_of, LinuxProcess, ProcessExt};
use linux_object::thread::{CurrentThreadExt, ThreadExt};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const SIGKILL: usize = 9;
const SIGUSR1: usize = 10;
const SIGTERM: usize = 15;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "lider", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

async fn by_number(sys: &Syscall<'_>, num: Sys, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num as u32, args).await
}

/// `proc`'s leader starts a worker and calls `exit(2)`; the worker, which
/// never runs, is returned once the leader is out of the thread list.
fn the_leader_exits_first(proc: &Arc<Process>) -> Arc<Thread> {
    let worker = on_a_thread(proc, |sys| {
        Box::pin(async move {
            assert_eq!(sys.thread.id(), sys.thread.proc().id(), "the leader");
            let worker = Thread::create_linux(&sys.thread.proc()).unwrap();
            sys.thread.exit_linux(3);
            worker
        })
    });
    // Its thread leaves the list when its future is dropped, just after it
    // has answered.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while proc.thread_ids() != [worker.id()] {
        assert!(
            std::time::Instant::now() < deadline,
            "{:?}",
            proc.thread_ids()
        );
        std::thread::yield_now();
    }
    worker
}

#[test]
fn an_exited_leader_is_kept_and_takes_a_signal_that_does_nothing() {
    let proc = a_process(45_361);
    let worker = the_leader_exits_first(&proc);
    let pid = proc.id() as usize;
    assert_eq!(exited_leader_of(&proc).map(|t| t.id()), Some(proc.id()));

    let got = on_a_thread(&a_process(45_362), move |sys| {
        Box::pin(async move {
            [
                by_number(sys, Sys::TGKILL, [pid, pid, 0, 0, 0, 0]).await,
                by_number(sys, Sys::TGKILL, [pid, pid, SIGUSR1, 0, 0, 0]).await,
                by_number(sys, Sys::TGKILL, [pid, pid, SIGKILL, 0, 0, 0]).await,
                by_number(sys, Sys::TKILL, [pid, SIGTERM, 0, 0, 0, 0]).await,
                by_number(sys, Sys::KILL, [pid, 0, 0, 0, 0, 0]).await,
                // Only the leader: a tid that is nobody is nobody.
                by_number(sys, Sys::TGKILL, [pid, pid + 1_000, 0, 0, 0, 0]).await,
            ]
        })
    });
    assert_eq!(got, [0, 0, 0, 0, 0, -(LxError::ESRCH as isize)]);
    assert!(
        !matches!(proc.status(), Status::Exited(_)),
        "the SIGKILL at the leader killed the process"
    );
    assert!(
        worker.lock_linux().signals.is_empty(),
        "a signal reached the worker"
    );
    assert!(proc.linux().shared_pending().is_empty());

    proc.exit(0);
    worker.terminate_abandoned();
}

/// Only the leader is kept: a worker that exits is gone, and a leader that
/// is the last thread takes its process with it.
#[test]
fn a_worker_or_a_last_thread_is_not_kept() {
    let proc = a_process(45_363);
    Thread::create_linux(&proc).unwrap();
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            assert_ne!(sys.thread.id(), sys.thread.proc().id(), "a worker");
            sys.thread.exit_linux(0);
        })
    });
    assert!(exited_leader_of(&proc).is_none());
    proc.exit(0);

    let alone = a_process(45_364);
    on_a_thread(&alone, |sys| {
        Box::pin(async move {
            sys.thread.exit_linux(0);
        })
    });
    assert!(exited_leader_of(&alone).is_none());
    assert!(matches!(alone.status(), Status::Exited(_)));
}
