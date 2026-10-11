//! The numbers `prctl(2)` and `sched_setattr(2)` are made of, written out
//! here by hand from `<linux/prctl.h>` and `<linux/sched.h>` and compared
//! against nothing else in the crate.
//!
//! They all used to live inside the call that reads them, where the `match`
//! arm and the number it matches were the same constant: changing one left
//! every test in the file green and broke only the libc on the other side of
//! the syscall, which sends the number. Measured by mutation on 11-Oct, all
//! 24 of them survived.

use super::*;

/// Every option this kernel implements, by the number userspace sends.
///
/// One row each rather than a loop over the constants, so a number that
/// drifts is named: a loop over `PR_SET_NAME` would follow it wherever it
/// went, which is the whole failure this test exists to stop.
const OPTIONS: &[(&str, i32)] = &[
    ("PR_SET_PDEATHSIG", 1),
    ("PR_GET_PDEATHSIG", 2),
    ("PR_GET_DUMPABLE", 3),
    ("PR_SET_DUMPABLE", 4),
    ("PR_GET_KEEPCAPS", 7),
    ("PR_SET_KEEPCAPS", 8),
    ("PR_SET_NAME", 15),
    ("PR_GET_NAME", 16),
    ("PR_GET_SECCOMP", 21),
    ("PR_SET_SECCOMP", 22),
    ("PR_CAPBSET_READ", 23),
    ("PR_CAPBSET_DROP", 24),
    ("PR_SET_TIMERSLACK", 29),
    ("PR_GET_TIMERSLACK", 30),
    ("PR_SET_CHILD_SUBREAPER", 36),
    ("PR_GET_CHILD_SUBREAPER", 37),
    ("PR_SET_NO_NEW_PRIVS", 38),
    ("PR_GET_NO_NEW_PRIVS", 39),
    ("PR_GET_TID_ADDRESS", 40),
    ("PR_SET_THP_DISABLE", 41),
    ("PR_GET_THP_DISABLE", 42),
];

/// What this file declares, paired with its name, so the two lists can be
/// compared element by element.
fn declared() -> alloc::vec::Vec<(&'static str, i32)> {
    alloc::vec![
        ("PR_SET_PDEATHSIG", PR_SET_PDEATHSIG),
        ("PR_GET_PDEATHSIG", PR_GET_PDEATHSIG),
        ("PR_GET_DUMPABLE", PR_GET_DUMPABLE),
        ("PR_SET_DUMPABLE", PR_SET_DUMPABLE),
        ("PR_GET_KEEPCAPS", PR_GET_KEEPCAPS),
        ("PR_SET_KEEPCAPS", PR_SET_KEEPCAPS),
        ("PR_SET_NAME", PR_SET_NAME),
        ("PR_GET_NAME", PR_GET_NAME),
        ("PR_GET_SECCOMP", PR_GET_SECCOMP),
        ("PR_SET_SECCOMP", PR_SET_SECCOMP),
        ("PR_CAPBSET_READ", PR_CAPBSET_READ),
        ("PR_CAPBSET_DROP", PR_CAPBSET_DROP),
        ("PR_SET_TIMERSLACK", PR_SET_TIMERSLACK),
        ("PR_GET_TIMERSLACK", PR_GET_TIMERSLACK),
        ("PR_SET_CHILD_SUBREAPER", PR_SET_CHILD_SUBREAPER),
        ("PR_GET_CHILD_SUBREAPER", PR_GET_CHILD_SUBREAPER),
        ("PR_SET_NO_NEW_PRIVS", PR_SET_NO_NEW_PRIVS),
        ("PR_GET_NO_NEW_PRIVS", PR_GET_NO_NEW_PRIVS),
        ("PR_GET_TID_ADDRESS", PR_GET_TID_ADDRESS),
        ("PR_SET_THP_DISABLE", PR_SET_THP_DISABLE),
        ("PR_GET_THP_DISABLE", PR_GET_THP_DISABLE),
    ]
}

