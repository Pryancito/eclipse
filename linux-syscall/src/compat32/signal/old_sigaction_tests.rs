//! The pre-POSIX `sigaction` (67), whose `struct old_sigaction` is sixteen
//! bytes with its members in ANOTHER ORDER.
//!
//! Not the twenty of `compat_sigaction` with a word taken off: the mask is
//! one `u32` and it is the SECOND member, so three of the four fields are at
//! a different offset than in the call next door. Everything here was
//! measured on a Linux host first, with an i386 program that installs an
//! action through 67 and reads it back through 174 and the other way round;
//! the comments say which number each measurement came from.
//!
//! The number answered `ENOSYS` before this batch, so a libc that installs
//! its handlers with 67 --- which is what `signal(2)`'s "original" call is
//! --- got nothing at all. Every call here goes through `compat32_syscall`
//! **by number** so that deleting the arm fails a test instead of restoring
//! that `ENOSYS` quietly.
//!
//! Fixed pids live in 48_61x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::{align_of, offset_of, size_of};
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::Signal;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's numbers. 67 has no native twin at all.
const SIGACTION: u32 = 67;
const RT_SIGACTION: u32 = 174;
const SIGALTSTACK: u32 = 186;

const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

const USER_AT: usize = 0x3100_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// Where an action goes in and where the old one comes back, both 4-aligned,
/// which is what a 32-bit libc really passes.
const ACT_AT: usize = USER_AT;
const OLD_AT: usize = USER_AT + 0x100;
/// And the same pair at **1 modulo 4**, which is what Linux accepts and the
/// typed read and write refuse. Measured: 67, 174 and 186 all answer 0 for a
/// structure one byte off a word boundary.
const ACT_AT_ODD: usize = USER_AT + 0x201;
const OLD_AT_ODD: usize = USER_AT + 0x301;

/// The handler, the restorer and the mask the measurements used: a plain
/// handler with `SA_RESTORER` and no `SA_SIGINFO`, which is the action musl's
/// i386 `__libc_sigaction` builds for `signal(3)`.
const HANDLER: u32 = 0x0804_8120;
const RESTORER: u32 = 0x0804_9000;
/// SIGUSR2 (12) in the low word, so the mask names a signal below 32 --- the
/// only ones this layout can name.
const LOW_MASK: u32 = 1 << (Signal::SIGUSR2 as u32 - 1);
/// And a realtime signal for the half that does NOT fit: signal 40.
const HIGH_MASK: u32 = 1 << (40 - 32 - 1);

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "oldsigaction", lp).unwrap();
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

/// The sixteen bytes of a `struct old_sigaction`, in the order an i386
/// program writes them: handler, **mask**, flags, restorer.
fn old_action_bytes(handler: u32, mask: u32, flags: u32, restorer: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&handler.to_ne_bytes());
    v.extend_from_slice(&mask.to_ne_bytes());
    v.extend_from_slice(&flags.to_ne_bytes());
    v.extend_from_slice(&restorer.to_ne_bytes());
    v
}

/// And the twenty of the `rt_` one, whose mask is last and is two words.
fn narrow_action_bytes(handler: u32, flags: u32, restorer: u32, mask: [u32; 2]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&handler.to_ne_bytes());
    v.extend_from_slice(&flags.to_ne_bytes());
    v.extend_from_slice(&restorer.to_ne_bytes());
    v.extend_from_slice(&mask[0].to_ne_bytes());
    v.extend_from_slice(&mask[1].to_ne_bytes());
    v
}

/// `SA_RESTORER`, the flag musl sets and the one that says the trampoline in
/// `sa_restorer` is the one to use.
fn restorer_flag() -> u32 {
    SignalActionFlags::RESTORER.bits() as u32
}

// ---------------------------------------------------------------------------
// The structure
// ---------------------------------------------------------------------------

#[test]
fn an_old_sigaction_is_sixteen_bytes_with_the_mask_second() {
    type A = OldSignalAction32;
    assert_eq!(size_of::<A>(), 16, "compat_old_sigaction is sixteen bytes");
    assert_eq!(offset_of!(A, handler), 0);
    assert_eq!(offset_of!(A, mask), 4, "the mask is the SECOND member");
    assert_eq!(offset_of!(A, flags), 8);
    assert_eq!(offset_of!(A, restorer), 12);
    assert_eq!(align_of::<A>(), 4);
    // And it is not the `rt_` layout with a word off: there the mask is last
    // and the flags are where this one's mask is.
    assert_eq!(offset_of!(SignalAction32, flags), 4);
    assert_eq!(offset_of!(SignalAction32, mask), 12);
    assert_eq!(size_of::<SignalAction32>(), 20);
}

