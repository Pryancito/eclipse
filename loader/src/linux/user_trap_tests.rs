//! What brings a thread out of user mode, and what the kernel does about it.
//!
//! The decisions around a trap had tests -- which signal a CPU exception is
//! (`cpu_fault_signal`), whether a fault signal can be delivered at all
//! (`fault_signal_kills`), what a report says about an address
//! (`fault_report_tests`) -- and the code that *takes* them had none, because
//! all of it wants a [`CurrentThread`] and a `CurrentThread`'s field is
//! private: only `zircon-object`'s own tests can build one out of an
//! `Arc<Thread>`. From here the way in is `Thread::start`, which hands the
//! callback a `CurrentThread` synchronously; see [`with_a_linux_thread`].
//!
//! That reaches the three the run loop is made of and nothing had called:
//! `thread_should_stop`, the condition on every one of its four exits;
//! `fault_kills` and `force_fault_signal`, which decide whether a fault
//! reaches a handler or ends the process; and `handle_user_trap`, the router
//! that turns a trap reason into one of them.
//!
//! Two limits of the fixture, both measured rather than assumed:
//!
//!  * The router reads the trap cause off the context, and only x86_64 keeps
//!    it there (RISC-V reads `scause`, AArch64 `ESR_EL1`), so the tests that
//!    build a trap use [`UserContext::with_raw_trap`] and say so.
//!  * A page fault is still out of reach even there: decoding vector 14 reads
//!    `CR2` for the faulting address, which is a live register of whoever is
//!    running, so a context cannot carry one. `TrapReason::Interrupt` is out
//!    too -- `handle_irq` is `unimplemented!()` under `libos`, there being no
//!    second CPU to interrupt.

use super::*;
use core::task::{Context as PollContext, Poll, Waker};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ThreadFn;

type ThreadFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What a test body returns once its assertions are done: the thread's whole
/// life, which is to end.
fn done(ct: CurrentThread) -> ThreadFuture {
    Box::pin(async move { drop(ct) })
}

/// A pid no other test is using. `Process::create_linux` is given its pid by
/// the boot code rather than allocating one, and these run in parallel in one
/// address space, so two processes built with the same number would be two
/// processes answering to it.
fn a_pid() -> KoID {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x4000_0000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Run `f` with a `CurrentThread` of a Linux process of its own.
///
/// `Thread::start` calls the body synchronously, on this very thread, before
/// it spawns the future the body returns -- so the assertions go INSIDE `f`,
/// where a panic is this test's failure, and not in the future, where it
/// would be some executor's.
///
/// `f` is a plain `fn` and not a closure because that is what `ThreadFn` is,
/// which is why each test below carries its own `body`.
fn with_a_linux_thread(f: ThreadFn) {
    let job = zircon_object::task::ROOT_JOB.clone();
    let proc = Process::create_linux(&job, RamFS::new(), 0, None, a_pid())
        .expect("a Linux process on an empty ramfs");
    Thread::create_linux(&proc)
        .expect("its first thread")
        .start(f)
        .expect("a thread that has never been started starts");
}

/// A handler the process installed itself: any address will do, the
/// dispositions being the two reserved values.
fn a_handler() -> SignalAction {
    SignalAction {
        handler: 0x4000,
        flags: SignalActionFlags::empty(),
        restorer: 0,
        mask: Sigset::empty(),
    }
}

/// Install `action` for `signal` on this thread's process.
fn install(ct: &CurrentThread, signal: Signal, action: SignalAction) {
    ct.proc().linux().set_signal_action(signal, action);
}

/// Block `signal` on this thread.
fn block(ct: &CurrentThread, signal: Signal) {
    let thread = ct.inner();
    let mut linux = thread.lock_linux();
    let mut mask = linux.signal_mask();
    mask.insert(signal);
    linux.set_signal_mask(mask);
}

/// Is `signal` pending on this thread?
fn pending(ct: &CurrentThread, signal: Signal) -> bool {
    let thread = ct.inner();
    let is = thread.lock_linux().signals.contains(signal);
    is
}

/// Take `signal` and the `siginfo_t` it was queued with. Panics unless it is
/// pending, so a test that means to read the information says first that
/// there is some.
fn take_pending(ct: &CurrentThread, signal: Signal) -> SigInfo {
    let thread = ct.inner();
    let mut linux = thread.lock_linux();
    assert!(
        linux.signals.contains(signal),
        "{:?} is not pending, so there is no siginfo_t to read",
        signal
    );
    linux.take_siginfo(signal)
}

/// Drive `fut` to its answer without anything to wake it.
///
/// Nothing here is allowed to park: a trap handler that answers `Pending`
/// with no waker is a thread that never goes back to user mode, so the
/// budget running out is the failure and not a reason to wait.
fn answered<T>(fut: impl Future<Output = T>) -> T {
    let mut fut = Box::pin(fut);
    let mut cx = PollContext::from_waker(Waker::noop());
    for _ in 0..64 {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
    }
    panic!("the trap handler parked with nothing left to wake it");
}

// ---- the condition on every exit from the run loop --------------------

/// The ordinary answer, given once per syscall and once per fault: keep
/// going.
#[test]
fn a_running_thread_in_a_live_process_goes_back_to_user_mode() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        assert_eq!(ct.state(), ThreadState::Running);
        assert!(ct.proc().exit_code().is_none());
        assert!(!thread_should_stop(&ct));
        done(ct)
    }
    with_a_linux_thread(body);
}

