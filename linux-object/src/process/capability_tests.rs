//! The privilege model, asked directly: what `capget(2)` publishes and
//! what every gate honours are the same answer, computed once.

use super::*;
use alloc::vec::Vec;

/// Every capability number named in this file, so a new one added to the
/// list is automatically measured by the tests below.
const NAMED: &[(u32, &str)] = &[
    (CAP_SETGID, "CAP_SETGID"),
    (CAP_SETPCAP, "CAP_SETPCAP"),
    (CAP_SYS_ADMIN, "CAP_SYS_ADMIN"),
    (CAP_SYS_BOOT, "CAP_SYS_BOOT"),
    (CAP_SYS_TIME, "CAP_SYS_TIME"),
    (CAP_SYSLOG, "CAP_SYSLOG"),
];

#[test]
fn the_capability_numbers_are_the_ones_capability_h_names() {
    // Wrong by one and a gate asks about somebody else's privilege.
    assert_eq!(CAP_SETGID, 6);
    assert_eq!(CAP_SETPCAP, 8);
    assert_eq!(CAP_SYS_ADMIN, 21);
    assert_eq!(CAP_SYS_BOOT, 22);
    assert_eq!(CAP_SYS_TIME, 25);
    assert_eq!(CAP_SYSLOG, 34);
    // CAP_CHECKPOINT_RESTORE, the last one Linux 5.15 defines.
    assert_eq!(CAP_LAST_CAP, 40);
}

#[test]
fn what_the_kernel_publishes_is_exactly_what_it_honours() {
    // The whole point of the change. A program reads its capability set
    // with `capget(2)` and decides from it whether to even try; the
    // kernel then has to honour exactly that set when the call arrives.
    for euid in [0u32, 1, 1000, u32::MAX] {
        let published = published_capabilities(euid);
        for cap in 0..64u32 {
            let bit = published & (1u64 << cap) != 0;
            assert_eq!(
                bit,
                has_capability(euid, cap),
                "euid {}, cap {}: published {}",
                euid,
                cap,
                bit
            );
        }
    }
}

#[test]
fn root_holds_every_capability_this_kernel_names() {
    for &(cap, name) in NAMED {
        assert!(has_capability(0, cap), "root lacks {}", name);
    }
}

#[test]
fn nobody_else_holds_any_of_them() {
    for euid in [1u32, 100, 1000, u32::MAX] {
        for &(cap, name) in NAMED {
            assert!(!has_capability(euid, cap), "euid {} holds {}", euid, name);
        }
        assert_eq!(published_capabilities(euid), 0);
    }
}

#[test]
fn a_capability_number_this_kernel_does_not_reach_is_held_by_nobody() {
    // Not even by root: `capget` reports bits 0..=CAP_LAST_CAP, so a gate
    // asking about anything above it would be asking about a privilege
    // the kernel never told anyone they had.
    for cap in [CAP_LAST_CAP + 1, 41, 63, 64, u32::MAX] {
        assert!(!has_capability(0, cap), "cap {}", cap);
    }
}

#[test]
fn the_published_set_is_the_bottom_forty_one_bits_and_no_more() {
    // What `capget` used to write down as a constant, derived instead.
    assert_eq!(published_capabilities(0), (1u64 << 41) - 1);
    assert_eq!(published_capabilities(0).count_ones(), CAP_LAST_CAP + 1);
}

#[test]
fn every_named_capability_is_inside_the_range_the_kernel_reports() {
    let over: Vec<&str> = NAMED
        .iter()
        .filter(|&&(cap, _)| cap > CAP_LAST_CAP)
        .map(|&(_, name)| name)
        .collect();
    assert!(over.is_empty(), "past CAP_LAST_CAP: {:?}", over);
}

#[test]
fn the_named_capabilities_are_all_different() {
    for (i, &(a, na)) in NAMED.iter().enumerate() {
        for &(b, nb) in &NAMED[i + 1..] {
            assert_ne!(a, b, "{} and {} are the same number", na, nb);
        }
    }
}
