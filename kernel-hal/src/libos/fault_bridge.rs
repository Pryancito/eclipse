//! Turning the host's fault signals into the guest's trap frame.
//!
//! On a libos build the guest runs as a function call inside this very
//! process (`trapframe`'s `run_fncall`), so a guest instruction that faults
//! does not raise a trap for a kernel to catch: it raises a *host signal*. With
//! no handler for it, the host kills the whole process -- kernel, guest and
//! test harness together.
//!
//! That is what both `Zircon Core Test Libos` jobs die of. The core-tests
//! binary has a case that creates a thread, hooks its exception channel and
//! then asserts on purpose; the `ZX_ASSERT` compiles to `ud2`, and the test
//! waits for the `ZX_EXCP_UNDEFINED_INSTRUCTION` that follows. Observed here
//! (x86_64, this container, `core-tests=-l`):
//!
//! ```text
//! TASK_CREATE_EXCEPTION_CHANNEL <= Ok(())
//! OBJECT_WAIT_ASYNC <= Ok(())
//! ASSERT FAILED at (.../core/assert/assert_cpp.cc:62) ...
//! Illegal instruction (exit 132)
//! ```
//!
//! The kernel already knows what to do with a fault once it is in the trap
//! frame: `TrapReason::from_x86` reads the vector and the Zircon run loop
//! turns `UndefinedInstruction` into an exception on exactly that channel.
//! The only missing piece is the one here -- the handler that catches the
//! host signal and makes the guest's return look like a trap.
//!
//! # The guest's `syscall` instructions
//!
//! The same bridge carries the guest's system calls. A Zircon guest reaches
//! its kernel through the vDSO, and the real vDSO's stubs are `mov eax, N;
//! syscall; ret`. On a libos host a `syscall` instruction is a HOST system
//! call, so the libos build used to boot a vDSO rebuilt with every stub
//! turned into `mov eax, N; jmp [entry]` -- eleven bytes each where the real
//! ones are eight. Fuchsia's core tests do not all go through a stub's
//! public symbol: the internal syscalls (`zx_channel_call_finish` and its
//! three siblings) have no public symbol, and the test that exercises them
//! finds its stub by *distance from a public one*, with the distance baked in
//! from the real vDSO's layout (`vdso-code.h`). On the rebuilt vDSO that
//! distance lands one byte short, in the last byte of the previous stub's
//! immediate, and the guest executes `add %bh, %bh` before the jump: `rbx`
//! comes back from the call corrupted, the SafeStack canary check at the
//! test's return fails, and the abort takes the whole test binary with it.
//! Measured with gdb in this container on
//! `ChannelInternalTest.CallFinishWithoutPreviouslyCallingCallReturnsBadState`:
//! `rbx` `0x1200269df0` before the call, `0x1200263af0` after.
//!
//! So the libos build boots the real vDSO, byte for byte the one baremetal
//! boots, and [`trap_guest_syscalls`] makes the host refuse every `syscall`
//! instruction executed from a guest address: a seccomp filter keyed on the
//! instruction pointer answers `SECCOMP_RET_TRAP`, the host delivers
//! `SIGSYS` with the guest's registers, and the handler here lays the same
//! frame it lays for a fault, with the x86 syscall vector in it and
//! [`SYSCALL_INSTRUCTION`] for the error code, so that the kernel reads the
//! arguments where a `syscall` instruction leaves them (`r10`, `r12`, `r13`
//! for the fourth, seventh and eighth) rather than where a `call`ed stub
//! does. The kernel's own syscalls are never from a guest address and pass
//! the filter untouched.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use nix::libc;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

/// `arch_prctl` codes. The bridge needs them to tell a guest fault from the
/// kernel's own: `trapframe` enters the guest with `FS` pointing at the
/// guest's thread area and `GS` at the kernel's, and leaves `GS` zero until a
/// thread has gone out at least once.
const ARCH_SET_GS: libc::c_int = 0x1001;
const ARCH_GET_FS: libc::c_int = 0x1003;
const ARCH_GET_GS: libc::c_int = 0x1004;

