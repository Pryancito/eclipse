//! `pselect6_time64` (413) next to i386's plain `pselect6` (308).
//!
//! This is the second call that is in BOTH of this layer's rules at once,
//! after `utimensat_time64`, and it is the one that makes the shape plain,
//! because here the two structures are not even the same kind of thing:
//!
//! | what it passes | whose width |
//! |---|---|
//! | the timeout, a `struct timespec` | the NUMBER's: 8 bytes by 308, 16 by 413 |
//! | the sixth argument's `{ const sigset_t *ss; size_t ss_len; }` | the PROCESS's: 8 bytes by either number |
//!
//! The second row is why `pselect6` was out of the translation table until
//! now. Its timeout was always converted; nothing converted those two words,
//! so a 32-bit `pselect6` read the length out of whatever followed the pack.
//!
//! Each test makes the same call twice from the same 32-bit process, once by
//! each number, so neither "always narrow" nor "always wide" passes.
//!
//! Fixed pids live in 48_50x.

use super::super::*;
use super::tests::{as_mut, put};
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::error::LxError;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's two numbers for the same call.
const I386_PSELECT6: u32 = 308;
const I386_PSELECT6_TIME64: u32 = 413;

const EINVAL: isize = -(LxError::EINVAL as isize);

const USER_AT: usize = 0x2500_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;
/// The timeout, 4-aligned and not 8-aligned: what a 32-bit program's
/// `struct timespec *` actually is, at either width, because on i386 an
/// `__s64` aligns to 4.
const TS_AT: usize = USER_AT + 0x404;
/// The sixth argument's pack.
const ARG_AT: usize = USER_AT + 0x504;
/// And the `sigset_t` the pack points at. An i386 `sigset_t` is
/// `unsigned long sig[2]`, which aligns to 4, so this one is 4 mod 8 too.
const SIG_AT: usize = USER_AT + 0x604;

/// The size `check_sigsetsize` insists on: `sizeof(sigset_t)`, which is eight
/// in both ABIs.
const SIGSETSIZE: u32 = 8;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "pselect-time64", lp).unwrap();
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

/// Lay a `struct __kernel_timespec` down at `at`: two 64-bit words, with
/// `0xaa` behind them so that a read of the wrong width is readable.
fn put_wide_timespec(at: usize, sec: u64, nsec: u64) {
    let mut bytes = alloc::vec::Vec::new();
    bytes.extend_from_slice(&sec.to_ne_bytes());
    bytes.extend_from_slice(&nsec.to_ne_bytes());
    bytes.extend_from_slice(&[0xAAu8; 16]);
    put(at, &bytes);
}

/// The same at the width a 32-bit process speaks: two FOUR-byte words.
fn put_narrow_timespec(at: usize, sec: u32, nsec: u32) {
    let mut bytes = alloc::vec::Vec::new();
    bytes.extend_from_slice(&sec.to_ne_bytes());
    bytes.extend_from_slice(&nsec.to_ne_bytes());
    bytes.extend_from_slice(&[0xAAu8; 24]);
    put(at, &bytes);
}

/// A timeout of zero, which is the one value whose BYTES are valid at both
/// widths: all zeros read either way is `{ 0, 0 }`. The tests about the sixth
/// argument use it so that the timeout is not a second variable --- and a
/// NULL timeout cannot serve, because that means "wait for ever" and with no
/// descriptor to wait on the call never comes back.
fn put_zero_timeout(at: usize) {
    put(at, &[0u8; 16]);
}

/// Lay the sixth argument's pack down at `at` as a 32-bit process does it:
/// two FOUR-byte words, with `0xaa` behind them. Read at the native width,
/// `ss` comes out as the pointer with the length shifted in above it and
/// `ss_len` out of the poison.
fn put_narrow_pack(at: usize, ss: u32, ss_len: u32) {
    let mut bytes = alloc::vec::Vec::new();
    bytes.extend_from_slice(&ss.to_ne_bytes());
    bytes.extend_from_slice(&ss_len.to_ne_bytes());
    bytes.extend_from_slice(&[0xAAu8; 16]);
    put(at, &bytes);
}

