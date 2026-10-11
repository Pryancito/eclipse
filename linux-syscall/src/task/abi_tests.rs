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

/// Every option code `<linux/prctl.h>` has ever handed out, with the name
/// Linux gave it, in numeric order. 17, 18, 48 and 49 are absent because
/// Linux has never assigned them.
///
/// The whole table and not a sample of it. The claim being made is that a
/// number this kernel declares is either Linux's number for that same option
/// or not a `prctl` option number at all, and a sample can only make that
/// claim about the numbers it happens to hold.
const ASSIGNED: &[(&str, i32)] = &[
    ("PR_SET_PDEATHSIG", 1),
    ("PR_GET_PDEATHSIG", 2),
    ("PR_GET_DUMPABLE", 3),
    ("PR_SET_DUMPABLE", 4),
    ("PR_GET_UNALIGN", 5),
    ("PR_SET_UNALIGN", 6),
    ("PR_GET_KEEPCAPS", 7),
    ("PR_SET_KEEPCAPS", 8),
    ("PR_GET_FPEMU", 9),
    ("PR_SET_FPEMU", 10),
    ("PR_GET_FPEXC", 11),
    ("PR_SET_FPEXC", 12),
    ("PR_GET_TIMING", 13),
    ("PR_SET_TIMING", 14),
    ("PR_SET_NAME", 15),
    ("PR_GET_NAME", 16),
    ("PR_GET_ENDIAN", 19),
    ("PR_SET_ENDIAN", 20),
    ("PR_GET_SECCOMP", 21),
    ("PR_SET_SECCOMP", 22),
    ("PR_CAPBSET_READ", 23),
    ("PR_CAPBSET_DROP", 24),
    ("PR_GET_TSC", 25),
    ("PR_SET_TSC", 26),
    ("PR_GET_SECUREBITS", 27),
    ("PR_SET_SECUREBITS", 28),
    ("PR_SET_TIMERSLACK", 29),
    ("PR_GET_TIMERSLACK", 30),
    ("PR_TASK_PERF_EVENTS_DISABLE", 31),
    ("PR_TASK_PERF_EVENTS_ENABLE", 32),
    ("PR_MCE_KILL", 33),
    ("PR_MCE_KILL_GET", 34),
    ("PR_SET_MM", 35),
    ("PR_SET_CHILD_SUBREAPER", 36),
    ("PR_GET_CHILD_SUBREAPER", 37),
    ("PR_SET_NO_NEW_PRIVS", 38),
    ("PR_GET_NO_NEW_PRIVS", 39),
    ("PR_GET_TID_ADDRESS", 40),
    ("PR_SET_THP_DISABLE", 41),
    ("PR_GET_THP_DISABLE", 42),
    ("PR_MPX_ENABLE_MANAGEMENT", 43),
    ("PR_MPX_DISABLE_MANAGEMENT", 44),
    ("PR_SET_FP_MODE", 45),
    ("PR_GET_FP_MODE", 46),
    ("PR_CAP_AMBIENT", 47),
    ("PR_SVE_SET_VL", 50),
    ("PR_SVE_GET_VL", 51),
    ("PR_GET_SPECULATION_CTRL", 52),
    ("PR_SET_SPECULATION_CTRL", 53),
    ("PR_PAC_RESET_KEYS", 54),
    ("PR_SET_TAGGED_ADDR_CTRL", 55),
    ("PR_GET_TAGGED_ADDR_CTRL", 56),
    ("PR_SET_IO_FLUSHER", 57),
    ("PR_GET_IO_FLUSHER", 58),
    ("PR_SET_SYSCALL_USER_DISPATCH", 59),
    ("PR_PAC_SET_ENABLED_KEYS", 60),
    ("PR_PAC_GET_ENABLED_KEYS", 61),
    ("PR_SCHED_CORE", 62),
    ("PR_SME_SET_VL", 63),
    ("PR_SME_GET_VL", 64),
    ("PR_SET_MDWE", 65),
    ("PR_GET_MDWE", 66),
    ("PR_SET_MEMORY_MERGE", 67),
    ("PR_GET_MEMORY_MERGE", 68),
    ("PR_RISCV_V_SET_CONTROL", 69),
    ("PR_RISCV_V_GET_CONTROL", 70),
    ("PR_RISCV_SET_ICACHE_FLUSH_CTX", 71),
    ("PR_PPC_GET_DEXCR", 72),
    ("PR_PPC_SET_DEXCR", 73),
    ("PR_GET_SHADOW_STACK_STATUS", 74),
    ("PR_SET_SHADOW_STACK_STATUS", 75),
    ("PR_LOCK_SHADOW_STACK_STATUS", 76),
    ("PR_TIMER_CREATE_RESTORE_IDS", 77),
    ("PR_FUTEX_HASH", 78),
    ("PR_RSEQ_SLICE_EXTENSION", 79),
    ("PR_GET_CFI", 80),
    ("PR_SET_CFI", 81),
    ("PR_GET_AUXV", 0x4155_5856),
    ("PR_SET_VMA", 0x5356_4d41),
    ("PR_SET_PTRACER", 0x5961_6d61),
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

/// And every option this kernel declares is Linux's number for that same
/// option, checked against the whole UAPI table rather than a handful of it.
///
/// `every_prctl_option_is_the_number_linux_prctl_h_gives` above says each of
/// the 21 equals the literal written beside it; this says the same thing from
/// the other side, so a new option added here on a number Linux already gave
/// to something else is caught at the number instead of at the name. A
/// `prctl` that answers for an option it never implemented is worse than one
/// that answers `EINVAL`: `PR_SET_MM` arriving as `PR_SET_THP_DISABLE` would
/// take a pointer for a flag.
#[test]
fn no_option_this_kernel_declares_takes_a_number_linux_gave_to_another() {
    for (name, number) in declared() {
        match ASSIGNED.iter().find(|&&(_, n)| n == number) {
            Some(&(linux_name, _)) => assert_eq!(
                linux_name, name,
                "{} is declared as {}, which <linux/prctl.h> gives to {}",
                name, number, linux_name
            ),
            None => panic!(
                "{} is {}, a number <linux/prctl.h> has never assigned",
                name, number
            ),
        }
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
