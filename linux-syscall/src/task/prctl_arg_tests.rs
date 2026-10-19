//! Three `prctl(2)` options that took their argument as it came.

use super::*;

/// `PR_CAPBSET_DROP` needs `CAP_SETPCAP`, judged before the number:
/// an unprivileged caller hears `EPERM` even for a number that is not
/// a capability, and root hears `EINVAL` for that one.
#[test]
fn dropping_from_the_bounding_set_needs_cap_setpcap_first() {
    assert_eq!(capbset_drop_verdict(0, false), Err(LxError::EPERM));
    assert_eq!(capbset_drop_verdict(999, false), Err(LxError::EPERM));
    assert_eq!(capbset_drop_verdict(0, true), Ok(()));
    assert_eq!(capbset_drop_verdict(CAP_LAST_CAP as usize, true), Ok(()));
    assert_eq!(
        capbset_drop_verdict(CAP_LAST_CAP as usize + 1, true),
        Err(LxError::EINVAL)
    );
    assert_eq!(capbset_drop_verdict(usize::MAX, true), Err(LxError::EINVAL));
}

/// `PR_SET_TIMERSLACK`'s argument is a `long`: zero or negative means
/// the default, and nothing else is clamped.
#[test]
fn a_non_positive_timer_slack_is_the_default() {
    assert_eq!(timerslack_arg(0), 0);
    assert_eq!(timerslack_arg(usize::MAX), 0, "-1 is the default, not 2^64");
    assert_eq!(timerslack_arg(isize::MIN as usize), 0);
    assert_eq!(timerslack_arg(1), 1);
    assert_eq!(timerslack_arg(50_000), 50_000);
    assert_eq!(timerslack_arg(isize::MAX as usize), isize::MAX as u64);
}

/// `PR_SET_NAME` stores bytes: to the first NUL, at most 15, and a byte
/// that is not UTF-8 is kept as a `?` rather than refused.
#[test]
fn the_comm_is_bytes_to_the_first_nul_and_never_refused() {
    // The NUL is written as an escape, not as a raw byte: a raw one in
    // the source makes grep, diff and half the editors in the tree call
    // the whole file binary and skip it.
    assert_eq!(comm_from_user_bytes(b"worker\0junk"), "worker");
    assert_eq!(comm_from_user_bytes(b"worker"), "worker");
    assert_eq!(comm_from_user_bytes(b""), "");
    assert_eq!(
        comm_from_user_bytes(b"0123456789abcdefXYZ"),
        "0123456789abcde",
        "15 bytes at most"
    );
    // Latin-1 "Señal": the ñ is one byte that is not UTF-8.
    assert_eq!(comm_from_user_bytes(b"Se\xf1al"), "Se?al");
    // A JVM cutting "Thread-ñ" at 15 bytes leaves half a character.
    let mut cut = b"pool-1-thread-".to_vec();
    cut.push(0xc3); // first byte of a two-byte sequence, no second
    assert_eq!(comm_from_user_bytes(&cut), "pool-1-thread-?");
    // Whole multibyte characters survive.
    assert_eq!(comm_from_user_bytes("señal".as_bytes()), "señal");
}