/// Its own `exit(2)`, and the three ways a thread is killed from outside:
/// all of them leave it `Dying`, and `Dying` is not a state to enter user
/// mode from.
#[test]
fn a_thread_told_to_exit_does_not_go_back_to_user_mode() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        ct.exit();
        assert!(
            matches!(ct.state(), ThreadState::Dying | ThreadState::Dead),
            "a thread that has exited is {:?}",
            ct.state()
        );
        assert!(thread_should_stop(&ct));
        done(ct)
    }
    with_a_linux_thread(body);
}

/// And the other reason, which is not about this thread at all: an
/// `exit_group` in a sibling, or a fatal signal to the process. A thread must
/// not run one more user instruction in a process that has already reported
/// its status to its parent.
///
/// The two halves of the condition are not separable from here, and that is
/// worth stating rather than hiding: `Process::exit` stops every thread as it
/// records the status, so both `Dying` and an exit code arrive together. The
/// test above is the first half on its own (a thread that exits in a live
/// process); the second half on its own is the window between the two, which
/// nothing here can hold open.
#[test]
fn a_process_that_exits_stops_every_thread_in_it() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        assert!(!thread_should_stop(&ct));
        ct.proc().exit(0);
        assert_eq!(ct.proc().exit_code(), Some(0));
        assert!(thread_should_stop(&ct));
        done(ct)
    }
    with_a_linux_thread(body);
}

// ---- whether a fault reaches a handler --------------------------------

/// Nothing installed, so the default action decides, and for SIGSEGV the
/// default action is death.
#[test]
fn a_fault_signal_with_no_handler_kills() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        assert_eq!(ct.proc().linux().signal_action(Signal::SIGSEGV).handler, 0);
        assert!(fault_kills(&ct, Signal::SIGSEGV));
        done(ct)
    }
    with_a_linux_thread(body);
}

/// A handler is waiting for it. This is a JIT, a garbage collector's write
/// barrier, a guard page: a fault the program asked for, taken thousands of
/// times a second, and it must not end anything.
#[test]
fn a_fault_signal_a_handler_waits_for_does_not_kill() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGSEGV, a_handler());
        assert!(!fault_kills(&ct, Signal::SIGSEGV));
        done(ct)
    }
    with_a_linux_thread(body);
}

/// A handler that cannot run is not a handler. The faulting instruction is
/// re-executed on the way back, so a fault signal held off by the mask would
/// be taken again, and again: `force_sig` terminates instead.
#[test]
fn a_fault_signal_the_thread_blocked_kills_anyway() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGSEGV, a_handler());
        block(&ct, Signal::SIGSEGV);
        assert!(fault_kills(&ct, Signal::SIGSEGV));
        done(ct)
    }
    with_a_linux_thread(body);
}

// ---- and what delivering it does --------------------------------------

