//! `SA_RESTART`: a syscall a signal cut short, made again when the
//! handler returns.

use super::*;
use linux_object::error::LxError;
use linux_object::signal::SignalActionFlags;

// The numbers differ between x86_64 and the generic table.
const READ: usize = if cfg!(target_arch = "x86_64") { 0 } else { 63 };
const PPOLL: usize = if cfg!(target_arch = "x86_64") {
    271
} else {
    73
};
const ARGS: [usize; 6] = [3, 0x7ff0_1000, 1, 0, 0, 0];
/// Where the syscall instruction is.
const AT: usize = 0x40_1000;

fn eintr() -> usize {
    -(LxError::EINTR as isize) as usize
}

/// The context a `read(3, buf, 1)` comes back to user mode with when a
/// signal cuts it short: past its syscall instruction, with `EINTR` in
/// the return register.
fn read_cut_short() -> UserContext {
    let mut ctx = UserContext::default();
    {
        let regs = ctx.general_mut();
        cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                regs.rax = READ;
                (regs.rdi, regs.rsi, regs.rdx) = (ARGS[0], ARGS[1], ARGS[2]);
            } else if #[cfg(target_arch = "aarch64")] {
                regs.x8 = READ;
                (regs.x0, regs.x1, regs.x2) = (ARGS[0], ARGS[1], ARGS[2]);
            } else if #[cfg(target_arch = "riscv64")] {
                regs.a7 = READ;
                (regs.a0, regs.a1, regs.a2) = (ARGS[0], ARGS[1], ARGS[2]);
            }
        }
    }
    ctx.set_field(
        UserContextField::InstrPointer,
        AT + Interrupted::SYSCALL_INSN_LEN,
    );
    ctx.set_field(UserContextField::ReturnValue, eintr());
    ctx
}

fn handler(flags: SignalActionFlags) -> SignalAction {
    SignalAction {
        handler: 0x40_2000,
        flags,
        restorer: 0x40_3000,
        mask: Sigset::empty(),
    }
}

#[test]
fn a_read_cut_short_leaves_itself_to_make_again() {
    assert_eq!(
        Interrupted::after(READ, &ARGS, eintr()),
        Some(Interrupted {
            num: READ,
            arg0: 3,
            by_handler: ByHandler::WithSaRestart,
            stopped: false
        })
    );
}

#[test]
fn only_eintr_is_cut_short() {
    assert_eq!(Interrupted::after(READ, &ARGS, 1), None, "it read a byte");
    let eagain = -(LxError::EAGAIN as isize) as usize;
    assert_eq!(Interrupted::after(READ, &ARGS, eagain), None);
}

#[test]
fn a_poll_with_a_timeout_cut_short_is_not_made_again() {
    // `ARGS[2]` is its timeout, and made again it would start over.
    assert_eq!(Interrupted::after(PPOLL, &ARGS, eintr()), None);
}

/// `ppoll(fds, 1, NULL, NULL)`: no timeout, so nothing to start over.
const UNTIMED: [usize; 6] = [0x7ff0_1000, 1, 0, 0, 0, 0];

#[test]
fn an_untimed_poll_a_handler_ran_over_comes_back_eintr_even_with_sa_restart() {
    // Linux answers EINTR from a poll whenever a handler ran.
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(PPOLL, &UNTIMED, eintr());
    assert!(slot.is_some(), "a stop would make it again");
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::RESTART), &mut ctx);
    assert_eq!(
        ctx.get_field(UserContextField::InstrPointer),
        AT + Interrupted::SYSCALL_INSN_LEN
    );
    assert_eq!(slot, None);
}

#[test]
fn a_stop_makes_the_syscall_it_cut_short_again() {
    // Ctrl-Z on a program in `read`, and `fg`: the read goes on, and
    // the program never sees an EINTR it did not ask for.
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(READ, &ARGS, eintr());
    Interrupted::no_handler_ran(&mut slot);
    Interrupted::leave(&mut slot, &mut ctx);
    assert_eq!(ctx.get_field(UserContextField::InstrPointer), AT);
    assert_eq!(syscall_num(&ctx), READ);
    assert_eq!(syscall_args(&ctx), ARGS);
    assert_eq!(slot, None);
}