/// The signals a guest fault can arrive as, and the x86 vector each one
/// stands for.
///
/// `SIGSEGV` is listed as `#GP`, which is what it stays for a fault the host
/// itself took as one (a non-canonical address). The ones the host took as a
/// page fault go on as `#PF`, with the CPU's error code and the faulting
/// address (`fault_address`), because a hosted guest does have faults to
/// serve: a write to a page that lost `WRITE` to a copy-on-write child. See
/// `bridge_fault`.
///
/// `SIGSYS` is not a fault: it is what the seccomp filter of
/// [`trap_guest_syscalls`] turns a guest's `syscall` instruction into, and it
/// carries the vector the kernel's own `syscall` entry would have written.
const SIGNAL_VECTORS: [(Signal, usize); 7] = [
    (Signal::SIGILL, 6),   // #UD, invalid opcode
    (Signal::SIGSEGV, 13), // #GP, general protection
    (Signal::SIGBUS, 17),  // #AC, alignment check
    (Signal::SIGFPE, 0),   // #DE, divide error
    (Signal::SIGTRAP, 3),  // #BP, breakpoint
    (Signal::SIGSYS, SYSCALL_VECTOR),
    (Signal::SIGALRM, PREEMPT_VECTOR),
];

/// The vector a guest's time slice ends with: the first external interrupt
/// vector, which `TrapReason::from_x86` reads as `Interrupt`, and which the
/// run loops answer by giving the CPU to the next thread.
///
/// A guest thread runs on its host thread until it makes a syscall or
/// faults; nothing else took the CPU from it. A guest that spins waiting for
/// another guest thread that has no CPU to run on therefore waited for
/// ever, once there were more spinners than host CPUs:
/// `PortStressTest.CloseWaitRace` starts five threads that busy-wait on a
/// counter the fifth increments, on a four-CPU host, and hung every libos run
/// that reached it. Real hardware has a timer interrupt; this is it.
pub const PREEMPT_VECTOR: usize = 0x20;

/// The slice: the host timer's period on a thread that has been out in a guest.
const PREEMPT_SLICE_NS: libc::c_long = 10_000_000;

/// Where the guest's code is, for telling a tick that landed on it from one
/// that landed on the kernel's own way in or out, which runs with the
/// guest's `FS` base loaded (see `tick`).
static GUEST_START: AtomicUsize = AtomicUsize::new(0);
static GUEST_END: AtomicUsize = AtomicUsize::new(0);

/// The `trap_num` the kernel reads as "a system call": what `run_fncall`
/// writes before every trip out and what `TrapReason::from_x86` answers
/// `Syscall` to.
const SYSCALL_VECTOR: usize = 0x100;

/// The error code the bridge writes next to [`SYSCALL_VECTOR`] for a syscall
/// that arrived through a `syscall` instruction, as opposed to the zero
/// `run_fncall` writes for one that jumped into the fncall entry from a
/// `call`ed stub.
///
/// The two leave the arguments in different places. A `call`ed stub keeps
/// the C calling convention: the fourth argument in `rcx`, the seventh and
/// eighth on the stack where the caller pushed them. The real vDSO's stubs
/// follow Zircon's x86-64 syscall ABI instead -- `mov %rcx, %r10` because
/// the instruction overwrites `rcx`, and the seventh and eighth arguments
/// moved into `r12` and `r13` (saved and restored around the `syscall`) --
/// and the loader has to know which one it is reading.
pub const SYSCALL_INSTRUCTION: usize = 1;

/// `#PF`, as the host reports it in `REG_TRAPNO` and as the kernel reads it
/// from the trap frame.
const PAGE_FAULT_VECTOR: i64 = 14;

/// The bits of a `#PF` error code the kernel decodes: present, write, user,
/// reserved, instruction fetch. The host's `REG_ERR` carries them as the
/// CPU wrote them; anything above is not for the kernel.
const PAGE_FAULT_ERROR_BITS: usize = 0x1f;

/// `rflags.TF`: the trap flag, which single-steps the code it is live in.
const X86_FLAGS_TF: u64 = 0x100;

/// The faulting linear address of the page fault this host thread last
/// bridged: the kernel's `CR2`, which a hosted kernel cannot read as a
/// register. Kernel context only, right after the trap, before another one
/// on this thread.
pub fn fault_address() -> Option<usize> {
    ANCHOR_ROW.with(|row| {
        row.get()
            .map(|slot| ANCHOR_FAULT_ADDR[slot].load(Ordering::Acquire))
    })
}

