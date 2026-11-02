//! That these two numbers are answered here and the other two are not.
//!
//! The arithmetic of the split offset is tested where it lives
//! ([`super::super::wide`]); what is left to pin is the routing, and the
//! routing is the part that is easy to get wrong, because `readv` and
//! `writev` are **not** here. Their only difference from the native calls is
//! the width of a `struct iovec` entry, and
//! [`Syscall::read_iovecs`](crate::Syscall::read_iovecs) asks the calling
//! process about that, so the number is all there is to translate and they go
//! through the table like any other converted call (`i386_abi_narrow.in`).
//! `preadv` and `pwritev` cannot: their offset arrives split across two
//! registers, which is a difference the handler cannot see.

use super::super::{name, native_number};

/// The division of labour: 145 and 146 through the table, 333 and 334 here.
#[test]
fn the_scatter_gather_calls_are_answered_here_and_not_by_the_table() {
    for (num, called, native) in [(145, "readv", 19), (146, "writev", 20)] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            Some(native),
            "i386 {} ({}) is converted by the handler, so the table maps it",
            num,
            called
        );
    }
    for (num, called) in [(333, "preadv"), (334, "pwritev")] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) must not be mapped: its offset is in two registers",
            num,
            called
        );
    }
}

/// And the two that are deliberately still unmapped, so that adding them is a
/// decision and not an oversight.
///
/// `preadv2` and `pwritev2` split their offset like `preadv` but with one
/// extra rule --- `(-1, -1)` means "use the file position" --- and
/// `process_vm_readv` carries two lists in two different address spaces, of
/// which only one is 32-bit. Each needs a line of its own, so each answers
/// `ENOSYS` with its name until it gets one.
#[test]
fn the_vectored_calls_that_still_need_a_rule_of_their_own_stay_unmapped() {
    for (num, called) in [
        (378, "preadv2"),
        (379, "pwritev2"),
        (347, "process_vm_readv"),
        (348, "process_vm_writev"),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(native_number(num), None, "i386 {} ({})", num, called);
    }
}
