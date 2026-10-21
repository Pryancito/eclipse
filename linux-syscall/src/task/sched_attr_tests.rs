//! `sched_setattr(2)` and `sched_getattr(2)` with the struct as Linux 6.18
//! has it, version 1, 56 bytes, made by number from a thread of the
//! process. Each case was measured there first, with glibc and with musl.
//! Fixed pids live in 47_9xx here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const E2BIG: isize = -(LxError::E2BIG as isize);
const EINVAL: isize = -(LxError::EINVAL as isize);
const EOPNOTSUPP: isize = -(LxError::EOPNOTSUPP as isize);
const EPERM: isize = -(LxError::EPERM as isize);
const ESRCH: isize = -(LxError::ESRCH as isize);

/// A pid no test here makes.
const NOBODY: usize = 47_999;
/// The four bytes `sched_getattr` must leave alone.
const UNTOUCHED: u8 = 0xa5;

fn a_process(pid: KoID, uid: u32) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    if uid != 0 {
        lp.set_resgid(uid, uid, uid).unwrap();
        lp.set_resuid(uid, uid, uid).unwrap();
    }
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "sched_attr", lp).unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

async fn by_number(sys: &Syscall<'_>, num: Sys, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num as u32, args).await
}

/// What a caller builds: a page and a bit, the struct at the start, `size`
/// saying how much of it there is.
fn a_struct(size: u32, policy: u32, flags: u64, nice: i32, priority: u32) -> Vec<u8> {
    let attr = SchedAttr {
        size,
        sched_policy: policy,
        sched_flags: flags,
        sched_nice: nice,
        sched_priority: priority,
        ..Default::default()
    };
    let mut buf = alloc::vec![0u8; 2 * PAGE_SIZE];
    buf[..SCHED_ATTR_SIZE_VER1].copy_from_slice(&attr.to_bytes());
    buf
}

fn word(buf: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

async fn setattr(sys: &Syscall<'_>, pid: usize, buf: &mut [u8]) -> isize {
    let at = buf.as_mut_ptr() as usize;
    by_number(sys, Sys::SCHED_SETATTR, [pid, at, 0, 0, 0, 0]).await
}

/// `sched_getattr(0, buf, size)` into a buffer of `UNTOUCHED` bytes.
async fn getattr(sys: &Syscall<'_>, size: usize) -> (isize, Vec<u8>) {
    let mut buf = alloc::vec![UNTOUCHED; 2 * PAGE_SIZE];
    let at = buf.as_mut_ptr() as usize;
    let got = by_number(sys, Sys::SCHED_GETATTR, [0, at, size, 0, 0, 0]).await;
    (got, buf)
}

/// The policy and the nice of the calling thread, by number.
async fn policy_and_nice(sys: &Syscall<'_>) -> (isize, isize) {
    (
        by_number(sys, Sys::SCHED_GETSCHEDULER, [0, 0, 0, 0, 0, 0]).await,
        20 - by_number(sys, Sys::GETPRIORITY, [0, 0, 0, 0, 0, 0]).await,
    )
}

/// `sched_getattr` writes as much of the 56 bytes as the caller asked for
/// and says how much that was; past 56, zeros up to `size`, and nothing
/// after. 47 and a page and a byte are `EINVAL`.
#[test]
fn getattr_writes_the_fifty_six_byte_struct() {
    let proc = a_process(47_901, 0);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut got = Vec::new();
            for size in [56, 52, 48, PAGE_SIZE, 47, PAGE_SIZE + 1] {
                got.push(getattr(sys, size).await);
            }
            got
        })
    });
    let sizes: Vec<_> = got.iter().map(|(r, b)| (*r, word(b, 0))).collect();
    assert_eq!(
        sizes,
        [
            (0, 56),
            (0, 52),
            (0, 48),
            (0, 56),
            (EINVAL, u32::from_ne_bytes([UNTOUCHED; 4])),
            (EINVAL, u32::from_ne_bytes([UNTOUCHED; 4])),
        ]
    );
    // The util clamps this kernel does not have read 0, and stop where the
    // caller's struct stops.
    assert_eq!((word(&got[0].1, 48), word(&got[0].1, 52)), (0, 0));
    assert_eq!(&got[0].1[56..60], &[UNTOUCHED; 4]);
    assert_eq!(
        (word(&got[1].1, 48), &got[1].1[52..56]),
        (0, &[UNTOUCHED; 4][..])
    );
    assert_eq!(&got[2].1[48..52], &[UNTOUCHED; 4]);
    let page = &got[3].1;
    assert!(
        page[56..PAGE_SIZE].iter().all(|&b| b == 0),
        "the tail is zeroed"
    );
    assert_eq!(page[PAGE_SIZE], UNTOUCHED, "and nothing past `size`");
}