/// Install the bridge. Idempotent in effect, and the only thing it does to a
/// signal the guest cannot have raised is nothing: a fault that did not come
/// from guest code is handed straight back to the default action, so a kernel
/// bug still dies where it happened, with its core and its backtrace.
///
/// # Safety
///
/// Installs process-wide signal handlers. Must run before any guest does.
pub unsafe fn init() {
    // The tick must not land inside a fault handler: it would dress the
    // handler's own context up as the guest's. The fault handlers block it,
    // and it blocks itself (no NODEFER).
    let mut without_tick = SigSet::empty();
    without_tick.add(Signal::SIGALRM);
    for (signal, _) in SIGNAL_VECTORS {
        let action = if signal == Signal::SIGALRM {
            SigAction::new(
                SigHandler::SigAction(on_fault),
                // RESTART: a tick that reaches the kernel's own side anyway
                // (the window between the timer being disarmed and the
                // signal arriving) must not fail its blocking syscall.
                SaFlags::SA_SIGINFO | SaFlags::SA_ONSTACK | SaFlags::SA_RESTART,
                SigSet::empty(),
            )
        } else {
            SigAction::new(
                SigHandler::SigAction(on_fault),
                // ONSTACK so a guest that faulted with a stack pointer it
                // cannot write through -- the stack-overflow case -- still
                // reaches the handler. NODEFER so the handler's own mistakes
                // are not hidden behind a blocked signal.
                SaFlags::SA_SIGINFO | SaFlags::SA_ONSTACK | SaFlags::SA_NODEFER,
                without_tick,
            )
        };
        if let Err(err) = sigaction(signal, &action) {
            error!("libos: cannot bridge {:?} to the guest: {}", signal, err);
        }
    }
}

/// How many host threads the bridge can recognise at once.
///
/// One slot per thread that has ever gone out to a guest. They are never
/// freed: a thread's alternate stack is leaked for the same reason (see
/// `ensure_altstack`), so a stale slot stays truthful rather than dangling.
const ANCHORS: usize = 256;

/// The alternate-stack pointer of each registered thread, and the kernel FS
/// base that goes with it. Two flat arrays of atomics rather than a map
/// because the signal handler reads them: an atomic load is
/// async-signal-safe, a lock or an allocation is not.
static ANCHOR_STACK: [AtomicUsize; ANCHORS] = [const { AtomicUsize::new(0) }; ANCHORS];
static ANCHOR_BASE: [AtomicU64; ANCHORS] = [const { AtomicU64::new(0) }; ANCHORS];
/// Whether a tick landed on this thread while it was NOT out in the guest
/// (see `note_tick_off_guest`): the kernel's own `need_resched`, read and
/// cleared by `take_resched` before the next trip out.
static ANCHOR_RESCHED: [AtomicBool; ANCHORS] = [const { AtomicBool::new(false) }; ANCHORS];
/// The linear address of the last page fault bridged on this thread: the
/// `CR2` the kernel reads back through `fault_address` when the trap frame
/// says `#PF`. One per thread, written only by the handler on that thread.
static ANCHOR_FAULT_ADDR: [AtomicUsize; ANCHORS] = [const { AtomicUsize::new(0) }; ANCHORS];

/// Where a given alternate stack's slot starts. Linear probing from here, and
/// nothing is ever removed, so an empty slot means "not registered" and ends
/// the search.
fn anchor_slot(ss_sp: usize) -> usize {
    (ss_sp >> 12) % ANCHORS
}

/// This host thread's kernel FS base, or `None` when this thread has never
/// been out to a guest.
///
/// It used to come from `gs:0`, which `trapframe` keeps pointing at the
/// kernel's own thread area. That read is a dereference of an address the
/// GUEST controls: `wrgsbase` is a user-mode instruction, so a guest that sets
/// its own `GS` base makes this handler fault inside itself and the host kills
/// the process with nothing said. The alternate stack is the one per-thread
/// place the handler can find without a segment register at all --
/// `sigaltstack` is async-signal-safe and hands back the pointer the thread
/// registered.
///
/// # Why the base is in a table and not under the stack
///
/// It was in the word just below `ss_sp`, which is wrong on any thread whose
/// alternate stack is not ours. Rust's own std registers a `sigaltstack` on
/// every thread it spawns, for its stack-overflow handler, and those stacks
/// are mapped with a guard page BELOW them -- so on an async-std worker that
/// had not yet entered a guest, reading `ss_sp - 8` touched the guard page,
/// faulted inside the handler, and with `SA_NODEFER` did it again on top of
/// itself. The table is checked against `ss_sp` first, so a stack we did not
/// register is simply not ours and no memory outside it is ever read.
/// What the bridge knows about the host thread the handler runs on.
struct Anchor {
    /// The alternate stack `ensure_altstack` registered for it.
    ss_sp: usize,
    /// The kernel's own FS base on it.
    base: u64,
    /// Its row in the anchor tables.
    slot: usize,
}