#[test]
fn a_stop_makes_an_untimed_poll_again_too() {
    // `-ERESTARTNOHAND`: with no handler, Linux makes it again. And
    // `pause()` is this on aarch64 and riscv64, where musl has no pause.
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(PPOLL, &UNTIMED, eintr());
    Interrupted::no_handler_ran(&mut slot);
    Interrupted::leave(&mut slot, &mut ctx);
    assert_eq!(ctx.get_field(UserContextField::InstrPointer), AT);
    assert_eq!(syscall_num(&ctx), PPOLL);
}

#[test]
fn with_no_stop_the_program_gets_its_eintr() {
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(READ, &ARGS, eintr());
    Interrupted::leave(&mut slot, &mut ctx);
    assert_eq!(
        ctx.get_field(UserContextField::InstrPointer),
        AT + Interrupted::SYSCALL_INSN_LEN
    );
    assert_eq!(ctx.get_field(UserContextField::ReturnValue), eintr());
    assert_eq!(slot, None, "the slot is spent on the way out");
}

#[test]
fn a_handler_after_the_stop_decides_as_it_would_have() {
    // Stopped, then continued by a SIGCONT that has a handler without
    // SA_RESTART: Linux runs the handler and the read is EINTR.
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(READ, &ARGS, eintr());
    Interrupted::no_handler_ran(&mut slot);
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::SIGINFO), &mut ctx);
    Interrupted::leave(&mut slot, &mut ctx);
    assert_eq!(
        ctx.get_field(UserContextField::InstrPointer),
        AT + Interrupted::SYSCALL_INSN_LEN
    );
    assert_eq!(ctx.get_field(UserContextField::ReturnValue), eintr());
}

#[test]
fn a_handler_with_sa_restart_goes_back_to_the_syscall_instruction() {
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(READ, &ARGS, eintr());
    Interrupted::enter_handler(
        &mut slot,
        &handler(SignalActionFlags::RESTART | SignalActionFlags::SIGINFO),
        &mut ctx,
    );
    assert_eq!(ctx.get_field(UserContextField::InstrPointer), AT);
    assert_eq!(
        syscall_num(&ctx),
        READ,
        "the syscall number was left where EINTR put it"
    );
    assert_eq!(
        syscall_args(&ctx),
        ARGS,
        "the first argument was left where EINTR put it"
    );
    assert_eq!(slot, None);
}

#[test]
fn a_handler_without_it_leaves_the_program_its_eintr() {
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(READ, &ARGS, eintr());
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::SIGINFO), &mut ctx);
    assert_eq!(
        ctx.get_field(UserContextField::InstrPointer),
        AT + Interrupted::SYSCALL_INSN_LEN
    );
    assert_eq!(ctx.get_field(UserContextField::ReturnValue), eintr());
    assert_eq!(slot, None, "a later handler could still restart it");
}

const FUTEX: usize = if cfg!(target_arch = "x86_64") {
    202
} else {
    98
};

/// `futex(uaddr, op | FUTEX_PRIVATE_FLAG, 0, NULL)`.
fn futex(op: usize) -> [usize; 6] {
    [0x7ff0_2000, op | 0x80, 0, 0, 0, 0]
}

const FUTEX_WAIT_BITSET: usize = 9;

/// Taking a PI lock is `-ERESTARTNOINTR` in Linux: made again whatever
/// the handler. glibc takes an `EINTR` from it for the lock taken.
#[test]
fn a_pi_lock_cut_short_is_made_again_by_a_handler_without_sa_restart() {
    const FUTEX_LOCK_PI: usize = 6;
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(FUTEX, &futex(FUTEX_LOCK_PI), eintr());
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::SIGINFO), &mut ctx);
    assert_eq!(ctx.get_field(UserContextField::InstrPointer), AT);
    assert_eq!(syscall_num(&ctx), FUTEX);
    assert_eq!(slot, None);
}

/// An untimed futex wait is `-ERESTARTSYS`, like a `read`: glibc's
/// `sem_wait` gave up the wait with `EINTR` when a `signal()` handler
/// ran over it.
#[test]
fn an_untimed_futex_wait_is_made_again_by_sa_restart() {
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(FUTEX, &futex(FUTEX_WAIT_BITSET), eintr());
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::RESTART), &mut ctx);
    assert_eq!(ctx.get_field(UserContextField::InstrPointer), AT);
    assert_eq!(syscall_num(&ctx), FUTEX);

    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(FUTEX, &futex(FUTEX_WAIT_BITSET), eintr());
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::SIGINFO), &mut ctx);
    assert_eq!(
        ctx.get_field(UserContextField::InstrPointer),
        AT + Interrupted::SYSCALL_INSN_LEN,
        "without SA_RESTART it is EINTR"
    );
}

