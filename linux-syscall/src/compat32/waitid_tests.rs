//! `waitid` (284), which has **two** out-parameters and they differ in two
//! different ways.
//!
//! | what goes out | what changes with the width |
//! |---|---|
//! | `infop`, a `siginfo_t` | nothing of the SIZE: 128 bytes either way, union at byte 12 against byte 16 |
//! | `rusage`, a `struct rusage` | the size itself: **72 bytes against 144** |
//!
//! The second was going out through the typed `write_if_not_null`, i.e. 144
//! bytes into a 32-bit caller's 72-byte buffer: **72 bytes past the end**,
//! over whatever the caller keeps behind it. `wait4` (114), which answers the
//! same question, had gone through the narrow put since it was written, which
//! is what makes this one worth a test of its own rather than a line in the
//! table: the same structure, out of two calls, by two different routes.
//!
//! That overrun is why the rusage test here writes a POISON pattern over the
//! 72 bytes behind the buffer and asks for it back untouched. Asserting on
//! the buffer's own contents would not see it: those 72 bytes are correct
//! either way, because a narrow `rusage` is the native one with its words
//! halved and the halving happens from the front.
//!
//! Fixed pids live in 48_54x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::siginfo32::SIGINFO_LEN;
use linux_object::signal::Signal;
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, Thread, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's number for it. **Not** the native one: `waitid` is 247 on x86_64,
/// which is why the native test below goes in by `Sys::WAITID` and not by
/// this. Asking the native dispatcher for 284 answers `ENOSYS`, which is how
/// that half of the test first failed.
const I386_WAITID: u32 = 284;

/// `<linux/wait.h>`: wait for one child by pid, and tell me when it exits.
const P_PID: i32 = 1;
const WEXITED: usize = 4;

const USER_AT: usize = 0x2900_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// The `siginfo_t`, at an address a 32-bit program's own can have: 4 mod 8,
/// because its union is 4-aligned there.
const INFO_AT: usize = USER_AT + 0x104;
/// The `rusage`. On i386 it is twelve `__kernel_long_t` and two `timeval`s of
/// two each, so 4-aligned as well.
const RUSAGE_AT: usize = USER_AT + 0x304;
/// The same two, 8-ALIGNED, for the native half.
const INFO_AT_WIDE: usize = USER_AT + 0x800;
const RUSAGE_AT_WIDE: usize = USER_AT + 0xa00;

/// `sizeof(struct rusage)` at each width: two `timeval`s and fourteen words,
/// which is 18 words of four bytes there and of eight here.
const RUSAGE_LEN_32: usize = 18 * 4;
/// See [`RUSAGE_LEN_32`].
const RUSAGE_LEN_64: usize = 18 * 8;

/// What is laid behind a 32-bit caller's `rusage` buffer, and must still be
/// there afterwards.
const POISON: u8 = 0xA5;

/// Where a 32-bit program reads `_sifields._sigchld` from: straight after the
/// three `int`s, because the union is 4-aligned and there is no padding.
const SI_PID_32: usize = 12;
/// See [`SI_PID_32`].
const SI_UID_32: usize = 16;
/// See [`SI_PID_32`].
const SI_STATUS_32: usize = 20;
/// And natively, where four bytes of padding come first.
const SI_PID_64: usize = 16;
/// See [`SI_PID_64`].
const SI_STATUS_64: usize = 24;

/// The code the child exits with, chosen so that none of the three `int`s can
/// be mistaken for another: a status that equalled a pid would let an offset
/// slip.
const EXIT_CODE: i64 = 0x51;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "waitid32", lp).unwrap();
    Thread::create_linux(&proc).unwrap();
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

/// A child that has already exited, so the wait finds it without blocking.
/// Everything here runs on one thread, so a blocking wait would hang the
/// suite rather than fail it.
fn a_zombie_child(parent: &alloc::sync::Arc<Process>) -> KoID {
    let child = Process::fork_from(parent).unwrap();
    Thread::create_linux(&child).unwrap();
    let pid = child.id();
    child.exit(EXIT_CODE);
    pid
}

