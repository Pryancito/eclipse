//! KMS lookups answered by the node's own card.
//!
//! The fixture that matters is two cards of the SAME model: they list the
//! same CRTC, connector and plane ids, which is what two RTX 2060 SUPER do,
//! and the only way to tell them apart is something each says differently
//! about the same id (here, its EDID).

use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// Leaves `GPU_NODES` empty however the test ends; see `topology::tests`.
/// Declared AFTER the `Screen` so it drops BEFORE it, while the lock is held.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        *GPU_NODES.lock() = Vec::new();
    }
}

/// A whole EDID block that passes its checksum, told apart by `serial`.
fn edid(serial: u8) -> [u8; 128] {
    let mut block = [0u8; 128];
    block[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    block[12] = serial;
    block[18] = 1; // EDID 1.4
    block[19] = 4;
    let sum = block[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    block[127] = 0u8.wrapping_sub(sum);
    block
}

/// **`card1` answers with `card1`'s connector, even when `card0` has one with
/// the same id.** The machine-wide walk took the first driver that knew the
/// id, which is always `card0`'s, so the second card's monitor was reported as
/// the first card's.
#[test]
fn two_cards_with_the_same_ids_each_answer_for_their_own() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // Registration inserts at 0: the card attached LAST is `card0`.
    let _second = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-second")
            .console_at(0x01, 0x00)
            .with_ids(1001, 1002, 3001)
            .with_edid(edid(2)),
    );
    let _first = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-first")
            .console_at(0x65, 0x00)
            .with_ids(1001, 1002, 3001)
            .with_edid(edid(1)),
    );
    build_gpu_nodes();

    assert_eq!(get_connector_edid_on(0, 1002), Some(edid(1)));
    assert_eq!(
        get_connector_edid_on(1, 1002),
        Some(edid(2)),
        "card1 answered with card0's monitor"
    );
    assert!(get_crtc_on(1, 1001).is_some());
    assert!(get_connector_on(1, 1002).is_some());
    assert!(get_plane_on(1, 3001).is_some());
}

/// A node does not find another card's objects: an id only `card0` has is
/// ENOENT on `card1`, as `drm_mode_object_find` on the wrong device would be.
#[test]
fn a_node_does_not_find_the_other_cards_objects() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _second = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-second")
            .console_at(0x01, 0x00)
            .with_ids(2101, 2102, 2103),
    );
    let _first = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-first")
            .console_at(0x65, 0x00)
            .with_ids(2001, 2002, 2003),
    );
    build_gpu_nodes();

    assert!(get_crtc_on(0, 2001).is_some());
    assert!(get_crtc_on(1, 2001).is_none(), "card1 found card0's CRTC");
    assert!(
        get_connector_on(1, 2002).is_none(),
        "card1 found card0's connector"
    );
    assert!(get_plane_on(1, 2003).is_none(), "card1 found card0's plane");
    assert!(get_crtc_on(0, 2101).is_none(), "card0 found card1's CRTC");
}

/// `GETPLANERESOURCES` lists the node's own card's planes, matching the CRTCs
/// `GETRESOURCES` lists there; it used to list every card's.
#[test]
fn each_node_lists_its_own_cards_planes() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _second = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-second")
            .console_at(0x01, 0x00)
            .with_ids(2101, 2102, 2103),
    );
    let _first = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-first")
            .console_at(0x65, 0x00)
            .with_ids(2001, 2002, 2003),
    );
    build_gpu_nodes();

    let ids = |minor| -> Vec<u32> { get_planes_typed_on(minor).iter().map(|p| p.id).collect() };
    assert_eq!(ids(0), alloc::vec![2003]);
    assert_eq!(ids(1), alloc::vec![2103]);
}

/// A card that cannot scan out, next to one that can, lists no CRTC and so no
/// plane either: a plane list must not name CRTCs the node does not have.
#[test]
fn a_render_only_node_lists_no_planes() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_ids(2001, 2002, 2003),
    );
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));
    build_gpu_nodes();

    let render_only = (0..2u32)
        .find(|&m| resources_for_node(m).0.is_empty())
        .expect("one node lists no CRTC");
    assert!(get_planes_typed_on(render_only).is_empty());
}

/// With no node table every minor is the primary's (the rule the whole core
/// keeps before `build_gpu_nodes`), so the lookups are the machine-wide ones.
#[test]
fn with_no_node_table_the_lookup_is_the_machine_wide_one() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-only").with_ids(2001, 2002, 2003));

    assert_eq!(
        get_crtc_on(5, 2001).map(|c| c.id),
        get_crtc(2001).map(|c| c.id)
    );
    assert_eq!(
        get_connector_on(5, 2002).map(|c| c.id),
        get_connector(2002).map(|c| c.id)
    );
    assert_eq!(
        get_plane_on(5, 2003).map(|p| p.id),
        get_plane(2003).map(|p| p.id)
    );
    let ids = |v: Vec<DrmPlane>| -> Vec<u32> { v.iter().map(|p| p.id).collect() };
    assert_eq!(ids(get_planes_typed_on(5)), ids(get_planes_typed()));
}
