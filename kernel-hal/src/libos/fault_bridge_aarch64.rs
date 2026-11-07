//! Turning the host's signals into the guest's trap frame, on aarch64.
//!
//! The x86_64 bridge (`fault_bridge.rs`) says why there is one: a hosted
//! guest runs as a function call inside this very process, so a guest
//! instruction that faults raises a HOST signal, and with no handler for it
//! the host kills kernel, guest and harness together. This is the same
//! bridge for the other Linux host, with the one difference the
//! architecture makes: here it carries EVERY way back, not only the faults.
//!
//! # Why every way back
//!
//! On x86_64 a guest syscall through a `call`ed stub can walk into the
//! kernel on its own (`syscall_fn_entry`), because the entry finds the
//! kernel's stack through `gs`, a register the guest does not use. aarch64
//! has one thread-pointer register, `tpidr_el0`, the guest's TLS lives in
//! it, and the guest can write it with one unprivileged `msr`. The old
//! entry read the kernel's context pointer from `tpidr_el0 + 48` and trusted
//! it. So on this host the guest never walks in: its `svc` is trapped by the
//! seccomp filter and arrives as `SIGSYS` like a fault, and the handler finds
//! the kernel's stack through bookkeeping of its own, keyed on the host
//! thread's alternate signal stack -- a thing the guest cannot reach.
//! `trapframe`'s hosted aarch64 path says the rest.
//!
//! # What the kernel reads
//!
//! `TrapReason::from_aarch64` wants the vector-table entry (`trap_num`:
//! source and kind), the syndrome (`ESR_EL1`) and the faulting address
//! (`FAR_EL1`). A hosted kernel has no such registers, so the handler writes
//! the three into the `UserContext` itself, from what the signal says: the
//! host records the syndrome of a memory fault in the signal context
//! (`esr_context`), and for the signals it does not -- an undefined
//! instruction, a `brk`, a trapped `svc` -- the syndrome is reconstructed,
//! since each of those has one exception class.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use nix::libc;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use trapframe::{HostSigContext, UserContext};

/// The vector-table entry of a synchronous exception from EL0 (AArch64):
/// `Kind::Synchronous << 16 | Source::LowerAArch64`.
const SYNC_FROM_USER: usize = 2;

/// The vector-table entry a guest's time slice ends with: an IRQ from EL0,
/// which `TrapReason::from_aarch64` reads as `Interrupt` and the run loops
/// answer by giving the CPU to the next thread. The hosted interrupt
/// controller has no number for it (`irq_num` answers `usize::MAX` under
/// libos) and `handle_irq` is a no-op; the yield is the point.
pub const PREEMPT_VECTOR: usize = (1 << 16) | 2;

/// The slice: the host timer's period on a thread that has been out in a guest.
const PREEMPT_SLICE_NS: libc::c_long = 10_000_000;

/// Where the guest's code is: what tells a signal the guest raised from one
/// the kernel's own code raised, since on this host the program counter is
/// the one thing a guest cannot make look like the kernel's.
static GUEST_START: AtomicUsize = AtomicUsize::new(0);
static GUEST_END: AtomicUsize = AtomicUsize::new(0);

/// `ESR_EL1.EC` values (`Syndrome::from`), shifted into place, with the
/// instruction-length bit set where the instruction is a 32-bit one.
const ESR_IL: usize = 1 << 25;
const EC_UNKNOWN: usize = 0;
const EC_SVC64: usize = 0b010101 << 26;
const EC_INSN_ABORT_LOWER: usize = 0b100000 << 26;
const EC_DATA_ABORT_LOWER: usize = 0b100100 << 26;
const EC_TRAPPED_FPU: usize = 0b101100 << 26;
const EC_BRK: usize = 0b111100 << 26;
/// The data/instruction fault status of a translation fault at level 3.
const FSC_TRANSLATION_L3: usize = 0b000111;
/// `ESR_EL1.WnR` of a data abort: the access was a write.
const ESR_WNR: usize = 1 << 6;

/// The `brk #imm16` encoding: `0xd420_0000 | imm16 << 5`.
const BRK_OPCODE_MASK: u32 = 0xffe0_001f;
const BRK_OPCODE: u32 = 0xd420_0000;

/// The signals a guest can come back on.
const SIGNALS: [Signal; 7] = [
    Signal::SIGILL,
    Signal::SIGSEGV,
    Signal::SIGBUS,
    Signal::SIGFPE,
    Signal::SIGTRAP,
    Signal::SIGSYS,
    Signal::SIGALRM,
];