/// An all-zero `sigset_t`: blocks nothing, so the call is not changed by it
/// and what the test measures is only whether the pack was read.
fn put_empty_sigset(at: usize) {
    put(at, &[0u8; 8]);
}

/// The timeout is the NUMBER's. The same bytes --- a `struct
/// __kernel_timespec` of `{ 0, 2_000_000_000 }` --- are an out-of-range
/// nanosecond count read wide, and read narrow they are the two LOW halves of
/// those words, so `{ 0, 0 }`: a timeout of zero, which with no descriptor to
/// wait on returns at once.
///
/// So one number answers `EINVAL` and the other `0`, from one process, over
/// one buffer.
#[test]
fn the_two_pselect6_numbers_read_the_timeout_at_different_widths() {
    let proc = a_process(48_500, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_wide_timespec(TS_AT, 0, 2_000_000_000);
            let n = sys
                .compat32_syscall(I386_PSELECT6, [0, 0, 0, 0, TS_AT, 0])
                .await;
            put_wide_timespec(TS_AT, 0, 2_000_000_000);
            let w = sys
                .compat32_syscall(I386_PSELECT6_TIME64, [0, 0, 0, 0, TS_AT, 0])
                .await;
            (n, w)
        })
    });
    assert_eq!(
        narrow, 0,
        "308 read eight bytes, so the timeout is {{0, 0}} and the call returns"
    );
    assert_eq!(
        wide, EINVAL,
        "413 read sixteen, so tv_nsec is two thousand million"
    );
}

/// And the sixth argument is the PROCESS's, by either number: eight bytes.
///
/// A pack of `{ &sigset, 8 }` is what a 32-bit program writes. Read at the
/// native width, `ss` would be `0x8_xxxx_xxxx` --- the pointer with the
/// length shifted in above it --- and `ss_len` would come out of the `0xaa`
/// poison after the pack, so `check_sigsetsize` would answer `EINVAL`.
///
/// Both numbers answering `0` is therefore the measurement: handing the
/// number's width to this read instead of the process's makes 413 fail.
#[test]
fn the_sixth_argument_is_eight_bytes_by_either_number() {
    let proc = a_process(48_501, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_empty_sigset(SIG_AT);
            put_narrow_pack(ARG_AT, SIG_AT as u32, SIGSETSIZE);
            // A timeout of zero and not a null one, so nothing but the pack
            // can fail the call and nothing can make it wait.
            put_zero_timeout(TS_AT);
            let n = sys
                .compat32_syscall(I386_PSELECT6, [0, 0, 0, 0, TS_AT, ARG_AT])
                .await;
            put_zero_timeout(TS_AT);
            let w = sys
                .compat32_syscall(I386_PSELECT6_TIME64, [0, 0, 0, 0, TS_AT, ARG_AT])
                .await;
            (n, w)
        })
    });
    assert_eq!(narrow, 0, "308 read the pack at eight bytes");
    assert_eq!(wide, 0, "413 read the pack at eight bytes TOO");
}

/// The other half of that claim, so the test above cannot pass on a kernel
/// that ignores the pack: a pack whose LENGTH is wrong is refused.
///
/// Without this, a handler that never read the sixth argument at all would
/// answer `0` to both numbers and look right.
#[test]
fn a_pack_with_the_wrong_length_is_refused_by_either_number() {
    let proc = a_process(48_502, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_empty_sigset(SIG_AT);
            // Four, which is `sizeof(sigset_t)` in no ABI.
            put_narrow_pack(ARG_AT, SIG_AT as u32, 4);
            put_zero_timeout(TS_AT);
            let n = sys
                .compat32_syscall(I386_PSELECT6, [0, 0, 0, 0, TS_AT, ARG_AT])
                .await;
            put_zero_timeout(TS_AT);
            let w = sys
                .compat32_syscall(I386_PSELECT6_TIME64, [0, 0, 0, 0, TS_AT, ARG_AT])
                .await;
            (n, w)
        })
    });
    assert_eq!(narrow, EINVAL, "308 looked at the length");
    assert_eq!(wide, EINVAL, "413 looked at the length");
}

