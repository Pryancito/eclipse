//! What one node advertises, decided ([`topology_for`]) and resolved
//! ([`resources_for_node`]).
//!
//! The decision half takes plain facts, so it needs no registry and no lock.
//! The resolving half drives the process-wide driver registry, so it goes
//! through [`kms_emu`], which holds the DRM test lock for as long as its
//! `Screen` lives.

use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// Leaves `GPU_NODES` empty however the test ends, like `node_tests`: a table
/// left behind holds an `Arc` to a dead emulated GPU and decides later tests'
/// answers. Declared AFTER the `Screen` so it drops BEFORE it, while the lock
/// is still held.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        *GPU_NODES.lock() = Vec::new();
    }
}

/// A box with no framebuffer panel and a card that can scan out: the
/// hardware-KMS case.
fn hardware_kms_card() -> NodeFacts {
    NodeFacts {
        software_kms: false,
        drives_the_panel: true,
        panel_belongs_to_a_node: true,
        is_first_node: true,
        own_hardware_kms: true,
        any_hardware_kms: true,
    }
}

/// A box on the software blit, asked about the node whose card the blit goes
/// through.
fn software_panel_card() -> NodeFacts {
    NodeFacts {
        software_kms: true,
        drives_the_panel: true,
        panel_belongs_to_a_node: true,
        is_first_node: true,
        own_hardware_kms: false,
        any_hardware_kms: false,
    }
}

/// **One panel, one node.** Under software KMS the blit goes to the single
/// `UefiDisplay` of the GOP whichever node asked, so a second node answering
/// with a CRTC of its own would be a second wlroots output painting into the
/// same framebuffer. The card that does not drive the panel lists nothing.
#[test]
fn under_software_kms_only_the_card_that_drives_the_panel_lists_it() {
    assert_eq!(
        topology_for(software_panel_card()),
        NodeTopology::SoftwarePanel
    );
    assert_eq!(
        topology_for(NodeFacts {
            drives_the_panel: false,
            is_first_node: false,
            ..software_panel_card()
        }),
        NodeTopology::RenderOnly,
        "a second node must not advertise the same panel"
    );
}

/// When the panel belongs to no card at all -- it is the firmware
/// framebuffer, and no GPU registered one -- it still has to be advertised
/// somewhere or the compositor has nothing to open. The first node carries it.
#[test]
fn a_panel_that_belongs_to_no_card_is_still_advertised_by_the_first_node() {
    let orphan = NodeFacts {
        drives_the_panel: false,
        panel_belongs_to_a_node: false,
        ..software_panel_card()
    };
    assert_eq!(topology_for(orphan), NodeTopology::SoftwarePanel);
    assert_eq!(
        topology_for(NodeFacts {
            is_first_node: false,
            ..orphan
        }),
        NodeTopology::RenderOnly,
        "only one node carries an orphan panel, and it is the first"
    );
}

/// A card that can scan out answers with its own topology; one that cannot,
/// on a box where another card can, lists nothing. **This is the whole of what
/// the old index-based special case did, now reached from what the card can
/// do** -- and it is why the answer is per node: the machine-wide walk dropped
/// the second card's topology with a cross-driver `any()` and then handed the
/// merged list to every node alike.
#[test]
fn a_card_lists_its_own_topology_and_a_card_that_cannot_scan_out_lists_nothing() {
    assert_eq!(topology_for(hardware_kms_card()), NodeTopology::OwnCard);
    assert_eq!(
        topology_for(NodeFacts {
            own_hardware_kms: false,
            drives_the_panel: false,
            is_first_node: false,
            ..hardware_kms_card()
        }),
        NodeTopology::RenderOnly
    );
}

/// The VirtIO-only box: nothing in the machine can scan out and there is no
/// framebuffer panel either. The merged walk let every driver through rather
/// than leaving userspace with no topology at all, and so does this -- a node
/// with no KMS anywhere to compare against still answers for its own card.
#[test]
fn with_no_kms_anywhere_a_node_still_answers_for_its_own_card() {
    assert_eq!(
        topology_for(NodeFacts {
            own_hardware_kms: false,
            any_hardware_kms: false,
            ..hardware_kms_card()
        }),
        NodeTopology::OwnCard
    );
}

/// An index decides nothing. Two nodes with the same facts get the same
/// answer, whichever one is first -- except for an orphan panel, which is
/// about the panel and not about the card and has its own test above.
#[test]
fn being_the_first_node_does_not_by_itself_change_the_answer() {
    for first in [true, false] {
        assert_eq!(
            topology_for(NodeFacts {
                is_first_node: first,
                ..hardware_kms_card()
            }),
            NodeTopology::OwnCard,
            "is_first_node={first}"
        );
    }
}