/// A timed one is not: `EINTR` from any handler, as from `poll`.
#[test]
fn a_timed_futex_wait_cut_short_is_not() {
    let mut args = futex(FUTEX_WAIT_BITSET);
    args[3] = 0x7ff0_3000;
    let mut ctx = read_cut_short();
    let mut slot = Interrupted::after(FUTEX, &args, eintr());
    assert!(slot.is_some(), "a stop would make it again");
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::RESTART), &mut ctx);
    assert_eq!(
        ctx.get_field(UserContextField::InstrPointer),
        AT + Interrupted::SYSCALL_INSN_LEN
    );
}

#[test]
fn a_handler_over_no_syscall_changes_nothing() {
    let mut ctx = read_cut_short();
    let mut slot = None;
    Interrupted::enter_handler(&mut slot, &handler(SignalActionFlags::RESTART), &mut ctx);
    assert_eq!(
        ctx.get_field(UserContextField::InstrPointer),
        AT + Interrupted::SYSCALL_INSN_LEN
    );
}

#[test]
fn the_syscall_instruction_is_two_bytes_on_x86_64_and_four_elsewhere() {
    // `0f 05`; `svc #0` and `ecall` are one 32-bit word.
    let expected = if cfg!(target_arch = "x86_64") { 2 } else { 4 };
    assert_eq!(Interrupted::SYSCALL_INSN_LEN, expected);
}

// ---- the two registers readers everything above is fed from -----------
//
// `read_cut_short` fills three argument registers and leaves the other
// three at zero, which is enough to decide whether a `read` is made
// again but says nothing about WHICH register each argument came out of.
// Seven distinct values do.

/// Seven values that cannot be mistaken for one another: the syscall
/// number and its six arguments.
const NUM: usize = 0x77;
const SIX: [usize; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];

/// A context as a syscall arrives in it, with every register the two
/// readers look at holding a value of its own.
fn a_call_with_distinct_registers() -> UserContext {
    let mut ctx = UserContext::default();
    let regs = ctx.general_mut();
    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            regs.rax = NUM;
            (regs.rdi, regs.rsi, regs.rdx) = (SIX[0], SIX[1], SIX[2]);
            (regs.r10, regs.r8, regs.r9) = (SIX[3], SIX[4], SIX[5]);
            // What the `syscall` instruction itself left in `rcx`: the
            // address to come back to. Not an argument, and the value
            // that gets read instead when the fourth one is taken from
            // the C calling convention by mistake.
            regs.rcx = 0x40_1002;
        } else if #[cfg(target_arch = "aarch64")] {
            regs.x8 = NUM;
            (regs.x0, regs.x1, regs.x2) = (SIX[0], SIX[1], SIX[2]);
            (regs.x3, regs.x4, regs.x5) = (SIX[3], SIX[4], SIX[5]);
        } else if #[cfg(target_arch = "riscv64")] {
            regs.a7 = NUM;
            (regs.a0, regs.a1, regs.a2) = (SIX[0], SIX[1], SIX[2]);
            (regs.a3, regs.a4, regs.a5) = (SIX[3], SIX[4], SIX[5]);
        }
    }
    ctx
}

/// The number comes from a register of its own, not from an argument.
///
/// On x86_64 that register is `rax`, which is also where the result goes
/// -- the reason `Interrupted` has to put the number back to make a call
/// again at all.
#[test]
fn the_syscall_number_comes_from_its_own_register() {
    let ctx = a_call_with_distinct_registers();
    assert_eq!(syscall_num(&ctx), NUM);
    assert!(
        !SIX.contains(&syscall_num(&ctx)),
        "the number was read out of an argument register"
    );
}

/// The six arguments, in order, each from the register the Linux syscall
/// ABI puts it in.
///
/// On x86_64 the fourth travels in `r10`, NOT in `rcx`: the `syscall`
/// instruction overwrites `rcx` with the return address, which is why
/// Linux moves that one argument off the C calling convention. Reading
/// `rcx` here would hand every four-argument syscall -- `openat`,
/// `sendto`, `futex`, `ppoll` -- a code address where its fourth
/// argument belongs, and these are the values `Interrupted::after`
/// decides on.
#[test]
fn the_six_argument_registers_are_read_in_abi_order() {
    let ctx = a_call_with_distinct_registers();
    assert_eq!(syscall_args(&ctx), SIX);
}