/// Kept for the loader, which on x86_64 tells a syscall that arrived through
/// a `syscall` instruction from one that jumped into the fncall entry by this
/// error code. Here every syscall arrives through the bridge and the
/// arguments are in `x0`-`x7` either way; the constant exists so the loader
/// compiles the same on both hosts.
pub const SYSCALL_INSTRUCTION: usize = 1;

/// Install the bridge. See the x86_64 bridge's `init`.
///
/// # Safety
///
/// Installs process-wide signal handlers. Must run before any guest does.
pub unsafe fn init() {
    let mut without_tick = SigSet::empty();
    without_tick.add(Signal::SIGALRM);
    for signal in SIGNALS {
        let action = if signal == Signal::SIGALRM {
            SigAction::new(
                SigHandler::SigAction(on_fault),
                SaFlags::SA_SIGINFO | SaFlags::SA_ONSTACK | SaFlags::SA_RESTART,
                SigSet::empty(),
            )
        } else {
            SigAction::new(
                SigHandler::SigAction(on_fault),
                SaFlags::SA_SIGINFO | SaFlags::SA_ONSTACK | SaFlags::SA_NODEFER,
                without_tick,
            )
        };
        if let Err(err) = sigaction(signal, &action) {
            error!("libos: cannot bridge {:?} to the guest: {}", signal, err);
        }
    }
}

/// How many host threads the bridge can recognise at once. See the x86_64
/// bridge: one slot per thread that has ever gone out, never freed.
const ANCHORS: usize = 256;

/// The alternate-stack pointer of each registered thread, the kernel's
/// `tpidr_el0` on it, the `UserContext` it is out in, and whether a tick
/// landed on it while it was not out. Flat arrays of atomics because the
/// signal handler reads them: an atomic load is async-signal-safe, a lock,
/// an allocation or a thread-local is not -- and a thread-local here would
/// go through the GUEST's `tpidr_el0`.
static ANCHOR_STACK: [AtomicUsize; ANCHORS] = [const { AtomicUsize::new(0) }; ANCHORS];
static ANCHOR_TP: [AtomicUsize; ANCHORS] = [const { AtomicUsize::new(0) }; ANCHORS];
static ANCHOR_CTX: [AtomicUsize; ANCHORS] = [const { AtomicUsize::new(0) }; ANCHORS];
static ANCHOR_RESCHED: [AtomicBool; ANCHORS] = [const { AtomicBool::new(false) }; ANCHORS];

fn anchor_slot(ss_sp: usize) -> usize {
    (ss_sp >> 12) % ANCHORS
}

/// What the bridge knows about the host thread the handler runs on.
struct Anchor {
    /// The kernel's own thread pointer on it.
    tp: usize,
    /// Its row in the anchor tables.
    slot: usize,
}

/// Raw syscall, for the handler: libc's wrappers write `errno`, a
/// thread-local, through whatever `tpidr_el0` holds, which in the handler
/// is the guest's.
unsafe fn raw_syscall2(nr: usize, a0: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "svc #0",
        in("x8") nr,
        inout("x0") a0 => ret,
        in("x1") a1,
        options(nostack),
    );
    ret
}

const SYS_SIGALTSTACK: usize = 132;

unsafe fn anchor() -> Option<Anchor> {
    let mut current = libc::stack_t {
        ss_sp: ptr::null_mut(),
        ss_flags: 0,
        ss_size: 0,
    };
    if raw_syscall2(
        SYS_SIGALTSTACK,
        0,
        &mut current as *mut libc::stack_t as usize,
    ) != 0
        || current.ss_sp.is_null()
        || current.ss_flags & libc::SS_DISABLE != 0
    {
        return None;
    }
    let ss_sp = current.ss_sp as usize;
    let first = anchor_slot(ss_sp);
    for step in 0..ANCHORS {
        let slot = (first + step) % ANCHORS;
        match ANCHOR_STACK[slot].load(Ordering::Acquire) {
            0 => return None,
            found if found == ss_sp => {
                let tp = ANCHOR_TP[slot].load(Ordering::Acquire);
                return (tp != 0).then_some(Anchor { tp, slot });
            }
            _ => {}
        }
    }
    None
}

