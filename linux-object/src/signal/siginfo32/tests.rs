//! What a 32-bit handler reads out of its `siginfo_t`.
//!
//! Every test here asserts on the BYTES, at the offsets a 32-bit program
//! reads them from, because that is the whole of the contract: the struct is
//! 128 bytes at either width, so there is no size to get wrong and no fault
//! to catch. A wrong conversion is 128 bytes that look fine.

use super::*;
use crate::signal::{SigInfo, Signal, SignalCode};

/// The three `int`s at the front, which every layout shares.
fn header(bytes: &[u8]) -> (i32, i32, i32) {
    (word(bytes, 0), word(bytes, 4), word(bytes, 8))
}

/// The `int` at `at`, as a 32-bit program reads it.
fn word(bytes: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[test]
fn a_32_bit_siginfo_is_the_same_128_bytes_as_a_native_one() {
    // So nothing about a wrong conversion is a short write: the length is
    // right whatever the contents are.
    let info = SigInfo::bare(Signal::SIGINT);
    assert_eq!(info.to_i386_bytes().len(), SIGINFO_LEN);
    assert_eq!(info.as_bytes().len(), SIGINFO_LEN);
}

#[test]
fn the_union_starts_four_bytes_earlier_than_it_does_natively() {
    // The error that needs no pointer at all to happen: `SI_USER`'s members
    // are two `int`s and need no widening, and copying the native bytes
    // across still hands the handler the padding as its `si_pid`.
    let info = SigInfo::from_user(Signal::SIGTERM, 4321, 1000, SignalCode::USER);
    let native = info.as_bytes();
    assert_eq!(word(native, 16), 4321, "si_pid natively");
    assert_eq!(word(native, 12), 0, "and four bytes of padding before it");

    let narrow = info.to_i386_bytes();
    assert_eq!(header(&narrow), (Signal::SIGTERM as i32, 0, 0));
    assert_eq!(word(&narrow, 12), 4321, "si_pid, where i386 reads it");
    assert_eq!(word(&narrow, 16), 1000, "si_uid");
}

#[test]
fn a_kill_tells_the_handler_who_sent_it() {
    // What `kill(2)` leaves, and what a daemon's SIGTERM handler reads to
    // decide whether the sender is allowed to ask.
    for code in [SignalCode::USER, SignalCode::TKILL, SignalCode::KERNEL] {
        let info = SigInfo::from_user(Signal::SIGUSR1, 99, 65534, code);
        assert_eq!(siginfo_layout(info.signo, info.code), SiginfoLayout::Kill);
        let b = info.to_i386_bytes();
        assert_eq!(header(&b), (Signal::SIGUSR1 as i32, 0, code.0));
        assert_eq!((word(&b, 12), word(&b, 16)), (99, 65534));
    }
}

#[test]
fn a_fault_tells_the_handler_the_address_in_four_bytes() {
    // `si_addr` is the one member every fault handler reads: a JIT deciding
    // whether the fault is its own guard page, a garbage collector's write
    // barrier, Rust's stack-overflow handler.
    let info = SigInfo::fault(Signal::SIGSEGV, SignalCode::SEGV_MAPERR, 0x0804_9ffc);
    assert_eq!(siginfo_layout(info.signo, info.code), SiginfoLayout::Fault);
    let b = info.to_i386_bytes();
    assert_eq!(
        header(&b),
        (Signal::SIGSEGV as i32, 0, SignalCode::SEGV_MAPERR.0)
    );
    assert_eq!(word(&b, 12) as u32, 0x0804_9ffc);
    assert_eq!(
        word(&b, 16),
        0,
        "the native address's high half is not a second member"
    );
}

#[test]
fn the_four_fault_signals_all_carry_an_address() {
    // Their `si_code`s are all `1`, so the signal is the only thing that
    // separates `SEGV_MAPERR` from `CLD_EXITED`.
    for signal in [
        Signal::SIGSEGV,
        Signal::SIGBUS,
        Signal::SIGILL,
        Signal::SIGFPE,
    ] {
        let info = SigInfo::fault(signal, SignalCode(1), 0x1000);
        assert_eq!(
            siginfo_layout(info.signo, info.code),
            SiginfoLayout::Fault,
            "{signal:?}"
        );
        assert_eq!(word(&info.to_i386_bytes(), 12), 0x1000);
    }
}

#[test]
fn a_sigchld_carries_three_ints_and_the_status_is_the_third() {
    // `si_code` 1 is `CLD_EXITED` here and `SEGV_MAPERR` on a fault: read as
    // a fault, this child's `si_status` would be the high half of an address
    // nobody faulted at.
    let info = SigInfo::child_state_change(1234, 0, 42 << 8);
    assert_eq!(siginfo_layout(info.signo, info.code), SiginfoLayout::Chld);
    let b = info.to_i386_bytes();
    assert_eq!(
        header(&b),
        (Signal::SIGCHLD as i32, 0, SignalCode::CLD_EXITED.0)
    );
    assert_eq!(word(&b, 12), 1234, "si_pid");
    assert_eq!(word(&b, 16), 0, "si_uid");
    assert_eq!(word(&b, 20), 42, "si_status");
}

#[test]
fn a_killed_child_is_not_mistaken_for_a_fault() {
    let info = SigInfo::child_state_change(7, 0, Signal::SIGKILL as i32);
    assert_eq!(siginfo_layout(info.signo, info.code), SiginfoLayout::Chld);
    let b = info.to_i386_bytes();
    assert_eq!(word(&b, 8), SignalCode::CLD_KILLED.0);
    assert_eq!(word(&b, 20), Signal::SIGKILL as i32, "si_status");
}

#[test]
fn a_queued_signal_carries_its_sigval_after_the_two_ints() {
    // `sigqueue(3)`'s whole purpose is the `sigval`, and it is a pointer: the
    // one member of `_rt` that halves. It sits at byte 8 of the union
    // natively and at byte 8 of the narrow one too -- the two `int`s before
    // it are the same width -- so what moves is only the union's own start.
    let info = SigInfo::queued(Signal::SIGUSR2, 500, 1000, 0xdead_beef);
    assert_eq!(siginfo_layout(info.signo, info.code), SiginfoLayout::Rt);
    let b = info.to_i386_bytes();
    assert_eq!(header(&b), (Signal::SIGUSR2 as i32, 0, SignalCode::QUEUE.0));
    assert_eq!(word(&b, 12), 500, "si_pid");
    assert_eq!(word(&b, 16), 1000, "si_uid");
    assert_eq!(word(&b, 20) as u32, 0xdead_beef, "si_value");
}

#[test]
fn a_timer_expiry_carries_its_id_overrun_and_value() {
    // `_timer` shares `_rt`'s shape, and glibc's `SIGEV_THREAD` helper acts
    // only on a signal whose `si_ptr` is the timer it registered -- so a
    // `si_value` four bytes out of place is a timer callback that never runs.
    let info = SigInfo::timer(Signal::SIGALRM, 3, 2, 0x4000_1000);
    assert_eq!(siginfo_layout(info.signo, info.code), SiginfoLayout::Rt);
    let b = info.to_i386_bytes();
    assert_eq!(word(&b, 8), SignalCode::TIMER.0);
    assert_eq!(word(&b, 12), 3, "si_timerid");
    assert_eq!(word(&b, 16), 2, "si_overrun");
    assert_eq!(word(&b, 20) as u32, 0x4000_1000, "si_value");
}

#[test]
fn everything_the_layout_does_not_name_is_zero() {
    // Not a copy of whatever the native struct had there, and not the kernel
    // stack: the rest of a `siginfo_t` is 112 bytes handed to userspace, and
    // Linux clears it.
    let info = SigInfo::fault(Signal::SIGSEGV, SignalCode::SEGV_ACCERR, 0xffff_f000);
    let b = info.to_i386_bytes();
    assert!(
        b[16..].iter().all(|&x| x == 0),
        "a fault's union is one pointer and the rest is cleared"
    );
}

#[test]
fn the_signal_is_half_of_the_question_and_the_code_the_other_half() {
    // The mutation this pins: dropping the signal from `siginfo_layout` and
    // keying on `si_code` alone. Code 1 is three different members.
    assert_eq!(
        siginfo_layout(Signal::SIGCHLD as i32, SignalCode(1)),
        SiginfoLayout::Chld
    );
    assert_eq!(
        siginfo_layout(Signal::SIGSEGV as i32, SignalCode(1)),
        SiginfoLayout::Fault
    );
    // And a positive code on a signal with no union member of its own: two
    // `int`s, as Linux's `default` has it.
    assert_eq!(
        siginfo_layout(Signal::SIGINT as i32, SignalCode(1)),
        SiginfoLayout::Kill
    );
    // A negative code is shared by every signal and outranks it: a
    // `SI_QUEUE` SIGCHLD is an `_rt`, not a `_sigchld`.
    assert_eq!(
        siginfo_layout(Signal::SIGCHLD as i32, SignalCode::QUEUE),
        SiginfoLayout::Rt
    );
}
