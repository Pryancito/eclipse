use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// Leaves `GPU_NODES` empty however the test ends. A table left behind
/// holds an `Arc` to a dead emulated GPU and makes `is_compute_minor`
/// answer true for minors a later test expects to be plain KMS cards.
///
/// Declare it AFTER the [`kms_emu::Screen`], so it drops BEFORE it: the
/// `Screen` holds the DRM test lock, and a reset that ran after the lock
/// was released could erase a table another test had just built under it.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        *GPU_NODES.lock() = Vec::new();
    }
}

/// `(card minor, pci bdf)` per node, in index order: what userspace ends
/// up seeing in `/dev/dri` and `/sys/class/drm`.
fn built() -> Vec<(u32, Option<(u32, u8, u8, u8)>)> {
    build_gpu_nodes();
    gpu_nodes()
        .iter()
        .map(|n| (n.card_minor(), n.driver.pci_bdf()))
        .collect()
}

/// The two-card box this is all about: one RTX drives the screen, the other
/// is headless. Both have to be nameable -- `vulkaninfo` reported a single
/// card because `card0` and `card1` were two views of the compute card and
/// the console card had no node at all.
#[test]
fn the_console_card_gets_a_node_of_its_own_on_a_two_card_box() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // Registration order is the reverse of the PCI probe order
    // (`insert(0)`), so the console card goes in first and the compute one
    // ends up primary -- exactly the box under test.
    let _console = screen.attach_gpu(EmuGpu::hardware_kms("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));

    let nodes = built();
    assert_eq!(
        nodes.len(),
        3,
        "expected card0/card1 for the compute card and card2 for the console one, got {:x?}",
        nodes
    );
    let compute = Some((0, 0x65, 0x00, 0));
    let console = Some((0, 0x01, 0x00, 0));
    assert_eq!(nodes[0], (0, compute), "card0 must stay the primary GPU");
    assert_eq!(
        nodes[1],
        (1, compute),
        "card1 is still the headless compute view of card0's card"
    );
    assert_eq!(
        nodes[2],
        (2, console),
        "the card driving the screen got no node"
    );
}

/// A card with no role at all -- the VirtIO-like driver QEMU gives us --
/// still gets its pair. The old filter asked for `is_compute_gpu`, so a
/// second such card was simply dropped.
#[test]
fn a_card_with_no_role_is_named_too() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _first = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-0"));
    let _second = screen.attach_gpu(EmuGpu::new("emu-gpu-1"));

    assert_eq!(
        built().len(),
        2,
        "two registered cards, two node pairs -- neither declares a role"
    );
}

/// One card stays one node pair: nothing to duplicate and nothing to add,
/// which is the console-only and QEMU case and must not have changed.
#[test]
fn a_single_card_still_gets_exactly_one_pair() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _only = screen.attach_gpu(EmuGpu::hardware_kms("emu-only").console_at(0x01, 0x00));

    let nodes = built();
    assert_eq!(nodes.len(), 1, "got {:x?}", nodes);
    assert_eq!(nodes[0].0, 0);
}

/// Past the head, the order is the PCI address, not the registration
/// order: `card2` has to name the same card after a reboot.
#[test]
fn the_cards_past_the_head_are_sorted_by_pci_address() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // Registered high BDF first, so registration order and BDF order
    // disagree for the two cards that land past the head.
    let _high = screen.attach_gpu(EmuGpu::hardware_kms("emu-high").console_at(0x65, 0x00));
    let _low = screen.attach_gpu(EmuGpu::hardware_kms("emu-low").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x0b, 0x00));

    let nodes = built();
    assert_eq!(nodes.len(), 4, "got {:x?}", nodes);
    assert_eq!(
        (nodes[2].1, nodes[3].1),
        (Some((0, 0x01, 0x00, 0)), Some((0, 0x65, 0x00, 0))),
        "card2/card3 are not in PCI order: {:x?}",
        nodes
    );
}