/// The fault frame `fault_fn_entry` reads, for the thread whose alternate
/// stack starts at `ss_sp`: the words `ensure_altstack` reserved just under
/// that stack. Only for a stack the bridge registered itself.
fn fault_frame(ss_sp: usize) -> *mut u64 {
    (ss_sp - FAULT_FRAME_BYTES) as *mut u64
}

const FAULT_FRAME_BYTES: usize = trapframe::FAULT_FRAME_WORDS * size_of::<u64>();

unsafe fn anchor() -> Option<Anchor> {
    let mut current = libc::stack_t {
        ss_sp: ptr::null_mut(),
        ss_flags: 0,
        ss_size: 0,
    };
    if libc::sigaltstack(ptr::null(), &mut current) != 0
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
            // An empty slot: the probe for `ss_sp` would have stopped here
            // when it was written, so it is not registered. Not our stack.
            0 => return None,
            found if found == ss_sp => {
                let base = ANCHOR_BASE[slot].load(Ordering::Acquire);
                return (base != 0).then_some(Anchor { ss_sp, base, slot });
            }
            _ => {}
        }
    }
    None
}

/// Remember this thread's alternate stack and the kernel FS base that goes
/// with it, so the handler can find the base from the stack alone: its row.
/// `None` when the table is full, which turns the bridge off for this thread
/// rather than guessing.
fn register_anchor(ss_sp: usize, base: u64) -> Option<usize> {
    let first = anchor_slot(ss_sp);
    for step in 0..ANCHORS {
        let slot = (first + step) % ANCHORS;
        // A thread registers once, and its `ss_sp` is an address no other
        // live thread can also have, so claiming an empty slot is enough.
        if ANCHOR_STACK[slot]
            .compare_exchange(0, ss_sp, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            ANCHOR_BASE[slot].store(base, Ordering::Release);
            return Some(slot);
        }
    }
    None
}

/// Read one of this thread's segment bases.
unsafe fn arch_get_base(which: libc::c_int) -> Option<u64> {
    let mut base: u64 = 0;
    (libc::syscall(libc::SYS_arch_prctl, which, &mut base) == 0).then_some(base)
}

/// Hand the signal back to the host: clear our handler and return, so the
/// faulting instruction runs again and the default action takes it. This is
/// the path for the kernel's own faults, which must not be dressed up as a
/// guest's.
unsafe fn leave_to_the_host(signal: libc::c_int) {
    if let Ok(signal) = Signal::try_from(signal) {
        let _ = sigaction(
            signal,
            &SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty()),
        );
    }
}

/// The handler. Everything it does is on the `ucontext` the host handed it:
/// it lays the words `trapframe::fault_fn_entry` expects on this thread's
/// fault frame (`fault_frame`), points the stack pointer at them and the
/// instruction pointer at that entry, so the ordinary return from this
/// handler lands in the entry with the guest's registers still in place, and
/// the entry walks back into the kernel.
///
/// It reads no Rust thread-local and takes no lock, because it runs with the
/// GUEST's `FS` base: a `thread_local!` here would read the guest's memory.
extern "C" fn on_fault(
    signal: libc::c_int,
    _info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    // Every line of this is a raw pointer or a syscall; the signature cannot
    // be `unsafe fn` because that is not what `sigaction` takes.
    unsafe { bridge_fault(signal, context) }
}