/// A death by signal, which `wait` reports as `WIFSIGNALED` -- not an exit
/// with status 139. `128 + signo` is a shell's way of printing a death, and
/// a process that exits with it has not died of a signal as far as any
/// parent can tell: `bash` printed nothing for a segfault and `system()` saw
/// a program that had exited.
#[test]
fn an_undeliverable_fault_exits_the_process_as_killed_by_that_signal() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        force_fault_signal(
            &ct,
            Signal::SIGSEGV,
            SigInfo::fault(Signal::SIGSEGV, SignalCode::SEGV_MAPERR, 0),
        );
        assert_eq!(
            ct.proc().exit_code(),
            Some(linux_object::process::exit_code_killed_by(
                Signal::SIGSEGV as u8
            )),
            "the process must look killed by SIGSEGV, not exited"
        );
        assert!(thread_should_stop(&ct));
        done(ct)
    }
    with_a_linux_thread(body);
}

/// The other branch: the signal is queued for the next pass of the run loop,
/// with the information that says WHERE the fault was and WHY -- which is
/// the whole of what a `SA_SIGINFO` handler is told, and what a guard-page
/// or a probe-the-memory handler reads to decide what to do.
#[test]
fn a_fault_a_handler_waits_for_is_queued_with_where_and_why() {
    const BAD: usize = 0xdead_0000;
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGSEGV, a_handler());
        let info = SigInfo::fault(Signal::SIGSEGV, SignalCode::SEGV_ACCERR, BAD);
        force_fault_signal(&ct, Signal::SIGSEGV, info);
        assert!(
            ct.proc().exit_code().is_none(),
            "a fault its handler takes ends nothing"
        );
        // `SigInfo` is not `Debug`, so the comparison carries its own words:
        // what a handler is told is these 128 bytes and nothing else.
        assert!(
            take_pending(&ct, Signal::SIGSEGV) == info,
            "the handler must be told the same address and code the fault carried"
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

/// Faulting again on the same instruction before the handler has run. The
/// signal is already pending, so queueing it a second time would change
/// nothing and the thread would return to the same instruction and fault
/// again: ~71k faults, every CPU spinning, downloads stalled. The second
/// one terminates.
#[test]
fn a_second_fault_before_the_handler_ran_kills_instead_of_looping() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGSEGV, a_handler());
        let info = SigInfo::fault(Signal::SIGSEGV, SignalCode::SEGV_MAPERR, 0);
        force_fault_signal(&ct, Signal::SIGSEGV, info);
        assert!(pending(&ct, Signal::SIGSEGV));
        assert!(ct.proc().exit_code().is_none());

        assert!(
            fault_kills(&ct, Signal::SIGSEGV),
            "a copy already pending is a handler that has not run yet"
        );
        force_fault_signal(&ct, Signal::SIGSEGV, info);
        assert_eq!(
            ct.proc().exit_code(),
            Some(linux_object::process::exit_code_killed_by(
                Signal::SIGSEGV as u8
            ))
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

// ---- the router -------------------------------------------------------
//
// `handle_user_trap` reads the cause off the context, so these build one. See
// the note at the top for why x86_64 and why not vector 14.

/// The pc the tests put in the context: an address no mapping covers, so the
/// report cannot read bytes at it (which is the point -- see
/// `the_report_does_not_read_the_code_at_an_address_nobody_mapped`).
#[cfg(target_arch = "x86_64")]
const AT: usize = 0x1_0000_1000;

/// #DE, the divide error: a context straight out of [`UserContext::new`]
/// already carries it, vector 0 being the first of the table.
#[cfg(target_arch = "x86_64")]
fn a_trap(vector: usize) -> Box<UserContext> {
    let mut ctx = Box::new(UserContext::with_raw_trap(vector, 0));
    ctx.set_field(UserContextField::InstrPointer, AT);
    ctx
}

/// Divide by zero with nothing installed for SIGFPE. The default action for
/// it is death, so the process ends -- and on the way out it prints the
/// report, which is what `dump_user_fault_context` is for and what a #DE
/// used to get none of.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_divide_error_nobody_handles_kills_the_process_as_sigfpe() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        assert!(matches!(
            a_trap(0x00).trap_reason(),
            TrapReason::GernelFault(0x00)
        ));
        let out = answered(handle_user_trap(&ct, a_trap(0x00)));
        assert!(
            out.is_ok(),
            "a fault is handled, not an error of the kernel"
        );
        assert!(out.unwrap().is_none(), "no syscall to make again");
        assert_eq!(
            ct.proc().exit_code(),
            Some(linux_object::process::exit_code_killed_by(
                Signal::SIGFPE as u8
            )),
            "a #DE nobody handles is a death by SIGFPE"
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

/// And with a handler installed it is delivered instead, as Linux delivers
/// it: `FPE_INTDIV`, at the instruction that divided. The code matters --
/// a handler that means to fix up an integer division has to tell it from
/// the `FPE_FLTDIV` of a floating-point one.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_divide_error_a_handler_waits_for_arrives_as_fpe_intdiv_at_the_instruction() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGFPE, a_handler());
        let out = answered(handle_user_trap(&ct, a_trap(0x00)));
        assert!(out.expect("handled").is_none());
        assert!(
            ct.proc().exit_code().is_none(),
            "a fault its handler takes ends nothing"
        );
        assert!(
            take_pending(&ct, Signal::SIGFPE)
                == SigInfo::fault(Signal::SIGFPE, SignalCode::FPE_INTDIV, AT),
            "a #DE is FPE_INTDIV at the faulting instruction"
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

/// #GP, which is what a misaligned `movaps` or a non-canonical pointer
/// raises -- not a page fault. The CPU gives no address for it, so Linux
/// sends `SI_KERNEL` with `si_addr` zero rather than inventing one; naming
/// the pc there would point a handler at the instruction as though that were
/// the address it had touched.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_general_protection_fault_arrives_as_a_sigsegv_from_the_kernel_with_no_address() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGSEGV, a_handler());
        let out = answered(handle_user_trap(&ct, a_trap(0x0d)));
        assert!(out.expect("handled").is_none());
        assert!(
            take_pending(&ct, Signal::SIGSEGV)
                == SigInfo::fault(Signal::SIGSEGV, SignalCode::KERNEL, 0),
            "a #GP carries no faulting address"
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

/// #UD: an instruction the CPU does not know. `ILL_ILLOPC` at the pc, which
/// is the one fault kind where the pc IS the thing complained about.
#[test]
#[cfg(target_arch = "x86_64")]
fn an_undefined_instruction_arrives_as_sigill_at_the_instruction() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGILL, a_handler());
        let out = answered(handle_user_trap(&ct, a_trap(0x06)));
        assert!(out.expect("handled").is_none());
        assert!(
            take_pending(&ct, Signal::SIGILL)
                == SigInfo::fault(Signal::SIGILL, SignalCode::ILL_ILLOPC, AT),
            "an undefined instruction is ILL_ILLOPC at the pc"
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

/// #AC. The trap frame carries the code, not the data address, so there is
/// none to report -- the same reason the #GP above sends zero.
#[test]
#[cfg(target_arch = "x86_64")]
fn an_unaligned_access_arrives_as_sigbus_with_no_address() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        install(&ct, Signal::SIGBUS, a_handler());
        let out = answered(handle_user_trap(&ct, a_trap(0x11)));
        assert!(out.expect("handled").is_none());
        assert!(
            take_pending(&ct, Signal::SIGBUS)
                == SigInfo::fault(Signal::SIGBUS, SignalCode::BUS_ADRALN, 0),
            "an unaligned access reports no address"
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

/// #BP is the odd one out, and deliberately: it is queued straight into the
/// pending set instead of going through `force_fault_signal`, so a debugger's
/// breakpoint is not turned into a kill by the disposition the way a real
/// fault is. A process stopped at a breakpoint with SIGTRAP blocked is a
/// debugger's business; it is not an instruction that re-executes, so there
/// is no loop to stand against.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_breakpoint_is_a_sigtrap_the_disposition_does_not_get_to_decline() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        block(&ct, Signal::SIGTRAP);
        assert!(
            fault_kills(&ct, Signal::SIGTRAP),
            "blocked and unhandled: a FAULT signal in this state would kill"
        );
        let out = answered(handle_user_trap(&ct, a_trap(0x03)));
        assert!(out.expect("handled").is_none());
        assert!(
            ct.proc().exit_code().is_none(),
            "a breakpoint ends nothing, whatever the disposition says"
        );
        assert!(pending(&ct, Signal::SIGTRAP));
        done(ct)
    }
    with_a_linux_thread(body);
}

/// A syscall, all the way through: the number comes out of the register the
/// ABI puts it in, the call is made, and its answer goes back in the register
/// the program will read. `getpid` because its answer is a number no other
/// call could return by accident -- these pids start at 0x4000_0000.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_syscall_returns_its_answer_in_the_register_the_program_reads() {
    /// `__NR_getpid` on x86_64 (`linux-syscall/src/syscall.h.in`).
    const GETPID: usize = 39;
    fn body(ct: CurrentThread) -> ThreadFuture {
        let mut ctx = a_trap(0x100);
        assert!(matches!(ctx.trap_reason(), TrapReason::Syscall));
        ctx.general_mut().rax = GETPID;
        let out = answered(handle_user_trap(&ct, ctx));
        assert!(out.expect("a syscall is handled").is_none());
        let ret = ct
            .with_context(|ctx| ctx.get_field(UserContextField::ReturnValue))
            .expect("the context came back");
        assert_eq!(
            ret,
            ct.proc().id() as usize,
            "getpid must answer with this process's pid"
        );
        done(ct)
    }
    with_a_linux_thread(body);
}

/// And the pc it comes back on is the one the context arrived with. On
/// x86_64 the hardware has already moved it past the two bytes of `syscall`
/// -- the return address is in `rcx` and the trap entry puts it in `rip` --
/// so `advance_pc` is a no-op here and only RISC-V, whose `sepc` points AT
/// the `ecall`, has four to add. Advancing it again would land the thread two
/// bytes into the next instruction. It is also the pc `Interrupted::rewind`
/// subtracts `SYSCALL_INSN_LEN` from to make a cut-off syscall again, which
/// `syscall_restart_tests` pins from the other end.
#[test]
#[cfg(target_arch = "x86_64")]
fn a_syscall_does_not_move_the_pc_the_hardware_already_moved() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        let mut ctx = a_trap(0x100);
        ctx.general_mut().rax = 39; // getpid, as above
        answered(handle_user_trap(&ct, ctx)).expect("handled");
        let pc = ct
            .with_context(|ctx| ctx.get_field(UserContextField::InstrPointer))
            .expect("the context came back");
        assert_eq!(pc, AT, "the pc moved twice");
        done(ct)
    }
    with_a_linux_thread(body);
}

