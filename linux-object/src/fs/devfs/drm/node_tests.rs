use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// Leaves `GPU_NODES` empty however the test ends. A table left behind
/// holds an `Arc` to a dead emulated GPU and makes the per-node
/// `GETRESOURCES` answer ([`topology::resources_for_node`]) come out of that
/// dead card for minors a later test expects to be plain KMS cards.
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
/// is headless. **Two cards, two node pairs, nothing named twice, and the one
/// with the monitor on it is `card0`.**
///
/// This has been wrong three times. First the console card got no node at all,
/// so `vulkaninfo` reported a single card while `card0` and `card1` were two
/// views of the headless one. Then it got `card2`, and `card1` stayed a second
/// view of `card0` -- so the obvious
/// `WLR_DRM_DEVICES=/dev/dri/card0:/dev/dri/card1` named ONE card twice and
/// could never reach the second monitor. Then a pair was a card, as in Linux,
/// but `card0` was still the last-probed card: once each node answered
/// `GETRESOURCES` for its own card, `card0` had no CRTCs and labwc could not
/// open it. Now `card0` is the card the panel goes through.
#[test]
fn the_console_card_gets_a_node_of_its_own_on_a_two_card_box() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // Registration order is the reverse of the PCI probe order
    // (`insert(0)`), so the console card goes in first and the headless one
    // ends up primary -- exactly the box under test, and the reason the head
    // is `get_display_driver` and not the primary.
    let _console = screen.attach_gpu(EmuGpu::hardware_kms("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));

    let nodes = built();
    assert_eq!(
        nodes.len(),
        2,
        "two cards, two pairs: card0 for the card with the monitor and card1 for the other, got {:x?}",
        nodes
    );
    let compute = Some((0, 0x65, 0x00, 0));
    let console = Some((0, 0x01, 0x00, 0));
    assert_eq!(
        nodes[0],
        (0, console),
        "card0 must be the card the panel goes through, not the primary"
    );
    assert_eq!(
        nodes[1],
        (1, compute),
        "card1 is the OTHER card, not a second view of card0's"
    );
    assert!(
        nodes[0].1 != nodes[1].1,
        "no card may be named twice: {:x?}",
        nodes
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

/// Past the head and the compute pin, the order is the PCI address, not the
/// registration order: `card2` has to name the same card after a reboot.
///
/// Four cards, because the first two indices are decided by name and not by
/// address -- index 0 is [`get_display_driver`] and index 1 is
/// [`get_compute_driver`] -- so a box has to have two cards left over before
/// there is anything for the sort to get wrong.
#[test]
fn the_cards_past_the_head_are_sorted_by_pci_address() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // Registered low BDF first, so for the two cards that land past the pin
    // (`low` and `high`) the registry order is the reverse of the BDF order
    // and taking either one unsorted gives a different answer.
    let _low = screen.attach_gpu(EmuGpu::hardware_kms("emu-low").console_at(0x01, 0x00));
    let _high = screen.attach_gpu(EmuGpu::hardware_kms("emu-high").console_at(0x65, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x0b, 0x00));
    // Attached last, so `insert(0)` makes it the first non-compute card in the
    // registry and therefore the head.
    let _head = screen.attach_gpu(EmuGpu::hardware_kms("emu-head").console_at(0x7f, 0x00));

    let nodes = built();
    assert_eq!(nodes.len(), 4, "four cards, four pairs, got {:x?}", nodes);
    assert_eq!(
        nodes[0].1,
        Some((0, 0x7f, 0x00, 0)),
        "card0 is the display card: {:x?}",
        nodes
    );
    assert_eq!(
        nodes[1].1,
        Some((0, 0x0b, 0x00, 0)),
        "card1 is the compute pick: {:x?}",
        nodes
    );
    assert_eq!(
        (nodes[2].1, nodes[3].1),
        (Some((0, 0x01, 0x00, 0)), Some((0, 0x65, 0x00, 0))),
        "card2/card3 are not in PCI order: {:x?}",
        nodes
    );
}

/// The identity of every node of a two-card box, which is what decides how
/// many Vulkan devices NVK enumerates. The second card is nouveau once its
/// RM is up, and `eclipse-compute` while it is cold -- a node that
/// enumerated as nouveau and died on the first EXEC would be worse than no
/// node.
///
/// There is no third node any more: `card1` used to be a second view of
/// `card0`'s card, pinned to `eclipse-compute` however ready that card was,
/// and the real second card sat on `card2`. Now `card1` IS the second card,
/// so the readiness question moved from minor 2 to minor 1 and the alias
/// assertions are gone with the alias.
///
/// `card0` is the card with the monitor, so the cold one here is the headless
/// card -- the other way round from how this box boots, which is the point:
/// the readiness question belongs to the card and not to the minor.
#[test]
fn the_second_card_is_a_nouveau_device_only_once_its_rm_is_up() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_nouveau_uapi(true),
    );
    let compute = screen.attach_gpu(
        EmuGpu::new("emu-compute")
            .compute_at(0x65, 0x00)
            .with_nouveau_uapi(false),
    );
    build_gpu_nodes();
    let id = |minor| node_driver_id(minor, true);

    // The card with the monitor: one nouveau device, as today.
    assert_eq!(id(0), NodeDriverId::Nouveau, "card0");
    assert_eq!(id(128), NodeDriverId::Nouveau, "renderD128");
    assert_eq!(
        id(1),
        NodeDriverId::Compute,
        "a cold card was offered to NVK"
    );
    assert_eq!(id(129), NodeDriverId::Compute, "renderD129");
    // And there is no node past the two cards to ask about.
    assert_eq!(built().len(), 2, "two cards, two pairs");

    // Its RM attaches: now it is a real second device.
    compute.set_nouveau_ready(true);
    assert_eq!(id(1), NodeDriverId::Nouveau, "card1");
    assert_eq!(id(129), NodeDriverId::Nouveau, "renderD129");
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
    assert_eq!(node_driver_id(1, false), NodeDriverId::Compute, "card1");
}

