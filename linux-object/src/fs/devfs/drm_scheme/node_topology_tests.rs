//! `GETRESOURCES` through the ioctl, on each node of a two-card box.
//!
//! The decision and its resolution are tested next to themselves, in
//! `drm::topology`. What is tested here is the arm: that this entry point
//! asks for the node it arrived on, that the counts it reports are that
//! node's, and that `drmIsKMS` therefore passes on the card with the monitor
//! and fails on the one without a scanout.
//!
//! This arm had **no multi-card test at all** before. The index-based special
//! case it used to start with -- zeros for every node past 0 -- was the thing
//! keeping a compositor on `card0`, and every existing GETRESOURCES test opens
//! minor 0 on a one-card host, where that case never fired. Deleting it broke
//! nothing in the suite, which is not the same as it having been covered.

use super::gl_client_sequence_tests::{blank_card_res, Client};
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// Leaves the node table empty however the test ends: one left behind holds an
/// `Arc` to a dead emulated GPU and decides what later tests' nodes answer.
/// Declared AFTER the `Screen` so it drops BEFORE it, while the DRM test lock
/// is still held.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        drm::clear_gpu_nodes_for_test();
    }
}

/// The counts `drmIsKMS` reads, as one open node reports them.
fn kms_counts(minor: u32) -> (u32, u32, u32) {
    let client = Client::open(minor);
    let mut res = blank_card_res();
    client
        .ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut res)
        .expect("GETRESOURCES");
    (res.count_crtcs, res.count_connectors, res.count_encoders)
}

/// **The two-card box, end to end.** `card0` is the card with the monitor and
/// `card1` is the headless one, and each reports its own card.
///
/// It used to be the other way round and for the wrong reason: `card0`
/// reported the whole machine's topology because it was index 0 and `card1`
/// reported zeros because it was not, so the compositor opened the card that
/// cannot scan out and the kernel blitted to the other one underneath. Making
/// each node answer for its own card fixed the answer and broke the desktop,
/// because `card0` was still the last-probed card: the node the session opens
/// then honestly reported no CRTCs and labwc exited. Both halves are here now
/// -- the node that says it is a KMS card is the one whose card can scan out,
/// and that node is the one userspace opens.
#[test]
fn getresources_answers_for_the_node_it_arrived_on() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));
    drm::build_gpu_nodes();

    assert_eq!(
        kms_counts(0),
        (1, 1, 1),
        "card0 has the monitor: drmIsKMS has to pass here"
    );
    assert_eq!(
        kms_counts(1),
        (0, 0, 0),
        "card1 cannot scan out: drmIsKMS has to fail here"
    );
}

/// Two cards that can both scan out get one output each, and the ids are the
/// card's own. With one list per machine the de-duplication in the merged walk
/// collapsed two cards into one topology; a node that answers for itself does
/// not have the question.
#[test]
fn two_cards_that_can_both_scan_out_each_report_their_own_ids() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _second = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-second")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _first = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-first")
            .console_at(0x65, 0x00)
            .with_topology(&[2001, 2003], &[2002]),
    );
    drm::build_gpu_nodes();

    assert_eq!(kms_counts(0), (2, 1, 1), "card0 lists its own two CRTCs");
    assert_eq!(kms_counts(1), (1, 1, 1), "card1 lists its own one");
}

/// Two cards that can both scan out, with ids that tell them apart: `card0`
/// has CRTC 2001 / connector 2002 / plane 2003, `card1` the 21xx ones.
fn two_scanout_cards(screen: &kms_emu::Screen) -> (kms_emu::Gpu, kms_emu::Gpu) {
    let second = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-second")
            .console_at(0x01, 0x00)
            .with_ids(2101, 2102, 2103),
    );
    let first = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-first")
            .console_at(0x65, 0x00)
            .with_ids(2001, 2002, 2003),
    );
    drm::build_gpu_nodes();
    (first, second)
}

/// **Properties, atomic objects and leases look in the node's own card**, as
/// `drm_mode_object_find` looks in one device: `card1` does not find
/// `card0`'s CRTC, connector or plane, and finds its own.
#[test]
fn object_lookups_behind_properties_atomic_and_leases_are_per_node() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _cards = two_scanout_cards(&screen);

    for id in [2001, 2002, 2003] {
        assert!(
            mode_object_kind(0, id).is_some(),
            "card0 lost its own {}",
            id
        );
        assert!(
            mode_object_kind(1, id).is_none(),
            "card1 found card0's {}",
            id
        );
        assert!(
            atomic_object(1, id).is_none(),
            "card1 staged card0's {}",
            id
        );
        assert!(find_mode_object(1, id, DRM_MODE_OBJECT_ANY, true).is_none());
    }
    assert_eq!(mode_object_kind(1, 2101), Some(DRM_MODE_OBJECT_CRTC));
    assert_eq!(mode_object_kind(1, 2102), Some(DRM_MODE_OBJECT_CONNECTOR));
    assert_eq!(mode_object_kind(1, 2103), Some(DRM_MODE_OBJECT_PLANE));

    assert!(
        matches!(
            lease_objects(1, &[2001, 2002], false),
            Err(FsError::EntryNotFound)
        ),
        "card1 leased card0's CRTC and connector"
    );
    let leased = lease_objects(1, &[2101, 2102], false).expect("card1 leases its own pipe");
    assert!(
        leased.contains(&2103),
        "the CRTC's primary plane comes along"
    );
}