/// A null sixth argument is "do not touch the mask", not a bad address: the
/// pack is only read when the argument is non-zero.
#[test]
fn a_null_sixth_argument_is_not_a_pack_at_all() {
    let proc = a_process(48_503, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put_zero_timeout(TS_AT);
            sys.compat32_syscall(I386_PSELECT6_TIME64, [0, 0, 0, 0, TS_AT, 0])
                .await
        })
    });
    assert_eq!(got, 0);
}

/// And the time LEFT, written back at the width of the number.
///
/// `poll_select_finish` hands the caller what is left of the timeout, which
/// is the only thing this call writes --- so it is the only way to measure
/// the width of the write rather than of the read. A call that returns at
/// once over a two-second timeout reports nearly the whole two seconds.
///
/// What is measured is the WIDTH of that write and not its value: under
/// `libos` the elapsed time of even an immediate pass exceeds the timeout, so
/// the time left comes back **zero**, which is a run of zero bytes as long as
/// the width written. Where the `0xaa` poison starts again is therefore the
/// whole measurement.
///
/// ## What the first version of this test did NOT measure
///
/// For 413 the wide input was `{ 2, 0 }`, whose bytes 8..16 are already zero
/// --- so "byte 8 stopped being poison" was true before the call, and the
/// narrow write-back left the test green. The wide input here carries a
/// NON-ZERO nanosecond count instead, so bytes 8..12 hold `0x1dcd6500`
/// going in and only a sixteen-byte write can clear them. Same lesson as
/// every other time in this directory: an assertion the input already
/// satisfies measures nothing.
#[test]
fn the_time_left_is_written_at_the_width_of_the_number() {
    let proc = a_process(48_504, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // Two seconds, laid down at each width in turn, because here the
            // bytes have to be VALID at the width under test: what is being
            // measured is the write-back and not the read.
            put(TS_AT, &[0xAAu8; 32]);
            put_narrow_timespec(TS_AT, 2, 0);
            let n = sys
                .compat32_syscall(I386_PSELECT6, [0, 0, 0, 0, TS_AT, 0])
                .await;
            let narrow = (n, super::tests::get(TS_AT, 32));
            put(TS_AT, &[0xAAu8; 32]);
            // Half a second of nanoseconds, so that bytes 8..12 are not
            // already what a wide write would leave there.
            put_wide_timespec(TS_AT, 2, 500_000_000);
            let w = sys
                .compat32_syscall(I386_PSELECT6_TIME64, [0, 0, 0, 0, TS_AT, 0])
                .await;
            (narrow, (w, super::tests::get(TS_AT, 32)))
        })
    });
    assert_eq!(narrow.0, 0, "pselect6 (308)");
    assert_eq!(wide.0, 0, "pselect6_time64 (413) is not wired");
    assert_eq!(
        super::tests::word(&narrow.1, 8),
        0xaaaa_aaaa,
        "308 wrote eight bytes of time left: {:x?}",
        &narrow.1[..16]
    );
    assert_eq!(
        super::tests::word(&wide.1, 8),
        0,
        "413 wrote sixteen bytes, so the nanoseconds that went in are gone \
         and not left behind by an eight-byte write: {:x?}",
        &wide.1[..24]
    );
    assert_eq!(
        super::tests::word(&wide.1, 16),
        0xaaaa_aaaa,
        "and nothing past sixteen: {:x?}",
        &wide.1[..24]
    );
}
