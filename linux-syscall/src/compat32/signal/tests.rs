//! The two structures, and the three numbers that carry them.
//!
//! Everything process-level here goes through `compat32_syscall` **by
//! number**, not by calling `sys_rt_sigaction32` directly: what is being
//! pinned is that the arm exists. The pre-POSIX `sigaction` (67) has its own
//! file, `old_sigaction_tests`, its structure being another one. Call the handler by name and deleting the
//! arm from [`crate::compat32`] leaves every test green and every 32-bit
//! `sigaction` answering `ENOSYS` again, which is what it answered before
//! this batch.
//!
//! Fixed pids live in 48_44x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::{align_of, offset_of, size_of};
use kernel_hal::context::UserContext;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::{SigInfo, Signal, SignalStack};
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's own numbers, spelled out here: entering the layer the way a 32-bit
/// program does is the whole point.
const RT_SIGACTION: u32 = 174;
const SIGALTSTACK: u32 = 186;
const RT_SIGRETURN: u32 = 173;
/// And the pre-POSIX `sigreturn`, which returns from the OTHER i386 frame.
const SIGRETURN: u32 = 119;

const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);
const EFAULT: isize = -(LxError::EFAULT as isize);

// ---------------------------------------------------------------------------
// The structures
// ---------------------------------------------------------------------------

#[test]
fn a_32_bit_sigaction_is_twenty_bytes_where_a_native_one_is_thirty_two() {
    type A = SignalAction32;
    assert_eq!(offset_of!(A, handler), 0);
    assert_eq!(offset_of!(A, flags), 4);
    assert_eq!(offset_of!(A, restorer), 8);
    assert_eq!(offset_of!(A, mask), 12);
    assert_eq!(align_of::<A>(), 4, "nothing in i386 is 8-byte aligned");
    assert_eq!(size_of::<A>(), 20);
    assert_eq!(size_of::<SignalAction>(), 32);
}

#[test]
fn the_mask_is_the_one_member_that_does_not_change_width() {
    // So the narrow struct is 20 bytes and not 16: three members halve and
    // the fourth does not, because `_NSIG` is 64 in both ABIs. A conversion
    // written as "halve every word" would put `sa_mask` at byte 12 and read
    // only four of its eight bytes -- losing every realtime signal from the
    // mask a handler runs under.
    assert_eq!(size_of::<Sigset>(), 8);
    assert_eq!(
        size_of::<SignalAction32>() - offset_of!(SignalAction32, mask),
        8
    );
}

#[test]
fn a_sigaction_crosses_the_width_both_ways() {
    // A mask with a bit in its HIGH word: SIGRTMIN+2 is bit 36, so a
    // conversion that keeps one word of the two passes every test about
    // SIGINT and holds off the wrong signals for a realtime one.
    let narrow = SignalAction32 {
        handler: 0x0804_8120,
        flags: SignalActionFlags::SIGINFO.bits() as u32
            | SignalActionFlags::RESTORER.bits() as u32
            | SignalActionFlags::RESTART.bits() as u32,
        restorer: 0x0804_9000,
        mask: [1 << 2, 1 << 4],
    };
    let native = narrow.to_native();
    assert_eq!(native.handler, 0x0804_8120);
    assert_eq!(native.restorer, 0x0804_9000);
    assert!(native.flags.contains(SignalActionFlags::SIGINFO));
    assert!(native.flags.contains(SignalActionFlags::RESTART));
    assert_eq!(native.mask.val(), (1 << 2) | (1u64 << 36));
    assert_eq!(
        SignalAction32::from_native(native),
        narrow,
        "and back, unchanged"
    );
}

#[test]
fn the_highest_flag_a_32_bit_program_can_set_survives_the_crossing() {
    // `SA_RESETHAND` is bit 31, the top of a `compat_ulong_t`: a conversion
    // through an `i32` would sign-extend it into the kernel's `usize` and a
    // conversion that dropped it would make every one-shot handler permanent.
    let narrow = SignalAction32 {
        flags: 0x8000_0000,
        ..Default::default()
    };
    let native = narrow.to_native();
    assert!(native.flags.contains(SignalActionFlags::RESETHAND));
    assert_eq!(native.flags.bits(), 0x8000_0000);
    assert_eq!(SignalAction32::from_native(native).flags, 0x8000_0000);
}