/// A `Syscall` as a `&mut` one, since `compat32_syscall` takes `&mut self`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

fn get(at: usize, len: usize) -> Vec<u8> {
    let p: kernel_hal::user::UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped above")
}

fn word(bytes: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// **A 32-bit process reads its child's report at the offsets a 32-bit
/// program reads them from.**
///
/// Two mutations die here: the entry for 284 out of `i386_abi_narrow.in`,
/// which puts the call back to `ENOSYS`, and `siginfo_put_if_not_null`
/// reverted to the typed write, which leaves `si_pid` in the padding and
/// hands the pid back as `si_uid`.
#[test]
fn a_32_bit_process_reads_its_childs_report_at_its_own_offsets() {
    let parent = a_process(48_540, Abi::Linux32);
    let pid = a_zombie_child(&parent);
    let (rc, info) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(INFO_AT, &[0u8; SIGINFO_LEN]);
            let rc = sys
                .compat32_syscall(
                    I386_WAITID,
                    [P_PID as usize, pid as usize, INFO_AT, WEXITED, 0, 0],
                )
                .await;
            (rc, get(INFO_AT, SIGINFO_LEN))
        })
    });
    assert_eq!(
        rc, 0,
        "284 answers 0 and puts what it found in infop; ENOSYS here means it \
         is in neither i386 table"
    );
    assert_eq!(word(&info, 0), Signal::SIGCHLD as i32, "si_signo");
    assert_eq!(
        word(&info, SI_PID_32),
        pid as i32,
        "si_pid, where a 32-bit program reads it; zero here means the native \
         bytes went across and this is the padding"
    );
    assert_eq!(word(&info, SI_UID_32), 0, "si_uid of a root child");
    assert_eq!(
        word(&info, SI_STATUS_32),
        EXIT_CODE as i32,
        "si_status is the code it exited with"
    );
    parent.exit(0);
}

/// **And its `rusage` is 72 bytes, not 144.**
///
/// The one test of this batch whose subject is a buffer OVERRUN rather than a
/// wrong value: the 72 bytes behind the caller's `struct rusage` are poisoned
/// first and have to come back untouched. Reverting the write to the typed
/// one puts the native 144 bytes there and wipes them.
#[test]
fn a_32_bit_rusage_is_seventy_two_bytes_and_nothing_behind_it_is_touched() {
    let parent = a_process(48_541, Abi::Linux32);
    let pid = a_zombie_child(&parent);
    let (rc, behind) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(INFO_AT, &[0u8; SIGINFO_LEN]);
            // The buffer, and then the poison immediately behind it, exactly
            // as wide as the overrun would be.
            put(RUSAGE_AT, &[0u8; RUSAGE_LEN_32]);
            put(
                RUSAGE_AT + RUSAGE_LEN_32,
                &[POISON; RUSAGE_LEN_64 - RUSAGE_LEN_32],
            );
            let rc = sys
                .compat32_syscall(
                    I386_WAITID,
                    [P_PID as usize, pid as usize, INFO_AT, WEXITED, RUSAGE_AT, 0],
                )
                .await;
            (
                rc,
                get(RUSAGE_AT + RUSAGE_LEN_32, RUSAGE_LEN_64 - RUSAGE_LEN_32),
            )
        })
    });
    assert_eq!(rc, 0, "284 with a rusage asked for");
    assert!(
        behind.iter().all(|&b| b == POISON),
        "the {} bytes behind a 32-bit caller's rusage were written over: the \
         native 144 bytes went into a 72-byte buffer",
        RUSAGE_LEN_64 - RUSAGE_LEN_32
    );
    parent.exit(0);
}

