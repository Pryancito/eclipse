//! `nanosleep` slept the whole way and looked for a signal only when it
//! woke: a handler installed for `SIGTERM` waited out the entire
//! `sleep(60)`, and `alarm(1)` never cut a `sleep(10)` short.
extern crate std;

use super::*;
use crate::signal::{SignalAction, SignalActionFlags, SIG_IGN};
use crate::thread::ThreadExt;
use core::time::Duration;
use rcore_fs_ramfs::RamFS;

fn a_sleeper(pid: KoID) -> (Arc<Process>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "sleeper",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

fn usr1_action(proc: &Arc<Process>, handler: usize) {
    proc.linux().set_signal_action(
        LinuxSignal::SIGUSR1,
        SignalAction {
            handler,
            flags: SignalActionFlags::empty(),
            restorer: 0,
            mask: Sigset::default(),
        },
    );
}

/// SIGUSR1 at `pid`, from another host thread, `after` from now.
fn usr1_later(pid: KoID, after: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        let _ = send_signal_to_process(pid as usize, LinuxSignal::SIGUSR1);
    });
}

fn sleep_for(thread: &Arc<Thread>, length: Duration) -> (LxResult<()>, Duration) {
    let start = std::time::Instant::now();
    let deadline = kernel_hal::timer::timer_now() + length;
    let r = async_std::task::block_on(interruptible_sleep_until(thread, deadline));
    (r, start.elapsed())
}

#[test]
fn a_caught_signal_ends_the_sleep_at_once_with_eintr() {
    let (proc, thread) = a_sleeper(43_401);
    usr1_action(&proc, 0x1000);
    usr1_later(proc.id(), Duration::from_millis(50));
    let (r, took) = sleep_for(&thread, Duration::from_secs(3));
    assert_eq!(r, Err(LxError::EINTR));
    assert!(
        took < Duration::from_secs(1),
        "slept {:?} past the signal",
        took
    );
    assert!(
        thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the signal that woke the sleep must still be pending for delivery"
    );
}

#[test]
fn a_signal_already_pending_never_starts_the_sleep() {
    let (proc, thread) = a_sleeper(43_402);
    usr1_action(&proc, 0x1000);
    send_signal_to_process(proc.id() as usize, LinuxSignal::SIGUSR1).unwrap();
    let (r, took) = sleep_for(&thread, Duration::from_secs(3));
    assert_eq!(r, Err(LxError::EINTR));
    assert!(took < Duration::from_millis(500), "slept {:?}", took);
}

#[test]
fn with_no_signal_the_sleep_lasts_until_its_deadline() {
    let (_proc, thread) = a_sleeper(43_403);
    let (r, took) = sleep_for(&thread, Duration::from_millis(150));
    assert_eq!(r, Ok(()));
    assert!(
        took >= Duration::from_millis(150),
        "woke early after {:?}",
        took
    );
}

#[test]
fn an_ignored_or_blocked_signal_does_not_end_the_sleep() {
    let (proc, thread) = a_sleeper(43_404);
    usr1_action(&proc, SIG_IGN);
    usr1_later(proc.id(), Duration::from_millis(30));
    let (r, took) = sleep_for(&thread, Duration::from_millis(250));
    assert_eq!(r, Ok(()), "an ignored signal interrupted the sleep");
    assert!(
        took >= Duration::from_millis(250),
        "woke early after {:?}",
        took
    );

    usr1_action(&proc, 0x1000);
    thread
        .lock_linux()
        .set_signal_mask(Sigset::new(1 << (LinuxSignal::SIGUSR1 as u64 - 1)));
    usr1_later(proc.id(), Duration::from_millis(30));
    let (r, took) = sleep_for(&thread, Duration::from_millis(250));
    assert_eq!(r, Ok(()), "a blocked signal interrupted the sleep");
    assert!(
        took >= Duration::from_millis(250),
        "woke early after {:?}",
        took
    );
}
