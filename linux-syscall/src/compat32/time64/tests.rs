//! The one family where the width is the call's and not the process's.
//!
//! Each test here makes the SAME call twice from the SAME 32-bit process, by
//! the two numbers, and asserts that the two answers are different widths.
//! That pairing is the whole point: an assertion about 403 alone passes on a
//! kernel that narrows everything, and an assertion about 265 alone passes on
//! one that narrows nothing.
//!
//! Everything goes through `compat32_syscall` by number, so deleting an arm
//! from [`crate::compat32`] fails here rather than leaving a 32-bit program
//! with no clock.
//!
//! Fixed pids live in 48_45x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::error::LxError;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's numbers, spelled here: entering the layer the way a 32-bit program
/// does is the point.
const CLOCK_GETTIME: u32 = 265;
const CLOCK_GETTIME64: u32 = 403;
const CLOCK_SETTIME64: u32 = 404;
const CLOCK_GETRES: u32 = 266;
const CLOCK_GETRES_TIME64: u32 = 406;
const CLOCK_NANOSLEEP_TIME64: u32 = 407;
const TIMERFD_CREATE: u32 = 322;
const TIMERFD_SETTIME: u32 = 325;
const TIMERFD_GETTIME: u32 = 326;
const TIMERFD_GETTIME64: u32 = 410;
const TIMERFD_SETTIME64: u32 = 411;
const FUTEX: u32 = 240;
const FUTEX_TIME64: u32 = 422;

/// `FUTEX_WAIT`, and the flag that keeps the wait inside this process so the
/// shared table is not consulted.
const FUTEX_WAIT: u32 = 0;
const FUTEX_PRIVATE_FLAG: u32 = 128;

const CLOCK_MONOTONIC: usize = 1;
const CLOCK_REALTIME: usize = 0;
/// A clock number nothing implements, for the `EINVAL` the two entries share.
const CLOCK_NONSENSE: usize = 99;

const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);
const ETIMEDOUT: isize = -(LxError::ETIMEDOUT as isize);

const USER_AT: usize = 0x2200_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;
/// Where a `timespec` goes, poisoned so that its width is readable.
const TS_AT: usize = USER_AT;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "time64", lp).unwrap();
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

fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

fn get(at: usize, len: usize) -> Vec<u8> {
    let p: kernel_hal::user::UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped above")
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// A `Syscall` as a `&mut` one, since `compat32_syscall` takes `&mut self`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// How many bytes of the poisoned buffer a call wrote, counted as the words
/// that are no longer `0xaaaaaaaa`.
///
/// A monotonic clock's seconds can legitimately be a small number and its
/// nanoseconds anything, so neither word is reliably non-poison on its own.
/// What IS reliable is the opposite: the words the kernel did not touch are
/// still exactly the poison, and the four words of a 16-byte answer cannot
/// all be 0xaaaaaaaa.
fn poison_left_from(bytes: &[u8], first_word: usize) -> bool {
    (first_word..bytes.len() / 4).all(|w| word(bytes, w * 4) == 0xaaaa_aaaa)
}

#[test]
fn the_two_numbers_answer_the_same_process_at_different_widths() {
    // The pair that is the whole defect. `clock_gettime64` from a 32-bit
    // process must write a `struct __kernel_timespec`: two SIXTY-FOUR-bit
    // words. `clock_gettime` from the same process writes two 32-bit ones.
    // A kernel that asked the process would answer both the same way, and a
    // musl built in the last few years calls ONLY the 64-bit form -- so the
    // program would read its `tv_nsec` out of the top half of its `tv_sec`.
    let proc = a_process(48_450, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(TS_AT, &[0xAAu8; 32]);
            let r1 = sys
                .compat32_syscall(CLOCK_GETTIME, [CLOCK_MONOTONIC, TS_AT, 0, 0, 0, 0])
                .await;
            let narrow = (r1, get(TS_AT, 32));
            put(TS_AT, &[0xAAu8; 32]);
            let r2 = sys
                .compat32_syscall(CLOCK_GETTIME64, [CLOCK_MONOTONIC, TS_AT, 0, 0, 0, 0])
                .await;
            (narrow, (r2, get(TS_AT, 32)))
        })
    });
    assert_eq!(narrow.0, 0, "clock_gettime (265)");
    assert_eq!(wide.0, 0, "clock_gettime64 (403) is not wired");
    assert!(
        poison_left_from(&narrow.1, 2),
        "clock_gettime wrote past the eight bytes a 32-bit timespec is: {:x?}",
        &narrow.1[..16]
    );
    assert!(
        !poison_left_from(&wide.1, 2),
        "clock_gettime64 wrote only eight bytes: the caller's tv_nsec is the \
         top half of its tv_sec. {:x?}",
        &wide.1[..16]
    );
    assert!(
        poison_left_from(&wide.1, 4),
        "clock_gettime64 wrote past sixteen bytes: {:x?}",
        &wide.1
    );
    proc.exit(0);
}

