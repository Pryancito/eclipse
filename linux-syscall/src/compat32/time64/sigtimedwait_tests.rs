//! `rt_sigtimedwait_time64` (421) next to i386's plain `rt_sigtimedwait`
//! (177).
//!
//! The THIRD call that is in both of this layer's rules at once, after
//! `utimensat_time64` and `pselect6` --- and the first where the two
//! structures travel in opposite directions:
//!
//! | what it passes | whose width |
//! |---|---|
//! | the timeout, a `struct timespec`, inwards | the NUMBER's: 8 bytes by 177, 16 by 421 |
//! | the `siginfo_t`, outwards | the PROCESS's: a `compat_siginfo` by either number |
//!
//! The second row is not a halving at all. A `siginfo_t` is 128 bytes at both
//! widths; what moves is where its union STARTS, byte 12 against byte 16,
//! because i386 aligns it to four and there is no padding after `si_code`. So
//! a wrong conversion here is never a short write and never faults: it is 128
//! bytes of plausible rubbish, with `si_uid` holding the sender's pid.
//!
//! Each test makes the same call from the same 32-bit process by each number,
//! so neither "always narrow" nor "always wide" passes --- and the siginfo
//! tests also make it from a NATIVE process, because a conversion that is
//! always applied passes every test that only ever asks a 32-bit one.
//!
//! Fixed pids live in 48_51x.

use super::super::*;
use super::tests::{as_mut, get, put, word};
use crate::on_a_thread::on_a_thread;
use crate::signal::queue_signal_to_thread;
use alloc::boxed::Box;
use linux_object::error::LxError;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::{SigInfo, Signal};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's two numbers for the same call. 177 is the number at both widths.
const I386_RT_SIGTIMEDWAIT: u32 = 177;
const I386_RT_SIGTIMEDWAIT_TIME64: u32 = 421;

const EINVAL: isize = -(LxError::EINVAL as isize);

const USER_AT: usize = 0x2600_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;
/// The waited-for set. An i386 `sigset_t` is `unsigned long sig[2]`, which
/// aligns to 4, so this one is 4 mod 8 as a 32-bit program's would be.
const SET_AT: usize = USER_AT + 0x104;
/// The `siginfo_t` the call writes. 128 bytes, 4-aligned on i386.
const INFO_AT: usize = USER_AT + 0x204;
/// The timeout, 4-aligned too, because on i386 an `__s64` aligns to 4.
const TS_AT: usize = USER_AT + 0x404;
/// The same two, 8-ALIGNED, for the native test. Natively a `sigset_t` and a
/// `siginfo_t` really are 8-aligned, so the typed read and write are right to
/// insist; it is only a 32-bit program whose own are 4-aligned. Using the
/// 4-aligned addresses there would make the native test fail for the
/// alignment and tell nothing about the conversion.
const SET_AT_WIDE: usize = USER_AT + 0x700;
const INFO_AT_WIDE: usize = USER_AT + 0x800;

/// The size `check_sigsetsize` insists on: `sizeof(sigset_t)`, eight in both
/// ABIs.
const SIGSETSIZE: usize = 8;

/// The signal every test waits for: a real-time one, so that `SigInfo::queued`
/// gives it the `_rt` layout, which is the one whose union holds a POINTER.
const WAITED: Signal = Signal::SIGRT34;

/// What the sender claims to be, chosen so that each lands in a different
/// word and a four-byte slip is visible.
const SENDER_PID: i32 = 0x1111_2222;
const SENDER_UID: u32 = 0x3333_4444;
const SENDER_VALUE: usize = 0x5555_6666;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "sigtimedwait", lp).unwrap();
    proc.vmar()
        .map_at(
            USER_AT,
            VmObject::new_paged(USER_PAGES),
            0,
            USER_PAGES * PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .expect("pages of the process's own, at an address a u32 can hold");
    proc
}

/// A `sigset_t` with only [`WAITED`] in it, eight bytes at both widths.
fn put_waited_set(at: usize) {
    put(at, &(1u64 << (WAITED as u64 - 1)).to_ne_bytes());
}

