//! What the per-card CRTC and plane ids have to guarantee: the first card
//! sees exactly what it always saw, two cards never share an id, and no id
//! ever lands in the connector space.
use super::*;
use core::sync::atomic::AtomicU32;

/// A one-card box must see exactly the ids this driver has always
/// advertised. This is the test that fails if the new arithmetic is ever
/// let loose on slot 0.
#[test]
fn the_first_card_keeps_the_ids_userspace_has_always_seen() {
    assert_eq!(crtc_id(0), 2001);
    assert_eq!(plane_id(0), 3001);
}

/// The bug: two cards answered the same CRTC id, so `get_resources`
/// de-duplicated the second card's away.
#[test]
fn two_cards_never_share_a_crtc_or_a_plane() {
    let mut seen = alloc::vec::Vec::new();
    for slot in 0..MAX_KMS_CARDS {
        for id in [crtc_id(slot), plane_id(slot)] {
            assert!(
                !seen.contains(&id),
                "slot {} reuses id {}, which is the collision this exists to stop",
                slot,
                id
            );
            seen.push(id);
        }
    }
    assert_eq!(seen.len(), 2 * MAX_KMS_CARDS as usize);
}

/// Connectors, CRTCs and planes are ONE id space: the core hands them all to
/// userspace from one table and looks them up by number. An overlap resolves
/// a `GETCRTC` onto a connector.
/// Connectors, CRTCs and planes are ONE id space: the core hands them all to
/// userspace from one table and looks them up by number. An overlap resolves
/// a `GETCRTC` onto a connector.
///
/// For slots 1 and up. **Slot 0 is inside the connector space and always
/// was** -- 2001 is `rm_connector_id(10, 0)` and 3001 is
/// `rm_connector_id(20, 0)` -- which this test states rather than hides,
/// because it is the reason the per-card ids start at a base of their own
/// instead of counting up from 2001.
#[test]
fn no_card_but_the_first_lands_in_the_connector_space() {
    // `rm_connector_id(instance, bit) = 1001 + 100 * instance + bit`, both
    // under 32. Spelled out here rather than called, so this test still
    // fails if that formula is widened: that is the whole point of it.
    let connector = |id: u32| {
        (0..32u32).any(|instance| (0..32u32).any(|bit| 1001 + 100 * instance + bit == id))
    };
    for slot in 1..MAX_KMS_CARDS {
        for id in [crtc_id(slot), plane_id(slot)] {
            assert!(
                !connector(id),
                "slot {} takes id {}, which a connector can also be",
                slot,
                id
            );
        }
    }
    // The exception, written down: the first card's two ids ARE reachable as
    // connector ids, on a box with eleven or twenty-one NVIDIA cards. Kept
    // on purpose (moving them is a uAPI change for the box that works
    // today), and the reason slots 1.. do not simply count up from 2001.
    assert!(connector(crtc_id(0)), "2001 is rm_connector_id(10, 0)");
    assert!(connector(plane_id(0)), "3001 is rm_connector_id(20, 0)");
    // The compile-time `const _` next to the bases covers the rest, so a
    // lowered base is a build failure and never reaches this test.
}

/// Reservations hand out 0, 1, 2, ... in registration order.
#[test]
fn slots_come_out_in_order() {
    let next = AtomicU32::new(0);
    for expected in 0..4 {
        assert_eq!(alloc_kms_slot_from(&next), expected);
    }
}

/// Past the cap a card SHARES the last slot's ids rather than walking into
/// the plane range -- it has no panel the core can reach anyway, and a
/// wrong id is worse than a shared one. The counter still advances, so it
/// never rewinds onto a card that already has a slot.
#[test]
fn a_card_past_the_cap_shares_the_last_slot_and_takes_no_ones_plane() {
    let next = AtomicU32::new(MAX_KMS_CARDS - 1);
    let last = alloc_kms_slot_from(&next);
    let over = alloc_kms_slot_from(&next);
    let further = alloc_kms_slot_from(&next);
    assert_eq!(last, MAX_KMS_CARDS - 1);
    assert_eq!(over, MAX_KMS_CARDS);
    assert_eq!(further, MAX_KMS_CARDS + 1);
    assert_eq!(crtc_id(over), crtc_id(last));
    assert_eq!(crtc_id(further), crtc_id(last));
    assert_eq!(plane_id(over), plane_id(last));
    assert!(crtc_id(over) < PLANE_ID_BASE, "never into the plane range");
}

/// A test GPU is slot 0, fixed: every existing test of this driver asserts
/// 2001 and 3001, and the suite builds more GPUs than there are slots.
#[test]
fn a_test_gpu_is_the_first_card() {
    assert_eq!(test_kms_slot(), 0);
    assert_eq!(crtc_id(test_kms_slot()), 2001);
    assert_eq!(plane_id(test_kms_slot()), 3001);
}