// ---- the report itself ------------------------------------------------

/// The report is printed from the path that is already killing the process,
/// so every one of its reads is one that must not make things worse. Asked
/// about a thread whose registers are not at home -- which is every thread
/// between `wait_for_run` and `put_context` -- it must print what it can and
/// not panic: a panic here is raised inside the fault handler, where there
/// is nothing left to take it.
#[test]
fn the_report_survives_a_thread_whose_registers_are_away() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        let vmar = ct.proc().vmar();
        let ctx = answered(ct.wait_for_run());
        assert!(
            ct.with_context(|_| ()).is_err(),
            "the context is away, which is what this test is about"
        );
        dump_user_fault_context(&ct, &vmar, 0, true);
        ct.put_context(ctx);
        done(ct)
    }
    with_a_linux_thread(body);
}

/// And the guard that matters most: the bytes at the pc are read with an
/// unchecked copy and no fault fixup, so reading them at an address nothing
/// maps turns a recoverable user SIGSEGV into an unresolved KERNEL page
/// fault. Every desktop process that jumped to one bad address once took the
/// machine down this way. Both guards are asked here -- an unmapped pc with
/// the data-fault flag set, and a mapped-or-not pc on an instruction-fetch
/// fault -- and neither may read anything.
#[test]
fn the_report_does_not_read_the_code_at_an_address_nobody_mapped() {
    fn body(ct: CurrentThread) -> ThreadFuture {
        let vmar = ct.proc().vmar();
        const NOWHERE: usize = 0x1_0000_1000;
        assert!(
            !addr_is_executable(&vmar, NOWHERE),
            "nothing is mapped there, which is the premise"
        );
        dump_user_fault_context(&ct, &vmar, NOWHERE, true);
        dump_user_fault_context(&ct, &vmar, NOWHERE, false);
        done(ct)
    }
    with_a_linux_thread(body);
}
