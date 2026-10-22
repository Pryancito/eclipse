//! A thread counts the faults it resolved, and a process's count is its
//! threads', the exited ones' included.

use super::*;
use crate::task::Job;

#[test]
fn a_thread_counts_its_own_faults() {
    let proc = Process::create(&Job::root(), "faults").unwrap();
    let (one, other) = (
        Thread::create(&proc, "one").unwrap(),
        Thread::create(&proc, "other").unwrap(),
    );
    for _ in 0..3 {
        one.note_minor_fault();
    }
    other.note_minor_fault();
    assert_eq!((one.minor_faults(), other.minor_faults()), (3, 1));
}

#[test]
fn a_process_counts_its_threads_faults_and_its_gone_ones() {
    let proc = Process::create(&Job::root(), "faults").unwrap();
    let (running, gone) = (
        Thread::create(&proc, "running").unwrap(),
        Thread::create(&proc, "gone").unwrap(),
    );
    running.note_minor_fault();
    for _ in 0..4 {
        gone.note_minor_fault();
    }

    assert_eq!(proc.minor_faults(), 5);
    gone.terminate_abandoned();
    assert_eq!(
        proc.minor_faults(),
        5,
        "a thread's faults moved when it left"
    );
    running.note_minor_fault();
    running.terminate_abandoned();
    assert_eq!(proc.minor_faults(), 6);
}