#[test]
fn the_conversion_widens_the_mask_with_zeros_and_drops_the_high_half() {
    // `siginitset(&new_ka.sa.sa_mask, mask)` inwards, and
    // `put_user(old_ka.sa.sa_mask.sig[0], ...)` outwards.
    let native = OldSignalAction32 {
        handler: HANDLER,
        mask: LOW_MASK,
        flags: restorer_flag(),
        restorer: RESTORER,
    }
    .to_native();
    assert_eq!(native.mask.val(), LOW_MASK as u64, "nothing above bit 31");
    assert_eq!(native.handler as u32, HANDLER);
    assert_eq!(native.restorer as u32, RESTORER);

    // And a mask with a realtime signal in it comes back with that half
    // gone, without an error: there is nowhere in sixteen bytes for it.
    let wide = SignalAction32 {
        handler: HANDLER,
        flags: restorer_flag(),
        restorer: RESTORER,
        mask: [LOW_MASK, HIGH_MASK],
    }
    .to_native();
    let back = OldSignalAction32::from_native(wide);
    assert_eq!(back.mask, LOW_MASK);
    assert_eq!(back.flags, restorer_flag());
}

// ---------------------------------------------------------------------------
// The call
// ---------------------------------------------------------------------------

/// **A 32-bit program installs a handler through number 67.**
///
/// The readback is through 174, which reads the action the kernel kept: that
/// is what says the sixteen bytes were read at the offsets above. A kernel
/// that read them in the `rt_` order would report `LOW_MASK` as the flags
/// and 0 as the restorer, and both of those are checked.
#[test]
fn a_32_bit_program_installs_a_handler_through_number_67() {
    let proc = a_process(48_610, Abi::Linux32);
    let (installed, old) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // Prime the half that 67 cannot name, through the call that can:
            // signal 40 blocked during the handler.
            put(
                ACT_AT,
                &narrow_action_bytes(HANDLER, restorer_flag(), RESTORER, [0, HIGH_MASK]),
            );
            let primed = sys
                .compat32_syscall(RT_SIGACTION, [Signal::SIGUSR1 as usize, ACT_AT, 0, 8, 0, 0])
                .await;
            assert_eq!(primed, 0);

            put(
                ACT_AT,
                &old_action_bytes(HANDLER, LOW_MASK, restorer_flag(), RESTORER),
            );
            let installed = sys
                .compat32_syscall(SIGACTION, [Signal::SIGUSR1 as usize, ACT_AT, 0, 0, 0, 0])
                .await;
            put(OLD_AT, &[0xAAu8; 32]);
            let read_back = sys
                .compat32_syscall(RT_SIGACTION, [Signal::SIGUSR1 as usize, 0, OLD_AT, 8, 0, 0])
                .await;
            assert_eq!(read_back, 0);
            (installed, get(OLD_AT, 32))
        })
    });
    assert_eq!(installed, 0, "number 67 is still not wired");
    assert_ne!(installed as isize, ENOSYS);
    assert_eq!(word(&old, 0), HANDLER, "sa_handler");
    assert_eq!(
        word(&old, 4),
        restorer_flag(),
        "sa_flags came out of the mask's word"
    );
    assert_eq!(word(&old, 8), RESTORER, "sa_restorer");
    assert_eq!(word(&old, 12), LOW_MASK, "the low half of sa_mask");
    assert_eq!(
        word(&old, 16),
        0,
        "the high half was not zeroed: 67 REPLACES the mask, it does not keep what it cannot name"
    );
    proc.exit(0);
}

