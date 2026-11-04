//! `rt_sigqueueinfo` (178) and `rt_tgsigqueueinfo` (335): the first calls of
//! this layer that read a `siginfo_t` **inwards**.
//!
//! Everything about the structure is the same fact as the 177 of the tanda 26
//! --- 128 bytes at both widths, union at byte 12 against byte 16, 4-aligned
//! on i386 --- but the conversion had to be written backwards, and a
//! conversion that runs backwards has one failure the forward one does not:
//! the SIGN a pointer widens with. `si_value` is a `compat_uptr_t`, and
//! `compat_ptr()` casts an **unsigned** int.
//!
//! ## What these tests can and cannot see
//!
//! The only way to read back what the kernel stored, from a test that enters
//! by NUMBER, is to let the signal be delivered and dequeue it with 177. That
//! round trip is deliberately **not** asked to catch the sign: going in, a
//! sign-extending bug stores `0xffff_ffff_8000_0000`; coming out,
//! `to_i386_bytes` truncates to the low four bytes and hands back
//! `0x8000_0000` again. The round trip closes either way, so asking it to
//! prove the sign would be a check its own input already satisfies.
//!
//! The sign is checked where it is observable, on the native bytes, in
//! `linux_object::signal::siginfo32`'s own tests. What the round trip here
//! CAN see is the offset: read straight across, the caller's `si_uid` becomes
//! its `si_pid` and comes back in the wrong one of the two. So `si_pid` and
//! `si_uid` are given different values on purpose.
//!
//! Fixed pids live in 48_53x.

use super::super::*;
use super::tests::{as_mut, get, put, word};
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::error::LxError;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::siginfo32::SIGINFO_LEN;
use linux_object::signal::Signal;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// The three numbers, which are the native ones too.
const RT_SIGQUEUEINFO: u32 = 178;
const RT_TGSIGQUEUEINFO: u32 = 335;
const RT_SIGTIMEDWAIT: u32 = 177;
const RT_SIGPROCMASK: u32 = 175;

const SIG_BLOCK: usize = 0;
const SIGSETSIZE: usize = 8;

const EFAULT: isize = -(LxError::EFAULT as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

const USER_AT: usize = 0x2800_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// The `siginfo_t` going IN, at an address a 32-bit program's own can have:
/// 4 modulo 8, because its union is 4-aligned there.
const IN_AT: usize = USER_AT + 0x104;
/// And the one coming back OUT, 4 mod 8 as well.
const OUT_AT: usize = USER_AT + 0x304;
/// The waited-for `sigset_t`, 4 mod 8.
const SET_AT: usize = USER_AT + 0x504;
/// The mask to block with, likewise.
const MASK_AT: usize = USER_AT + 0x604;
/// And the 8-ALIGNED set, for the native half: natively a `siginfo_t` and a
/// `sigset_t` really are 8-aligned, so using the 4-aligned ones there would
/// fail for the alignment and say nothing about the conversion.
const IN_AT_WIDE: usize = USER_AT + 0x800;
const OUT_AT_WIDE: usize = USER_AT + 0x900;
const SET_AT_WIDE: usize = USER_AT + 0xa00;
const MASK_AT_WIDE: usize = USER_AT + 0xb00;

/// A real-time signal, so that the layout is `_rt`: two `int`s and then a
/// `sigval`, which is the one layout whose union holds a POINTER.
const QUEUED: Signal = Signal::SIGRT34;

/// `SI_QUEUE`, the code `sigqueue(3)` sends and the only one a process is
/// allowed to forge at itself.
const SI_QUEUE: i32 = -1;

/// Deliberately different from each other, so that taking one for the other
/// is visible. See the module note.
const SENDER_PID: i32 = 0x1111_2222;
const SENDER_UID: i32 = 0x3333_4444;
/// With the top bit set, which is the value a sign-extending widening would
/// turn into a kernel address. The round trip cannot see that (module note),
/// but it is the value a 32-bit program really can send.
const SENDER_VALUE: u32 = 0x8000_0000;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "queueinfo", lp).unwrap();
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

