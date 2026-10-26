//! The DRM object ids one card advertises: its CRTC and its primary plane.
//!
//! Every NVIDIA card used to hand out the SAME two, written as literals:
//! CRTC `2001` and plane `3001`. On one card that is fine and it is what
//! userspace has always seen. On two it is a collision in a global id space,
//! and the DRM core's own `get_resources` says so in a comment while working
//! around it: it merges every card's topology into one list and
//! **de-duplicates by id**, so of two cards reporting CRTC 2001 only the
//! first survives and the second card's CRTC simply is not there. A card
//! whose CRTC nobody can name cannot be given a framebuffer to scan out,
//! which is the floor under "one labwc desktop over
//! `WLR_DRM_DEVICES=/dev/dri/card0:/dev/dri/card1`".
//!
//! Connectors were already per card -- `rm_connector_id(instance, bit)` --
//! so they do NOT de-duplicate, and that asymmetry is the whole bug: the
//! merged list carried the second card's connectors next to only the first
//! card's CRTC.
//!
//! # Where the slot comes from, and why not the RM instance
//!
//! From a counter taken once in `NvidiaGpu::new`, next to the one that
//! reserves the card's slice of the GEM handle range
//! ([`crate::scheme::gem_mmap::alloc_handle_slice`]) -- the same problem and
//! the same shape of answer: a global id space with no card in the key, so
//! each card takes a disjoint piece of it at construction.
//!
//! NOT from `rm_device_instance`, which the connector ids use, because it is
//! `None` until this card's bring-up has run -- and before bring-up is
//! exactly when both cards answered `2001`. Not from the `/dev/dri` minor
//! either: a card can have more than one node (a `card` and a `renderD`, and
//! until 26-oct-2026 a two-card box had THREE nodes because the primary got
//! a second view of itself), so the node index is not the card.

/// First id of the per-card CRTC range, for every card but the first.
///
/// Above the connector space on purpose. `rm_connector_id(instance, bit)` is
/// `1001 + 100 * instance + bit` with both under 32, so it reaches 4132; a
/// CRTC numbered `2001 + slot` would have collided with the connectors of
/// RM instance 10 and up. [`RANGES_ARE_DISJOINT`] fails the build if the
/// connector formula ever grows into these.
pub const CRTC_ID_BASE: u32 = 5000;

/// First id of the per-card primary-plane range, for every card but the
/// first. See [`CRTC_ID_BASE`].
pub const PLANE_ID_BASE: u32 = 6000;

/// The id the first card keeps, whatever the scheme: this driver has
/// advertised CRTC 2001 and plane 3001 since before there was a second card,
/// and a one-card box must see exactly what it saw before. The same
/// boot-stable-alias rule `rm_connector_bit` keeps for connector 1001.
///
/// **These two are inside the connector space, and that is older than this
/// module.** `rm_connector_id(instance, bit)` is `1001 + 100 * instance +
/// bit`, so 2001 is exactly `rm_connector_id(10, 0)` and 3001 is
/// `rm_connector_id(20, 0)`. The overlap is kept rather than fixed: moving
/// them is a uAPI change for the box that works today, and it can only bite
/// a machine with eleven (or twenty-one) NVIDIA cards in it, which is past
/// [`MAX_KMS_CARDS`] worth of panels and well past anything this driver has
/// run on. So the disjointness the build checks is for slots **1 and up**;
/// slot 0 is a deliberate exception, and the day an eleventh card matters
/// the fix is to renumber the connectors, not these.
pub const FIRST_CRTC_ID: u32 = 2001;
/// See [`FIRST_CRTC_ID`].
pub const FIRST_PLANE_ID: u32 = 3001;

/// How many cards get ids of their own, matching the `/dev/dri` node table's
/// cap. Past that a card reuses the last slot rather than walking into the
/// plane range: it has no panel the core can reach anyway, and a wrong id is
/// worse than a shared one.
pub const MAX_KMS_CARDS: u32 = 64;