#[test]
fn a_32_bit_sigaltstack_is_twelve_bytes_because_the_padding_goes_too() {
    // Half, but not by halving: `ss_flags` is an `int` at both widths, so
    // what the native struct loses is the four bytes of padding its 8-byte
    // alignment put after that `int`.
    assert_eq!(size_of::<SignalStack32>(), 12);
    assert_eq!(size_of::<SignalStack>(), 24);
    assert_eq!(offset_of!(SignalStack32, flags), 4);
    assert_eq!(
        offset_of!(SignalStack, size) - offset_of!(SignalStack, flags),
        8,
        "the native one has padding here; this is what vanishes"
    );
}

// ---------------------------------------------------------------------------
// The numbers, through the 32-bit dispatch
// ---------------------------------------------------------------------------

const USER_AT: usize = 0x2100_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// Where the new action goes, and where the old one is read back.
const ACT_AT: usize = USER_AT;
const OLD_AT: usize = USER_AT + 0x100;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "signal32", lp).unwrap();
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

pub(super) fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

pub(super) fn get(at: usize, len: usize) -> Vec<u8> {
    let p: UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped above")
}

/// The twenty bytes a 32-bit `sigaction` is, as a program writes them.
fn narrow_action_bytes(a: SignalAction32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&a.handler.to_ne_bytes());
    v.extend_from_slice(&a.flags.to_ne_bytes());
    v.extend_from_slice(&a.restorer.to_ne_bytes());
    v.extend_from_slice(&a.mask[0].to_ne_bytes());
    v.extend_from_slice(&a.mask[1].to_ne_bytes());
    v
}

pub(super) fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// A `Syscall` as a `&mut` one, since `compat32_syscall` takes `&mut self`.
pub(super) fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// The action a 32-bit program installs, as musl's `__libc_sigaction` builds
/// it: a handler, `SA_RESTORER` with the trampoline that makes the
/// `rt_sigreturn`, and a mask with a realtime signal in it.
fn an_action() -> SignalAction32 {
    SignalAction32 {
        handler: 0x0804_8120,
        flags: SignalActionFlags::SIGINFO.bits() as u32 | SignalActionFlags::RESTORER.bits() as u32,
        restorer: 0x0804_9000,
        mask: [1 << 2, 1 << 4],
    }
}

#[test]
fn a_32_bit_program_installs_a_handler_through_number_174() {
    // Before this batch, 174 was in neither allowlist and answered ENOSYS:
    // musl's `signal()` returned -1 and the program ran on with no handler.
    let proc = a_process(48_440, Abi::Linux32);
    let (installed, old) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(ACT_AT, &narrow_action_bytes(an_action()));
            // Poison where the old action comes back, so a write of the
            // wrong width is visible.
            put(OLD_AT, &[0xAAu8; 32]);
            let installed = sys
                .compat32_syscall(RT_SIGACTION, [Signal::SIGUSR1 as usize, ACT_AT, 0, 8, 0, 0])
                .await;
            // And read it back, which is how a program learns what it got.
            let read_back = sys
                .compat32_syscall(RT_SIGACTION, [Signal::SIGUSR1 as usize, 0, OLD_AT, 8, 0, 0])
                .await;
            assert_eq!(read_back, 0);
            (installed, get(OLD_AT, 32))
        })
    });
    assert_eq!(installed, 0, "number 174 is still not wired");
    assert_eq!(word(&old, 0), 0x0804_8120, "sa_handler");
    assert_eq!(word(&old, 8), 0x0804_9000, "sa_restorer");
    assert_eq!(
        (word(&old, 12), word(&old, 16)),
        (1 << 2, 1 << 4),
        "sa_mask"
    );
    assert!(
        old[20..].iter().all(|&b| b == 0xAA),
        "the old action was written as more than twenty bytes: {:x?}",
        &old[20..]
    );
    proc.exit(0);
}

#[test]
fn a_native_process_still_gets_the_native_thirty_two_bytes() {
    // The pair that makes the test above measure something: the same call on
    // the native path writes twelve bytes more, so a kernel that had simply
    // narrowed everyone's would fail here.
    let proc = a_process(48_441, Abi::Linux);
    let old = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(OLD_AT, &[0xAAu8; 40]);
            let ret = sys
                .syscall(
                    crate::Sys::RT_SIGACTION as u32,
                    [Signal::SIGUSR1 as usize, 0, OLD_AT, 8, 0, 0],
                )
                .await;
            assert_eq!(ret, 0);
            get(OLD_AT, 40)
        })
    });
    assert!(
        old[..32].iter().any(|&b| b != 0xAA),
        "the native struct is 32 bytes and SIG_DFL is zero, so it shows"
    );
    assert!(
        old[32..].iter().all(|&b| b == 0xAA),
        "and not one byte past it"
    );
    proc.exit(0);
}