/// Lay a `struct __kernel_timespec` down at `at`: two 64-bit words.
fn put_wide_timespec(at: usize, sec: u64, nsec: u64) {
    let mut bytes = alloc::vec::Vec::new();
    bytes.extend_from_slice(&sec.to_ne_bytes());
    bytes.extend_from_slice(&nsec.to_ne_bytes());
    put(at, &bytes);
}

/// Poison the 128 bytes the call is to write, so that a byte it leaves alone
/// is readable as one it left alone.
fn poison_info(at: usize) {
    put(at, &[0xAAu8; 128]);
}

/// Queue [`WAITED`] to the calling thread so that the wait has something to
/// dequeue and comes back at once: what every test here measures is the
/// CONVERSION, not the waiting.
fn queue_to_self(sys: &Syscall<'_>) {
    let thread = alloc::sync::Arc::clone(sys.thread);
    queue_signal_to_thread(
        &thread,
        WAITED,
        SigInfo::queued(WAITED, SENDER_PID, SENDER_UID, SENDER_VALUE),
    )
    .expect("the thread's own pending set has room");
}

/// The timeout is the NUMBER's.
///
/// The same bytes --- `{ 0, 2_000_000_000 }` as a `struct __kernel_timespec`
/// --- are an out-of-range nanosecond count read wide, and read narrow they
/// are the two LOW halves of those words, so `{ 0, 0 }`: a timeout of zero,
/// which with the signal already pending is served. So one number answers
/// `EINVAL` and the other the signal's number, and no single width passes.
#[test]
fn the_two_numbers_read_the_timeout_at_different_widths() {
    let proc = a_process(48_510, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_waited_set(SET_AT);
            put_wide_timespec(TS_AT, 0, 2_000_000_000);
            queue_to_self(&sys);
            let n = sys
                .compat32_syscall(
                    I386_RT_SIGTIMEDWAIT,
                    [SET_AT, INFO_AT, TS_AT, SIGSETSIZE, 0, 0],
                )
                .await;
            let w = sys
                .compat32_syscall(
                    I386_RT_SIGTIMEDWAIT_TIME64,
                    [SET_AT, INFO_AT, TS_AT, SIGSETSIZE, 0, 0],
                )
                .await;
            (n, w)
        })
    });
    assert_eq!(
        narrow, WAITED as isize,
        "177 reads eight bytes, so the nanoseconds are the low half of the \
         first word and the timeout is zero: the pending signal is served"
    );
    assert_eq!(
        wide, EINVAL,
        "421 reads sixteen, so the nanosecond count is two thousand million \
         and out of range"
    );
    proc.exit(0);
}

/// **The `siginfo_t` is a `compat_siginfo` by either number.**
///
/// Its union begins at byte 12 for a 32-bit process and at byte 16 natively,
/// so `si_pid` is the word at 12 here. Written natively, byte 12 would hold
/// the four bytes of padding the native struct keeps after `si_code` and
/// `si_uid` would come out as the sender's pid: the shape of mistake that
/// looks like data.
#[test]
fn the_siginfo_is_the_32_bit_one_by_either_number() {
    let proc = a_process(48_511, Abi::Linux32);
    let (by_177, by_421) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_waited_set(SET_AT);
            put_wide_timespec(TS_AT, 0, 0);
            queue_to_self(&sys);
            poison_info(INFO_AT);
            let r = sys
                .compat32_syscall(
                    I386_RT_SIGTIMEDWAIT,
                    [SET_AT, INFO_AT, TS_AT, SIGSETSIZE, 0, 0],
                )
                .await;
            let a = (r, get(INFO_AT, 32));
            queue_to_self(&sys);
            poison_info(INFO_AT);
            let r = sys
                .compat32_syscall(
                    I386_RT_SIGTIMEDWAIT_TIME64,
                    [SET_AT, INFO_AT, TS_AT, SIGSETSIZE, 0, 0],
                )
                .await;
            (a, (r, get(INFO_AT, 32)))
        })
    });
    for (what, (ret, bytes)) in [("177", by_177), ("421", by_421)] {
        assert_eq!(ret, WAITED as isize, "{what} served the pending signal");
        assert_eq!(word(&bytes, 0), WAITED as u32, "{what}: si_signo");
        assert_eq!(
            word(&bytes, 12),
            SENDER_PID as u32,
            "{what}: si_pid at byte 12, where a 4-aligned union begins --- \
             not at 16: {:x?}",
            &bytes[..24]
        );
        assert_eq!(word(&bytes, 16), SENDER_UID, "{what}: si_uid");
        assert_eq!(
            word(&bytes, 20),
            SENDER_VALUE as u32,
            "{what}: si_value, the low half of the pointer"
        );
    }
    proc.exit(0);
}

