//! What ONE DRM node advertises as its KMS topology.
//!
//! In Linux a resource list belongs to a device: `drm_mode_getresources`
//! walks `dev->mode_config`, so what `card0` lists is card0's and what
//! `card1` lists is card1's. Here it was the other way round --
//! [`super::get_resources`] takes no minor at all and answers for the whole
//! machine out of one `DRM_STATE` -- and the thing that kept that from being
//! visible was a special case in the `GETRESOURCES` arm: any node past index
//! 0 was handed zeros so `drmIsKMS` would fail on it and the compositor would
//! stay on `card0`.
//!
//! That special case was the console/compute distinction, which is the one
//! thing this work is removing. But it was not only a policy, it was a plug,
//! and pulling it on its own would have made the machine-wide answer visible
//! on every node at once. [`super::get_resources`] has **two machine-wide
//! gates** inside it:
//!
//! 1. the software-KMS short circuit, which returns
//!    [`super::SYNTH_CRTC_ID`] and [`super::SYNTH_CONNECTOR_ID`] -- one
//!    panel for the whole machine -- before asking any driver, so two nodes
//!    would have advertised the *same* id and wlroots would have built two
//!    outputs with identical resource ids (0x0 dumb buffers, EINVAL: the
//!    failure its own de-duplication comment warns about);
//! 2. `has_hardware_kms`, computed with `drivers.iter().any(..)` and then
//!    applied per driver, which is a cross-driver gate: on a box where only
//!    the card that scans out has a surface-flip ladder, every other card's
//!    topology is dropped from the merged list entirely.
//!
//! So the node answers for its own card, and the old special case falls out
//! as the last line rather than the first. [`topology_for`] is the decision
//! on its own, from facts about the machine and never from a node's index.

use super::*;

/// What a node answers `GETRESOURCES` with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeTopology {
    /// The machine's software panel: the synthetic CRTC and connector. Exactly
    /// one node gets this, because there is exactly one panel behind it -- the
    /// blit goes to the single `UefiDisplay` of the GOP whichever node asked.
    SoftwarePanel,
    /// This node's own card, asked for its own CRTCs and connectors.
    OwnCard,
    /// A render-only device: no CRTCs, no connectors, so `drmIsKMS` fails here
    /// and a compositor uses this node to render and not to scan out. This is
    /// what the old index-based special case did to every node past 0; it is
    /// kept as an *answer*, reached from what the card can do.
    RenderOnly,
}

/// The facts [`topology_for`] decides from. Gathered by [`facts_for`], which
/// reads the driver registry; kept apart from the decision because the
/// registry is process-wide and the decision is what has to be pinned down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeFacts {
    /// There is a framebuffer panel and the card driving it has no hardware
    /// KMS, so scanout is the software blit ([`super::software_kms_active`]).
    pub software_kms: bool,
    /// This node's card is the one the software blit goes through
    /// ([`super::get_display_driver`]).
    pub drives_the_panel: bool,
    /// Some node's card is that one. When no node's card is -- the panel is
    /// the firmware framebuffer and belongs to no GPU -- the panel still has
    /// to be advertised somewhere, or the desktop has nothing to open.
    pub panel_belongs_to_a_node: bool,
    /// This node is the lowest-indexed one, which is where the panel goes when
    /// it belongs to no card.
    pub is_first_node: bool,
    /// This node's card has hardware KMS of its own.
    pub own_hardware_kms: bool,
    /// Some card in the machine has it.
    pub any_hardware_kms: bool,
}

/// Which topology a node with these facts advertises.
///
/// **A node's index decides nothing here**, with one exception that is about
/// the panel and not about the card: when the software panel belongs to no
/// card at all, the first node carries it, because something must.
pub fn topology_for(f: NodeFacts) -> NodeTopology {
    if f.software_kms {
        // One panel, one node. A card that does not drive it announces zero
        // rather than a synthetic id of its own: a second id would be a
        // second wlroots output painting into the same framebuffer.
        let carries_the_panel =
            f.drives_the_panel || (!f.panel_belongs_to_a_node && f.is_first_node);
        return if carries_the_panel {
            NodeTopology::SoftwarePanel
        } else {
            NodeTopology::RenderOnly
        };
    }
    // Its own card, when that card can scan out -- or when nothing in the
    // machine can, which is the VirtIO-only box the merged walk also let
    // through rather than leaving userspace with no topology at all.
    if f.own_hardware_kms || !f.any_hardware_kms {
        NodeTopology::OwnCard
    } else {
        NodeTopology::RenderOnly
    }
}

