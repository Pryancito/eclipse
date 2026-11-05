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

use core::convert::TryFrom;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
/// `SIGSEGV` becomes `#GP` rather than `#PF` on purpose: a page fault is only
/// meaningful with the address that caused it, which the trap frame has no
/// room for, and a libos guest has no demand paging to serve anyway -- every
/// one of these is fatal to the thread. `#GP` says "this thread did something
/// the machine refused" without claiming an address the frame cannot carry,
/// and the Zircon run loop answers both with a fatal exception.
const SIGNAL_VECTORS: [(Signal, usize); 5] = [
    (Signal::SIGILL, 6),   // #UD, invalid opcode
    (Signal::SIGSEGV, 13), // #GP, general protection
    (Signal::SIGBUS, 17),  // #AC, alignment check
    (Signal::SIGFPE, 0),   // #DE, divide error
    (Signal::SIGTRAP, 3),  // #BP, breakpoint
];

/// Install the bridge. Idempotent in effect, and the only thing it does to a
/// signal the guest cannot have raised is nothing: a fault that did not come
/// from guest code is handed straight back to the default action, so a kernel
/// bug still dies where it happened, with its core and its backtrace.
///
/// # Safety
///
/// Installs process-wide signal handlers. Must run before any guest does.
pub unsafe fn init() {
    for (signal, _) in SIGNAL_VECTORS {
        let action = SigAction::new(
            SigHandler::SigAction(on_fault),
            // ONSTACK so a guest that faulted with a stack pointer it cannot
            // write through -- the stack-overflow case -- still reaches the
            // handler. NODEFER so the handler's own mistakes are not hidden
            // behind a blocked signal.
            SaFlags::SA_SIGINFO | SaFlags::SA_ONSTACK | SaFlags::SA_NODEFER,
            SigSet::empty(),
        );
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
unsafe fn kernel_fsbase() -> Option<u64> {
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
                return (base != 0).then_some(base);
            }
            _ => {}
        }
    }
    None
}

/// Remember this thread's alternate stack and the kernel FS base that goes
/// with it, so the handler can find the base from the stack alone. `false`
/// when the table is full, which turns the bridge off for this thread rather
/// than guessing.
fn register_anchor(ss_sp: usize, base: u64) -> bool {
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
            return true;
        }
    }
    false
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
/// it lays the four words `trapframe::fault_fn_entry` expects below the
/// guest's stack pointer and points the instruction pointer at that entry, so
/// the ordinary return from this handler lands in the entry with the guest's
/// registers still in place, and the entry walks back into the kernel.
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
    let Some(kernel_fsbase) = kernel_fsbase() else {
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
        if signal != libc::SIGTRAP {
            return;
        }
    }
    let Some(fsbase) = arch_get_base(ARCH_GET_FS) else {
        return leave_to_the_host(signal);
    };
    if fsbase == kernel_fsbase || context.is_null() {
        // The kernel's own fault: it runs with its own fsbase loaded.
        return leave_to_the_host(signal);
    }

    let gregs = &mut (*(context as *mut libc::ucontext_t)).uc_mcontext.gregs;
    let rsp = gregs[libc::REG_RSP as usize] as u64;
    let frame = rsp - (trapframe::FAULT_FRAME_WORDS * size_of::<u64>()) as u64;
    // From the guest's stack pointer downwards: its own r11 (the entry needs
    // that register as scratch), the faulting rip, the vector, the error code.
    let words = frame as *mut u64;
    words.add(3).write(gregs[libc::REG_R11 as usize] as u64);
    words.add(2).write(gregs[libc::REG_RIP as usize] as u64);
    words.add(1).write(vector as u64);
    words.write(0);
    gregs[libc::REG_RSP as usize] = frame as i64;
    gregs[libc::REG_RIP as usize] = trapframe::fault_fn_entry as *const () as i64;
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
        let stack = vec![0u64; STACK_WORDS].into_boxed_slice();
        let stack = Box::leak(stack);
        // The kernel's own thread area, which is loaded right now -- this runs
        // from `enter_uspace`, before the trip out. The handler cannot ask for
        // it later: by then `FS` is the guest's and `GS` is whatever the guest
        // left there. See `kernel_fsbase`.
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
        // did not itself reserve. See `kernel_fsbase`.
        if !register_anchor(ss_sp as usize, base) {
            warn!("libos: the fault bridge has no room left to recognise this host thread");
            return;
        }
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