#[test]
fn a_native_process_is_untouched_by_either_number() {
    // 403 is an i386 number and a native process never sends it, but the
    // core is shared, so this is the pair that says the refactor changed
    // nothing for the caller that was already right.
    let proc = a_process(48_451, Abi::Linux);
    let native = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(TS_AT, &[0xAAu8; 32]);
            let r = sys
                .syscall(
                    crate::Sys::CLOCK_GETTIME as u32,
                    [CLOCK_MONOTONIC, TS_AT, 0, 0, 0, 0],
                )
                .await;
            (r, get(TS_AT, 32))
        })
    });
    assert_eq!(native.0, 0);
    assert!(
        !poison_left_from(&native.1, 2),
        "a native clock_gettime stopped writing sixteen bytes"
    );
    assert!(poison_left_from(&native.1, 4));
    proc.exit(0);
}

#[test]
fn clock_getres_at_either_width_from_one_process() {
    // Its answer is `{0, 1}`, whose `tv_sec` is a zero and whose `tv_nsec` is
    // a one: readable at both widths, and the one case where the VALUES say
    // which layout was written rather than the poison.
    let proc = a_process(48_452, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(TS_AT, &[0xAAu8; 32]);
            let a = sys
                .compat32_syscall(CLOCK_GETRES, [CLOCK_MONOTONIC, TS_AT, 0, 0, 0, 0])
                .await;
            let narrow = (a, get(TS_AT, 32));
            put(TS_AT, &[0xAAu8; 32]);
            let b = sys
                .compat32_syscall(CLOCK_GETRES_TIME64, [CLOCK_MONOTONIC, TS_AT, 0, 0, 0, 0])
                .await;
            (narrow, (b, get(TS_AT, 32)))
        })
    });
    assert_eq!((narrow.0, wide.0), (0, 0));
    // Narrow: sec at 0, nsec at 4.
    assert_eq!((word(&narrow.1, 0), word(&narrow.1, 4)), (0, 1));
    assert_eq!(word(&narrow.1, 8), 0xaaaa_aaaa, "and nothing after it");
    // Wide: sec in the first two words, nsec in the next two.
    assert_eq!(
        (
            word(&wide.1, 0),
            word(&wide.1, 4),
            word(&wide.1, 8),
            word(&wide.1, 12)
        ),
        (0, 0, 1, 0),
        "clock_getres_time64 did not write a 64-bit timespec"
    );
    assert_eq!(word(&wide.1, 16), 0xaaaa_aaaa);
    proc.exit(0);
}

#[test]
fn clock_settime64_reads_sixteen_bytes_and_shares_every_check() {
    // The way IN, where a wrong width is not a wrong number but a wrong
    // SECOND field: eight bytes read as sixteen take `tv_nsec` from whatever
    // follows. The checks -- the clock number, and `CAP_SYS_TIME` -- are the
    // native ones, reached through the same core, so a second copy of them
    // cannot drift.
    let proc = a_process(48_453, Abi::Linux32);
    let (nonsense, narrow_reading) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // Sixteen bytes whose WIDE reading is `{1, 2_000_000_000}` --- a
            // `tv_nsec` over a billion, which `settod_time` refuses --- and
            // whose NARROW reading is `{1, 0}`, a perfectly good time. So
            // `EINVAL` here says the sixteen bytes were read as sixteen, and
            // nothing is ever actually set.
            let mut ts = Vec::new();
            ts.extend_from_slice(&1u64.to_ne_bytes());
            ts.extend_from_slice(&2_000_000_000u64.to_ne_bytes());
            put(TS_AT, &ts);
            (
                sys.compat32_syscall(CLOCK_SETTIME64, [CLOCK_NONSENSE, TS_AT, 0, 0, 0, 0])
                    .await,
                sys.compat32_syscall(CLOCK_SETTIME64, [CLOCK_REALTIME, TS_AT, 0, 0, 0, 0])
                    .await,
            )
        })
    });
    assert_eq!(nonsense, EINVAL, "the clock check is not shared");
    assert_eq!(
        narrow_reading, EINVAL,
        "clock_settime64 read eight bytes: the tv_nsec of over a billion was          in the half it never looked at, and the time it would have set is          whatever followed the caller's structure"
    );
    proc.exit(0);
}

