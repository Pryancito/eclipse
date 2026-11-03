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

const CLOCK_MONOTONIC: usize = 1;
const CLOCK_REALTIME: usize = 0;
/// A clock number nothing implements, for the `EINVAL` the two entries share.
const CLOCK_NONSENSE: usize = 99;

const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

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
fn the_four_numbers_are_not_in_the_translation_table() {
    // A row would send 403 to the native `clock_gettime`, whose handler asks
    // the PROCESS -- and would answer eight bytes where the program waits
    // for sixteen. So the numbers must be answered before the table is
    // asked, which is what the arms in `compat32` do.
    for n in [
        CLOCK_GETTIME64,
        CLOCK_SETTIME64,
        CLOCK_GETRES_TIME64,
        CLOCK_NANOSLEEP_TIME64,
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
            for n in [408u32, 411, 412, 414, 422] {
                out.push(sys.compat32_syscall(n, [0, 0, 0, 0, 0, 0]).await);
            }
            out
        })
    });
    assert!(got.iter().all(|&r| r == ENOSYS), "{:?}", got);
    assert_eq!(crate::compat32::name(422), Some("futex_time64"));
    proc.exit(0);
}