/// `WAIT_VBLANK` on a secondary pipe is measured against the node's own
/// CRTCs: `card0` has two, `card1` one.
#[test]
fn a_secondary_vblank_pipe_is_counted_on_the_nodes_own_card() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _second = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-second")
            .console_at(0x01, 0x00)
            .with_topology(&[2101], &[2102]),
    );
    let _first = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-first")
            .console_at(0x65, 0x00)
            .with_topology(&[2001, 2003], &[2002]),
    );
    drm::build_gpu_nodes();

    let secondary = 0x1 | _DRM_VBLANK_SECONDARY; // _DRM_VBLANK_RELATIVE
    assert!(wait_vblank_check(0, secondary).is_ok());
    assert!(
        wait_vblank_check(1, secondary).is_err(),
        "card1 has one pipe; pipe 1 was counted against the whole machine"
    );
}

/// `SETCRTC` with `fb_id` on `crtc`/`connector` of the node `c` has open.
fn setcrtc(c: &Client, crtc: u32, connector: u32, fb_id: u32) -> Result<usize> {
    let connectors = [connector];
    let mut req = DrmModeGetCrtc {
        set_connectors_ptr: connectors.as_ptr() as u64,
        count_connectors: 1,
        crtc_id: crtc,
        fb_id,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 1,
        mode: make_modeinfo(64, 16),
    };
    c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
}

/// The framebuffer `GETCRTC` on `crtc` reports through `c`.
fn getcrtc_fb(c: &Client, crtc: u32) -> u32 {
    let mut res = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: crtc,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut res).expect("GETCRTC");
    res.fb_id
}

/// **Each card's CRTC has its own framebuffer**, as each `drm_device` has its
/// own `crtc->primary->state->fb`. A frame set and flipped on `card1` is what
/// `card1`'s `GETCRTC` reads back, and it is not on `card0`'s CRTC, not on the
/// panel, and its `RMFB` does not turn the panel off. With one framebuffer for
/// the machine, a client on the second card rebound `card0`'s CRTC and blitted
/// over its desktop.
#[test]
fn a_frame_on_card1_stays_on_card1() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _cards = two_scanout_cards(&screen);
    let c1 = Client::open(1);
    let c0 = Client::open(0);

    let buf = c1.create_dumb(64, 16);
    let fb = c1.addfb2(&buf);
    assert_eq!(setcrtc(&c1, 2101, 2102, fb), Ok(0), "SETCRTC on card1");
    assert_eq!(getcrtc_fb(&c1, 2101), fb, "card1 reads its own frame back");
    assert_ne!(getcrtc_fb(&c0, 2001), fb, "card0's CRTC took card1's frame");
    assert_eq!(drm::crtc_fb_on(drm::PANEL_CARD), 0);
    assert_ne!(
        drm::panel_fb_for_test(),
        fb,
        "card1's frame was blitted onto the panel"
    );

    let back = c1.addfb2(&c1.create_dumb(64, 16));
    c1.page_flip(2101, back, 7).expect("PAGE_FLIP on card1");
    assert_eq!(getcrtc_fb(&c1, 2101), back);
    assert_ne!(drm::panel_fb_for_test(), back, "and so was its flip");

    let blanked = drm::crtc_blanked();
    c1.rmfb(back).expect("RMFB");
    assert_eq!(
        getcrtc_fb(&c1, 2101),
        0,
        "RMFB of card1's scanout unbinds it"
    );
    assert_eq!(drm::crtc_blanked(), blanked, "and leaves the panel alone");
}

/// The other way round: the panel's frame is not what `card1` reports.
#[test]
fn a_frame_on_card0_is_not_card1s() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _cards = two_scanout_cards(&screen);
    let c0 = Client::open(0);
    let c1 = Client::open(1);

    let fb = c0.addfb2(&c0.create_dumb(64, 16));
    assert_eq!(setcrtc(&c0, 2001, 2002, fb), Ok(0), "SETCRTC on card0");
    assert_eq!(getcrtc_fb(&c0, 2001), fb);
    assert_eq!(getcrtc_fb(&c1, 2101), 0, "card1 reported card0's frame");
}