#[test]
fn clock_nanosleep_time64_reads_its_request_at_the_wide_width() {
    // A request of zero at EITHER width sleeps for nothing and returns 0, so
    // zero says nothing. What says something is sixteen bytes whose LOW half
    // is zero and whose high half is not: read as two 32-bit words that is a
    // request of "0 seconds, <big> nanoseconds" and `EINVAL` (nsec must be
    // under a billion); read as one 64-bit word it is the `tv_sec` of a
    // sleep, and the `tv_nsec` after it is zero.
    //
    // 0x0000_0001_0000_0000 is chosen so the wide reading is a `tv_sec` so
    // large that `try_into_duration` refuses it -- also EINVAL, but for the
    // opposite reason -- so the discriminating case is the one below.
    let proc = a_process(48_454, Abi::Linux32);
    let (wide_zero, narrow_rubbish) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // { tv_sec = 0, tv_nsec = 0 } at the WIDE width is sixteen zero
            // bytes. At the narrow width those same bytes are also a zero
            // request, so this one only says the call is wired.
            put(TS_AT, &[0u8; 16]);
            let wide_zero = sys
                .compat32_syscall(CLOCK_NANOSLEEP_TIME64, [CLOCK_MONOTONIC, 0, TS_AT, 0, 0, 0])
                .await;
            // Now the discriminating bytes: a 64-bit `tv_sec` of 0 and a
            // 64-bit `tv_nsec` of 2 000 000 000, which is over a billion and
            // so EINVAL at the wide width -- while the SAME bytes read
            // narrow are `tv_sec = 0`, `tv_nsec = 0`, a valid request that
            // returns 0.
            let mut ts = Vec::new();
            ts.extend_from_slice(&0u64.to_ne_bytes());
            ts.extend_from_slice(&2_000_000_000u64.to_ne_bytes());
            put(TS_AT, &ts);
            let narrow_rubbish = sys
                .compat32_syscall(CLOCK_NANOSLEEP_TIME64, [CLOCK_MONOTONIC, 0, TS_AT, 0, 0, 0])
                .await;
            (wide_zero, narrow_rubbish)
        })
    });
    assert_eq!(wide_zero, 0, "number 407 is not wired");
    assert_eq!(
        narrow_rubbish, EINVAL,
        "clock_nanosleep_time64 read its request eight bytes at a time: the \
         tv_nsec of over a billion was in the half it never looked at"
    );
    proc.exit(0);
}

#[test]
fn none_of_the_wired_numbers_are_in_the_translation_table() {
    // A row would send 403 to the native `clock_gettime`, whose handler asks
    // the PROCESS -- and would answer eight bytes where the program waits
    // for sixteen. So the numbers must be answered before the table is
    // asked, which is what the arms in `compat32` do.
    for n in [
        CLOCK_GETTIME64,
        CLOCK_SETTIME64,
        CLOCK_GETRES_TIME64,
        CLOCK_NANOSLEEP_TIME64,
        TIMERFD_GETTIME64,
        TIMERFD_SETTIME64,
        FUTEX_TIME64,
    ] {
        assert_eq!(
            crate::compat32::native_number(n),
            None,
            "i386 {} must not have a row in the allowlist",
            n
        );
    }
}

#[test]
fn the_rest_of_the_family_still_says_so_by_name() {
    // The ones left (see this module's documentation) answer `ENOSYS` with
    // their own name in the log, which is the report that says what to write
    // next. Pinned so that "not implemented" stays visible rather than
    // becoming a silent wrong width.
    let proc = a_process(48_455, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let mut out = Vec::new();
            for n in [408u32, 409, 412, 414, 421, 423] {
                out.push(sys.compat32_syscall(n, [0, 0, 0, 0, 0, 0]).await);
            }
            out
        })
    });
    assert!(got.iter().all(|&r| r == ENOSYS), "{:?}", got);
    assert_eq!(crate::compat32::name(412), Some("utimensat_time64"));
    proc.exit(0);
}

