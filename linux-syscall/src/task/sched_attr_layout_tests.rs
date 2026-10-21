//! `struct sched_attr`, field by field.
//!
//! `sched_setattr(2)` reads these bytes out of the caller's memory and
//! `sched_getattr(2)` writes them back, so the order of the fields is the ABI.
//! The size was already checked against `SCHED_ATTR_SIZE_VER0`, and that is
//! not the same thing: swapping two neighbours of the same width leaves the
//! forty-eight bytes exactly as they were. Three such swaps survived the whole
//! crate suite. The struct is version 1 now, 56 bytes, and its bytes are
//! read and written field by field (`SchedAttr::from_bytes` and `to_bytes`),
//! so those two are pinned to the same offsets.

use super::*;
use core::mem::{align_of, offset_of, size_of};

/// The worst of the three swaps is the first pair, because `size` is the
/// self-describing length `sys_sched_setattr` validates before it trusts
/// anything else: read the policy as the size, and the caller's
/// `SCHED_OTHER` becomes a forty-eight-byte claim of zero.
#[test]
fn struct_sched_attr_is_the_v1_fifty_six_byte_layout() {
    assert_eq!(size_of::<SchedAttr>(), 56);
    assert_eq!(size_of::<SchedAttr>(), SCHED_ATTR_SIZE_VER1);
    assert_eq!(align_of::<SchedAttr>(), 8);
    assert_eq!(offset_of!(SchedAttr, size), 0);
    assert_eq!(offset_of!(SchedAttr, sched_policy), 4);
    assert_eq!(offset_of!(SchedAttr, sched_flags), 8);
    assert_eq!(offset_of!(SchedAttr, sched_nice), 16);
    assert_eq!(offset_of!(SchedAttr, sched_priority), 20);
    assert_eq!(offset_of!(SchedAttr, sched_runtime), 24);
    assert_eq!(offset_of!(SchedAttr, sched_deadline), 32);
    assert_eq!(offset_of!(SchedAttr, sched_period), 40);
    assert_eq!(offset_of!(SchedAttr, sched_util_min), 48);
    assert_eq!(offset_of!(SchedAttr, sched_util_max), 52);
}

/// Every field goes to, and comes back from, its own offset: distinct
/// values in every field, so two fields swapped in either direction show.
#[test]
fn the_bytes_are_the_fields_at_their_offsets() {
    let attr = SchedAttr {
        size: 0x0101_0101,
        sched_policy: 0x0202_0202,
        sched_flags: 0x0303_0303_0404_0404,
        sched_nice: -5,
        sched_priority: 0x0606_0606,
        sched_runtime: 0x0707_0707_0808_0808,
        sched_deadline: 0x0909_0909_0a0a_0a0a,
        sched_period: 0x0b0b_0b0b_0c0c_0c0c,
        sched_util_min: 0x0d0d_0d0d,
        sched_util_max: 0x0e0e_0e0e,
    };
    let b = attr.to_bytes();
    let at = |offset: usize, len: usize| &b[offset..offset + len];
    assert_eq!(at(offset_of!(SchedAttr, size), 4), &attr.size.to_ne_bytes());
    assert_eq!(
        at(offset_of!(SchedAttr, sched_policy), 4),
        &attr.sched_policy.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_flags), 8),
        &attr.sched_flags.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_nice), 4),
        &attr.sched_nice.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_priority), 4),
        &attr.sched_priority.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_runtime), 8),
        &attr.sched_runtime.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_deadline), 8),
        &attr.sched_deadline.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_period), 8),
        &attr.sched_period.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_util_min), 4),
        &attr.sched_util_min.to_ne_bytes()
    );
    assert_eq!(
        at(offset_of!(SchedAttr, sched_util_max), 4),
        &attr.sched_util_max.to_ne_bytes()
    );
    let back = SchedAttr::from_bytes(&b);
    assert_eq!(back.to_bytes(), b);
    assert_eq!(
        (back.size, back.sched_nice, back.sched_util_max),
        (attr.size, -5, attr.sched_util_max)
    );
}

/// The two four-byte fields sit in the eight bytes between `sched_flags`
/// and `sched_runtime` with no padding, which is why eight fields of
/// mixed width come to exactly six words.
#[test]
fn the_nice_and_the_priority_share_one_word_with_no_padding() {
    assert_eq!(
        offset_of!(SchedAttr, sched_priority) - offset_of!(SchedAttr, sched_nice),
        4
    );
    assert_eq!(
        offset_of!(SchedAttr, sched_runtime) - offset_of!(SchedAttr, sched_nice),
        8
    );
}
