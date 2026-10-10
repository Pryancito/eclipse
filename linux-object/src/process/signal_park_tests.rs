//! A process-directed signal that every thread blocks lands on
//! `shared_pending` (pulled by the thread that can take it); and `pause`,
//! `sigsuspend` and `sigtimedwait` wake via `SignalPark` instead of polling.
extern crate std;

use super::*;
use crate::signal::{SignalAction, SignalActionFlags};
use crate::thread::ThreadExt;
use core::time::Duration;
use rcore_fs_ramfs::RamFS;

fn usr1() -> Sigset {
    Sigset::new(1 << (LinuxSignal::SIGUSR1 as u64 - 1))
}

/// A process with two threads, both blocking SIGUSR1.
fn two_blocking_threads(pid: KoID) -> (Arc<Process>, Arc<Thread>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "two",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let first = Thread::create_linux(&proc).unwrap();
    let second = Thread::create_linux(&proc).unwrap();
    first.lock_linux().set_signal_mask(usr1());
    second.lock_linux().set_signal_mask(usr1());
    (proc, first, second)
}

fn usr1_later(pid: KoID, after: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        let _ = send_signal_to_process(pid as usize, LinuxSignal::SIGUSR1);
    });
}

#[test]
fn a_process_signal_lands_on_the_thread_waiting_for_it_though_blocked() {
    let (proc, first, second) = two_blocking_threads(43_411);
    second.lock_linux().sigwait = usr1();
    send_signal_to_process(proc.id() as usize, LinuxSignal::SIGUSR1).unwrap();
    assert!(
        second.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the sigtimedwait thread never got the signal"
    );
    assert!(
        !first.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the signal was queued to the first thread as well"
    );
}

#[test]
fn with_nobody_waiting_a_blocked_signal_stays_on_shared_pending() {
    let (proc, first, second) = two_blocking_threads(43_412);
    send_signal_to_process(proc.id() as usize, LinuxSignal::SIGUSR1).unwrap();
    // Process-directed and every thread blocked: one bit on shared_pending,
    // not a private copy on the first tid (that doubled delivery on pull).
    assert!(
        proc.linux()
            .shared_pending()
            .contains(LinuxSignal::SIGUSR1),
        "blocked process signal never reached shared_pending"
    );
    assert!(!first.lock_linux().signals.contains(LinuxSignal::SIGUSR1));
    assert!(!second.lock_linux().signals.contains(LinuxSignal::SIGUSR1));
}

#[test]
fn a_blocked_signal_still_ends_the_park() {
    // What `sigtimedwait` relies on: the caller has the set blocked (and
    // names it in `sigwait`), and the park must wake on the queueing, not
    // on the deadline.
    let (proc, first, _second) = two_blocking_threads(43_413);
    first.lock_linux().sigwait = usr1();
    let mut park = SignalPark::new(&first);
    park.prepare();
    usr1_later(proc.id(), Duration::from_millis(50));
    let start = std::time::Instant::now();
    let deadline = kernel_hal::timer::timer_now() + Duration::from_secs(3);
    async_std::task::block_on(park.park(Some(deadline)));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "parked {:?}: the blocked signal did not wake it",
        start.elapsed()
    );
    // With `sigwait` set the send path delivers straight to this thread
    // (`wants_signal`); without it the bit would sit on shared_pending.
    assert!(first.lock_linux().signals.contains(LinuxSignal::SIGUSR1));
}

#[test]
fn pause_returns_eintr_the_moment_a_caught_signal_arrives() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        43_414,
        "paused",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    proc.linux().set_signal_action(
        LinuxSignal::SIGUSR1,
        SignalAction {
            handler: 0x1000,
            flags: SignalActionFlags::empty(),
            restorer: 0,
            mask: Sigset::default(),
        },
    );
    usr1_later(proc.id(), Duration::from_millis(50));
    let start = std::time::Instant::now();
    let e = async_std::task::block_on(wait_for_signal(&thread));
    assert_eq!(e, LxError::EINTR);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "woke after {:?}",
        start.elapsed()
    );
    assert!(
        thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the signal must still be pending for delivery"
    );
}