/// The util words of a 56-byte struct are read and, with no clamp flag,
/// ignored; a struct of 52 is taken too. Both used to be `E2BIG`, the first
/// as soon as a util word was not zero.
#[test]
fn setattr_reads_the_fifty_six_byte_struct() {
    let proc = a_process(47_902, 0);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut util = a_struct(56, SCHED_NORMAL as u32, 0, 3, 0);
            util[48..52].copy_from_slice(&100u32.to_ne_bytes());
            util[52..56].copy_from_slice(&900u32.to_ne_bytes());
            let a = setattr(sys, 0, &mut util).await;
            let after_util = policy_and_nice(sys).await;
            let b = setattr(sys, 0, &mut a_struct(52, SCHED_BATCH as u32, 0, 4, 0)).await;
            let after_52 = policy_and_nice(sys).await;
            (a, after_util, b, after_52)
        })
    });
    assert_eq!(
        got,
        (0, (SCHED_NORMAL as isize, 3), 0, (SCHED_BATCH as isize, 4))
    );
}

/// A size out of range, or a byte set past the 56, is `E2BIG`, and the
/// kernel writes the size it takes into the caller's `size`.
#[test]
fn a_struct_too_big_or_too_small_is_e2big_and_says_fifty_six() {
    let proc = a_process(47_903, 0);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut tail = a_struct(64, SCHED_NORMAL as u32, 0, 0, 0);
            tail[60] = 1;
            let mut empty_tail = a_struct(64, SCHED_NORMAL as u32, 0, 0, 0);
            let mut too_big = a_struct(PAGE_SIZE as u32 + 4, SCHED_NORMAL as u32, 0, 0, 0);
            let mut too_small = a_struct(30, SCHED_NORMAL as u32, 0, 0, 0);
            let mut got = Vec::new();
            for buf in [&mut tail, &mut empty_tail, &mut too_big, &mut too_small] {
                let r = setattr(sys, 0, buf).await;
                got.push((r, word(buf, 0)));
            }
            got
        })
    });
    assert_eq!(got, [(E2BIG, 56), (0, 64), (E2BIG, 56), (E2BIG, 56)]);
}

