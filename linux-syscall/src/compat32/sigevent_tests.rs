//! `timer_create` (259), whose `struct sigevent` moves **every** field.
//!
//! The other structures of this layer are either the native one with its
//! words halved, or the native one at a looser alignment. This is neither:
//! its first member is a `union sigval`, which holds a pointer, so it is
//! eight bytes natively and four to a 32-bit program, and every field behind
//! it shifts. The table of offsets is in [`crate::narrow::sigevent`].
//!
//! ## How these tests see an offset
//!
//! `timer_create` hands back nothing but a timer id, so there is no structure
//! to read back and compare. What it does do is **refuse** a `sigevent` that
//! names no real signal (`good_sigevent()`: `EINVAL`), and that is the whole
//! lever here. One buffer is enough, because the two readings overlap:
//!
//! ```text
//! byte:        0       4       8      12
//! i386:     value   signo  notify    tid
//! native:   value (8 bytes) signo  notify
//! ```
//!
//! A 32-bit caller's `sigev_notify` and a native caller's `sigev_signo` are
//! **the same four bytes**. So a buffer whose bytes 8..12 are zero is a valid
//! `SIGEV_SIGNAL` request read at the i386 offsets and an invalid signal 0
//! read at the native ones --- one buffer, and the two widths disagree about
//! it by construction rather than by a value picked to make a point.
//!
//! Fixed pids live in 48_55x.

use super::*;
use crate::narrow::sigevent::sigevent_get;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::error::LxError;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::Signal;
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, Thread, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's number. The native one is 107 on aarch64 and riscv64 and **222**
/// here; `Sys::TIMER_CREATE` is the one to go through for a native process,
/// for the reason `waitid_tests` gives.
const I386_TIMER_CREATE: u32 = 259;

const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

const CLOCK_MONOTONIC: usize = 1;

const USER_AT: usize = 0x2a00_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// The `struct sigevent`, at an address a 32-bit program's own can have: 4
/// modulo 8, because every member of a `compat_sigevent` is an `int` and the
/// structure is 4-aligned there.
const SEV_AT: usize = USER_AT + 0x104;
/// And the 8-ALIGNED one for the native half: natively `sigev_value` is a
/// union holding a pointer, so the structure really is 8-aligned and using
/// the 4-aligned address there would fail for the alignment and say nothing
/// about the offsets.
const SEV_AT_WIDE: usize = USER_AT + 0x800;
/// Where the timer id comes back. A `timer_t` is an `int` at both widths.
const ID_AT: usize = USER_AT + 0xc00;

/// `SIGEV_SIGNAL`, which is zero --- and being zero is what makes the shared
/// four bytes an invalid signal to the other reading.
const SIGEV_SIGNAL: i32 = 0;
const SIGEV_THREAD_ID: i32 = 4;

/// A real signal, for whichever offset is meant to hold one.
const NOTIFY_SIGNO: i32 = Signal::SIGALRM as i32;

/// A `sival_ptr` with the top bit set: two gigabytes up to a 32-bit program,
/// and the value a sign-extending widening would turn into a kernel address.
const SIVAL: u32 = 0x8000_0000;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "sigevent32", lp).unwrap();
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

/// The 64 bytes a 32-bit program hands over: `sival` at 0, the signal at 4,
/// the notify at 8, the thread id at 12.
fn narrow_sigevent(signo: i32, notify: i32, tid: i32) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0..4].copy_from_slice(&SIVAL.to_ne_bytes());
    b[4..8].copy_from_slice(&signo.to_ne_bytes());
    b[8..12].copy_from_slice(&notify.to_ne_bytes());
    b[12..16].copy_from_slice(&tid.to_ne_bytes());
    b
}

/// And the native shape: `sival` is eight bytes, so the signal is at 8, the
/// notify at 12, and the thread id at 16 --- it sits in a union that holds
/// pointers, so it starts 8-aligned.
fn native_sigevent(signo: i32, notify: i32, tid: i32) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[0..8].copy_from_slice(&(SIVAL as u64).to_ne_bytes());
    b[8..12].copy_from_slice(&signo.to_ne_bytes());
    b[12..16].copy_from_slice(&notify.to_ne_bytes());
    b[16..20].copy_from_slice(&tid.to_ne_bytes());
    b
}

/// The number each ABI really comes in by, for the reason `waitid_tests`
/// gives: 259 through the i386 translation table for a 32-bit process, and
/// the native number through the native dispatcher for a native one.
async fn timer_create_as(sys: &mut Syscall<'_>, abi: Abi, args: [usize; 6]) -> isize {
    match abi {
        Abi::Linux32 => sys.compat32_syscall(I386_TIMER_CREATE, args).await,
        _ => sys.syscall(Sys::TIMER_CREATE as u32, args).await,
    }
}