/// And the NATIVE process still gets the native one, which is the half the
/// test above cannot see.
///
/// Without this, writing the 32-bit bytes unconditionally would pass
/// everything else in this file: the conversion has to be asked for, and
/// asked of the PROCESS. Natively `si_pid` is at byte 16 and byte 12 is the
/// padding the kernel zeroes.
#[test]
fn a_native_process_still_gets_the_native_siginfo() {
    let proc = a_process(48_512, Abi::Linux);
    let (ret, bytes) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_waited_set(SET_AT_WIDE);
            put_wide_timespec(TS_AT, 0, 0);
            queue_to_self(&sys);
            poison_info(INFO_AT_WIDE);
            let r = sys
                .sys_rt_sigtimedwait(
                    SET_AT_WIDE.into(),
                    INFO_AT_WIDE.into(),
                    TS_AT.into(),
                    SIGSETSIZE,
                )
                .await;
            (r, get(INFO_AT_WIDE, 32))
        })
    });
    assert_eq!(ret, Ok(WAITED as usize), "the pending signal is served");
    assert_eq!(word(&bytes, 0), WAITED as u32, "si_signo");
    assert_eq!(
        word(&bytes, 12),
        0,
        "byte 12 is the native struct's padding, which the kernel zeroes: \
         {:x?}",
        &bytes[..24]
    );
    assert_eq!(
        word(&bytes, 16),
        SENDER_PID as u32,
        "si_pid, at byte 16 natively"
    );
    proc.exit(0);
}

/// A null `siginfo_t` writes nothing at either width: `sigwait(3)` passes one.
#[test]
fn a_null_siginfo_is_not_written_at_either_width() {
    let proc = a_process(48_513, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_waited_set(SET_AT);
            put_wide_timespec(TS_AT, 0, 0);
            poison_info(INFO_AT);
            queue_to_self(&sys);
            let n = sys
                .compat32_syscall(I386_RT_SIGTIMEDWAIT, [SET_AT, 0, TS_AT, SIGSETSIZE, 0, 0])
                .await;
            queue_to_self(&sys);
            let w = sys
                .compat32_syscall(
                    I386_RT_SIGTIMEDWAIT_TIME64,
                    [SET_AT, 0, TS_AT, SIGSETSIZE, 0, 0],
                )
                .await;
            ((n, w), get(INFO_AT, 16))
        })
    });
    assert_eq!(narrow.0, WAITED as isize, "177 with a null siginfo");
    assert_eq!(narrow.1, WAITED as isize, "421 with a null siginfo");
    assert_eq!(
        wide, [0xAAu8; 16],
        "nothing was written where the siginfo would have gone"
    );
    proc.exit(0);
}

/// The `sigsetsize` is still checked by either number, and before the
/// timeout is read: `check_sigsetsize` is the first thing the core does, so a
/// wrong size is `EINVAL` whatever the timeout says.
#[test]
fn a_wrong_sigsetsize_is_refused_by_either_number() {
    let proc = a_process(48_514, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_waited_set(SET_AT);
            put_wide_timespec(TS_AT, 0, 0);
            let n = sys
                .compat32_syscall(I386_RT_SIGTIMEDWAIT, [SET_AT, INFO_AT, TS_AT, 4, 0, 0])
                .await;
            let w = sys
                .compat32_syscall(
                    I386_RT_SIGTIMEDWAIT_TIME64,
                    [SET_AT, INFO_AT, TS_AT, 4, 0, 0],
                )
                .await;
            (n, w)
        })
    });
    assert_eq!(narrow, EINVAL, "177 with a four-byte sigset_t");
    assert_eq!(wide, EINVAL, "421 with a four-byte sigset_t");
    proc.exit(0);
}
