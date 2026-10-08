//! What the crash-time trail records about an ioctl's outcome.
//!
//! The recording itself needs a process and a user address space, so what
//! is checked here is the part that does not: the translation from an
//! ioctl `Result` to the number userspace saw, and the refusal to read an
//! argument struct that has no first word.

use super::*;

/// The negative errno, not a bare -1: the whole point of the trail is that
/// a reader can tell `EINVAL` (userspace asked for something we do not
/// have) from `ENOENT` (it named an object that is gone).
#[test]
fn a_failed_ioctl_is_recorded_as_its_own_errno() {
    assert_eq!(trail_ret(&Err(FsError::InvalidParam)), -22);
    assert_eq!(trail_ret(&Err(FsError::EntryNotFound)), -2);
    assert_eq!(trail_ret(&Err(FsError::NotSupported)), -38);
    assert_eq!(trail_ret(&Err(FsError::BadAddress)), -14);
}

#[test]
fn a_successful_ioctl_is_recorded_as_its_return_value() {
    assert_eq!(trail_ret(&Ok(0)), 0);
    assert_eq!(trail_ret(&Ok(7)), 7);
}

/// `MODE_RMFB` and friends carry a bare `__u32`. Reading eight bytes off
/// one would run past the struct the client allocated, so an argument that
/// short has no first word at all.
#[test]
fn an_argument_struct_shorter_than_a_word_has_no_first_word() {
    // `_IOC_SIZE` 4 (`MODE_RMFB`) and 0 (`SET_MASTER`), at a plausible
    // user address: the size is what refuses them, not the address.
    assert!(!arg_word_readable(0xC004_64AF, 0x1000));
    assert!(!arg_word_readable(0x0000_641E, 0x1000));
    // 16 bytes (`GETPARAM`) at the same address is the case that does get
    // read, so the test above is about the size and nothing else.
    assert!(arg_word_readable(0xC010_6440, 0x1000));
}

/// A null pointer is never read: `ucheck` is the same gate the dispatch
/// arms use, and a trail entry is not worth a fault.
///
/// Only the null case is asserted. The upper bound of the user range is
/// the host's under `libos`, where `user_range_ok` accepts every address,
/// so a "kernel address is refused" assertion here would be testing this
/// build rather than the rule.
#[test]
fn a_null_argument_is_not_read() {
    assert!(!arg_word_readable(0xC010_6440, 0));
    assert_eq!(first_arg_word(0xC010_6440, 0), 0);
}