fn register_anchor(ss_sp: usize, tp: usize) -> Option<usize> {
    let first = anchor_slot(ss_sp);
    for step in 0..ANCHORS {
        let slot = (first + step) % ANCHORS;
        if ANCHOR_STACK[slot]
            .compare_exchange(0, ss_sp, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            ANCHOR_TP[slot].store(tp, Ordering::Release);
            return Some(slot);
        }
    }
    None
}

unsafe fn read_tpidr() -> usize {
    let tp: usize;
    core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags));
    tp
}

unsafe fn write_tpidr(tp: usize) {
    core::arch::asm!("msr tpidr_el0, {}", in(reg) tp, options(nomem, nostack, preserves_flags));
}

/// Hand the signal back to the host: clear our handler and return, so the
/// faulting instruction runs again and the default action takes it. The
/// path for the kernel's own faults.
unsafe fn leave_to_the_host(signal: libc::c_int) {
    if let Ok(signal) = Signal::try_from(signal) {
        let _ = sigaction(
            signal,
            &SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty()),
        );
    }
}

/// Tell the kernel the context this host thread is about to go out in, so
/// the handler can fill it. Kernel context, right before `run_fncall`.
pub fn publish_context(ctx: *mut UserContext) {
    ANCHOR_ROW.with(|row| {
        if let Some(slot) = row.get() {
            ANCHOR_CTX[slot].store(ctx as usize, Ordering::Release);
        }
    });
}

extern "C" fn on_fault(
    signal: libc::c_int,
    _info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    // Every line of this is a raw pointer or a syscall; the signature cannot
    // be `unsafe fn` because that is not what `sigaction` takes.
    unsafe { bridge_fault(signal, context) }
}

/// Is `pc` the guest's? Its own code, or the old fncall entry, which is the
/// one kernel address a guest jumps to on purpose (the hosted Linux
/// personality's musl, with `rcore_syscall_entry` patched to it).
fn is_guest_pc(pc: usize) -> bool {
    let start = GUEST_START.load(Ordering::Relaxed);
    let end = GUEST_END.load(Ordering::Relaxed);
    (start < end && (start..end).contains(&pc))
        || pc == trapframe::syscall_fn_entry as *const () as usize
}

unsafe fn bridge_fault(signal: libc::c_int, context: *mut libc::c_void) {
    if context.is_null() || !SIGNALS.iter().any(|known| *known as libc::c_int == signal) {
        return leave_to_the_host(signal);
    }
    let uc = &mut *(context as *mut libc::ucontext_t);
    let mc = &mut *(ptr::addr_of_mut!(uc.uc_mcontext) as *mut HostSigContext);
    let pc = mc.pc as usize;
    if !is_guest_pc(pc) {
        // The kernel's own. A tick is the one signal it can get without
        // anything being wrong: the thread's slice is over all the same,
        // and the next trip out is told so.
        if signal == libc::SIGALRM {
            return note_tick_off_guest();
        }
        return leave_to_the_host(signal);
    }
    // A thread the bridge does not know, or one that is not out: a tick is
    // dropped (its default action would kill the process), a fault is the
    // host's.
    let Some(Anchor { tp, slot }) = anchor() else {
        return if signal == libc::SIGALRM {
            note_tick_off_guest()
        } else {
            leave_to_the_host(signal)
        };
    };
    let ctx = ANCHOR_CTX[slot].load(Ordering::Acquire) as *mut UserContext;
    if ctx.is_null() {
        return if signal == libc::SIGALRM {
            ANCHOR_RESCHED[slot].store(true, Ordering::Release)
        } else {
            leave_to_the_host(signal)
        };
    }
    let ctx = &mut *ctx;
    // The guest's registers out, and the way back in: from here the
    // handler's return lands in `fault_fn_entry`.
    let recorded = ctx.take_from_host(mc);
    // The kernel's thread pointer back. The host keeps `tpidr_el0` out of
    // the signal frame, so this is the one place it is put back, and from
    // here on a thread-local is the kernel's again.
    write_tpidr(tp);

    let (trap_num, esr, far) = match signal {
        libc::SIGALRM => (PREEMPT_VECTOR, 0, 0),
        // A `svc` the seccomp filter trapped. The host saved the pc AFTER
        // the instruction, as the hardware does for an `svc`, and `elr` is
        // where the stub's `ret` carries on.
        libc::SIGSYS => (SYNC_FROM_USER, EC_SVC64 | ESR_IL, 0),
        // The old entry, reached by `bl`: a syscall whose return address is
        // in `x30`. The `brk` there is the kernel's, not an exception of the
        // guest's.
        libc::SIGTRAP if pc == trapframe::syscall_fn_entry as *const () as usize => {
            ctx.elr = ctx.general.x30;
            (SYNC_FROM_USER, EC_SVC64 | ESR_IL, 0)
        }
        // `brk #imm`: the pc is AT the instruction, so the immediate can be
        // read back from it. The guest just executed the word, so it is
        // mapped and readable.
        libc::SIGTRAP => {
            let word = (pc as *const u32).read_unaligned();
            let imm = if word & BRK_OPCODE_MASK == BRK_OPCODE {
                ((word >> 5) & 0xffff) as usize
            } else {
                0
            };
            (SYNC_FROM_USER, EC_BRK | ESR_IL | imm, pc)
        }
        libc::SIGILL => (SYNC_FROM_USER, EC_UNKNOWN | ESR_IL, pc),
        libc::SIGFPE => (SYNC_FROM_USER, EC_TRAPPED_FPU | ESR_IL, pc),
        // A memory fault: the host recorded the syndrome the CPU gave it,
        // and the kernel's page-fault handler reads the access kind from it
        // (`WnR`) and the address from `far`. Without a record (an
        // alignment fault raised by the host itself, say) the fault is
        // reported as a translation fault at the faulting address, which is
        // what makes the kernel map the page or refuse.
        _ => {
            let far = mc.fault_address as usize;
            let esr = match recorded {
                Some(esr) => esr as usize,
                None if far == pc => EC_INSN_ABORT_LOWER | ESR_IL | FSC_TRANSLATION_L3,
                None => EC_DATA_ABORT_LOWER | ESR_IL | FSC_TRANSLATION_L3 | ESR_WNR,
            };
            (SYNC_FROM_USER, esr, far)
        }
    };
    ctx.trap_num = trap_num;
    ctx.esr = esr;
    ctx.far = far;
}

