//! The three calls that pass a `sigset_t` and nothing else: `rt_sigprocmask`
//! (175), `rt_sigpending` (176) and `rt_sigsuspend` (179).
//!
//! A `sigset_t` is **eight bytes in both ABIs** --- `_NSIG` is 64 either way
//! --- so nothing about these is a halving. The only thing that differs is the
//! ALIGNMENT: on i386 it is `unsigned long sig[2]` and aligns to four, so a
//! 32-bit program's own can sit at an address congruent to 4 modulo 8, which
//! `UserPtr`'s typed read and write refuse. That is the third form of the
//! width trap, the one where the bytes are the same and only the address
//! moves.
//!
//! The three were in NEITHER i386 allowlist, so for a 32-bit process they
//! answered `ENOSYS`: **a 32-bit program could not block a signal at all.**
//! The probe's step 84 found it by being unable to reach its first `kill`.
//!
//! Every call here goes through `compat32_syscall` BY NUMBER, so that
//! deleting the table entry fails a test rather than leaving everything green
//! with `ENOSYS` restored.
//!
//! Fixed pids live in 48_52x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::Signal;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's numbers, which are the native ones too.
const RT_SIGPROCMASK: u32 = 175;
const RT_SIGPENDING: u32 = 176;
const RT_SIGSUSPEND: u32 = 179;

const SIG_BLOCK: usize = 0;

const EFAULT: isize = -(LxError::EFAULT as isize);
const EINTR: isize = -(LxError::EINTR as isize);

const USER_AT: usize = 0x2700_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// A mask at an address a 32-bit program's own can have: **4 modulo 8**. This
/// is the address the typed read and write refuse and Linux accepts.
const MASK_AT: usize = USER_AT + 0x104;
/// Where the old mask is asked for, 4 mod 8 as well.
const OLD_AT: usize = USER_AT + 0x204;
/// And an 8-ALIGNED pair, for the native half: natively a `sigset_t` really is
/// 8-aligned, so the typed read and write are right to insist there, and it is
/// only a 32-bit program whose own is not.
const MASK_AT_WIDE: usize = USER_AT + 0x300;
const OLD_AT_WIDE: usize = USER_AT + 0x400;

/// `sizeof(sigset_t)`, eight in both ABIs.
const SIGSETSIZE: usize = 8;

/// The signal every test blocks: a real-time one, so no default action of the
/// process's can interfere with what is being measured.
const BLOCKED: Signal = Signal::SIGRT34;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "sigmask", lp).unwrap();
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
    p.write_array(bytes).expect("mapped and writable");
}

fn get(at: usize, len: usize) -> Vec<u8> {
    let p: kernel_hal::user::UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped and readable")
}

/// The bit of [`BLOCKED`], as the eight bytes a `sigset_t` is.
fn one_signal() -> [u8; 8] {
    (1u64 << (BLOCKED as u64 - 1)).to_ne_bytes()
}

/// The mask as a `u64`, read back out of user memory.
fn mask_at(at: usize) -> u64 {
    let b = get(at, 8);
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[..8]);
    u64::from_ne_bytes(w)
}

/// **A 32-bit process blocks a signal with its mask 4 mod 8, and reads it
/// back at an address 4 mod 8 too.**
///
/// Before this batch the two answers were `ENOSYS`, because 175 was in
/// neither allowlist. With the entry but without the unaligned read they
/// would be `EFAULT`, because a `Sigset` is a `u64` and the typed read wants
/// eight. Both of those are what a 32-bit program got where Linux serves it.
#[test]
fn a_32_bit_process_blocks_a_signal_with_its_mask_four_aligned() {
    let proc = a_process(48_520, Abi::Linux32);
    let (blocked, read_back, old) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = super::tests::as_mut(sys);
            put(MASK_AT, &one_signal());
            put(OLD_AT, &[0xAAu8; 8]);
            let blocked = sys
                .compat32_syscall(
                    RT_SIGPROCMASK,
                    [SIG_BLOCK, MASK_AT, OLD_AT, SIGSETSIZE, 0, 0],
                )
                .await;
            // Now with a null `set`, which is how a program reads its own
            // mask without changing it. The answer goes to the same kind of
            // address.
            put(OLD_AT, &[0xAAu8; 8]);
            let read_back = sys
                .compat32_syscall(RT_SIGPROCMASK, [SIG_BLOCK, 0, OLD_AT, SIGSETSIZE, 0, 0])
                .await;
            (blocked, read_back, mask_at(OLD_AT))
        })
    });
    assert_eq!(blocked, 0, "175 blocking with a mask 4 mod 8");
    assert_eq!(
        read_back, 0,
        "175 reading the mask back to an address 4 mod 8"
    );
    assert_eq!(
        old,
        1u64 << (BLOCKED as u64 - 1),
        "the mask that comes back is the one that was set, written eight \
         bytes wide: {old:#x}"
    );
    proc.exit(0);
}