/// The 128 bytes a 32-bit process would hand to `rt_sigqueueinfo`: the three
/// `int`s, then `_rt`'s two `int`s and its `sigval` from byte 12 on.
fn narrow_in() -> [u8; SIGINFO_LEN] {
    let mut b = [0u8; SIGINFO_LEN];
    b[0..4].copy_from_slice(&(QUEUED as i32).to_ne_bytes());
    b[4..8].copy_from_slice(&0i32.to_ne_bytes());
    b[8..12].copy_from_slice(&SI_QUEUE.to_ne_bytes());
    b[12..16].copy_from_slice(&SENDER_PID.to_ne_bytes());
    b[16..20].copy_from_slice(&SENDER_UID.to_ne_bytes());
    b[20..24].copy_from_slice(&SENDER_VALUE.to_ne_bytes());
    b
}

/// And the native shape of the same thing, for the native half: union at byte
/// 16, and the `sigval` eight bytes wide behind the two `int`s.
fn native_in() -> [u8; SIGINFO_LEN] {
    let mut b = [0u8; SIGINFO_LEN];
    b[0..4].copy_from_slice(&(QUEUED as i32).to_ne_bytes());
    b[4..8].copy_from_slice(&0i32.to_ne_bytes());
    b[8..12].copy_from_slice(&SI_QUEUE.to_ne_bytes());
    b[16..20].copy_from_slice(&SENDER_PID.to_ne_bytes());
    b[20..24].copy_from_slice(&SENDER_UID.to_ne_bytes());
    b[24..32].copy_from_slice(&(SENDER_VALUE as u64).to_ne_bytes());
    b
}

/// The bit of [`QUEUED`], as the eight bytes a `sigset_t` is at either width.
fn one_signal() -> [u8; 8] {
    (1u64 << (QUEUED as u64 - 1)).to_ne_bytes()
}

/// **A 32-bit process queues a `siginfo_t` to itself and gets the same one
/// back.**
///
/// Four mutations die here, each with this name on it:
///
/// - the entry for 178 out of `i386_abi_narrow.in`, which puts the call back
///   to `ENOSYS`;
/// - the entry for 177, likewise;
/// - `siginfo_get` reverted to the typed `info.read()`, which answers
///   `EFAULT` at an address 4 mod 8;
/// - `from_i386_bytes` reverted to a straight copy of the 128 bytes, which
///   brings the caller's `si_uid` back as its `si_pid`.
#[test]
fn a_32_bit_process_queues_a_siginfo_to_itself_and_reads_the_same_one_back() {
    let proc = a_process(48_530, Abi::Linux32);
    let (blocked, queued, waited, out) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // Block it first, so that the queued signal waits in the pending
            // set instead of being delivered to a handler this process has
            // none of.
            put(MASK_AT, &one_signal());
            let blocked = sys
                .compat32_syscall(RT_SIGPROCMASK, [SIG_BLOCK, MASK_AT, 0, SIGSETSIZE, 0, 0])
                .await;
            put(IN_AT, &narrow_in());
            let queued = sys
                .compat32_syscall(RT_SIGQUEUEINFO, [48_530, QUEUED as usize, IN_AT, 0, 0, 0])
                .await;
            // And dequeue it, which writes the `siginfo_t` back out at the
            // process's own width.
            put(SET_AT, &one_signal());
            put(OUT_AT, &[0xAAu8; SIGINFO_LEN]);
            let waited = sys
                .compat32_syscall(RT_SIGTIMEDWAIT, [SET_AT, OUT_AT, 0, SIGSETSIZE, 0, 0])
                .await;
            (blocked, queued, waited, get(OUT_AT, SIGINFO_LEN))
        })
    });
    assert_eq!(blocked, 0, "175 blocking the signal first");
    assert_ne!(
        queued, ENOSYS,
        "178 is not in the i386 table: a 32-bit program cannot queue a \
         siginfo at all"
    );
    assert_ne!(
        queued, EFAULT,
        "178 read its siginfo typed, which refuses an address 4 mod 8"
    );
    assert_eq!(queued, 0, "178 from a 32-bit process");
    assert_eq!(waited, QUEUED as isize, "177 dequeued the signal");
    // The three `int`s, which no conversion touches.
    assert_eq!(word(&out, 0), QUEUED as u32, "si_signo");
    assert_eq!(word(&out, 8) as i32, SI_QUEUE, "si_code");
    // And the union, at the offset a 32-bit program reads it from. This is
    // the pair the straight copy gets wrong.
    assert_eq!(
        word(&out, 12) as i32,
        SENDER_PID,
        "si_pid; the caller's si_uid here means the union was copied across \
         without moving"
    );
    assert_eq!(word(&out, 16) as i32, SENDER_UID, "si_uid");
    assert_eq!(
        word(&out, 20),
        SENDER_VALUE,
        "si_value, the low four bytes of the pointer"
    );
    proc.exit(0);
}

