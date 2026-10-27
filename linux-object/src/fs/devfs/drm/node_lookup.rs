//! KMS object lookups scoped to the node they arrive on.
//!
//! In Linux an object id is looked up in ONE device: `drm_mode_object_find`
//! walks `dev->mode_config.object_idr` of the device behind the fd, so
//! `card0` and `card1` can both have a CRTC 1001 and neither ever sees the
//! other's. Here [`super::get_crtc`] and friends walk every registered driver
//! and take the first that answers, and two cards of the same model answer
//! the same ids (two RTX 2060 SUPER both say CRTC 1001, plane 3001). Since
//! `GETRESOURCES` already answers per node ([`super::resources_for_node`]),
//! `card1` listed its own CRTC and then `GETCRTC` on that id came back with
//! `card0`'s: the second card's topology was advertised and then answered by
//! the first.
//!
//! These are the same lookups asking only the node's own card. With no node
//! table, or under `drm.panel_on_card0` (which advertises the machine-wide
//! walk on the first node), they ask every driver exactly as before.

use super::topology::{facts_for, panel_on_card0};
use super::*;

/// The drivers a lookup on `minor` may ask: the node's own card when it has
/// one, every driver otherwise (no node table, or the legacy machine-wide
/// topology on the first node).
fn drivers_for_node(minor: u32) -> Vec<Arc<dyn DrmScheme>> {
    if panel_on_card0() {
        return snapshot_drivers();
    }
    match driver_for_minor(minor) {
        Some(driver) => alloc::vec![driver],
        None => snapshot_drivers(),
    }
}

/// The card whose CRTC state a request on `minor` reads and writes: the
/// node's own, or the panel's when there is no node table or the legacy
/// machine-wide topology is on (every node then shows the panel's objects,
/// so it must show the panel's framebuffer too).
///
/// Takes `GPU_NODES`, so never call it with `DRM_STATE` held.
pub fn card_of_minor(minor: u32) -> u32 {
    if panel_on_card0() {
        return PANEL_CARD;
    }
    card_index_of_minor(minor).unwrap_or(PANEL_CARD)
}

/// [`get_crtc`] as node `minor` sees it.
pub fn get_crtc_on(minor: u32, id: u32) -> Option<DrmCrtc> {
    get_crtc_among(|| drivers_for_node(minor), card_of_minor(minor), id)
}

/// [`get_connector`] as node `minor` sees it.
pub fn get_connector_on(minor: u32, id: u32) -> Option<DrmConnector> {
    get_connector_among(|| drivers_for_node(minor), id)
}

/// [`get_connector_edid`] as node `minor` sees it.
pub fn get_connector_edid_on(minor: u32, id: u32) -> Option<[u8; 128]> {
    get_connector_edid_among(drivers_for_node(minor), id)
}

/// [`get_plane`] as node `minor` sees it.
pub fn get_plane_on(minor: u32, id: u32) -> Option<DrmPlane> {
    get_plane_among(|| drivers_for_node(minor), card_of_minor(minor), id)
}

/// [`get_planes_typed`] as node `minor` sees it: the planes of the card whose
/// CRTCs `GETRESOURCES` lists on this node, and none on a node that lists no
/// CRTC. Decided by the same [`topology_for`] as the resources, so a node never
/// lists planes for CRTCs it does not have.
pub fn get_planes_typed_on(minor: u32) -> Vec<DrmPlane> {
    if panel_on_card0() {
        return get_planes_typed();
    }
    let Some(driver) = driver_for_minor(minor) else {
        return get_planes_typed();
    };
    let card = card_of_minor(minor);
    match topology_for(facts_for(minor)) {
        NodeTopology::SoftwarePanel => get_planes_typed(),
        NodeTopology::RenderOnly => Vec::new(),
        NodeTopology::OwnCard => driver
            .get_planes()
            .into_iter()
            .filter_map(|id| driver.get_plane(id))
            .map(|plane| with_core_scanout_state(card, plane))
            .collect(),
    }
}

/// [`crtc_count`] as node `minor` sees it: the CRTCs `GETRESOURCES` lists
/// there, so a `WAIT_VBLANK` on pipe 1 of `card1` is measured against
/// `card1`'s pipes and not the merged list.
pub fn crtc_count_on(minor: u32) -> usize {
    if software_kms_active() {
        return 1;
    }
    resources_for_node(minor).0.len()
}

#[cfg(test)]
mod tests;