/// A tick landed on this host thread while it was on the kernel's side. See
/// the x86_64 bridge's `note_tick_off_guest`.
unsafe fn note_tick_off_guest() {
    if let Some(anchor) = anchor() {
        ANCHOR_RESCHED[anchor.slot].store(true, Ordering::Release);
    }
}

/// Whether a tick landed on this host thread since the last trip out, and
/// clear it. Kernel context only.
pub fn take_resched() -> bool {
    ANCHOR_ROW.with(|row| {
        row.get()
            .is_some_and(|slot| ANCHOR_RESCHED[slot].swap(false, Ordering::AcqRel))
    })
}

thread_local! {
    /// This host thread's row in the anchor tables, from `ensure_altstack`.
    static ANCHOR_ROW: core::cell::Cell<Option<usize>> = const { core::cell::Cell::new(None) };
}

/// Arm this host thread's slice timer, once. See the x86_64 bridge's
/// `ensure_tick`: the same timer, the same reasons.
pub fn ensure_tick() {
    use core::cell::Cell;
    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
    }
    ARMED.with(|armed| {
        if armed.get() {
            return;
        }
        armed.set(true);
        // SAFETY: a zeroed `sigevent` is a valid one; every field read by
        // the host is set below.
        let mut event: libc::sigevent = unsafe { core::mem::zeroed() };
        event.sigev_notify = libc::SIGEV_THREAD_ID;
        event.sigev_signo = libc::SIGALRM;
        // SAFETY: `gettid` takes nothing and cannot fail.
        event.sigev_notify_thread_id = unsafe { libc::syscall(libc::SYS_gettid) } as libc::c_int;
        let mut timer: libc::c_int = 0;
        // SAFETY: the raw syscall, with the kernel's own `int` timer id.
        let created = unsafe {
            libc::syscall(
                libc::SYS_timer_create,
                libc::CLOCK_MONOTONIC,
                &event as *const libc::sigevent,
                &mut timer as *mut libc::c_int,
            )
        };
        if created != 0 {
            warn!(
                "libos: no slice timer for this host thread; its guests run unpreempted: {}",
                last_os_error()
            );
            return;
        }
        let slice = libc::timespec {
            tv_sec: 0,
            tv_nsec: PREEMPT_SLICE_NS,
        };
        let spec = libc::itimerspec {
            it_interval: slice,
            it_value: slice,
        };
        // SAFETY: a timer this thread created, and a spec that lives for the
        // call.
        let armed_now = unsafe {
            libc::syscall(
                libc::SYS_timer_settime,
                timer,
                0,
                &spec as *const libc::itimerspec,
                ptr::null_mut::<libc::itimerspec>(),
            )
        };
        if armed_now != 0 {
            warn!(
                "libos: cannot arm this host thread's slice timer; its guests run unpreempted: {}",
                last_os_error()
            );
        }
    });
}

