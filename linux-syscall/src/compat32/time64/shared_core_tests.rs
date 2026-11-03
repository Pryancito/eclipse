//! The three numbers whose core already existed and only needed the width
//! handed in: `utimensat_time64` (412), `ppoll_time64` (414) and
//! `sched_rr_get_interval_time64` (423).
//!
//! Each test makes the SAME call twice from the SAME 32-bit process, by the
//! narrow number and by the `_time64` one, and asserts that the two answers
//! are different widths --- the pairing the rest of this directory is built
//! on. An assertion about 412 alone passes on a kernel that widens
//! everything, and one about 320 alone on a kernel that narrows everything.
//!
//! Fixed pids live in 48_49x.

use super::super::*;
use super::tests::{as_mut, get, put, word};
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::error::LxError;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's numbers: the narrow one and its `_time64` twin, side by side.
const I386_UTIMENSAT: u32 = 320;
const I386_UTIMENSAT_TIME64: u32 = 412;
const I386_PPOLL: u32 = 309;
const I386_PPOLL_TIME64: u32 = 414;
const I386_SCHED_RR_GET_INTERVAL: u32 = 161;
const I386_SCHED_RR_GET_INTERVAL_TIME64: u32 = 423;

const AT_FDCWD: usize = -100isize as usize;
const EINVAL: isize = -(LxError::EINVAL as isize);

const USER_AT: usize = 0x2400_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;
const PATH_AT: usize = USER_AT;
/// 4-aligned and NOT 8-aligned, which is the alignment a 32-bit program's
/// buffer has for every one of these structures --- the `_time64` ones
/// included, because on i386 an `__s64` aligns to 4.
const BUF_AT: usize = USER_AT + 0x404;

const AN_OLD_SECOND: u64 = 1_000_000;
const A_DIFFERENT_OLD_SECOND: u64 = 2_000_000;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let inode = lp.root_inode().create("f", FileType::File, 0o644).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.atime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    metadata.mtime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    inode.set_metadata(&metadata).unwrap();
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "time64-core", lp).unwrap();
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

/// Lay a pair of 64-bit `timespec`s down at `at`, with `0xaa` for the
/// sixteen bytes behind them.
fn put_wide_pair(at: usize, a: (u64, u64), m: (u64, u64)) {
    let mut bytes = alloc::vec::Vec::new();
    for v in [a.0, a.1, m.0, m.1] {
        bytes.extend_from_slice(&v.to_ne_bytes());
    }
    bytes.extend_from_slice(&[0xAAu8; 16]);
    put(at, &bytes);
}

/// 412 is the number a modern i386 musl actually sends for `utimensat(2)`
/// and `futimens(3)`, and its pair is a `struct __kernel_timespec[2]`:
/// **thirty-two** bytes, where 320's pair is sixteen. Both from the same
/// process, in one test, so neither "always narrow" nor "always wide" passes.
/// The seconds the file really has, asked of the filesystem through the
/// `Syscall` under test --- which is the only way to ask in the middle of
/// one `on_a_thread`, and one `on_a_thread` is the only way to make two
/// calls from the SAME process: a second one on it answers `BAD_STATE`.
fn times_through(sys: &Syscall<'_>) -> (i64, i64) {
    let metadata = sys
        .linux_process()
        .lookup_inode("/f")
        .unwrap()
        .metadata()
        .unwrap();
    (metadata.atime.sec, metadata.mtime.sec)
}

/// Both times back to the epoch, so the second half of the test starts where
/// the first half did.
fn reset_through(sys: &Syscall<'_>) {
    let inode = sys.linux_process().lookup_inode("/f").unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.atime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    metadata.mtime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    inode.set_metadata(&metadata).unwrap();
}