/// `rt_tgsigqueueinfo` (335) reads it the same way.
///
/// Its own entry in the table, and its own `siginfo_get` call: the 178's
/// tests would all pass with the 335 left typed.
#[test]
fn rt_tgsigqueueinfo_reads_its_siginfo_at_the_callers_width_too() {
    let proc = a_process(48_531, Abi::Linux32);
    let (queued, waited, out) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let tid = sys.thread.id() as usize;
            put(MASK_AT, &one_signal());
            sys.compat32_syscall(RT_SIGPROCMASK, [SIG_BLOCK, MASK_AT, 0, SIGSETSIZE, 0, 0])
                .await;
            put(IN_AT, &narrow_in());
            // `tgkill`'s shape: the thread GROUP and then the thread.
            let queued = sys
                .compat32_syscall(
                    RT_TGSIGQUEUEINFO,
                    [48_531, tid, QUEUED as usize, IN_AT, 0, 0],
                )
                .await;
            put(SET_AT, &one_signal());
            put(OUT_AT, &[0xAAu8; SIGINFO_LEN]);
            let waited = sys
                .compat32_syscall(RT_SIGTIMEDWAIT, [SET_AT, OUT_AT, 0, SIGSETSIZE, 0, 0])
                .await;
            (queued, waited, get(OUT_AT, SIGINFO_LEN))
        })
    });
    assert_ne!(queued, ENOSYS, "335 is not in the i386 table");
    assert_ne!(queued, EFAULT, "335 read its siginfo typed");
    assert_eq!(queued, 0, "335 from a 32-bit process");
    assert_eq!(waited, QUEUED as isize, "177 dequeued it");
    assert_eq!(word(&out, 12) as i32, SENDER_PID, "si_pid");
    assert_eq!(word(&out, 16) as i32, SENDER_UID, "si_uid");
    proc.exit(0);
}

/// **A native process still reads a native `siginfo_t`.**
///
/// Without this, a `siginfo_get` that converted unconditionally would pass
/// both tests above. It is the same guard the 177 needed, and it is the whole
/// reason the native bytes are built by hand here: the union four bytes
/// further along, and an eight-byte `sigval`.
#[test]
fn a_native_process_still_hands_in_a_native_siginfo() {
    let proc = a_process(48_532, Abi::Linux);
    let (queued, waited, out) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(MASK_AT_WIDE, &one_signal());
            sys.sys_rt_sigprocmask(SIG_BLOCK as i32, MASK_AT_WIDE.into(), 0.into(), SIGSETSIZE)
                .unwrap();
            put(IN_AT_WIDE, &native_in());
            let queued = sys.sys_rt_sigqueueinfo(48_532, QUEUED as usize, IN_AT_WIDE.into());
            put(SET_AT_WIDE, &one_signal());
            put(OUT_AT_WIDE, &[0xAAu8; SIGINFO_LEN]);
            let waited = sys
                .sys_rt_sigtimedwait(SET_AT_WIDE.into(), OUT_AT_WIDE.into(), 0.into(), SIGSETSIZE)
                .await;
            (queued, waited, get(OUT_AT_WIDE, SIGINFO_LEN))
        })
    });
    assert_eq!(queued, Ok(0), "rt_sigqueueinfo from a native process");
    assert_eq!(waited, Ok(QUEUED as usize), "and the wait dequeued it");
    // The native offsets: union at 16, `sigval` eight bytes wide.
    assert_eq!(word(&out, 16) as i32, SENDER_PID, "si_pid, natively");
    assert_eq!(word(&out, 20) as i32, SENDER_UID, "si_uid, natively");
    assert_eq!(
        word(&out, 24),
        SENDER_VALUE,
        "si_value's low half, natively"
    );
    proc.exit(0);
}