/// A second card that does NOT serve the uAPI stays a compute node even
/// with the flag on and the card up: capability is not a flag's to grant.
#[test]
fn a_second_card_without_the_uapi_is_never_nouveau() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_nouveau_uapi(true),
    );
    // No `with_nouveau_uapi` at all: this card does not serve the uAPI.
    let _other = screen.attach_gpu(EmuGpu::new("emu-other").compute_at(0x65, 0x00));
    build_gpu_nodes();

    assert_eq!(node_driver_id(1, true), NodeDriverId::Compute, "card1");
}

/// The invariant the collapse exists for, asked of every card at once: **no
/// card is named twice.**
///
/// The two-card test above checks the shape it should have; this checks the
/// property that makes the shape right, whatever the roles and however many
/// cards. A `card1` that is a second view of `card0` is what made
/// `WLR_DRM_DEVICES=/dev/dri/card0:/dev/dri/card1` name one card twice, and
/// the failure it hides is silent: every ioctl works, the second monitor
/// simply never lights.
#[test]
fn no_card_is_ever_named_by_two_node_pairs() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // The order matters, and getting it wrong is how this test first passed
    // without testing anything. `register_driver` inserts at 0, so the card
    // attached LAST is the primary -- and the duplicate only ever appeared
    // when the primary was also `get_compute_driver`'s pick. So the single
    // headless card goes last: primary and pick are the same card, which is
    // exactly the two-RTX box.
    let _console = screen.attach_gpu(EmuGpu::hardware_kms("emu-console").console_at(0x01, 0x00));
    let _third = screen.attach_gpu(EmuGpu::hardware_kms("emu-third").console_at(0x0b, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));

    let nodes = built();
    assert_eq!(nodes.len(), 3, "three cards, three pairs: {:x?}", nodes);
    let mut seen: Vec<Option<(u32, u8, u8, u8)>> = Vec::new();
    for (index, bdf) in &nodes {
        assert!(
            !seen.contains(bdf),
            "card{} names a card an earlier node already named: {:x?}",
            index,
            nodes
        );
        seen.push(*bdf);
    }
}