#[test]
fn the_sigsetsize_check_is_the_same_eight_at_either_width() {
    // `sa_mask` does not change width, so a 32-bit program passes 8 like
    // everyone else -- and the check is the native one, reached through
    // `sigaction_with`, so there is no narrow copy of it to get wrong.
    let proc = a_process(48_442, Abi::Linux32);
    let (eight, four, sixteen) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(ACT_AT, &narrow_action_bytes(an_action()));
            let sig = Signal::SIGUSR2 as usize;
            (
                sys.compat32_syscall(RT_SIGACTION, [sig, ACT_AT, 0, 8, 0, 0])
                    .await,
                sys.compat32_syscall(RT_SIGACTION, [sig, ACT_AT, 0, 4, 0, 0])
                    .await,
                sys.compat32_syscall(RT_SIGACTION, [sig, ACT_AT, 0, 16, 0, 0])
                    .await,
            )
        })
    });
    assert_eq!(eight, 0);
    assert_eq!((four, sixteen), (EINVAL, EINVAL));
    proc.exit(0);
}

#[test]
fn a_disposition_for_sigkill_is_refused_through_the_32_bit_entry_too() {
    // The check lives in `sigaction_with` and not in either entry point, so
    // this is pinning that the narrow entry really goes through it: a second
    // copy of the call could install a handler for SIGKILL.
    let proc = a_process(48_443, Abi::Linux32);
    let (kill, stop, reading_kill) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(ACT_AT, &narrow_action_bytes(an_action()));
            (
                sys.compat32_syscall(RT_SIGACTION, [Signal::SIGKILL as usize, ACT_AT, 0, 8, 0, 0])
                    .await,
                sys.compat32_syscall(RT_SIGACTION, [Signal::SIGSTOP as usize, ACT_AT, 0, 8, 0, 0])
                    .await,
                // Reading SIGKILL's disposition is allowed; only setting one
                // is not (`do_sigaction`'s `act &&`).
                sys.compat32_syscall(RT_SIGACTION, [Signal::SIGKILL as usize, 0, OLD_AT, 8, 0, 0])
                    .await,
            )
        })
    });
    assert_eq!((kill, stop), (EINVAL, EINVAL));
    assert_eq!(reading_kill, 0);
    proc.exit(0);
}

#[test]
fn a_32_bit_program_installs_an_alternate_stack_through_number_186() {
    let proc = a_process(48_444, Abi::Linux32);
    let old = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let mut ss = Vec::new();
            ss.extend_from_slice(&(USER_AT as u32 + 0x800).to_ne_bytes());
            ss.extend_from_slice(&0u32.to_ne_bytes());
            ss.extend_from_slice(&0x800u32.to_ne_bytes());
            put(ACT_AT, &ss);
            put(OLD_AT, &[0xAAu8; 24]);
            let set = sys
                .compat32_syscall(SIGALTSTACK, [ACT_AT, 0, 0, 0, 0, 0])
                .await;
            assert_eq!(set, 0, "number 186 is still not wired");
            let read_back = sys
                .compat32_syscall(SIGALTSTACK, [0, OLD_AT, 0, 0, 0, 0])
                .await;
            assert_eq!(read_back, 0);
            get(OLD_AT, 24)
        })
    });
    assert_eq!(word(&old, 0), USER_AT as u32 + 0x800, "ss_sp");
    assert_eq!(word(&old, 8), 0x800, "ss_size, at byte 8 and not byte 16");
    assert!(
        old[12..].iter().all(|&b| b == 0xAA),
        "the old stack was written as more than twelve bytes"
    );
    proc.exit(0);
}

