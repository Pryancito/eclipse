//! `nanosleep(2)` took no `rem` at all, and its sleep could not be cut
//! short by a signal: a signal that arrived in the middle was seen at
//! the deadline, and a loop that restarts on `EINTR` with `rem`
//! restarted from whatever was in the buffer.
extern crate std;

use super::*;
use core::time::Duration;
use linux_object::process::send_signal_to_process;
use linux_object::signal::{Signal as LinuxSignal, SignalAction, SignalActionFlags, Sigset};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

// `libos` addresses are ordinary host addresses, so a local is the
// caller's buffer.
fn user_out<T>(slot: &mut T) -> UserOutPtr<T> {
    UserOutPtr::from(slot as *mut T as usize)
}

#[test]
fn what_is_left_is_the_deadline_minus_now_and_never_negative() {
    let rem = nanosleep_remaining(Duration::from_millis(2_750), Duration::from_millis(1_000));
    assert_eq!((rem.sec, rem.nsec), (1, 750_000_000));
    let rem = nanosleep_remaining(Duration::from_secs(1), Duration::from_secs(5));
    assert_eq!((rem.sec, rem.nsec), (0, 0));
}

#[test]
fn a_signal_ends_the_sleep_and_leaves_the_rest_in_rem() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        43_501,
        "sleeper",
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
    let pid = proc.id() as usize;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let _ = send_signal_to_process(pid, LinuxSignal::SIGUSR1);
    });
    let mut slot = TimeSpec { sec: 99, nsec: 99 };
    let deadline = kernel_hal::timer::timer_now() + Duration::from_secs(3);
    let start = std::time::Instant::now();
    let r = async_std::task::block_on(sleep_or_eintr(
        &thread,
        deadline,
        user_out(&mut slot),
        false,
    ));
    assert_eq!(r, Err(LxError::EINTR));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "slept past the signal"
    );
    assert_eq!(
        slot.sec, 2,
        "rem {:?} is not what was left of the 3 s",
        slot
    );
    assert!(
        slot.nsec > 500_000_000 && slot.nsec < 1_000_000_000,
        "rem {:?}",
        slot
    );
}

/// And the remainder at the other width: to a 32-bit caller a `struct
/// timespec` is eight bytes, so the two words past it are nobody's to write.
/// The native path wrote sixteen whatever the caller was, which is four
/// bytes of seconds, four of zero, and eight bytes of the next variable
/// along.
#[test]
fn the_remainder_of_a_32_bit_sleeper_is_eight_bytes_and_not_sixteen() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        43_502,
        "sleeper32",
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
    let pid = proc.id() as usize;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let _ = send_signal_to_process(pid, LinuxSignal::SIGUSR1);
    });
    // Four words of poison, so that "wrote eight bytes" and "wrote sixteen"
    // are told apart by what is still poison afterwards.
    let mut slot = [0xaaaa_aaaau32; 4];
    let deadline = kernel_hal::timer::timer_now() + Duration::from_secs(3);
    let r = async_std::task::block_on(sleep_or_eintr(
        &thread,
        deadline,
        UserOutPtr::from(slot.as_mut_ptr() as usize),
        true,
    ));
    assert_eq!(r, Err(LxError::EINTR));
    assert_eq!(slot[0], 2, "the seconds, in the first word");
    assert!(
        slot[1] > 500_000_000 && slot[1] < 1_000_000_000,
        "the nanoseconds, in the second word: {}",
        slot[1]
    );
    assert_eq!(slot[2], 0xaaaa_aaaa, "the third word is not ours to write");
    assert_eq!(slot[3], 0xaaaa_aaaa, "nor the fourth");
}