/// **If any card in the machine can scan out, `card0` can.**
///
/// This is the invariant whose violation took the desktop down, and it spans
/// two modules that can each be right on their own: this file decides WHICH
/// card is `card0` and [`topology`] decides what a node ADVERTISES. Both were
/// correct and the pair was not -- `card0` was the last-probed card, the
/// topology answer for it was honestly zero CRTCs, and labwc exited with
/// "Unable to open /dev/dri/card0 as KMS device / Found 0 GPUs, cannot create
/// backend".
///
/// There is no udevd here, so nothing enumerates the other nodes and finds the
/// one with resources: the session names `card0` itself
/// (`WLR_DRM_DEVICES=/dev/dri/card0`, hardcoded in four places). That is why
/// this is an invariant here and not in Linux.
///
/// Asserted as "no node outranks node 0" rather than "node 0 has the panel",
/// so it holds whatever the shape of the box: it is false exactly when some
/// node could have been opened and the one userspace opens could not.
fn no_node_outranks_card0() {
    let nodes = gpu_nodes();
    let scans_out = |minor: u32| !resources_for_node(minor).0.is_empty();
    let someone = nodes.iter().any(|n| scans_out(n.card_minor()));
    assert_eq!(
        someone,
        scans_out(0),
        "a node can scan out and card0 cannot -- that is the desktop refusing to start: {:x?}",
        nodes
            .iter()
            .map(|n| (n.card_minor(), resources_for_node(n.card_minor())))
            .collect::<Vec<_>>()
    );
}

/// The invariant on **his box**: two cards, neither with a surface-flip
/// ladder, so the panel is the software blit through the card that is not the
/// primary.
#[test]
fn node_zero_can_scan_out_whenever_any_card_can() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(EmuGpu::new("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));
    build_gpu_nodes();
    assert!(
        software_kms_active(),
        "this is the box the desktop comes up on"
    );
    assert!(
        !Arc::ptr_eq(
            &get_primary_driver().expect("a primary"),
            &get_display_driver().expect("a display driver")
        ),
        "the primary and the display card have to differ or this proves nothing"
    );

    no_node_outranks_card0();
    assert_eq!(
        resources_for_node(0),
        (alloc::vec![SYNTH_CRTC_ID], alloc::vec![SYNTH_CONNECTOR_ID]),
        "card0 is the node the session opens, so it is the one that carries the panel"
    );
}

/// And with real hardware KMS, where the topology comes from the card rather
/// than from the GOP: the same invariant, reached down the other branch of
/// [`topology::topology_for`].
#[test]
fn node_zero_can_scan_out_with_hardware_kms_too() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));
    build_gpu_nodes();

    no_node_outranks_card0();
    assert_eq!(
        resources_for_node(0),
        (alloc::vec![2101], alloc::vec![2102]),
        "card0 answers with the ids of the card the monitor is on"
    );
}

/// Three cards, one monitor, and the card with it registered in the middle --
/// so neither "the first registered" nor "the last registered" is the right
/// answer and only asking which card drives the display gives it.
#[test]
fn node_zero_can_scan_out_with_the_display_card_in_the_middle() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _a = screen.attach_gpu(EmuGpu::new("emu-a").compute_at(0x0b, 0x00));
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _b = screen.attach_gpu(EmuGpu::new("emu-b").compute_at(0x65, 0x00));
    build_gpu_nodes();

    no_node_outranks_card0();
    assert_eq!(
        resources_for_node(0),
        (alloc::vec![2101], alloc::vec![2102]),
        "card0 is neither the first nor the last card registered"
    );
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

/// The order between the kernel's GPU policy and init(1).
///
/// `set_console_bringup_expected` has to run before userspace is spawned,
/// because `/proc/gpuready` answers `none` -- "start now" -- until it has, and
/// a boot that reads that answer starts its compositor on a card whose RM is
/// still cold. The ordering itself lives in `zCore/src/main.rs` and cannot be
/// reached from here; what can be is the predicate that notices it went wrong,
/// which is the only thing standing between the race coming back and nobody
/// finding out.
#[test]
fn a_bring_up_declared_after_userspace_asked_is_late() {
    assert!(
        super::bringup_declared_too_late(true, true),
        "a bring-up declared after /proc/gpuready was read is late: userspace has \
         already been told `none`"
    );
    assert!(
        !super::bringup_declared_too_late(true, false),
        "declared before anyone asked is the correct order"
    );
}

/// A box that schedules no bring-up answers `none` for the whole boot, so a
/// read before the declaration told userspace the truth and nothing is late.
/// Without this the check would cry wolf on every single-GPU boot.
#[test]
fn a_box_with_no_bring_up_is_never_late() {
    assert!(!super::bringup_declared_too_late(false, true));
    assert!(!super::bringup_declared_too_late(false, false));
}