/// `rt_sigpending` reports it at the same kind of address.
///
/// Pending means "sent but withheld by the mask", so this also checks that
/// the block above actually took: a signal queued after it shows up here
/// rather than being delivered.
#[test]
fn rt_sigpending_reports_a_blocked_signal_to_a_four_aligned_buffer() {
    let proc = a_process(48_521, Abi::Linux32);
    let (blocked, pending, bits) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = super::tests::as_mut(sys);
            put(MASK_AT, &one_signal());
            let blocked = sys
                .compat32_syscall(RT_SIGPROCMASK, [SIG_BLOCK, MASK_AT, 0, SIGSETSIZE, 0, 0])
                .await;
            let thread = alloc::sync::Arc::clone(sys.thread);
            crate::signal::queue_signal_to_thread(
                &thread,
                BLOCKED,
                linux_object::signal::SigInfo::queued(BLOCKED, 1, 0, 0),
            )
            .expect("room in the pending set");
            put(OLD_AT, &[0xAAu8; 8]);
            let pending = sys
                .compat32_syscall(RT_SIGPENDING, [OLD_AT, SIGSETSIZE, 0, 0, 0, 0])
                .await;
            (blocked, pending, mask_at(OLD_AT))
        })
    });
    assert_eq!(blocked, 0, "175 blocked it");
    assert_eq!(pending, 0, "176 wrote to an address 4 mod 8");
    assert_eq!(
        bits,
        1u64 << (BLOCKED as u64 - 1),
        "and what it wrote is the blocked signal, not poison: {bits:#x}"
    );
    proc.exit(0);
}

/// `rt_sigsuspend` reads its mask at the same kind of address.
///
/// It is the one of the three that blocks, so the measurable thing is which
/// error comes back: with a signal already pending that the temporary mask
/// does **not** block, the wait ends at once and `sigsuspend` owes its caller
/// `EINTR`. `EFAULT` is what the typed read gave instead, and `ENOSYS` what
/// the missing table entry gave before that --- so the three outcomes are
/// distinct and only the right one is `EINTR`.
#[test]
fn rt_sigsuspend_reads_a_four_aligned_mask_and_answers_eintr() {
    let proc = a_process(48_522, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = super::tests::as_mut(sys);
            let thread = alloc::sync::Arc::clone(sys.thread);
            crate::signal::queue_signal_to_thread(
                &thread,
                BLOCKED,
                linux_object::signal::SigInfo::queued(BLOCKED, 1, 0, 0),
            )
            .expect("room in the pending set");
            // An empty temporary mask: it blocks nothing, so the signal that
            // is already pending is deliverable and the wait ends at once.
            put(MASK_AT, &[0u8; 8]);
            sys.compat32_syscall(RT_SIGSUSPEND, [MASK_AT, SIGSETSIZE, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(
        got, EINTR,
        "179 read the mask and waited: EFAULT would be the typed read and \
         ENOSYS the missing table entry"
    );
    proc.exit(0);
}

/// **And a NATIVE process still gets the aligned read and write**, which is
/// the half the three tests above cannot see.
///
/// Reading and writing unaligned unconditionally would pass every one of
/// them. Natively a `sigset_t` is 8-aligned, so a native caller handing in an
/// address 4 mod 8 is handing in one the typed access refuses, and
/// `self.narrow_abi()` is what decides. The same call at an 8-aligned address
/// succeeds, so what this pins is the branch and not that the call is broken.
#[test]
fn a_native_process_is_still_held_to_the_aligned_access() {
    let proc = a_process(48_523, Abi::Linux);
    let (misaligned, aligned, old) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = super::tests::as_mut(sys);
            put(MASK_AT, &one_signal());
            let misaligned =
                sys.sys_rt_sigprocmask(SIG_BLOCK as i32, MASK_AT.into(), 0usize.into(), SIGSETSIZE);
            put(MASK_AT_WIDE, &one_signal());
            put(OLD_AT_WIDE, &[0xAAu8; 8]);
            let aligned = sys.sys_rt_sigprocmask(
                SIG_BLOCK as i32,
                MASK_AT_WIDE.into(),
                OLD_AT_WIDE.into(),
                SIGSETSIZE,
            );
            (misaligned, aligned, mask_at(OLD_AT_WIDE))
        })
    });
    assert_eq!(
        misaligned
            .map(|v| v as isize)
            .unwrap_or_else(|e| -(e as isize)),
        EFAULT,
        "a native mask 4 mod 8 is one the typed read refuses, and nothing \
         here asks for the unaligned one on its behalf"
    );
    assert_eq!(aligned, Ok(0), "the same call 8-aligned");
    assert_eq!(old, 0, "and the old mask it wrote was the empty one");
    proc.exit(0);
}