/// A `struct itimerspec` is two `timespec`s, so sixteen bytes narrow and
/// thirty-two wide, and a `timerfd` reports it on the way out.
///
/// The pair is what says it: the SAME armed timer read by 326 and by 410
/// from the SAME process. A kernel that asked the process would answer both
/// alike, and a modern i386 musl only ever sends 410 --- so the program
/// would take its interval out of the top half of its `it_value.tv_sec` and
/// find every timer it ever set to be zero.
#[test]
fn the_timerfd_reports_its_itimerspec_at_the_width_of_the_number() {
    let proc = a_process(48_456, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = sys
                .compat32_syscall(TIMERFD_CREATE, [CLOCK_MONOTONIC, 0, 0, 0, 0, 0])
                .await;
            assert!(fd >= 0, "timerfd_create (322): {}", fd);
            let fd = fd as usize;
            // Armed through the NARROW number, so what follows cannot be a
            // test of one width all the way through: the value goes in as
            // sixteen bytes and comes out as sixteen or thirty-two.
            // { it_interval = {0, 0}, it_value = {100, 0} }
            let mut arm = Vec::new();
            for v in [0u32, 0, 100, 0] {
                arm.extend_from_slice(&v.to_ne_bytes());
            }
            put(TS_AT, &arm);
            let armed = sys
                .compat32_syscall(TIMERFD_SETTIME, [fd, 0, TS_AT, 0, 0, 0])
                .await;
            assert_eq!(armed, 0, "timerfd_settime (325)");

            put(TS_AT, &[0xAAu8; 64]);
            let a = sys
                .compat32_syscall(TIMERFD_GETTIME, [fd, TS_AT, 0, 0, 0, 0])
                .await;
            let narrow = (a, get(TS_AT, 64));
            put(TS_AT, &[0xAAu8; 64]);
            let b = sys
                .compat32_syscall(TIMERFD_GETTIME64, [fd, TS_AT, 0, 0, 0, 0])
                .await;
            (narrow, (b, get(TS_AT, 64)))
        })
    });
    assert_eq!((narrow.0, wide.0), (0, 0), "both numbers must answer");
    // Narrow: interval at 0 and 4, value at 8 and 12. The remaining time is
    // a hair under the hundred seconds it was armed with.
    assert_eq!((word(&narrow.1, 0), word(&narrow.1, 4)), (0, 0), "interval");
    assert!(
        (90..=100).contains(&word(&narrow.1, 8)),
        "it_value.tv_sec at the narrow offset: {:x?}",
        &narrow.1[..16]
    );
    assert!(
        poison_left_from(&narrow.1, 4),
        "timerfd_gettime wrote past the sixteen bytes a narrow itimerspec is"
    );
    // Wide: each member is two words, so `it_value.tv_sec` is at byte 16 and
    // its top half --- byte 20 --- is a zero the narrow layout never has.
    assert_eq!(
        (word(&wide.1, 0), word(&wide.1, 8)),
        (0, 0),
        "interval, wide"
    );
    assert!(
        (90..=100).contains(&word(&wide.1, 16)),
        "it_value.tv_sec is not at byte 16: timerfd_gettime64 wrote a narrow \
         itimerspec and the caller reads its interval out of the top half of \
         its own tv_sec. {:x?}",
        &wide.1[..32]
    );
    assert_eq!(word(&wide.1, 20), 0, "the top half of it_value.tv_sec");
    assert!(
        poison_left_from(&wide.1, 8),
        "timerfd_gettime64 wrote past thirty-two bytes"
    );
    proc.exit(0);
}