/// The util clamp flags: `EINVAL` in a struct too short to hold them,
/// before the pid is looked up; then `ESRCH`, the policy's `EINVAL` and
/// `EPERM` in their turn; and only then `EOPNOTSUPP`, this kernel having no
/// util clamps, with nothing changed.
#[test]
fn the_util_clamps_are_eopnotsupp_after_every_other_check() {
    const CLAMP_MIN: u64 = 0x20;
    let root = a_process(47_904, 0);
    let got = on_a_thread(&root, |sys| {
        Box::pin(async move {
            let short = |pid| async move {
                setattr(
                    sys,
                    pid,
                    &mut a_struct(48, SCHED_NORMAL as u32, CLAMP_MIN, 0, 0),
                )
                .await
            };
            let fifo = a_struct(56, SCHED_FIFO as u32, CLAMP_MIN, 0, 10);
            [
                short(NOBODY).await,
                short(0).await,
                setattr(sys, NOBODY, &mut fifo.clone()).await,
                setattr(
                    sys,
                    0,
                    &mut a_struct(56, SCHED_NORMAL as u32, CLAMP_MIN, 0, 5),
                )
                .await,
                setattr(sys, 0, &mut fifo.clone()).await,
                setattr(sys, 0, &mut a_struct(56, SCHED_NORMAL as u32, 0x60, -5, 0)).await,
            ]
        })
    });
    assert_eq!(got, [EINVAL, EINVAL, ESRCH, EINVAL, EOPNOTSUPP, EOPNOTSUPP]);
    let stranger = a_process(47_905, 1000);
    let got = on_a_thread(&stranger, |sys| {
        Box::pin(async move {
            let mut fifo = a_struct(56, SCHED_FIFO as u32, CLAMP_MIN, 0, 10);
            let refused = setattr(sys, 0, &mut fifo).await;
            let mut clamp = a_struct(56, SCHED_NORMAL as u32, CLAMP_MIN, 2, 0);
            (
                refused,
                setattr(sys, 0, &mut clamp).await,
                policy_and_nice(sys).await,
            )
        })
    });
    assert_eq!(got, (EPERM, EOPNOTSUPP, (SCHED_NORMAL as isize, 0)));
}

/// What `EOPNOTSUPP` leaves: the thread as it was, policy, priority and
/// nice.
#[test]
fn a_refused_clamp_changes_nothing() {
    let proc = a_process(47_906, 0);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let set = setattr(sys, 0, &mut a_struct(56, SCHED_NORMAL as u32, 0, 7, 0)).await;
            let mut fifo = a_struct(56, SCHED_FIFO as u32, 0x40, -3, 10);
            let refused = setattr(sys, 0, &mut fifo).await;
            let mut param = 0i32;
            let param_at = &mut param as *mut i32 as usize;
            let read = by_number(sys, Sys::SCHED_GETPARAM, [0, param_at, 0, 0, 0, 0]).await;
            (set, refused, policy_and_nice(sys).await, read, param)
        })
    });
    assert_eq!(got, (0, EOPNOTSUPP, (SCHED_NORMAL as isize, 7), 0, 0));
}

/// A nice out of range is the nearest one in range, as Linux's
/// `sched_copy_attr` has it: 50 is 19, -50 is -20.
#[test]
fn setattr_clamps_the_nice() {
    let proc = a_process(47_907, 0);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut got = Vec::new();
            for nice in [50, -50, i32::MAX, i32::MIN] {
                let r = setattr(sys, 0, &mut a_struct(56, SCHED_NORMAL as u32, 0, nice, 0)).await;
                got.push((r, policy_and_nice(sys).await.1));
            }
            got
        })
    });
    assert_eq!(got, [(0, 19), (0, -20), (0, 19), (0, -20)]);
}

/// A flag Linux does not know is `EINVAL` once the task is found, and a pid
/// that names nobody is `ESRCH` whatever the flags say.
#[test]
fn the_task_is_looked_up_before_the_flags() {
    let proc = a_process(47_908, 0);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let unknown = || a_struct(56, SCHED_NORMAL as u32, 0x80, 0, 0);
            [
                setattr(sys, NOBODY, &mut unknown()).await,
                setattr(sys, 0, &mut unknown()).await,
                // A policy that is none, or a priority out of band, is the
                // task's to refuse too.
                setattr(sys, NOBODY, &mut a_struct(56, 77, 0, 0, 0)).await,
                setattr(sys, NOBODY, &mut a_struct(56, SCHED_FIFO as u32, 0, 0, 200)).await,
                // A negative one is refused before.
                setattr(sys, NOBODY, &mut a_struct(56, u32::MAX, 0, 0, 0)).await,
            ]
        })
    });
    assert_eq!(got, [ESRCH, EINVAL, ESRCH, ESRCH, EINVAL]);
}