/// **A native process still gets both structures natively.**
///
/// Without this, conversions applied unconditionally would pass both tests
/// above, and every native `waitid` in the system would be broken. This is
/// the guard that the width comes from the PROCESS and is actually consulted.
#[test]
fn a_native_process_still_gets_both_structures_natively() {
    let parent = a_process(48_542, Abi::Linux);
    let pid = a_zombie_child(&parent);
    let (rc, info, rusage) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(INFO_AT_WIDE, &[0u8; SIGINFO_LEN]);
            put(RUSAGE_AT_WIDE, &[POISON; RUSAGE_LEN_64]);
            let rc = sys
                .syscall(
                    Sys::WAITID as u32,
                    [
                        P_PID as usize,
                        pid as usize,
                        INFO_AT_WIDE,
                        WEXITED,
                        RUSAGE_AT_WIDE,
                        0,
                    ],
                )
                .await;
            (
                rc,
                get(INFO_AT_WIDE, SIGINFO_LEN),
                get(RUSAGE_AT_WIDE, RUSAGE_LEN_64),
            )
        })
    });
    assert_eq!(rc, 0, "waitid from a native process");
    assert_eq!(
        word(&info, SI_PID_64),
        pid as i32,
        "si_pid at the NATIVE offset; finding it at 12 means the conversion \
         ran for a native caller"
    );
    assert_eq!(
        word(&info, SI_STATUS_64),
        EXIT_CODE as i32,
        "si_status, natively"
    );
    // A native `rusage` really is 144 bytes, so the whole poisoned buffer is
    // the call's to write: the child's own CPU time replaced it.
    assert!(
        rusage.iter().any(|&b| b != POISON),
        "the native rusage was not written at all"
    );
    // And the tail of it was written too, which a 72-byte put would have
    // left as poison.
    assert!(
        rusage[RUSAGE_LEN_32..].iter().any(|&b| b != POISON),
        "only the first 72 bytes were written: the narrow put ran for a \
         native caller"
    );
    parent.exit(0);
}

/// A null `rusage` is not written at either width.
///
/// `sys_waitid` copies the usage out under `if (err > 0)` and both puts here
/// are the `_if_not_null` form; a program that asks only for `infop` --- which
/// is what `waitid(2)`'s own prototype suggests and what glibc's
/// `waitid(3)` passes --- must not have address zero written to.
#[test]
fn a_null_rusage_is_left_alone_by_either_width() {
    for (pid, abi, info_at) in [
        (48_543u64, Abi::Linux32, INFO_AT),
        (48_544, Abi::Linux, INFO_AT_WIDE),
    ] {
        let parent = a_process(pid, abi);
        let child = a_zombie_child(&parent);
        let rc = on_a_thread(&parent, move |sys| {
            Box::pin(async move {
                let mut sys = as_mut(sys);
                put(info_at, &[0u8; SIGINFO_LEN]);
                sys.compat32_syscall(
                    I386_WAITID,
                    [P_PID as usize, child as usize, info_at, WEXITED, 0, 0],
                )
                .await
            })
        });
        assert_eq!(rc, 0, "a {abi:?} waitid with no rusage asked for");
        parent.exit(0);
    }
}

/// And a null `infop` is not written either, at either width.
///
/// Both of these are what keeps the null checks from being dead branches that
/// nothing exercises: `waitid(P_PID, pid, NULL, WEXITED, NULL)` is legal and
/// is how a program that only wants the child reaped calls it.
#[test]
fn a_null_infop_is_left_alone_by_either_width() {
    for (pid, abi) in [(48_545u64, Abi::Linux32), (48_546, Abi::Linux)] {
        let parent = a_process(pid, abi);
        let child = a_zombie_child(&parent);
        let rc = on_a_thread(&parent, move |sys| {
            Box::pin(async move {
                let mut sys = as_mut(sys);
                sys.compat32_syscall(
                    I386_WAITID,
                    [P_PID as usize, child as usize, 0, WEXITED, 0, 0],
                )
                .await
            })
        });
        assert_eq!(rc, 0, "a {abi:?} waitid with no infop asked for");
        parent.exit(0);
    }
}