#[test]
fn every_prctl_option_is_the_number_linux_prctl_h_gives() {
    let mine = declared();
    assert_eq!(mine.len(), OPTIONS.len(), "an option was added or dropped");
    for (&(name, want), &(same, got)) in OPTIONS.iter().zip(mine.iter()) {
        assert_eq!(name, same, "the two lists are out of order");
        assert_eq!(got, want, "{} is {} and should be {}", name, got, want);
    }
}

/// Two options sharing a number makes the second `match` arm unreachable,
/// which is one option silently answering `EINVAL` while its sibling runs.
/// rustc does catch that today -- `unreachable_patterns` sees through the
/// constants, so making one of these equal to another is a compile error --
/// but it catches it at the `match`, so the day an option stops having an arm
/// of its own the collision goes quiet. This says it at the numbers.
#[test]
fn no_two_prctl_options_share_a_number() {
    let mine = declared();
    for (i, &(a, x)) in mine.iter().enumerate() {
        for &(b, y) in &mine[i + 1..] {
            assert_ne!(x, y, "{} and {} are both {}", a, b, x);
        }
    }
}

/// And the gaps are gaps: a number Linux gave to an option this kernel does
/// not implement must not be one of ours, or the call would answer for an
/// option it never implemented.
#[test]
fn the_numbers_this_kernel_does_not_implement_are_not_taken() {
    let mine = declared();
    // PR_GET_UNALIGN 5, PR_SET_UNALIGN 6, PR_GET_FPEMU 9, PR_SET_FPEMU 10,
    // PR_MCE_KILL 33, PR_MCE_KILL_GET 34, PR_SET_MM 35, PR_SET_FP_MODE 45,
    // PR_SET_VMA 0x53564d41.
    for taken in [5, 6, 9, 10, 33, 34, 35, 45, 0x5356_4d41] {
        assert!(
            !mine.iter().any(|&(_, v)| v == taken),
            "{} is Linux's for an option this kernel does not implement",
            taken
        );
    }
}

/// `PR_SET_TIMERSLACK`'s default, which is what a non-positive argument
/// restores and what `PR_GET_TIMERSLACK` reports until somebody sets one.
#[test]
fn the_default_timer_slack_is_the_fifty_microseconds_linux_starts_with() {
    assert_eq!(TIMERSLACK_DEFAULT_NS, 50_000, "50 us in nanoseconds");
}

/// `SCHED_RESET_ON_FORK` is a flag OR-ed into the policy word, so it has to
/// stay clear of every policy number or a policy would read as the flag.
#[test]
fn sched_reset_on_fork_is_the_bit_linux_sched_h_gives() {
    assert_eq!(SCHED_RESET_ON_FORK, 0x4000_0000);
    // SCHED_NORMAL 0 .. SCHED_DEADLINE 6, and the EXT policy 7.
    for policy in 0..=7usize {
        assert_eq!(
            policy & SCHED_RESET_ON_FORK,
            0,
            "policy {} overlaps the flag",
            policy
        );
    }
}

/// `ARG_MAX` is a bound, not an ABI number, so what is pinned is what it is
/// for: it has to stay strictly under the stack image the loader lays out
/// --- the `assert!` in that builder panicked the kernel from an ordinary
/// userspace command --- while still leaving room for one long argument,
/// which is Linux's `MAX_ARG_STRLEN` of 32 pages.
#[test]
fn the_argument_cap_stays_under_the_stack_and_over_one_long_argument() {
    let stack = USER_STACK_PAGES * PAGE_SIZE;
    assert!(
        ARG_MAX < stack,
        "{} is not under the {}-byte stack",
        ARG_MAX,
        stack
    );
    assert!(
        ARG_MAX >= 32 * PAGE_SIZE,
        "{} leaves no room for one MAX_ARG_STRLEN argument",
        ARG_MAX
    );
}