#[test]
fn a_stack_too_small_is_refused_through_the_32_bit_entry_too() {
    // `MINSIGSTKSZ`, checked in `sigaltstack_with` and so shared.
    let proc = a_process(48_445, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let mut ss = Vec::new();
            ss.extend_from_slice(&(USER_AT as u32 + 0x800).to_ne_bytes());
            ss.extend_from_slice(&0u32.to_ne_bytes());
            ss.extend_from_slice(&16u32.to_ne_bytes());
            put(ACT_AT, &ss);
            sys.compat32_syscall(SIGALTSTACK, [ACT_AT, 0, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(got, -(LxError::ENOMEM as isize));
    proc.exit(0);
}

#[test]
fn number_173_reaches_the_sigreturn_and_is_not_enosys() {
    // There is no frame on this thread's stack, so the answer is EINVAL --
    // which is the point: `ENOSYS` would mean the arm is missing, and a
    // handler that cannot return is a handler that kills its process. The
    // 32-bit branch inside `sys_rt_sigreturn` is what reads the frame, and
    // it is reached by asking the process, not by the number.
    let proc = a_process(48_446, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            as_mut(sys)
                .compat32_syscall(RT_SIGRETURN, [0, 0, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(got, EINVAL, "number 173 answered {}", got);
    assert_ne!(got, ENOSYS);
    proc.exit(0);
}

#[test]
fn the_three_numbers_are_not_in_the_translation_table() {
    // They are answered by the dispatch before it asks the table, so a row
    // for any of them would send a 32-bit structure to a native handler.
    for n in [RT_SIGACTION, SIGALTSTACK, RT_SIGRETURN] {
        assert_eq!(
            crate::compat32::native_number(n),
            None,
            "i386 {} must not have a row in the allowlist",
            n
        );
    }
    // And the pre-POSIX pair has no row either, for the same reason as the
    // three above now that both are answered by the dispatch: 67's
    // `old_sigaction` is sixteen bytes and x86_64's 67 is `shmdt`. See
    // `old_sigaction_tests`.
    for n in [67u32, 119] {
        assert_eq!(crate::compat32::native_number(n), None);
    }
}

/// **Number 119 reaches the old `sigreturn`.**
///
/// As with 173: no frame on this thread's stack, so `EINVAL` --- and
/// `ENOSYS` is what it answered before this batch, which is what a 32-bit
/// program got from every handler installed WITHOUT `SA_SIGINFO`. musl's
/// i386 `__libc_sigaction` points `sa_restorer` at `__restore`, a plain
/// `sigreturn`, for exactly those, so the handler of a `signal(3)` call ran
/// and then died on the way out.
#[test]
fn number_119_reaches_the_old_sigreturn_and_is_not_enosys() {
    let proc = a_process(48_560, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            as_mut(sys)
                .compat32_syscall(SIGRETURN, [0, 0, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(got, EINVAL, "number 119 answered {}", got);
    assert_ne!(got, ENOSYS);
    proc.exit(0);
}

/// **The old `sigreturn` restores the registers from a bare `sigcontext_32`,
/// and the mask from its two halves.**
///
/// The old frame has no `ucontext`: the `sigcontext_32` is in the frame
/// itself, so `ip` is 56 bytes in and `sp` 28, without the twenty bytes of
/// `uc_flags`, `uc_link` and `uc_stack` the `rt_` frame puts first. Reading
/// it as a `ucontext` would resume at whatever those twenty bytes earlier
/// happen to hold --- and nothing would fault or log, which is the shape of
/// bug this test exists for.
///
/// The mask is the other half: an old frame carries its low 32 bits in
/// `sc.oldmask` and its high 32 in the `extramask` behind the
/// `_fpstate_32`, 624 bytes further on. A program that blocks a real-time
/// signal around a handler is the one that notices.
#[test]
fn the_old_sigreturn_restores_the_registers_and_the_split_mask() {
    const SC_AT: usize = USER_AT + 0x400;
    const RESUME_AT: u32 = 0x0804_9100;
    const RESUME_SP: u32 = USER_AT as u32 + 0xf00;
    const KEY_SP: usize = USER_AT + 0x1000;
    /// `sc.oldmask` and `extramask`: SIGUSR1 (bit 9) in the low half and the
    /// real-time signal 40 (bit 39) in the high one, so neither half can
    /// pass for the other and a mask read from one of them alone is visibly
    /// wrong.
    const LOW: u32 = 1 << 9;
    const HIGH: u32 = 1 << 7;
    /// `sizeof(struct sigcontext_32)` and `sizeof(struct _fpstate_32)`.
    const SC_LEN: usize = 88;
    const FPSTATE_LEN: usize = 624;

    let proc = a_process(48_561, Abi::Linux32);
    let (ret, ip, sp, mask) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            // A frame as `loader::linux::sigframe32::write_old_frame` leaves
            // one, from `&frame->sc` on: the registers, then the fpstate
            // nobody reads, then the high half of the mask.
            let mut frame = alloc::vec![0u8; SC_LEN + FPSTATE_LEN + 4];
            frame[28..32].copy_from_slice(&RESUME_SP.to_ne_bytes());
            frame[56..60].copy_from_slice(&RESUME_AT.to_ne_bytes());
            // `oldmask` is at byte 80, between `fpstate` and `cr2`.
            frame[80..84].copy_from_slice(&LOW.to_ne_bytes());
            let extramask = SC_LEN + FPSTATE_LEN;
            frame[extramask..extramask + 4].copy_from_slice(&HIGH.to_ne_bytes());
            put(SC_AT, &frame);

            // The kernel side: no `siginfo` in this frame, so that slot is
            // zero, and the `ucontext` slot holds `&frame->sc`.
            sys.thread
                .backup_context(UserContext::default(), 0, SC_AT, KEY_SP);
            sys.thread
                .with_context(|ctx| {
                    ctx.set_field(kernel_hal::context::UserContextField::StackPointer, KEY_SP)
                })
                .unwrap();

            let ret = as_mut(sys)
                .compat32_syscall(SIGRETURN, [0, 0, 0, 0, 0, 0])
                .await;
            let (ip, sp) = sys
                .thread
                .with_context(|ctx| (ctx.general().rip, ctx.general().rsp))
                .unwrap();
            let mask = sys.thread.lock_linux().signal_mask();
            (ret, ip, sp, mask)
        })
    });
    assert_ne!(ret, ENOSYS);
    assert_ne!(ret, EINVAL, "the frame was not found at the stack pointer");
    assert_eq!(
        ip, RESUME_AT as usize,
        "resumed somewhere else: the sigcontext was read as a ucontext"
    );
    assert_eq!(sp, RESUME_SP as usize);
    assert_eq!(
        mask.val(),
        LOW as u64 | ((HIGH as u64) << 32),
        "the mask comes from sc.oldmask AND the extramask behind the fpstate"
    );
    proc.exit(0);
}

#[test]
fn the_sigreturn_of_a_32_bit_process_reads_the_32_bit_frame() {
    // The mutation this exists for: `sys_rt_sigreturn` asking nothing and
    // always reading the native `ucontext`. Both frames sit at the same
    // address, a 116-byte one is inside a mapped page, and the native read
    // takes 936 bytes from it -- so nothing faults and nothing logs. The
    // process just resumes at an address out of 820 bytes of whatever
    // followed the frame, which is the hardest kind of bug to find from a
    // core file: a program that returns from a signal handler and jumps into
    // the weeds.
    const UCTX_AT: usize = USER_AT + 0x400;
    const RESUME_AT: u32 = 0x0804_9100;
    const RESUME_SP: u32 = USER_AT as u32 + 0xf00;
    const KEY_SP: usize = USER_AT + 0x1000;

    let proc = a_process(48_447, Abi::Linux32);
    let (ret, ip, sp) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            // A frame as `loader::linux::sigframe32` leaves one: the
            // `ucontext_ia32`'s `sigcontext_32` with the registers to resume
            // with, `ip` at byte 56 and `sp` at byte 28 of it, and the
            // `ucontext` itself starting with 20 bytes of `uc_flags`,
            // `uc_link` and `uc_stack`.
            let mut uc = alloc::vec![0u8; 116];
            uc[20 + 28..20 + 32].copy_from_slice(&RESUME_SP.to_ne_bytes());
            uc[20 + 56..20 + 60].copy_from_slice(&RESUME_AT.to_ne_bytes());
            put(UCTX_AT, &uc);
            put(ACT_AT, &SigInfo::default().to_i386_bytes());

            // And the kernel side of it: the interrupted context, found again
            // by the stack pointer the `sigreturn` is made with.
            sys.thread
                .backup_context(UserContext::default(), ACT_AT, UCTX_AT, KEY_SP);
            sys.thread
                .with_context(|ctx| {
                    ctx.set_field(kernel_hal::context::UserContextField::StackPointer, KEY_SP)
                })
                .unwrap();

            let ret = as_mut(sys)
                .compat32_syscall(RT_SIGRETURN, [0, 0, 0, 0, 0, 0])
                .await;
            let (ip, sp) = sys
                .thread
                .with_context(|ctx| (ctx.general().rip, ctx.general().rsp))
                .unwrap();
            (ret, ip, sp)
        })
    });
    assert_ne!(ret, ENOSYS);
    assert_eq!(
        ip, RESUME_AT as usize,
        "the sigreturn resumed somewhere else: the frame was read at the wrong width"
    );
    assert_eq!(sp, RESUME_SP as usize);
    proc.exit(0);
}

/// **A `sigreturn` for the wrong one of the two frames is refused**, and the
/// refusal is what keeps a kernel dereference off address zero.
///
/// The old frame's `siginfo` slot is zero, there being no `siginfo` in it. So
/// a 32-bit program parked on an old frame that asks for 173 --- a handler
/// without `SA_SIGINFO` whose `sa_restorer` is `__restore_rt`, which is a
/// program out of contract, but one a kernel must survive --- would send the
/// `rt_` path to read 128 bytes of `siginfo` from the null page. Both
/// directions answer `EINVAL` instead.
#[test]
fn a_sigreturn_for_the_other_frame_is_refused_rather_than_read() {
    const AT: usize = USER_AT + 0x600;
    const KEY_SP: usize = USER_AT + 0x1800;

    for (stored_siginfo, asked) in [(0usize, RT_SIGRETURN), (AT, SIGRETURN)] {
        let proc = a_process(
            if stored_siginfo == 0 { 48_562 } else { 48_563 },
            Abi::Linux32,
        );
        let got = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                put(AT, &[0u8; 128]);
                sys.thread
                    .backup_context(UserContext::default(), stored_siginfo, AT, KEY_SP);
                sys.thread
                    .with_context(|ctx| {
                        ctx.set_field(kernel_hal::context::UserContextField::StackPointer, KEY_SP)
                    })
                    .unwrap();
                as_mut(sys)
                    .compat32_syscall(asked, [0, 0, 0, 0, 0, 0])
                    .await
            })
        });
        assert_eq!(
            got, EINVAL,
            "number {asked} on the other frame answered {got}"
        );
        proc.exit(0);
    }
}

