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

/// **The two-card box, end to end.** `card0` is the compute card -- card
/// registration inserts at 0, so the card attached last is the primary -- and
/// `card1` is the one with the monitor.
///
/// Before, it was the other way round and for the wrong reason: `card0`
/// reported the whole machine's topology because it was index 0, and `card1`
/// reported zeros because it was not, so the compositor opened the card that
/// cannot scan out and the kernel blitted to the other one underneath. Now the
/// node that says it is a KMS card is the one whose card can scan out.
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
        (0, 0, 0),
        "card0 is the compute card: drmIsKMS has to fail here"
    );
    assert_eq!(
        kms_counts(1),
        (1, 1, 1),
        "card1 has the monitor: drmIsKMS has to pass here"
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