unsafe fn bridge_fault(signal: libc::c_int, context: *mut libc::c_void) {
    let Some(vector) = SIGNAL_VECTORS
        .iter()
        .find(|(known, _)| *known as libc::c_int == signal)
        .map(|(_, vector)| *vector)
    else {
        return leave_to_the_host(signal);
    };
    if signal == libc::SIGALRM && !tick_is_the_guest_s(context) {
        // A tick is the one signal the kernel's own side can get without
        // anything being wrong. It is not lost: the thread's slice is over
        // all the same, and the next trip out is told so.
        return note_tick_off_guest();
    }
    let Some(Anchor {
        ss_sp,
        base: kernel_fsbase,
        slot,
    }) = anchor()
    else {
        return leave_to_the_host(signal);
    };
    // A guest that set its own `GS` base. `trapframe` keeps the kernel's way
    // back in there -- `SWITCH_TO_KERNEL_STACK` is `mov rsp, gs:64` -- and
    // `wrgsbase` is a user-mode instruction, so the guest can take it away.
    // Measured in a libos build in this container, from `Property.RegisterGs`
    // of the Zircon core tests:
    //
    //   syscall_fn_entry+5:  mov %gs:0x40,%rsp
    //   si_addr = 0xdeb12     (the guest's GS base + 0x40)
    //   gs_base = 0xdead2     (the value the test asked for)
    //
    // Put the kernel's base back and let the instruction run again. The next
    // fault, if there is one, finds `GS` already right and takes the ordinary
    // path, so this cannot loop.
    //
    // The guest's own base is gone, which is a divergence: `trapframe`'s libos
    // entry has nowhere else to keep the kernel's, and it already discards the
    // guest's `gs_base` on every trip in (`push 0  # ignore gs_base`). Losing
    // it beats killing the kernel, the guest and the harness together, which
    // is what happened before.
    if arch_get_base(ARCH_GET_GS) != Some(kernel_fsbase) {
        libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, kernel_fsbase);
        // `SIGSEGV`, `SIGBUS`, `SIGILL` and `SIGFPE` are FAULTS: the host
        // saved the rip of the instruction that faulted, so returning runs it
        // again, now with `GS` right, and the next fault -- if there is one --
        // takes the ordinary path below. This cannot loop.
        //
        // `SIGTRAP` is a TRAP: the host saved the rip AFTER the `int3`, so
        // returning resumes past it and the guest's breakpoint exception
        // vanishes, leaving whoever waits on the exception channel waiting
        // for ever. `GS` is fixed either way; this one still has to be
        // translated, so fall through.
        //
        // `SIGSYS` is the same: the `syscall` instruction is behind the saved
        // rip and was never executed, so returning would skip it.
        if signal != libc::SIGTRAP && signal != libc::SIGSYS {
            return;
        }
    }
    let Some(fsbase) = arch_get_base(ARCH_GET_FS) else {
        return leave_to_the_host(signal);
    };
    if fsbase == kernel_fsbase || context.is_null() {
        // The kernel's own fault: it runs with its own fsbase loaded.
        if signal == libc::SIGALRM {
            return note_tick_off_guest();
        }
        return leave_to_the_host(signal);
    }

    let gregs = &mut (*(context as *mut libc::ucontext_t)).uc_mcontext.gregs;
    // The frame goes on memory of this bridge's own -- the words reserved
    // under this thread's alternate stack, see `ensure_altstack` -- and not
    // under the guest's stack pointer: that is the guest's red zone, and a
    // Zircon thread exiting detached has already unmapped its stack by the
    // time its `syscall` traps (`C11ThreadTest.SelfDetachAndFree`), so a
    // write there faulted inside this handler and took the process down.
    //
    // Nothing runs on these words between here and `fault_fn_entry` reading
    // them: the only signals the bridge handles are the synchronous ones,
    // and the entry's first instructions cannot raise any. An asynchronous
    // signal arriving there finds `rsp` OUTSIDE the alternate stack, so the
    // host switches to that stack's top for it and leaves the frame alone.
    let words = fault_frame(ss_sp);
    // In the order the entry reads them: the error code, the vector, the
    // faulting rip, the guest's own r11 (the entry needs that register as
    // scratch), the guest's own rsp and its rflags.
    //
    // A trapped `syscall` says so in the error code; the saved rip is already
    // the instruction after it, where the stub's own tail carries on.
    //
    // A `SIGSEGV` the host took as a page fault (`REG_TRAPNO` is the vector
    // the CPU raised) is handed on as one, with the CPU's own error code and
    // the faulting address kept for `fault_address`: the kernel's page-fault
    // handler serves it, which is what a write to a page `create_child`
    // just took `WRITE` from needs (`VmoCloneTestCase.Commit` died here as
    // a `#GP`). What the host took as `#GP` -- a non-canonical address --
    // stays one.
    let (vector, error_code) = if signal == libc::SIGSYS {
        (vector, SYSCALL_INSTRUCTION)
    } else if signal == libc::SIGSEGV && gregs[libc::REG_TRAPNO as usize] == PAGE_FAULT_VECTOR {
        ANCHOR_FAULT_ADDR[slot].store(gregs[libc::REG_CR2 as usize] as usize, Ordering::Release);
        (
            PAGE_FAULT_VECTOR as usize,
            gregs[libc::REG_ERR as usize] as usize & PAGE_FAULT_ERROR_BITS,
        )
    } else {
        (vector, 0)
    };
    words.write(error_code as u64);
    words.add(1).write(vector as u64);
    words.add(2).write(gregs[libc::REG_RIP as usize] as u64);
    words.add(3).write(gregs[libc::REG_R11 as usize] as u64);
    words.add(4).write(gregs[libc::REG_RSP as usize] as u64);
    // The guest's flags, for the record: the entry scrubs the live ones (DF,
    // AC, TF) before the kernel runs, so this is the only copy that survives.
    words.add(5).write(gregs[libc::REG_EFL as usize] as u64);
    // And TF comes off the live ones HERE, not in the entry: the host
    // restores these flags on its way out, and a guest that was being
    // single-stepped (the trap that brought it here was its own `TF`) would
    // step the entry's first instruction and trap again before the entry's
    // own scrub, with the guest's `fs` still loaded, so the bridge would
    // take that trap for another fault of the guest's, at an address inside
    // the entry, and never get out.
    gregs[libc::REG_EFL as usize] &= !(X86_FLAGS_TF as i64);
    gregs[libc::REG_RSP as usize] = words as i64;
    gregs[libc::REG_RIP as usize] = trapframe::fault_fn_entry as *const () as i64;
}