/// **The old frame is read through a CHECKED access, not dereferenced.**
///
/// A handler may unmap its own stack before it returns, and then the address
/// the kernel saved is one with nothing behind it. A plain dereference of one
/// does not fault politely in this kernel --- see the note on
/// `KernelHandler::check_user_range` --- so both pieces of the frame go
/// through `UserInPtr`, which asks first.
///
/// What this test asks for is the half of the check that does not need the
/// address space: an address that is not a user address at all, with
/// `set_strict_user_half` turned on because on the host every address looks
/// like a user one. The other half --- "this user range has no mapping" ---
/// is driven by `kernel_hal`'s own `refuse_user_range`, which is
/// `pub(crate)` there, so it is covered by that crate's tests of
/// `read_unaligned` rather than from here.
///
/// The mutation is the code this replaced: reading the `sigcontext_32` and
/// the `extramask` raw. It does not make this test fail; it kills the process
/// running it, which is the bug. Pointed out by Copilot on the #2424.
#[test]
fn the_old_frame_is_read_through_a_checked_access() {
    /// Not a user address: the kernel half. A `sigreturn` cannot really
    /// arrive with one, since the kernel stored the address itself --- what
    /// it stands in for here is the read being checked at all.
    const NOT_USER: usize = 0xffff_ffff_8000_0000;
    const KEY_SP: usize = USER_AT + 0x1900;

    let proc = a_process(48_564, Abi::Linux32);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            // On the host every address is a user-half address, because the
            // host's own heap and stack are where a test's buffers live; the
            // bound has to be asked for, and on the thread the call runs on.
            kernel_hal::user::set_strict_user_half(true);
            sys.thread
                .backup_context(UserContext::default(), 0, NOT_USER, KEY_SP);
            sys.thread
                .with_context(|ctx| {
                    ctx.set_field(kernel_hal::context::UserContextField::StackPointer, KEY_SP)
                })
                .unwrap();
            let got = as_mut(sys)
                .compat32_syscall(SIGRETURN, [0, 0, 0, 0, 0, 0])
                .await;
            kernel_hal::user::set_strict_user_half(false);
            got
        })
    });
    assert_eq!(got, EFAULT, "an unreadable frame answered {}", got);
    proc.exit(0);
}
