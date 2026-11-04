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

// ---------------------------------------------------------------------------
// And the other direction: the `siginfo_t` a 32-bit process HANDS the kernel,
// which `rt_sigqueueinfo(2)` and `rt_tgsigqueueinfo(2)` read.
//
// These assert on the NATIVE bytes, because that is what the rest of the
// kernel then reads out of the struct. The two directions are not each
// other's mirror as far as the tests go: going out, a member nothing names
// stays zero and that is all there is to check; coming in, there is a second
// thing to get wrong, which is the SIGN a pointer widens with.
// ---------------------------------------------------------------------------

/// Build the 128 bytes a 32-bit process would pass: the three `int`s and then
/// the words of the union from byte 12 on.
fn narrow_bytes(signo: i32, errno: i32, code: i32, union_words: &[u32]) -> [u8; SIGINFO_LEN] {
    let mut b = [0u8; SIGINFO_LEN];
    b[0..4].copy_from_slice(&signo.to_ne_bytes());
    b[4..8].copy_from_slice(&errno.to_ne_bytes());
    b[8..12].copy_from_slice(&code.to_ne_bytes());
    for (i, w) in union_words.iter().enumerate() {
        let at = NARROW_FIELDS_AT + 4 * i;
        b[at..at + 4].copy_from_slice(&w.to_ne_bytes());
    }
    b
}

#[test]
fn the_union_of_an_incoming_siginfo_moves_four_bytes_the_other_way() {
    // The mirror of `the_union_starts_four_bytes_earlier_than_it_does_natively`,
    // and the failure it pins is the one that needs no pointer either: read
    // straight across, a 32-bit caller's `si_pid` lands in the padding and
    // its `si_uid` becomes the pid.
    let narrow = narrow_bytes(Signal::SIGTERM as i32, 0, SignalCode::USER.0, &[4321, 1000]);
    let info = SigInfo::from_i386_bytes(&narrow);
    let native = info.as_bytes();
    assert_eq!(word(native, 16), 4321, "si_pid, at the native offset");
    assert_eq!(word(native, 20), 1000, "si_uid behind it");
    assert_eq!(
        word(native, 12),
        0,
        "and the four bytes of padding the native struct has are ZERO, not \
         the caller's si_pid"
    );
}

#[test]
fn a_pointer_coming_in_widens_with_zeros_and_not_with_sign() {
    // The seventh form of the width trap, the one Copilot found on the
    // #2414: `si_value` is a `compat_uptr_t`, and `compat_ptr()` is a cast of
    // an UNSIGNED int. A `sigqueue(3)` that sent 0x8000_0000 means two
    // gigabytes up.
    //
    // Sign-extending gives 0xffff_ffff_8000_0000, which is not merely a
    // different number: it is a kernel address, and it is what the receiving
    // handler would be handed as its `si_value.sival_ptr`.
    let narrow = narrow_bytes(
        Signal::SIGRT34 as i32,
        0,
        SignalCode::QUEUE.0,
        &[7, 11, 0x8000_0000],
    );
    let info = SigInfo::from_i386_bytes(&narrow);
    let native = info.as_bytes();
    let value = u64::from_ne_bytes([
        native[24], native[25], native[26], native[27], native[28], native[29], native[30],
        native[31],
    ]);
    assert_eq!(
        value, 0x8000_0000,
        "si_value widened with zeros; sign-extended it would be {:#x}",
        0xffff_ffff_8000_0000u64
    );
    assert_eq!(word(native, 16), 7, "si_pid of an _rt");
    assert_eq!(word(native, 20), 11, "si_uid of an _rt");
}

#[test]
fn what_the_layout_does_not_name_comes_in_as_zero() {
    // Not a shortcut: the caller's bytes past the live member are the OTHER
    // members of its union, and copying them across at native offsets would
    // put its rubbish where the receiver reads a real field. Linux fills a
    // zeroed `kernel_siginfo` member by member for the same reason.
    //
    // `_kill` has two `int`s, so everything from byte 24 of the native
    // struct on belongs to nobody -- even though the caller filled all 128.
    let mut narrow = [0xAAu8; SIGINFO_LEN];
    narrow[0..4].copy_from_slice(&(Signal::SIGTERM as i32).to_ne_bytes());
    narrow[4..8].copy_from_slice(&0i32.to_ne_bytes());
    narrow[8..12].copy_from_slice(&SignalCode::USER.0.to_ne_bytes());
    let info = SigInfo::from_i386_bytes(&narrow);
    let native = info.as_bytes();
    assert_eq!(
        word(native, 16),
        i32::from_ne_bytes([0xAA; 4]),
        "si_pid is the caller's, whatever it put there"
    );
    assert!(
        native[24..].iter().all(|&x| x == 0),
        "and past the two `int`s of `_kill` nothing of the caller's survives"
    );
}

#[test]
fn out_and_back_is_the_same_siginfo_for_every_layout_this_kernel_builds() {
    // The cheapest check that the two directions run the SAME table: if one
    // of them had a layout's members wrong, or stepped by the wrong width,
    // the round trip would not close. It is not a proof on its own --- two
    // identical mistakes cancel --- which is why the offsets above are
    // checked by hand as well.
    for info in [
        SigInfo::from_user(Signal::SIGTERM, 4321, 1000, SignalCode::USER),
        SigInfo::child_state_change(99, 7, 0x100),
        SigInfo::fault(Signal::SIGSEGV, SignalCode(1), 0xdead_beef),
        SigInfo::queued(Signal::SIGRT34, 7, 11, 0x7fff_0000),
    ] {
        let back = SigInfo::from_i386_bytes(&info.to_i386_bytes());
        assert_eq!(
            back.as_bytes(),
            info.as_bytes(),
            "round trip of a {:?}",
            siginfo_layout(info.signo, info.code)
        );
    }
}
