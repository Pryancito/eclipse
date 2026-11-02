//! That these four numbers are answered here and not through the table.
//!
//! The arithmetic of the split offset is tested where it lives
//! ([`super::super::wide`]); what is left to pin is the routing, and it is
//! worth pinning because `readv` and `writev` look exactly like calls that
//! belong in the allowlist. They have the same name and the same three
//! arguments on both sides. What differs is the shape of the memory the
//! second argument points at, which no table can see.

use super::super::{name, native_number};

/// None of the four is mapped by name, and each is named in the log.
#[test]
fn the_scatter_gather_calls_are_answered_here_and_not_by_the_table() {
    for (num, called) in [
        // The two whose arguments are identical and whose memory is not.
        (145, "readv"),
        (146, "writev"),
        // And the two that also split their offset across two registers.
        (333, "preadv"),
        (334, "pwritev"),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) must not be mapped: its iovec is half the width",
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