/// **Reading through 67 writes sixteen bytes and drops the high half.**
///
/// The action goes in through 174 with a realtime signal in its mask, so
/// there is something in that half to lose, and the call still answers 0.
#[test]
fn reading_through_67_writes_sixteen_bytes_and_drops_what_does_not_fit() {
    let proc = a_process(48_611, Abi::Linux32);
    let (read_back, old) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(
                ACT_AT,
                &narrow_action_bytes(HANDLER, restorer_flag(), RESTORER, [LOW_MASK, HIGH_MASK]),
            );
            let installed = sys
                .compat32_syscall(RT_SIGACTION, [Signal::SIGUSR1 as usize, ACT_AT, 0, 8, 0, 0])
                .await;
            assert_eq!(installed, 0);
            put(OLD_AT, &[0xAAu8; 24]);
            let read_back = sys
                .compat32_syscall(SIGACTION, [Signal::SIGUSR1 as usize, 0, OLD_AT, 0, 0, 0])
                .await;
            (read_back, get(OLD_AT, 24))
        })
    });
    assert_eq!(read_back, 0);
    assert_eq!(word(&old, 0), HANDLER, "sa_handler");
    assert_eq!(word(&old, 4), LOW_MASK, "sa_mask is the SECOND member");
    assert_eq!(word(&old, 8), restorer_flag(), "sa_flags");
    assert_eq!(word(&old, 12), RESTORER, "sa_restorer");
    assert!(
        old[16..].iter().all(|&b| b == 0xAA),
        "the old action was written as more than sixteen bytes: {:x?}",
        &old[16..]
    );
    proc.exit(0);
}

/// **Number 67 takes three arguments, so there is no `sigsetsize` to get
/// wrong.**
///
/// `compat_sys_sigaction` has three parameters and `do_sigaction` makes no
/// such check; the size the entry point hands `sigaction_with` is the one
/// that passes. A fourth register full of rubbish must therefore change
/// nothing --- measured: an i386 program's `int $0x80` leaves whatever was
/// in `%esi` there, and Linux answers 0.
#[test]
fn a_fourth_argument_is_not_read_as_a_sigsetsize() {
    let proc = a_process(48_612, Abi::Linux32);
    let (zero, four, rubbish) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(
                ACT_AT,
                &old_action_bytes(HANDLER, LOW_MASK, restorer_flag(), RESTORER),
            );
            let sig = Signal::SIGUSR2 as usize;
            (
                sys.compat32_syscall(SIGACTION, [sig, ACT_AT, 0, 0, 0, 0])
                    .await,
                sys.compat32_syscall(SIGACTION, [sig, ACT_AT, 0, 4, 0, 0])
                    .await,
                sys.compat32_syscall(SIGACTION, [sig, ACT_AT, 0, 0xdead_beef, 0, 0])
                    .await,
            )
        })
    });
    assert_eq!((zero, four, rubbish), (0, 0, 0));
    // The pair that makes that measure something: the `rt_` call DOES read
    // the fourth argument, and 4 is not eight.
    let proc2 = a_process(48_613, Abi::Linux32);
    let four_is_refused = on_a_thread(&proc2, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(
                ACT_AT,
                &narrow_action_bytes(HANDLER, restorer_flag(), RESTORER, [LOW_MASK, 0]),
            );
            sys.compat32_syscall(RT_SIGACTION, [Signal::SIGUSR2 as usize, ACT_AT, 0, 4, 0, 0])
                .await
        })
    });
    assert_eq!(four_is_refused, EINVAL);
    proc.exit(0);
    proc2.exit(0);
}

/// **The checks are `sigaction_with`'s, through this entry point too.**
///
/// A disposition for `SIGKILL` is refused and asking for one is allowed, and
/// signal 0 is out of range: none of that is copied here, which is the point
/// of the split.
#[test]
fn the_shared_checks_hold_through_number_67() {
    let proc = a_process(48_614, Abi::Linux32);
    let (kill, stop, reading_kill, zero) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(
                ACT_AT,
                &old_action_bytes(HANDLER, LOW_MASK, restorer_flag(), RESTORER),
            );
            (
                sys.compat32_syscall(SIGACTION, [Signal::SIGKILL as usize, ACT_AT, 0, 0, 0, 0])
                    .await,
                sys.compat32_syscall(SIGACTION, [Signal::SIGSTOP as usize, ACT_AT, 0, 0, 0, 0])
                    .await,
                sys.compat32_syscall(SIGACTION, [Signal::SIGKILL as usize, 0, OLD_AT, 0, 0, 0])
                    .await,
                sys.compat32_syscall(SIGACTION, [0, ACT_AT, 0, 0, 0, 0])
                    .await,
            )
        })
    });
    assert_eq!((kill, stop), (EINVAL, EINVAL));
    assert_eq!(reading_kill, 0);
    assert_eq!(zero, EINVAL, "signal 0 is not a signal");
    proc.exit(0);
}