/// Did this tick land on the guest's own code?
///
/// The alternatives are the kernel proper, which runs with its own `FS` base
/// and is told apart by that below, and `trapframe`'s way in and out
/// (`syscall_fn_entry`, `fault_fn_entry`, the tail of `syscall_fn_return`),
/// which run with the GUEST's base loaded and are told apart only by their
/// address. A tick there is not an interrupt: the way in has the catcher's frame
/// under its feet and the way out has the guest's registers half restored;
/// dressing either up as an interrupt would lose the trap being delivered,
/// so it marks the thread for a reschedule instead.
unsafe fn tick_is_the_guest_s(context: *mut libc::c_void) -> bool {
    if context.is_null() {
        return false;
    }
    let rip =
        (*(context as *const libc::ucontext_t)).uc_mcontext.gregs[libc::REG_RIP as usize] as usize;
    let start = GUEST_START.load(Ordering::Relaxed);
    let end = GUEST_END.load(Ordering::Relaxed);
    start < end && (start..end).contains(&rip)
}

/// A tick landed on this host thread while it was on the kernel's side --
/// in a syscall, on the way in or out, or parked. The thread's slice is up
/// just the same, so the next `enter_uspace` on it yields first instead of
/// going out: Linux's `TIF_NEED_RESCHED`, set from the timer interrupt and
/// honoured on the way back to user mode.
///
/// Why it cannot be dropped: a guest that makes a syscall every few
/// microseconds is never out in the guest for a whole 10 ms, so a tick that
/// only counted when it landed on guest code never counted at all.
/// `PortStressTest.CloseWaitRace` has four threads doing exactly that on a
/// four-CPU host, and the fifth thread, which is meant to end the test,
/// never got a CPU back.
///
/// Written through the anchor tables, not a thread-local: this runs with
/// whatever `FS` base the signal found, which may be the guest's.
unsafe fn note_tick_off_guest() {
    if let Some(anchor) = anchor() {
        ANCHOR_RESCHED[anchor.slot].store(true, Ordering::Release);
    }
}

/// Whether a tick landed on this host thread since the last trip out, and
/// clear it. Kernel context only: it goes through this thread's own cached
/// anchor row.
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