#[test]
fn the_two_utimensat_numbers_read_the_same_process_at_different_widths() {
    let proc = a_process(48_490, Abi::Linux32);
    let (wide, narrow) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(PATH_AT, b"/f\0");
            put_wide_pair(BUF_AT, (AN_OLD_SECOND, 0), (A_DIFFERENT_OLD_SECOND, 0));
            // By 412: the entries are sixteen bytes apart.
            let w = sys
                .compat32_syscall(I386_UTIMENSAT_TIME64, [AT_FDCWD, PATH_AT, BUF_AT, 0, 0, 0])
                .await;
            let wide = (w, times_through(&sys));
            // And by 320, the SAME bytes. The first entry's seconds are
            // still at 0, but the second entry is read from offset 8 ---
            // which in a wide pair is the HIGH half of the first seconds,
            // so zero.
            reset_through(&sys);
            let n = sys
                .compat32_syscall(I386_UTIMENSAT, [AT_FDCWD, PATH_AT, BUF_AT, 0, 0, 0])
                .await;
            (wide, (n, times_through(&sys)))
        })
    });
    assert_eq!(wide.0, 0, "utimensat_time64 (412) is not wired");
    assert_eq!(
        wide.1,
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "412 read its entries sixteen bytes apart"
    );
    assert_eq!(narrow.0, 0, "utimensat (320)");
    assert_eq!(
        narrow.1,
        (AN_OLD_SECOND as i64, 0),
        "320 read the second entry from offset 8, which here is the high \
         half of the first seconds: the two numbers are different widths"
    );
    proc.exit(0);
}

/// `ppoll_time64` (414) reads a `struct __kernel_timespec`, so a nanosecond
/// field out of range is `EINVAL`. The same bytes by 309 are a valid
/// timeout of zero, because the narrow read stops after eight bytes and the
/// nanoseconds it finds are the high half of the seconds.
#[test]
fn the_two_ppoll_numbers_read_the_timeout_at_different_widths() {
    let proc = a_process(48_491, Abi::Linux32);
    let (wide, narrow) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // tv_sec = 0, tv_nsec = two thousand million: not a nanosecond
            // count, so `timespec::valid()` refuses it.
            put(BUF_AT, &[0xAAu8; 32]);
            let mut bytes = alloc::vec::Vec::new();
            bytes.extend_from_slice(&0u64.to_ne_bytes());
            bytes.extend_from_slice(&2_000_000_000u64.to_ne_bytes());
            put(BUF_AT, &bytes);
            let w = sys
                .compat32_syscall(I386_PPOLL_TIME64, [0, 0, BUF_AT, 0, 0, 0])
                .await;
            let n = sys
                .compat32_syscall(I386_PPOLL, [0, 0, BUF_AT, 0, 0, 0])
                .await;
            (w, n)
        })
    });
    assert_eq!(
        wide, EINVAL,
        "414 read sixteen bytes and found a nanosecond field out of range"
    );
    assert_eq!(
        narrow, 0,
        "309 read eight bytes: tv_sec 0 and a tv_nsec that is the high half \
         of the seconds, so a timeout of zero on no descriptors"
    );
    proc.exit(0);
}

/// `sched_rr_get_interval_time64` (423) writes a `struct __kernel_timespec`:
/// sixteen bytes where 161 writes eight. Counted as the words that are no
/// longer the poison.
#[test]
fn the_two_interval_numbers_write_different_widths() {
    let proc = a_process(48_492, Abi::Linux32);
    let (narrow, wide) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(BUF_AT, &[0xAAu8; 32]);
            let n = sys
                .compat32_syscall(I386_SCHED_RR_GET_INTERVAL, [0, BUF_AT, 0, 0, 0, 0])
                .await;
            let narrow = (n, get(BUF_AT, 32));
            put(BUF_AT, &[0xAAu8; 32]);
            let w = sys
                .compat32_syscall(I386_SCHED_RR_GET_INTERVAL_TIME64, [0, BUF_AT, 0, 0, 0, 0])
                .await;
            (narrow, (w, get(BUF_AT, 32)))
        })
    });
    assert_eq!(narrow.0, 0, "sched_rr_get_interval (161)");
    assert_eq!(wide.0, 0, "sched_rr_get_interval_time64 (423) is not wired");
    // This thread is not SCHED_RR, so the interval is all zeros: the words
    // that stopped being poison are exactly the ones that were written.
    assert_eq!(
        (word(&narrow.1, 0), word(&narrow.1, 4)),
        (0, 0),
        "161 wrote the two narrow words"
    );
    assert_eq!(
        word(&narrow.1, 8),
        0xaaaa_aaaa,
        "and nothing past eight bytes: {:x?}",
        &narrow.1[..16]
    );
    assert_eq!(
        (word(&wide.1, 0), word(&wide.1, 8)),
        (0, 0),
        "423 wrote sixteen bytes"
    );
    assert_eq!(
        word(&wide.1, 12),
        0,
        "the high half of tv_nsec, which 161 never touches"
    );
    assert_eq!(
        word(&wide.1, 16),
        0xaaaa_aaaa,
        "and nothing past sixteen: {:x?}",
        &wide.1[..24]
    );
    proc.exit(0);
}