/// **A 32-bit process creates a timer whose `sigevent` is read at the offsets
/// a 32-bit program wrote it at.**
///
/// Three mutations die here, each with this name on it:
///
/// - the entry for 259 out of `i386_abi_narrow.in`, which puts the call back
///   to `ENOSYS` --- which is what a 32-bit program got until this tanda;
/// - `sigevent_get` reading the native offsets whatever the ABI, which takes
///   the caller's `sigev_notify` for its `sigev_signo`: those four bytes are
///   zero, so `good_sigevent()` answers `EINVAL`;
/// - and the same read done typed through a `UserInPtr<usize>` at `sevp`,
///   which refuses this address for the alignment.
#[test]
fn a_32_bit_process_creates_a_timer_at_its_own_sigevent_offsets() {
    let proc = a_process(48_550, Abi::Linux32);
    let rc = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(SEV_AT, &narrow_sigevent(NOTIFY_SIGNO, SIGEV_SIGNAL, 0));
            timer_create_as(
                &mut sys,
                Abi::Linux32,
                [CLOCK_MONOTONIC, SEV_AT, ID_AT, 0, 0, 0],
            )
            .await
        })
    });
    assert_ne!(
        rc, ENOSYS,
        "259 is not in the i386 table: a 32-bit program cannot create a \
         POSIX timer at all"
    );
    assert_ne!(
        rc, EINVAL,
        "the sigevent was read at the native offsets, so the signal came out \
         of the caller's sigev_notify and is 0"
    );
    assert_eq!(rc, 0, "259 from a 32-bit process");
}

/// **A native process still reads its `sigevent` at the native offsets.**
///
/// The mirror of the test above, and the one that catches the conversion
/// being applied to everybody: read at the i386 offsets, this buffer's
/// `sigev_signo` is the low half of its `sival`, which is zero, and the call
/// would be `EINVAL`.
#[test]
fn a_native_process_still_reads_the_native_offsets() {
    let proc = a_process(48_551, Abi::Linux);
    let rc = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(SEV_AT_WIDE, &native_sigevent(NOTIFY_SIGNO, SIGEV_SIGNAL, 0));
            timer_create_as(
                &mut sys,
                Abi::Linux,
                [CLOCK_MONOTONIC, SEV_AT_WIDE, ID_AT, 0, 0, 0],
            )
            .await
        })
    });
    assert_eq!(rc, 0, "the native 259 with a native sigevent");
}

/// **The thread id comes from the offset the caller wrote it at.**
///
/// `SIGEV_THREAD_ID` is the one notify that reads the fourth field, and
/// `good_sigevent()` demands it name a thread of the caller's own process. A
/// 32-bit caller writes it at byte 12; read at the native offset 16 it is
/// zero, which is `EINVAL`. So the id goes at 12 and nowhere else, and the
/// native offset is left zero on purpose: if the read moves, the call fails.
#[test]
fn the_thread_id_of_a_32_bit_caller_is_read_at_byte_twelve() {
    let proc = a_process(48_552, Abi::Linux32);
    let tid = proc.threads()[0].id();
    let rc = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(
                SEV_AT,
                &narrow_sigevent(NOTIFY_SIGNO, SIGEV_THREAD_ID, tid as i32),
            );
            timer_create_as(
                &mut sys,
                Abi::Linux32,
                [CLOCK_MONOTONIC, SEV_AT, ID_AT, 0, 0, 0],
            )
            .await
        })
    });
    assert_ne!(
        rc, EINVAL,
        "the thread id was read at the native offset 16, which is zero here"
    );
    assert_eq!(rc, 0, "SIGEV_THREAD_ID naming a thread of its own process");
}

/// **A `sival_ptr` coming in from a 32-bit process widens with zeros, not
/// with sign.**
///
/// This is the one thing a test entering by number cannot see: the value is
/// only ever handed back out through `to_i386_bytes`, which truncates it to
/// its low four bytes again, so a sign-extending read would close the round
/// trip just as well --- the lesson of the tanda 24. So it is checked here,
/// on the widened value, which is where it is observable at all.
///
/// `0x8000_0000` means two gigabytes up to a 32-bit program. Sign-extended it
/// would be `0xffff_ffff_8000_0000`, an address in no user map, which is what
/// `compat_ptr()` exists to prevent.
#[test]
fn the_sival_of_a_32_bit_caller_widens_with_zeros() {
    let proc = a_process(48_553, Abi::Linux32);
    let (narrow, native) = on_a_thread(&proc, |_sys| {
        Box::pin(async move {
            put(SEV_AT, &narrow_sigevent(NOTIFY_SIGNO, SIGEV_SIGNAL, 0));
            put(SEV_AT_WIDE, &native_sigevent(NOTIFY_SIGNO, SIGEV_SIGNAL, 0));
            (
                sigevent_get(true, SEV_AT).expect("mapped above"),
                sigevent_get(false, SEV_AT_WIDE).expect("mapped above"),
            )
        })
    });
    assert_eq!(
        narrow.value, SIVAL as usize,
        "sign-extended, this is 0xffff_ffff_8000_0000 and names no user page"
    );
    assert_eq!(narrow.signo, NOTIFY_SIGNO, "sigev_signo at byte 4");
    assert_eq!(narrow.notify, SIGEV_SIGNAL, "sigev_notify at byte 8");
    // And the native reading of the native image agrees about the value,
    // which is what says the two paths mean the same number by it.
    assert_eq!(native.value, SIVAL as usize, "the native sigev_value");
    assert_eq!(native.signo, NOTIFY_SIGNO, "sigev_signo at byte 8");
}