/// **A node answers with ITS OWN card's topology, not with some card's.**
/// Both cards here can scan out and list different ids, which is the only way
/// to tell the two apart: with one list per machine, `card1` answered with
/// `card0`'s.
#[test]
fn each_node_answers_with_the_topology_of_its_own_card() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // Registration inserts at 0, so the card attached LAST is the primary and
    // becomes `card0` -- the order of the real two-card box.
    let _second = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-second")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _first = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-first")
            .console_at(0x65, 0x00)
            .with_topology(&[2001], &[2002]),
    );
    build_gpu_nodes();

    assert_eq!(
        resources_for_node(0),
        (alloc::vec![2001], alloc::vec![2002]),
        "card0 is the card attached last"
    );
    assert_eq!(
        resources_for_node(1),
        (alloc::vec![2101], alloc::vec![2102]),
        "card1 must answer with its own card, not with card0's"
    );
}

/// The two-card box this work is about: the compute card has no scanout of its
/// own, the other drives the screen. The compute node lists nothing -- which
/// is what the deleted special case achieved by index -- and the card with the
/// monitor lists its own topology, which the machine-wide walk never let it do.
#[test]
fn the_card_without_scanout_lists_nothing_and_the_other_lists_its_own() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));
    build_gpu_nodes();

    assert_eq!(
        resources_for_node(0),
        (Vec::new(), Vec::new()),
        "card0 is the compute card: no CRTCs, so drmIsKMS fails on it"
    );
    assert_eq!(
        resources_for_node(1),
        (alloc::vec![2101], alloc::vec![2102]),
        "card1 has the monitor and now says so"
    );
}

/// **Under software KMS the panel goes to the card the blit goes through, and
/// that is not the primary.** On a two-card box the primary is the card
/// attached last, which is the compute one; the blit goes through
/// `get_display_driver()`, which is the card that is not a compute GPU. Asking
/// the primary instead would put the panel back on `card0` and silence the
/// card the monitor is on -- exactly the answer this module replaced, and on
/// the path the real box is on today.
///
/// Found by mutation: every other test here drives the hardware-KMS branch,
/// where this question is never asked, so the mutant that swapped the two
/// survived until this test existed.
#[test]
fn under_software_kms_the_panel_follows_the_blit_and_not_the_primary() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // Neither card can scan out, so the panel is the software blit; the
    // console card is the one it goes through, and it is NOT the primary.
    let _console = screen.attach_gpu(EmuGpu::new("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));
    build_gpu_nodes();
    assert!(
        software_kms_active(),
        "this test is about the software blit"
    );
    let primary = get_primary_driver().expect("a primary");
    let display = get_display_driver().expect("a display driver");
    assert!(
        !Arc::ptr_eq(&primary, &display),
        "the two have to differ or the test proves nothing"
    );

    assert_eq!(
        resources_for_node(0),
        (Vec::new(), Vec::new()),
        "card0 is the primary and the blit does NOT go through it"
    );
    assert_eq!(
        resources_for_node(1),
        (alloc::vec![SYNTH_CRTC_ID], alloc::vec![SYNTH_CONNECTOR_ID]),
        "card1 is the card the blit goes through, so it carries the panel"
    );
}

/// **The one-card box must answer exactly as the machine-wide walk did**,
/// because it is the only desktop that comes up. Checked against
/// [`get_resources`] itself rather than against a copy of what it returns, so
/// the two cannot drift apart.
#[test]
fn a_one_card_box_answers_exactly_as_the_machine_wide_walk_did() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _only = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-only")
            .console_at(0x01, 0x00)
            .with_topology(&[2001], &[2002]),
    );
    build_gpu_nodes();

    let (_, crtcs, connectors) = get_resources();
    assert_eq!(resources_for_node(0), (crtcs, connectors));
}

/// And with no node table at all -- the pre-table rule the whole DRM core
/// keeps, which is every minor being the primary's -- the answer is the
/// machine-wide walk, untouched. A node table is built late; a GETRESOURCES
/// before it must not change.
#[test]
fn with_no_node_table_the_answer_is_the_machine_wide_walk() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _only = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-only")
            .console_at(0x01, 0x00)
            .with_topology(&[2001], &[2002]),
    );
    assert!(gpu_nodes().is_empty(), "no table built on purpose");

    let (_, crtcs, connectors) = get_resources();
    assert_eq!(resources_for_node(0), (crtcs, connectors));
    assert_eq!(
        resources_for_node(7),
        (alloc::vec![2001], alloc::vec![2002]),
        "a minor with no node is the primary's, as everywhere else"
    );
}

/// The way back, for the one desktop that currently comes up: the flag puts
/// the topology on the first node and nothing on the rest, which is what the
/// index-based special case did.
#[test]
fn the_hatch_names_one_flag_and_only_that_flag() {
    assert!(panel_on_card0_in("LOG=error:drm.panel_on_card0"));
    assert!(!panel_on_card0_in("LOG=error:nvidia.multigpu"));
    assert!(!panel_on_card0_in(""));
}

/// What the hatch restores, on the two-card box: the first node answers with
/// the machine-wide walk and the rest with nothing -- the opposite assignment
/// to the one above, which is the point of having it.
#[test]
fn the_hatch_restores_the_answer_the_index_based_filter_gave() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));
    build_gpu_nodes();

    let (_, crtcs, connectors) = get_resources();
    assert_eq!(legacy_resources_for_node(0), (crtcs, connectors));
    assert_eq!(legacy_resources_for_node(1), (Vec::new(), Vec::new()));
}