/// Make the host refuse every `svc` executed from a guest address, so that
/// it reaches the kernel through the bridge. See the x86_64 bridge's
/// `trap_guest_syscalls`: the same filter, keyed on the instruction pointer.
///
/// # Safety
///
/// Process-wide: from here on a `svc` from inside `guest` arrives at
/// [`on_fault`] as `SIGSYS`, so [`init`] must have run first.
pub unsafe fn trap_guest_syscalls(guest: core::ops::Range<usize>) -> Result<(), String> {
    GUEST_START.store(guest.start, Ordering::Relaxed);
    GUEST_END.store(guest.end, Ordering::Relaxed);
    let mut filter = guest_syscall_filter(guest);
    let prog = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(format!("PR_SET_NO_NEW_PRIVS: {}", last_os_error()));
    }
    if libc::syscall(
        libc::SYS_seccomp,
        libc::SECCOMP_SET_MODE_FILTER,
        libc::SECCOMP_FILTER_FLAG_TSYNC,
        &prog as *const libc::sock_fprog,
    ) != 0
    {
        return Err(format!("SECCOMP_SET_MODE_FILTER: {}", last_os_error()));
    }
    Ok(())
}

fn last_os_error() -> String {
    format!("{}", nix::errno::Errno::last())
}

/// Offset of the instruction pointer's high half inside `seccomp_data`:
/// `nr` (u32), `arch` (u32), then `instruction_pointer` (u64), little-endian.
const SECCOMP_DATA_IP_HIGH: u32 = 12;