/// [`NodeFacts`] for `minor`, from the driver registry.
///
/// With no node table every minor is the primary's -- the pre-table rule the
/// whole DRM core keeps -- so a one-card box reports the one card as driving
/// the panel and as the first node, which is what makes [`resources_for_node`]
/// answer exactly as the machine-wide walk did there.
fn facts_for(minor: u32) -> NodeFacts {
    let software_kms = software_kms_active();
    let display = get_display_driver();
    let drivers = snapshot_drivers();
    let any_hardware_kms = drivers.iter().any(|d| d.has_hardware_kms());

    let node = driver_for_minor(minor);
    let Some(node) = node else {
        let own = get_primary_driver().is_some_and(|d| d.has_hardware_kms());
        return NodeFacts {
            software_kms,
            drives_the_panel: true,
            panel_belongs_to_a_node: true,
            is_first_node: true,
            own_hardware_kms: own,
            any_hardware_kms,
        };
    };

    let panel_belongs_to_a_node = match &display {
        Some(d) => gpu_nodes().iter().any(|n| Arc::ptr_eq(&n.driver, d)),
        None => false,
    };
    NodeFacts {
        software_kms,
        drives_the_panel: display.is_some_and(|d| Arc::ptr_eq(&node, &d)),
        panel_belongs_to_a_node,
        is_first_node: lowest_node_index().is_some_and(|i| card_index_of_minor(minor) == Some(i)),
        own_hardware_kms: node.has_hardware_kms(),
        any_hardware_kms,
    }
}

/// The lowest index in the node table, which is 0 whenever there is a table at
/// all. Read rather than assumed so a table built without a node 0 still has a
/// first node to carry a panel that belongs to no card.
fn lowest_node_index() -> Option<u32> {
    gpu_nodes().iter().map(|n| n.index).min()
}

/// The CRTCs and connectors node `minor` advertises, as `GETRESOURCES`
/// reports them.
///
/// Framebuffers are not here: that arm reports the caller's own
/// (`framebuffer_ids_for`), as `drm_mode_getresources` walks
/// `file_priv->fbs`.
///
/// **A driver is only asked when the answer is its own card's**, so the live
/// NV0073 display-control and DDC probe behind `NvidiaGpu::get_resources`
/// now runs on one card per call instead of on every hardware-KMS card --
/// the cost the machine-wide walk's own comment warned about.
pub fn resources_for_node(minor: u32) -> (Vec<u32>, Vec<u32>) {
    if panel_on_card0() {
        return legacy_resources_for_node(minor);
    }
    match topology_for(facts_for(minor)) {
        NodeTopology::RenderOnly => (Vec::new(), Vec::new()),
        NodeTopology::SoftwarePanel => (vec![SYNTH_CRTC_ID], vec![SYNTH_CONNECTOR_ID]),
        NodeTopology::OwnCard => match driver_for_minor(minor) {
            Some(d) => {
                let (_, crtcs, connectors) = d.get_resources();
                (crtcs, connectors)
            }
            // No node table, so there is no "own card" to ask and the answer
            // is the machine-wide walk this function replaced, unchanged.
            None => {
                let (_, crtcs, connectors) = get_resources();
                (crtcs, connectors)
            }
        },
    }
}

/// `drm.panel_on_card0` on the kernel cmdline: go back to advertising the
/// topology on the lowest node and nothing on the rest, which is what the
/// index-based special case did.
///
/// The one-card path does not need this -- [`resources_for_node`] answers
/// there exactly as the machine-wide walk did, by construction, and there is a
/// test that fails if that stops being true. A box with two cards is where the
/// answer changes on purpose: under software KMS the panel is advertised by the
/// card the blit goes through, which on a box whose first node is the compute
/// card means the compositor opens `card1` where it used to open `card0`. That
/// is what Linux does and it is the point of the change, but it is also the one
/// desktop that currently comes up, so it gets a one-word way back.
fn panel_on_card0() -> bool {
    panel_on_card0_in(kernel_hal::boot::cmdline().as_str())
}

/// [`panel_on_card0`] against a given cmdline, which is the only way to test
/// it: the real one comes from the bootloader.
fn panel_on_card0_in(cmdline: &str) -> bool {
    kernel_hal::cmdline::flag(cmdline, "drm.panel_on_card0")
}

/// What `GETRESOURCES` answered before this module: zeros past the lowest
/// node, and the machine-wide walk on it.
fn legacy_resources_for_node(minor: u32) -> (Vec<u32>, Vec<u32>) {
    let past_the_first = match (card_index_of_minor(minor), lowest_node_index()) {
        (Some(index), Some(first)) => index > first,
        _ => false,
    };
    if past_the_first {
        return (Vec::new(), Vec::new());
    }
    let (_, crtcs, connectors) = get_resources();
    (crtcs, connectors)
}

#[cfg(test)]
mod tests;