/// Arm this host thread's slice timer, once: a periodic `SIGALRM` every
/// `PREEMPT_SLICE_NS`, delivered to THIS thread (`SIGEV_THREAD_ID`; a
/// process-wide timer lands on whichever thread the host picks, which is as
/// likely parked as out in a guest). A tick that finds the thread out in the
/// guest is bridged as the timer interrupt; one that finds it anywhere else
/// marks the thread for a reschedule (`take_resched`).
///
/// It used to be armed for the trip out and disarmed on the way back, so the
/// kernel's own side never saw a tick. That was two `timer_settime` per
/// system call and, worse, a countdown that started over on every trip: see
/// `note_tick_off_guest`. The kernel's blocking host calls see `SIGALRM`
/// with `SA_RESTART` instead, which restarts the ones that can be and hands
/// `EINTR` to the pollers, which retry.
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
        // SAFETY: the raw syscall, with the kernel's own `int` timer id
        // rather than glibc's pointer-sized one, which is why the libc
        // wrapper is not used.
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

/// Make the host refuse every `syscall` instruction executed from a guest
/// address, so that it reaches the kernel through the bridge instead.
///
/// `guest` is the whole range a guest can ever execute from -- every hosted
/// address-space window -- and nothing the kernel itself runs from may lie
/// inside it: a kernel syscall caught by this filter would be dressed up as
/// a guest's and the kernel would wait for ever on its own futex. The seccomp
/// filter is installed on every thread of the process (`TSYNC`) and
/// inherited by every thread created after; `PR_SET_NO_NEW_PRIVS` is the
/// price the host asks for installing one unprivileged.
///
/// # Safety
///
/// Process-wide: from here on a `syscall` from inside `guest` arrives at
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
/// allow every other.
///
/// Classic BPF compares 32-bit words, so the range is read in units of
/// 4 GiB: only the high half of the pointer is loaded, and the range's ends
/// are rounded outwards to the nearest multiple -- the start down, the end
/// up -- so that nothing inside the range can slip past. The hosted windows
/// are 64 GiB apiece from a 4 GiB-aligned base, so for them the rounding
/// changes nothing.
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
        // A = instruction_pointer >> 32
        insn(
            libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
            0,
            0,
            SECCOMP_DATA_IP_HIGH,
        ),
        // below the range: allow
        insn(libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K, 0, 2, low),
        // at or past its end: allow
        insn(libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K, 1, 0, high),
        insn(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_TRAP),
        insn(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
    ]
}