/// Highest id the connector formula can produce: `rm_connector_id(31, 31)`.
const MAX_CONNECTOR_ID: u32 = 1001 + 100 * 31 + 31;

/// The three ranges may not overlap for slots 1 and up, and the build says
/// so. Slot 0 is the documented exception -- see [`FIRST_CRTC_ID`].
///
/// Anonymous (`const _`) on purpose: a named constant nobody reads is
/// `dead_code`, which is an error here (`deny(warnings)`), and wiring a
/// reader to it just to keep it alive would invite someone to delete the
/// reader. An anonymous const item is always evaluated and never unused.
///
/// Connectors, CRTCs and planes all live in ONE id space here (the DRM core
/// hands them to userspace from one table and looks them up by number), so
/// an overlap resolves a `GETCRTC` onto a connector. The CRTC and plane
/// bases were chosen above the connector space; this is what notices if the
/// connector formula is widened, or a base lowered, without the other side
/// being looked at.
const _: () = {
    assert!(MAX_CONNECTOR_ID < CRTC_ID_BASE);
    assert!(CRTC_ID_BASE + MAX_KMS_CARDS < PLANE_ID_BASE);
    // And neither range may swallow the two ids the first card keeps, which
    // would make slot 0 share with some slot n.
    assert!(FIRST_CRTC_ID < CRTC_ID_BASE);
    assert!(FIRST_PLANE_ID < CRTC_ID_BASE);
    // Slot 0's pair is itself disjoint, whatever the connector space says.
    assert!(FIRST_CRTC_ID != FIRST_PLANE_ID);
};

/// The DRM CRTC id of the card holding KMS slot `slot`.
pub const fn crtc_id(slot: u32) -> u32 {
    if slot == 0 {
        FIRST_CRTC_ID
    } else {
        CRTC_ID_BASE + clamp_slot(slot)
    }
}

/// The DRM id of the primary plane of the card holding KMS slot `slot`.
pub const fn plane_id(slot: u32) -> u32 {
    if slot == 0 {
        FIRST_PLANE_ID
    } else {
        PLANE_ID_BASE + clamp_slot(slot)
    }
}

/// Slots past the cap share the last one. See [`MAX_KMS_CARDS`].
const fn clamp_slot(slot: u32) -> u32 {
    if slot < MAX_KMS_CARDS {
        slot
    } else {
        MAX_KMS_CARDS - 1
    }
}

/// Reserve the next card's KMS slot. Called once per GPU, from
/// `NvidiaGpu::new`, like [`crate::scheme::gem_mmap::alloc_handle_slice`].
pub fn alloc_kms_slot() -> u32 {
    use core::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    alloc_kms_slot_from(&NEXT)
}

/// [`alloc_kms_slot`] over any counter, so a test can walk a private one to
/// the cap without spending the real reservations -- every GPU the binary
/// builds takes one from that counter and it never rewinds.
fn alloc_kms_slot_from(next: &core::sync::atomic::AtomicU32) -> u32 {
    use core::sync::atomic::Ordering;
    let slot = next.fetch_add(1, Ordering::Relaxed);
    if slot >= MAX_KMS_CARDS {
        crate::klog_warn!(
            "[NVIDIA] more than {} cards asked for a KMS id slot -- this one shares the last \
             card's CRTC and plane ids rather than taking a connector's",
            MAX_KMS_CARDS
        );
    }
    slot
}

/// The slot a test-built GPU (`NvidiaGpu::for_test`) advertises from: slot 0,
/// fixed, not a reservation.
///
/// Fixed because slot 0 is what every existing test of this driver was
/// written against -- it asserts CRTC 2001 and plane 3001 -- and because the
/// reservations are 64 for the whole binary while the test suite builds a GPU
/// per test: drawing from the real counter would give the 65th test a shared
/// slot and a warning, for a GPU that never had a second card beside it.
#[cfg(test)]
pub(super) const fn test_kms_slot() -> u32 {
    0
}

#[cfg(test)]
mod tests;
