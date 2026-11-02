//! The two things this file decides: how many words each call's block holds,
//! and which calls are answered at all.
//!
//! The word counts are the measured table in the module documentation. A
//! test that just restated the array would be worthless --- it would pass
//! whatever the array said --- so what is pinned here is the relationship
//! between the counts and the calls: the shapes that must agree with each
//! other, and the boundary of the table.

use super::*;

/// The call numbers are a fixed ABI, so the bound is too: 0 and anything
/// above 20 is `EINVAL`, which is what Linux's own bounds check against its
/// table answers, and NOT `ENOSYS`. The difference matters to a program that
/// probes for a call: `EINVAL` says "no such socket call", `ENOSYS` says
/// "this kernel has no `socketcall` at all".
#[test]
fn a_call_number_outside_the_table_is_einval_and_not_enosys() {
    assert_eq!(words_of(0), Err(LxError::EINVAL));
    assert_eq!(words_of(21), Err(LxError::EINVAL));
    assert_eq!(words_of(usize::MAX), Err(LxError::EINVAL));
    for call in 1..=MAX_CALL {
        assert!(words_of(call).is_ok(), "call {} is in the table", call);
    }
}

/// Every word count is between two and six, and six is the most: that is
/// what `MAX_WORDS` promises the buffer in [`read_args`], and a count above
/// it would write past the end of a fixed-size array.
#[test]
fn no_call_asks_for_more_words_than_the_buffer_holds() {
    for call in 1..=MAX_CALL {
        let words = words_of(call).unwrap();
        assert!(words >= 2, "call {} asks for {} words", call, words);
        assert!(
            words <= MAX_WORDS,
            "call {} asks for {} words, buffer holds {}",
            call,
            words,
            MAX_WORDS
        );
    }
    assert_eq!(
        (1..=MAX_CALL).map(|c| words_of(c).unwrap()).max(),
        Some(MAX_WORDS),
        "MAX_WORDS should be the real maximum and not a round number"
    );
}

/// The measured rows, by the shapes that have to agree. Each group is a set
/// of calls whose C prototypes have the same number of arguments, so a typo
/// in one row shows up as a disagreement inside its group rather than
/// needing the whole table restated.
#[test]
fn the_calls_with_the_same_prototype_ask_for_the_same_number_of_words() {
    let w = |c| words_of(c).unwrap();
    // `listen(fd, backlog)` and `shutdown(fd, how)`: two scalars.
    assert_eq!(w(4), 2);
    assert_eq!(w(13), w(4));
    // `bind`, `connect`: fd, address, length. `accept`, `getsockname`,
    // `getpeername`: fd, address, length POINTER. `socket`: three scalars.
    // `sendmsg`, `recvmsg`: fd, msghdr, flags.
    for three in [1, 2, 3, 5, 6, 7, 16, 17] {
        assert_eq!(w(three), 3, "call {} ({})", three, call_name(three));
    }
    // `socketpair`: three scalars and the pair. `send`, `recv`: fd, buffer,
    // length, flags. `accept4`: the three of `accept` and the flags.
    // `sendmmsg`: fd, vector, count, flags.
    for four in [8, 9, 10, 18, 20] {
        assert_eq!(w(four), 4, "call {} ({})", four, call_name(four));
    }
    // The option pair, and `recvmmsg` with its timeout pointer.
    for five in [14, 15, 19] {
        assert_eq!(w(five), 5, "call {} ({})", five, call_name(five));
    }
    // `sendto` and `recvfrom`, which are `send`/`recv` plus an address and
    // its length: the two widest, and the reason `MAX_WORDS` is six.
    assert_eq!(w(11), 6);
    assert_eq!(w(12), w(11));
}

/// `accept4` takes ONE more word than `accept`, and `sendto` two more than
/// `send`. Pinned on their own because these are the three pairs where
/// taking a count from the neighbouring row would read the caller's next
/// variable as an argument --- quietly, since the block is usually in the
/// middle of a page and reading past it succeeds.
#[test]
fn the_calls_that_extend_another_ask_for_exactly_the_extra_words() {
    assert_eq!(words_of(18).unwrap(), words_of(5).unwrap() + 1);
    assert_eq!(words_of(11).unwrap(), words_of(9).unwrap() + 2);
    assert_eq!(words_of(12).unwrap(), words_of(10).unwrap() + 2);
}

/// Every call number has a name, including the ones that are not wired: the
/// warning for an unwired call is all a program's author gets, and
/// `socketcall(17, ...)` says nothing to whoever reads it.
#[test]
fn every_call_in_the_table_has_a_name_and_nothing_outside_it_does() {
    for call in 1..=MAX_CALL {
        assert_ne!(call_name(call), "unknown", "call {}", call);
    }
    assert_eq!(call_name(0), "unknown");
    assert_eq!(call_name(MAX_CALL + 1), "unknown");
}

/// A null block is `EFAULT` before anything else happens, and the block is
/// read as 32-bit words with each one **zero**-extended.
///
/// A 32-bit pointer with its top bit set is an address in the third
/// gigabyte, which is where an i386 stack lives; sign-extending it would
/// make it a kernel-half address and the call would answer `EFAULT` for a
/// perfectly ordinary buffer.
#[test]
fn the_block_is_read_as_four_byte_words_each_zero_extended() {
    assert_eq!(read_args(0, 3).err(), Some(LxError::EFAULT));

    let block: [u32; 6] = [3, 0xbfff_f000, 0x7fff_ffff, 0xffff_ffff, 1, 0];
    let got = read_args(block.as_ptr() as usize, 6).unwrap();
    assert_eq!(got[0], 3);
    assert_eq!(got[1], 0xbfff_f000, "a stack pointer, not a kernel address");
    assert_eq!(got[2], 0x7fff_ffff);
    assert_eq!(got[3], 0xffff_ffff, "and not 0xffff_ffff_ffff_ffff");
    assert_eq!(got[4], 1);
    assert_eq!(got[5], 0);

    // Only the words asked for are read: a count of two leaves the rest at
    // zero rather than carrying whatever was next in the caller's memory.
    let two = read_args(block.as_ptr() as usize, 2).unwrap();
    assert_eq!(two, [3, 0xbfff_f000, 0, 0, 0, 0]);
}

/// `socketcall` itself is answered here and is NOT in the translation table:
/// it has no native twin, because x86_64 has the twenty calls as twenty
/// syscalls and no multiplexer.
#[test]
fn socketcall_is_answered_here_and_not_by_the_table() {
    use super::super::{name, native_number};
    assert_eq!(name(102), Some("socketcall"));
    assert_eq!(
        native_number(102),
        None,
        "socketcall has no native number to translate to"
    );
}