/// The identity of every node of a two-card box, which is what decides how
/// many Vulkan devices NVK enumerates. `card1` is the same card as `card0`
/// and must stay `eclipse-compute` however ready that card is; the second
/// card is nouveau once its RM is up, and `eclipse-compute` while it is
/// cold -- a node that enumerated as nouveau and died on the first EXEC
/// would be worse than no node.
#[test]
fn the_second_card_is_a_nouveau_device_only_once_its_rm_is_up() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_nouveau_uapi(false),
    );
    let _compute = screen.attach_gpu(
        EmuGpu::new("emu-compute")
            .compute_at(0x65, 0x00)
            .with_nouveau_uapi(true),
    );
    build_gpu_nodes();
    let id = |minor| node_driver_id(minor, true);

    // Cold console card: one nouveau device, as today.
    assert_eq!(id(0), NodeDriverId::Nouveau, "card0");
    assert_eq!(id(128), NodeDriverId::Nouveau, "renderD128");
    assert_eq!(id(1), NodeDriverId::Compute, "card1 is card0's own card");
    assert_eq!(id(129), NodeDriverId::Compute, "renderD129");
    assert_eq!(
        id(2),
        NodeDriverId::Compute,
        "a cold card was offered to NVK"
    );
    assert_eq!(id(130), NodeDriverId::Compute, "renderD130");

    // `nvidia.console_gpu` attaches its RM: now it is a real second device.
    console.set_nouveau_ready(true);
    assert_eq!(id(2), NodeDriverId::Nouveau, "card2");
    assert_eq!(id(130), NodeDriverId::Nouveau, "renderD130");
    assert_eq!(
        id(1),
        NodeDriverId::Compute,
        "the alias node turned into a second nouveau device for card0's card"
    );
    assert_eq!(id(0), NodeDriverId::Nouveau, "card0 moved");
}

/// Without `nvidia.nouveau_uapi` nothing is nouveau, however capable and
/// ready the cards are: the flag is the request and this is QEMU's answer.
#[test]
fn no_node_is_nouveau_while_the_flag_is_off() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_nouveau_uapi(true),
    );
    let _compute = screen.attach_gpu(
        EmuGpu::new("emu-compute")
            .compute_at(0x65, 0x00)
            .with_nouveau_uapi(true),
    );
    build_gpu_nodes();

    assert_eq!(node_driver_id(0, false), NodeDriverId::Software, "card0");
    assert_eq!(node_driver_id(2, false), NodeDriverId::Compute, "card2");
}

/// A second card that does NOT serve the uAPI stays a compute node even
/// with the flag on and the card up: capability is not a flag's to grant.
#[test]
fn a_second_card_without_the_uapi_is_never_nouveau() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _other = screen.attach_gpu(EmuGpu::hardware_kms("emu-other").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(
        EmuGpu::new("emu-compute")
            .compute_at(0x65, 0x00)
            .with_nouveau_uapi(true),
    );
    build_gpu_nodes();

    assert_eq!(node_driver_id(2, true), NodeDriverId::Compute, "card2");
}

#[test]
fn the_first_gpu_keeps_card0_and_render128() {
    let n = NodeMinors::new(0);
    assert_eq!(n.card(), 0);
    assert_eq!(n.render(), 128);
}

#[test]
fn names_are_derived_for_any_index() {
    // The four the old `match` knew, plus the third GPU that used to come
    // out as the literal string "card?".
    assert_eq!(node_name(0), "card0");
    assert_eq!(node_name(1), "card1");
    assert_eq!(node_name(2), "card2");
    assert_eq!(node_name(128), "renderD128");
    assert_eq!(node_name(129), "renderD129");
    assert_eq!(node_name(130), "renderD130");
}

#[test]
fn a_gpu_owns_exactly_its_own_two_minors() {
    let n = NodeMinors::new(2);
    assert_eq!(n.card(), 2);
    assert_eq!(n.render(), 130);
    assert!(n.owns(2));
    assert!(n.owns(130));
    // Not its neighbours', which is the whole point of the table.
    assert!(!n.owns(1));
    assert!(!n.owns(3));
    assert!(!n.owns(129));
    assert!(!n.owns(131));
}

#[test]
fn no_two_gpus_can_claim_one_minor_below_the_cap() {
    // Every minor under the cap is claimed by at most one GPU. This is
    // what `build_gpu_nodes` stops at, so assert it rather than trusting
    // the constant to have been chosen correctly.
    for i in 0..MAX_GPU_NODES {
        for j in 0..MAX_GPU_NODES {
            if i == j {
                continue;
            }
            let a = NodeMinors::new(i);
            let b = NodeMinors::new(j);
            assert!(!b.owns(a.card()), "card{} also belongs to GPU {}", i, j);
            assert!(
                !b.owns(a.render()),
                "renderD{} also belongs to GPU {}",
                a.render(),
                j
            );
        }
    }
}

#[test]
fn the_cap_stays_inside_the_primary_minor_range() {
    // `card{n}` only stops being a primary node name at RENDER_MINOR_BASE,
    // where it would name a render node instead and two GPUs would answer
    // to one minor. The cap is deliberately well short of that, at Linux's
    // own card0..card63 range.
    assert!(MAX_GPU_NODES <= RENDER_MINOR_BASE);
    assert!(NodeMinors::new(MAX_GPU_NODES - 1).card() < RENDER_MINOR_BASE);
    // And past RENDER_MINOR_BASE is where it really breaks, which is why
    // the cap exists at all.
    assert!(NodeMinors::new(0).owns(NodeMinors::new(RENDER_MINOR_BASE).card()));
}
