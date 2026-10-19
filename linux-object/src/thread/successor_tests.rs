//! The thread an `execve` from a non-leader thread runs its new image in.
//!
//! Linux's `de_thread()` hands the exec'ing thread the leader's pid, so the
//! new image starts with `gettid() == getpid()`. A tid here is the thread
//! object's id, which nothing changes, so [`ThreadExt::successor_as_leader`]
//! makes a new thread with the pid as its id and moves the exec'ing thread's
//! state across. Without it the new image ran with the old worker's tid:
//! winit's `is_main_thread()` panics on that, and `tgkill(pid, pid)` found no
//! thread (signal-probe, section `wait`).

use super::*;
use crate::process::LinuxProcess;
use crate::signal::Signal;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::ROOT_JOB;

fn bit(sig: Signal) -> Sigset {
    Sigset::new(1 << (sig as u64 - 1))
}

/// A process with its leader and one more thread, the one that `execve`s.
fn leader_and_worker(pid: KoID) -> (Arc<Process>, Arc<Thread>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "exec",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let leader = Thread::create_linux(&proc).unwrap();
    let worker = Thread::create_linux(&proc).unwrap();
    assert_eq!(leader.id(), pid, "the premise: the leader's tid is the pid");
    (proc, leader, worker)
}

/// What `de_thread` leaves: the worker alone in the list.
fn the_leader_gone(leader: &Arc<Thread>) {
    leader.terminate_abandoned();
}

#[test]
fn the_new_image_runs_with_the_pid_as_its_tid() {
    let (proc, leader, worker) = leader_and_worker(48_101);
    the_leader_gone(&leader);

    let successor = worker.successor_as_leader().unwrap();

    assert_eq!(successor.id(), proc.id());
    let mut ids = proc.thread_ids();
    ids.sort_unstable();
    let mut expected = alloc::vec![worker.id(), proc.id()];
    expected.sort_unstable();
    assert_eq!(ids, expected, "the worker is still listed until it exits");
}

/// execve(2): the mask and the pending signals are kept across an exec, and
/// so are the name the thread was given, its timer slack and its I/O
/// priority. They belong to the thread that called it, which is now the
/// new one.
#[test]
fn what_the_thread_carried_goes_across() {
    let (_proc, leader, worker) = leader_and_worker(48_102);
    {
        let mut lt = worker.lock_linux();
        lt.set_signal_mask(bit(Signal::SIGUSR1));
        lt.queue_signal(Signal::SIGUSR2, None);
        lt.comm = "worker".into();
        lt.timerslack_ns = 123_456;
    }
    the_leader_gone(&leader);

    let successor = worker.successor_as_leader().unwrap();

    let lt = successor.lock_linux();
    assert_eq!(lt.signal_mask().val(), bit(Signal::SIGUSR1).val());
    assert!(
        lt.signals.contains(Signal::SIGUSR2),
        "a pending signal was lost"
    );
    assert_eq!(lt.comm, "worker");
    assert_eq!(lt.timerslack_ns, 123_456);
    drop(lt);
    assert!(
        successor.has_deliverable_signal(),
        "the pending SIGUSR2 is unblocked, and the run loop's filter must say so"
    );
    let left = worker.lock_linux();
    assert!(
        left.signals.is_empty(),
        "the pending signal is the new thread's alone"
    );
    assert!(left.signal_mask().is_empty());
}

/// `sched_setaffinity` and `sched_setscheduler` are per thread, and the
/// thread that exec'd keeps its own.
#[test]
fn so_do_its_cpus_and_its_scheduling() {
    let (_proc, leader, worker) = leader_and_worker(48_103);
    worker.set_affinity(0b10).unwrap();
    worker.set_sched(0, 5, 0);
    the_leader_gone(&leader);

    let successor = worker.successor_as_leader().unwrap();

    assert_eq!(successor.affinity(), 0b10);
    assert_eq!(successor.sched_nice(), 5);
}

/// While the old leader is still listed the pid is still its tid: a second
/// thread with it is two threads with one tid, and taking one out of the list
/// by id takes out both. Nothing is moved, so the image can run on in the
/// thread that exec'd.
#[test]
fn not_while_the_old_leader_is_still_there() {
    let (proc, _leader, worker) = leader_and_worker(48_104);
    worker.lock_linux().set_signal_mask(bit(Signal::SIGUSR1));

    assert_eq!(worker.successor_as_leader().err(), Some(ZxError::BAD_STATE));
    assert_eq!(proc.thread_ids().len(), 2, "no thread was made");
    assert_eq!(
        worker.lock_linux().signal_mask().val(),
        bit(Signal::SIGUSR1).val(),
        "a refusal moved the thread's state anyway"
    );
}
