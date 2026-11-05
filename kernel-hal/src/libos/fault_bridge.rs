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
use nix::libc;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

/// `arch_prctl` codes. The bridge needs them to tell a guest fault from the
/// kernel's own: `trapframe` enters the guest with `FS` pointing at the
/// guest's thread area and `GS` at the kernel's, and leaves `GS` zero until a
/// thread has gone out at least once.
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

/// The `arch_prctl` bases, or `None` when this thread has never been out to a
/// guest (its `GS` base is still zero, so there is nothing to read there).
unsafe fn thread_bases() -> Option<(u64, u64)> {
    let mut gsbase: u64 = 0;
    if libc::syscall(libc::SYS_arch_prctl, ARCH_GET_GS, &mut gsbase) != 0 || gsbase == 0 {
        return None;
    }
    let mut fsbase: u64 = 0;
    if libc::syscall(libc::SYS_arch_prctl, ARCH_GET_FS, &mut fsbase) != 0 {
        return None;
    }
    // `trapframe` keeps the kernel's own fsbase at `gs:0` for the way back in.
    let kernel_fsbase = (gsbase as *const u64).read();
    Some((kernel_fsbase, fsbase))
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
    let Some((kernel_fsbase, fsbase)) = thread_bases() else {
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
    use core::ptr;

    const STACK_SIZE: usize = 64 * 1024;

    thread_local! {
        static REGISTERED: Cell<bool> = const { Cell::new(false) };
    }

    REGISTERED.with(|registered| {
        if registered.get() {
            return;
        }
        registered.set(true);
        // Leaked on purpose: the host reads it on every signal for as long as
        // this thread lives, and a thread that is going away has nothing left
        // to fault.
        let stack = vec![0u8; STACK_SIZE].into_boxed_slice();
        let stack = Box::leak(stack);
        let altstack = libc::stack_t {
            ss_sp: stack.as_mut_ptr().cast(),
            ss_flags: 0,
            ss_size: STACK_SIZE,
        };
        if unsafe { libc::sigaltstack(&altstack, ptr::null_mut()) } != 0 {
            warn!("libos: no alternate signal stack for this thread");
        }
    });
}
