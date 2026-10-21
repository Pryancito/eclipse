//! A timer's signal is found where it waits, on the thread it went to or
//! on the process, and only while it waits; a signal of the same number
//! from anybody else is not the timer's.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

/// Timer 7's `SIGALRM`, with `overrun` expiries in it, sent to `proc`.
fn timer_7_fires(proc: &Process, overrun: i32) {
    let info = SigInfo::timer(LinuxSignal::SIGALRM, 7, overrun, 0);
    send_signal_to_process_with_info(proc.id() as usize, LinuxSignal::SIGALRM, Some(info)).unwrap();
}

/// Look for timer `timerid`'s `SIGALRM` on `proc`, and make its overrun 5.
fn restamp(proc: &Process, tid: Option<&Thread>, timerid: i32) -> bool {
    with_pending_timer_signal(
        proc.id() as usize,
        tid.map(|t| t.id() as usize),
        LinuxSignal::SIGALRM,
        timerid,
        |info| info.set_timer_overrun(5),
    )
}

#[test]
fn on_the_thread_it_went_to_it_is_found_and_changed_in_place() {
    let (proc, thread) = a_process(44_311);
    timer_7_fires(&proc, 0);
    assert!(restamp(&proc, None, 7), "not found on the thread");
    let info = thread.lock_linux().take_siginfo(LinuxSignal::SIGALRM);
    assert_eq!(info.timer_id(), Some(7));
    assert_eq!(info.timer_overrun(), 5, "not changed in place");
    let b = info.as_bytes();
    assert_eq!(
        i32::from_ne_bytes([b[20], b[21], b[22], b[23]]),
        5,
        "not where glibc reads si_overrun"
    );
}

#[test]
fn on_the_process_while_every_thread_blocks_it_it_is_found_there() {
    let (proc, thread) = a_process(44_312);
    let mut blocked = Sigset::empty();
    blocked.insert(LinuxSignal::SIGALRM);
    thread.lock_linux().block_signals(&blocked);
    timer_7_fires(&proc, 0);
    assert!(
        !thread.lock_linux().signals.contains(LinuxSignal::SIGALRM),
        "the test needs it on the process"
    );
    assert!(restamp(&proc, None, 7), "not found on the process");
    thread.lock_linux().set_signal_mask(Sigset::empty());
    proc.linux().pull_shared_pending_into(&thread);
    let info = thread.lock_linux().take_siginfo(LinuxSignal::SIGALRM);
    assert_eq!(info.timer_overrun(), 5, "not changed where it waited");
}

#[test]
fn another_timer_or_sender_of_the_same_number_is_left_alone() {
    let (proc, thread) = a_process(44_313);
    timer_7_fires(&proc, 2);
    assert!(!restamp(&proc, None, 8), "timer 8 took timer 7's signal");
    assert_eq!(
        thread
            .lock_linux()
            .take_siginfo(LinuxSignal::SIGALRM)
            .timer_overrun(),
        2
    );
    let info = SigInfo::from_kernel(LinuxSignal::SIGALRM);
    send_signal_to_process_with_info(proc.id() as usize, LinuxSignal::SIGALRM, Some(info)).unwrap();
    assert!(!restamp(&proc, None, 0), "an alarm(2) is no timer's");
    assert!(
        thread.lock_linux().take_siginfo(LinuxSignal::SIGALRM)
            == SigInfo::from_kernel(LinuxSignal::SIGALRM),
        "the alarm's siginfo_t was changed"
    );
}

#[test]
fn once_taken_it_is_not_waiting_any_more() {
    let (proc, thread) = a_process(44_314);
    timer_7_fires(&proc, 0);
    thread.lock_linux().take_siginfo(LinuxSignal::SIGALRM);
    assert!(!restamp(&proc, None, 7), "found after it was taken");
    // `handle_signal` takes the bit before the `siginfo_t` is read.
    timer_7_fires(&proc, 0);
    thread.lock_linux().signals.remove(LinuxSignal::SIGALRM);
    assert!(!restamp(&proc, None, 7), "found with its bit gone");
}

#[test]
fn a_thread_directed_one_is_looked_for_on_its_thread_only() {
    let (proc, first) = a_process(44_315);
    let second = Thread::create_linux(&proc).unwrap();
    let info = SigInfo::timer(LinuxSignal::SIGALRM, 7, 0, 0);
    second
        .lock_linux()
        .queue_signal(LinuxSignal::SIGALRM, Some(info));
    assert!(
        !restamp(&proc, Some(&first), 7),
        "found on the wrong thread"
    );
    assert!(restamp(&proc, Some(&second), 7), "not found on its thread");
    assert_eq!(
        second
            .lock_linux()
            .take_siginfo(LinuxSignal::SIGALRM)
            .timer_overrun(),
        5
    );
}
