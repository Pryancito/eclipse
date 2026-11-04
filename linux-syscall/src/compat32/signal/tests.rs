//! The two structures, and the three numbers that carry them.
//!
//! Everything process-level here goes through `compat32_syscall` **by
//! number**, not by calling `sys_rt_sigaction32` directly: what is being
//! pinned is that the arm exists. Call the handler by name and deleting the
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
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's own numbers, spelled out here: entering the layer the way a 32-bit
/// program does is the whole point.
const RT_SIGACTION: u32 = 174;
const SIGALTSTACK: u32 = 186;
const RT_SIGRETURN: u32 = 173;

const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

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

fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

fn get(at: usize, len: usize) -> Vec<u8> {
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

fn word(bytes: &[u8], at: usize) -> u32 {
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
    // And the pre-POSIX pair is still absent, deliberately: their structures
    // are a third and a fourth layout. See this module's documentation.
    for n in [67u32, 119] {
        assert_eq!(crate::compat32::native_number(n), None);
    }
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