/// The filter: trap a syscall whose instruction pointer is inside `guest`,
/// allow every other. Classic BPF compares 32-bit words, so the range is
/// read in units of 4 GiB, rounded outwards.
fn guest_syscall_filter(guest: core::ops::Range<usize>) -> Vec<libc::sock_filter> {
    let low = (guest.start >> 32) as u32;
    let high = ((guest.end + 0xffff_ffff) >> 32) as u32;
    let insn = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    vec![
        insn(
            libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
            0,
            0,
            SECCOMP_DATA_IP_HIGH,
        ),
        insn(libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K, 0, 2, low),
        insn(libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K, 1, 0, high),
        insn(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_TRAP),
        insn(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
    ]
}

/// The per-thread alternate stack the handler runs on, registered on each
/// thread's first trip out, with the kernel's thread pointer recorded next
/// to it so the handler can put it back. See the x86_64 bridge's
/// `ensure_altstack`.
pub fn ensure_altstack() {
    use core::cell::Cell;

    const STACK_WORDS: usize = 64 * 1024 / size_of::<u64>();

    thread_local! {
        static REGISTERED: Cell<bool> = const { Cell::new(false) };
    }

    REGISTERED.with(|registered| {
        if registered.get() {
            return;
        }
        registered.set(true);
        // Leaked on purpose: the host reads it on every signal for as long as
        // this thread lives.
        let stack = Box::leak(vec![0u64; STACK_WORDS].into_boxed_slice());
        // SAFETY: a read of this thread's own thread pointer. Kernel
        // context, so it is the kernel's.
        let tp = unsafe { read_tpidr() };
        if tp == 0 {
            warn!("libos: cannot record this host thread's own thread pointer");
            return;
        }
        let ss_sp: *mut libc::c_void = stack.as_mut_ptr().cast();
        let Some(slot) = register_anchor(ss_sp as usize, tp) else {
            warn!("libos: the fault bridge has no room left to recognise this host thread");
            return;
        };
        ANCHOR_ROW.with(|row| row.set(Some(slot)));
        let altstack = libc::stack_t {
            ss_sp,
            ss_flags: 0,
            ss_size: STACK_WORDS * size_of::<u64>(),
        };
        // SAFETY: a stack this thread owns for as long as it lives.
        if unsafe { libc::sigaltstack(&altstack, ptr::null_mut()) } != 0 {
            warn!("libos: no alternate signal stack for this thread");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(filter: &[libc::sock_filter], ip: u64) -> u32 {
        let mut data = [0u8; 64];
        data[8..16].copy_from_slice(&ip.to_le_bytes());
        let mut a: u32 = 0;
        let mut pc = 0usize;
        loop {
            let i = &filter[pc];
            let code = i.code as u32;
            pc += 1;
            match code {
                c if c == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS => {
                    let at = i.k as usize;
                    a = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
                }
                c if c == libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K => {
                    pc += if a >= i.k { i.jt } else { i.jf } as usize;
                }
                c if c == libc::BPF_RET | libc::BPF_K => return i.k,
                other => panic!(
                    "the filter uses an instruction the test cannot run: {:#x}",
                    other
                ),
            }
        }
    }

    const WINDOWS: core::ops::Range<usize> = 0x2_0000_0000..0x4000_0000_0000;

    #[test]
    fn a_syscall_from_a_guest_address_traps_and_one_from_the_kernel_s_does_not() {
        let filter = guest_syscall_filter(WINDOWS);
        assert_eq!(run(&filter, 0x2_0000_0000), libc::SECCOMP_RET_TRAP);
        assert_eq!(run(&filter, 0x3fff_ffff_ffff), libc::SECCOMP_RET_TRAP);
        assert_eq!(run(&filter, 0x4000_0000_0000), libc::SECCOMP_RET_ALLOW);
        // Where the kernel's own syscalls come from on an aarch64 host: a
        // PIE binary at two thirds of the address space, its libraries and
        // stacks at the top, a non-PIE one at the bottom.
        for kernel in [0xaaaa_aaaa_a000u64, 0xffff_8000_0000, 0x40_0000] {
            assert_eq!(
                run(&filter, kernel),
                libc::SECCOMP_RET_ALLOW,
                "{:#x}",
                kernel
            );
        }
        let here = run as fn(&[libc::sock_filter], u64) -> u32 as *const () as u64;
        assert_eq!(run(&filter, here), libc::SECCOMP_RET_ALLOW);
    }

    /// What the bridge reconstructs for the signals the host records no
    /// syndrome for is what `TrapReason::from_aarch64` expects of the real
    /// register.
    #[test]
    fn the_reconstructed_syndromes_decode_as_the_hardware_s() {
        use crate::Syndrome;
        assert_eq!(Syndrome::from((EC_SVC64 | ESR_IL) as u32), Syndrome::Svc(0));
        assert_eq!(
            Syndrome::from((EC_BRK | ESR_IL | 0x1234) as u32),
            Syndrome::Brk(0x1234)
        );
        assert_eq!(
            Syndrome::from((EC_UNKNOWN | ESR_IL) as u32),
            Syndrome::Unknown
        );
        assert_eq!(
            Syndrome::from((EC_TRAPPED_FPU | ESR_IL) as u32),
            Syndrome::TrappedFpu
        );
        assert!(matches!(
            Syndrome::from((EC_DATA_ABORT_LOWER | ESR_IL | FSC_TRANSLATION_L3 | ESR_WNR) as u32),
            Syndrome::DataAbort {
                kind: crate::Fault::Translation,
                level: 3
            }
        ));
        assert!(matches!(
            Syndrome::from((EC_INSN_ABORT_LOWER | ESR_IL | FSC_TRANSLATION_L3) as u32),
            Syndrome::InstructionAbort {
                kind: crate::Fault::Translation,
                level: 3
            }
        ));
    }

    #[test]
    fn a_brk_immediate_is_read_back_from_the_instruction() {
        // `brk #0x1234` as the assembler encodes it.
        let word: u32 = BRK_OPCODE | (0x1234 << 5);
        assert_eq!(word & BRK_OPCODE_MASK, BRK_OPCODE);
        assert_eq!((word >> 5) & 0xffff, 0x1234);
        // `nop` is not a brk.
        assert_ne!(0xd503_201f & BRK_OPCODE_MASK, BRK_OPCODE);
    }

    #[test]
    fn a_tick_off_the_guest_is_taken_once_on_the_next_trip_out() {
        std::thread::spawn(|| {
            assert!(!take_resched());
            unsafe { note_tick_off_guest() };
            assert!(!take_resched());
            ensure_altstack();
            unsafe { note_tick_off_guest() };
            unsafe { note_tick_off_guest() };
            assert!(take_resched());
            assert!(!take_resched());
        })
        .join()
        .unwrap();
    }
}