/// The per-thread alternate stack the handler runs on.
///
/// `SA_ONSTACK` is a request, not a guarantee: without a stack registered for
/// THIS host thread the handler runs on the faulting stack instead, which is
/// the one case that cannot work -- a guest that overflowed its stack faults
/// again inside the handler and the host kills the process with nothing said.
/// Registered from kernel context, where a Rust thread-local is the kernel's
/// own, on each thread's first trip out to a guest.
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
        // Allocated as words, not bytes, so it is aligned enough for a
        // signal frame.
        //
        // Leaked on purpose: the host reads it on every signal for as long as
        // this thread lives, and a thread that is going away has nothing left
        // to fault.
        //
        // The first `FAULT_FRAME_WORDS` words are NOT part of the stack the
        // host is told about: they are the bridge's fault frame for this
        // thread (`fault_frame`), kept just under `ss_sp` so the handler finds
        // them from the stack pointer alone. See `bridge_fault`.
        let stack = vec![0u64; trapframe::FAULT_FRAME_WORDS + STACK_WORDS].into_boxed_slice();
        let stack = &mut Box::leak(stack)[trapframe::FAULT_FRAME_WORDS..];
        // The kernel's own thread area, which is loaded right now -- this runs
        // from `enter_uspace`, before the trip out. The handler cannot ask for
        // it later: by then `FS` is the guest's and `GS` is whatever the guest
        // left there. See `anchor`.
        let mut base: u64 = 0;
        // SAFETY: a read of this thread's own FS base into a local.
        if unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_GET_FS, &mut base) } != 0 || base == 0
        {
            warn!("libos: cannot record this host thread's own thread area");
            return;
        }
        let ss_sp: *mut libc::c_void = stack.as_mut_ptr().cast();
        // The base goes in the bridge's own table, keyed by this pointer, not
        // in a word under the stack: the handler must never read memory it
        // did not itself reserve. See `anchor`.
        let Some(slot) = register_anchor(ss_sp as usize, base) else {
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
            // Without it the bridge is off for this thread: the handler finds
            // no base and hands the signal back to the host. That is the
            // honest answer -- a handler running on a stack the guest just
            // overflowed is the one case that cannot work anyway.
            warn!("libos: no alternate signal stack for this thread");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a classic-BPF program over a `seccomp_data` whose instruction
    /// pointer is `ip`, the way the host would. Only the four instruction
    /// kinds the filter uses are implemented; anything else is a failure of
    /// the test, not an allow.
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
        // Both ends of the range, inclusive-exclusive.
        assert_eq!(run(&filter, 0x2_0000_0000), libc::SECCOMP_RET_TRAP);
        assert_eq!(run(&filter, 0x1a00_0ea0_4c), libc::SECCOMP_RET_TRAP);
        assert_eq!(run(&filter, 0x3fff_ffff_ffff), libc::SECCOMP_RET_TRAP);
        assert_eq!(run(&filter, 0x4000_0000_0000), libc::SECCOMP_RET_ALLOW);
        assert_eq!(run(&filter, 0x1_ffff_ffff), libc::SECCOMP_RET_ALLOW);
        // Where the kernel's own syscalls come from: a PIE binary, its libc,
        // a non-PIE binary, the host vDSO.
        for kernel in [
            0x5555_5555_4000u64,
            0x7fff_f7ca_b144,
            0x40_1000,
            0x7fff_ffff_e000,
        ] {
            assert_eq!(
                run(&filter, kernel),
                libc::SECCOMP_RET_ALLOW,
                "{:#x}",
                kernel
            );
        }
        // And from this very test, which is kernel code too.
        let here = run as fn(&[libc::sock_filter], u64) -> u32 as *const () as u64;
        assert_eq!(run(&filter, here), libc::SECCOMP_RET_ALLOW);
    }

    /// A range that does not start or end on a 4 GiB multiple is widened,
    /// never narrowed: the filter must not let a guest address through.
    #[test]
    fn a_range_off_the_4_gib_grid_is_rounded_outwards() {
        let filter = guest_syscall_filter(0x2_8000_0000..0x3_8000_0000);
        assert_eq!(run(&filter, 0x2_0000_0000), libc::SECCOMP_RET_TRAP);
        assert_eq!(run(&filter, 0x3_ffff_ffff), libc::SECCOMP_RET_TRAP);
        assert_eq!(run(&filter, 0x4_0000_0000), libc::SECCOMP_RET_ALLOW);
        assert_eq!(run(&filter, 0x1_ffff_ffff), libc::SECCOMP_RET_ALLOW);
    }

    /// The vector the bridge writes for a trapped syscall is the one the
    /// kernel's syscall path writes for itself, so `TrapReason` cannot tell
    /// them apart.
    #[test]
    fn a_trapped_syscall_carries_the_syscall_vector() {
        let (_, vector) = SIGNAL_VECTORS
            .iter()
            .find(|(signal, _)| *signal == Signal::SIGSYS)
            .expect("SIGSYS is bridged");
        assert_eq!(*vector, 0x100);
        // And the error code tells it from a `call`ed stub's entry, whose
        // `run_fncall` writes zero.
        assert_ne!(SYSCALL_INSTRUCTION, 0);
    }

    /// A tick that lands on the kernel's side of a thread is kept for that
    /// thread's next trip out, and for that one only: it is cleared by being
    /// taken, and a thread that has never been out has nothing to take.
    #[test]
    fn a_tick_off_the_guest_is_taken_once_on_the_next_trip_out() {
        std::thread::spawn(|| {
            assert!(
                !take_resched(),
                "a thread that never went out owes no yield"
            );
            // SAFETY: the handler's own path, run from kernel context on a
            // thread that only ever runs this test.
            unsafe { note_tick_off_guest() };
            assert!(
                !take_resched(),
                "a tick on a thread the bridge does not know is nobody's"
            );
            ensure_altstack();
            unsafe { note_tick_off_guest() };
            unsafe { note_tick_off_guest() };
            assert!(
                take_resched(),
                "the tick was not kept for the next trip out"
            );
            assert!(!take_resched(), "one yield per slice, not one per tick");
        })
        .join()
        .unwrap();
    }
}