/// **A native process has no number 67 at all.**
///
/// x86_64's 67 is `shmdt`, which is why this call cannot be a row in either
/// allowlist: a row would send sixteen bytes of `old_sigaction` to it.
#[test]
fn number_67_is_the_32_bit_entry_and_not_a_row_in_the_table() {
    assert_eq!(
        crate::compat32::native_number(SIGACTION),
        None,
        "67 must not have a row in the allowlist: it is answered by the dispatch"
    );
}

/// **67 reads and writes its sixteen bytes one byte off a word.**
///
/// Linux's `copy_from_user` and `put_user` ask `access_ok` and nothing about
/// alignment. Measured on a Linux host: 67 answers 0 for an action at 1
/// modulo 4, in and out, where the typed read and write of `UserPtr` answer
/// `EFAULT`.
#[test]
fn number_67_is_read_and_written_at_any_offset() {
    let proc = a_process(48_615, Abi::Linux32);
    let (installed, read_back, landed) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let sig = Signal::SIGUSR1 as usize;
            put(
                ACT_AT_ODD,
                &old_action_bytes(HANDLER, LOW_MASK, restorer_flag(), RESTORER),
            );
            let installed = sys
                .compat32_syscall(SIGACTION, [sig, ACT_AT_ODD, 0, 0, 0, 0])
                .await;
            let read_back = sys
                .compat32_syscall(SIGACTION, [sig, 0, OLD_AT_ODD, 0, 0, 0])
                .await;
            // The readback is taken HERE, inside the closure: the pages are
            // the process's own and only this thread has its address space
            // on. Reading them after `on_a_thread` has returned looked fine
            // run alone and took the whole binary down with SIGSEGV once
            // another test had switched the aspace in between.
            (installed, read_back, get(OLD_AT_ODD, 4))
        })
    });
    assert_eq!(installed, 0, "67 refused an action at 1 modulo 4");
    assert_eq!(read_back, 0, "67 refused a destination at 1 modulo 4");
    // And the readback really landed there, which a kernel that quietly
    // rounded the pointer would miss.
    assert_eq!(word(&landed, 0), HANDLER);
    proc.exit(0);
}

/// **And so do the other two narrow signal structures.**
///
/// The same measurement for 174's twenty bytes and 186's twelve: both
/// answer 0 at 1 modulo 4 on a Linux host, and both used the typed pair
/// here. One test, because it is one decision.
#[test]
fn the_other_two_narrow_signal_structures_are_too() {
    let proc = a_process(48_616, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let sig = Signal::SIGUSR1 as usize;
            let mut out = Vec::new();
            put(
                ACT_AT_ODD,
                &narrow_action_bytes(HANDLER, restorer_flag(), RESTORER, [LOW_MASK, 0]),
            );
            out.push((
                "174 in",
                sys.compat32_syscall(RT_SIGACTION, [sig, ACT_AT_ODD, 0, 8, 0, 0])
                    .await,
            ));
            out.push((
                "174 out",
                sys.compat32_syscall(RT_SIGACTION, [sig, 0, OLD_AT_ODD, 8, 0, 0])
                    .await,
            ));
            // `sigaltstack`'s twelve bytes: a stack the process owns, with
            // the structure describing it one byte off a word.
            let mut ss = Vec::new();
            ss.extend_from_slice(&((USER_AT + 0x1000) as u32).to_ne_bytes());
            ss.extend_from_slice(&0u32.to_ne_bytes());
            ss.extend_from_slice(&0x800u32.to_ne_bytes());
            put(ACT_AT_ODD, &ss);
            out.push((
                "186 in",
                sys.compat32_syscall(SIGALTSTACK, [ACT_AT_ODD, 0, 0, 0, 0, 0])
                    .await,
            ));
            out.push((
                "186 out",
                sys.compat32_syscall(SIGALTSTACK, [0, OLD_AT_ODD, 0, 0, 0, 0])
                    .await,
            ));
            out
        })
    });
    for (which, ret) in got {
        assert_eq!(ret, 0, "{} refused a structure at 1 modulo 4", which);
    }
    proc.exit(0);
}