/// `timerfd_settime64` (411) reads thirty-two bytes, and every check it
/// makes is the native one.
///
/// The way in, where a wrong width is not a wrong number but a wrong
/// MEMBER: sixteen bytes read as thirty-two take the value from whatever
/// follows the caller's structure, and thirty-two read as sixteen arm the
/// timer with the interval.
#[test]
fn timerfd_settime64_reads_thirty_two_bytes_and_shares_every_check() {
    let proc = a_process(48_457, Abi::Linux32);
    let (bad_flag, narrow_reading, old_out) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = sys
                .compat32_syscall(TIMERFD_CREATE, [CLOCK_MONOTONIC, 0, 0, 0, 0, 0])
                .await;
            assert!(fd >= 0, "timerfd_create (322): {}", fd);
            let fd = fd as usize;
            // Thirty-two bytes whose WIDE reading is
            // `{ {0, 0}, {0, 2_000_000_000} }` --- an `it_value.tv_nsec` over
            // a billion, which `ITimerSpec::valid` refuses --- and whose
            // NARROW reading is four zero words, a perfectly good disarm. So
            // `EINVAL` says the thirty-two bytes were read as thirty-two.
            let mut ts = Vec::new();
            ts.extend_from_slice(&0u64.to_ne_bytes()); // it_interval.tv_sec
            ts.extend_from_slice(&0u64.to_ne_bytes()); // it_interval.tv_nsec
            ts.extend_from_slice(&0u64.to_ne_bytes()); // it_value.tv_sec
            ts.extend_from_slice(&2_000_000_000u64.to_ne_bytes()); // it_value.tv_nsec
            put(TS_AT, &ts);
            // The flags check lives in the shared core, before the structure
            // is read at all: bit 1 is not a `timerfd_settime` flag.
            let bad_flag = sys
                .compat32_syscall(TIMERFD_SETTIME64, [fd, 2, TS_AT, 0, 0, 0])
                .await;
            let narrow_reading = sys
                .compat32_syscall(TIMERFD_SETTIME64, [fd, 0, TS_AT, 0, 0, 0])
                .await;
            // And the way OUT: arm it with a valid wide value, asking for the
            // old one into a poisoned buffer above the request.
            let mut arm = Vec::new();
            arm.extend_from_slice(&0u64.to_ne_bytes());
            arm.extend_from_slice(&0u64.to_ne_bytes());
            arm.extend_from_slice(&100u64.to_ne_bytes());
            arm.extend_from_slice(&0u64.to_ne_bytes());
            put(TS_AT, &arm);
            let old_at = TS_AT + 64;
            put(old_at, &[0xAAu8; 64]);
            let armed = sys
                .compat32_syscall(TIMERFD_SETTIME64, [fd, 0, TS_AT, old_at, 0, 0])
                .await;
            assert_eq!(armed, 0, "a valid wide itimerspec was refused");
            (bad_flag, narrow_reading, get(old_at, 64))
        })
    });
    assert_eq!(bad_flag, EINVAL, "the flags check is not shared");
    assert_eq!(
        narrow_reading, EINVAL,
        "timerfd_settime64 read sixteen bytes: the tv_nsec of over a billion \
         was in the half it never looked at, and the timer it armed is \
         whatever followed the caller's structure"
    );
    // The timer had just been disarmed, so the old value is all zeros --- but
    // at the wide width that is EIGHT words, and the ninth is still poison.
    assert!(
        (0..8).all(|w| word(&old_out, w * 4) == 0),
        "the old itimerspec came back at the narrow width: {:x?}",
        &old_out[..32]
    );
    assert!(
        poison_left_from(&old_out, 8),
        "timerfd_settime64 wrote past thirty-two bytes of old_value"
    );
    proc.exit(0);
}

/// `futex_time64` (422) reads its timeout as a `struct __kernel_timespec`.
///
/// This is the number behind every `pthread_cond_timedwait`, every
/// `sem_timedwait` and every `pthread_mutex_timedlock` of a modern i386
/// musl. Read eight bytes at a time, the deadline is not merely wrong: a
/// 1.5-second timeout becomes a `tv_sec` of 1 with the nanoseconds taken
/// from above the caller's structure, or --- the case below --- an
/// out-of-range request this kernel cannot see at all.
#[test]
fn the_futex_timeout_is_sixteen_bytes_for_number_422() {
    let proc = a_process(48_458, Abi::Linux32);
    // Leaked on purpose, like the words in `misc::futex_wiring_tests`:
    // `get_futex` keeps an `&'static AtomicI32` made from this address in the
    // process's table for good.
    let word_of_ours: &'static core::sync::atomic::AtomicI32 = alloc::boxed::Box::leak(
        alloc::boxed::Box::new(core::sync::atomic::AtomicI32::new(0)),
    );
    let uaddr = word_of_ours as *const core::sync::atomic::AtomicI32 as usize;
    let (wide, narrow) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // Sixteen bytes whose WIDE reading is `{0, 2_000_000_000}` --- a
            // `tv_nsec` over a billion, EINVAL --- and whose NARROW reading
            // is `{0, 0}`, a zero timeout, which parks for nothing and comes
            // straight back ETIMEDOUT. Two different errors, so neither can
            // be reached by accident.
            let mut ts = Vec::new();
            ts.extend_from_slice(&0u64.to_ne_bytes());
            ts.extend_from_slice(&2_000_000_000u64.to_ne_bytes());
            put(TS_AT, &ts);
            let op = (FUTEX_WAIT | FUTEX_PRIVATE_FLAG) as usize;
            let wide = sys
                .compat32_syscall(FUTEX_TIME64, [uaddr, op, 0, TS_AT, 0, 0])
                .await;
            let narrow = sys
                .compat32_syscall(FUTEX, [uaddr, op, 0, TS_AT, 0, 0])
                .await;
            (wide, narrow)
        })
    });
    assert_eq!(
        wide, EINVAL,
        "futex_time64 read its timeout eight bytes at a time: the tv_nsec of \
         over a billion was in the half it never looked at, so the wait used \
         a deadline the caller never asked for"
    );
    assert_eq!(
        narrow, ETIMEDOUT,
        "i386's own futex (240) stopped reading a narrow timespec"
    );
    proc.exit(0);
}
