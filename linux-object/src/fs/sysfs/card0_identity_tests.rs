//! `/sys/class/drm/card0` and `/dev/dri/card0` have to name the same card.
//!
//! libdrm reads the sysfs side to identify a node -- the `device` symlink, the
//! bus id, `vendor`, `config` -- and NVK filters candidate devices on the PCI
//! vendor it finds there before it opens anything. Every `GETPARAM` and `NVIF`
//! answer comes from the driver side. So a node whose two sides name different
//! cards reports one card's PCI id with another card's VRAM and topology, and
//! Mesa does not catch it: it asserts the two device ids agree, but Alpine
//! builds with `-Db_ndebug=true`, so the assert is compiled out.
//!
//! This was one card until 26-oct, when `card0` became the card the panel goes
//! through rather than the first driver in the registry
//! ([`crate::fs::devfs::drm::build_gpu_nodes`]), and [`display_pci_index`] was
//! still asking for the primary. Two cards, one of them silently wrong.

use super::*;
use crate::fs::devfs::drm;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use alloc::vec;

/// Leaves the node table empty however the test ends: one left behind decides
/// what later tests' nodes resolve to. Declared AFTER the `Screen` so it drops
/// BEFORE it, while the DRM test lock is still held.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        drm::clear_gpu_nodes_for_test();
    }
}

/// `a_real_machine`'s two RTX cards, by sysfs index and BDF.
const MONITOR_CARD: (usize, &str) = (2, "0000:02:00.0");
const HEADLESS_CARD: (usize, &str) = (4, "0000:03:00.0");

/// **The two-card box: `card0`'s sysfs identity is the card with the monitor.**
///
/// Not the primary driver, which on this box is the other card -- card
/// registration inserts at 0, so the card attached LAST is `drivers.first()`.
/// That is the whole failure mode: both sides were "the primary" and agreed by
/// accident until one of them moved.
#[test]
fn sysfs_card0_names_the_card_that_serves_card0() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _bus = test_bus::with(test_bus::a_real_machine());
    let _monitor = screen.attach_gpu(EmuGpu::hardware_kms("emu-monitor").console_at(0x02, 0x00));
    let _headless = screen.attach_gpu(EmuGpu::new("emu-headless").compute_at(0x03, 0x00));
    drm::build_gpu_nodes();

    let serving = drm::driver_for_minor(0).expect("some driver serves card0");
    assert_eq!(
        serving.pci_bdf(),
        Some((0, 0x02, 0x00, 0)),
        "the fixture is wrong if card0 is not the card with the monitor"
    );
    assert_eq!(
        drm_card0_pci_index(),
        Some(MONITOR_CARD.0),
        "card0's sysfs identity is not the card serving card0's ioctls"
    );
    assert_ne!(
        drm_card0_pci_index(),
        Some(HEADLESS_CARD.0),
        "card0's sysfs identity followed the primary driver instead of the node"
    );
    // What `GET_UNIQUE` hands Xorg's platform bus, and what libdrm compares.
    assert_eq!(
        drm_node_busid(0).as_deref(),
        Some(alloc::format!("pci:{}", MONITOR_CARD.1).as_str())
    );
    assert_eq!(
        drm_node_busid(1).as_deref(),
        Some(alloc::format!("pci:{}", HEADLESS_CARD.1).as_str()),
        "card1 is the other card and keeps its own BDF"
    );
    // Two real cards never share a PCI device, so the compute alias -- which
    // exists only for a node that WOULD share card0's -- stays out of it.
    assert!(!compute_alias_needed());
}

/// Stated as the invariant rather than as a fixture: whatever the box, the two
/// sides of `card0` resolve to the same PCI device. This is what fails when
/// one of them is changed and the other is not.
#[test]
fn the_two_sides_of_card0_resolve_to_one_pci_device() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _bus = test_bus::with(test_bus::a_real_machine());
    let _monitor = screen.attach_gpu(EmuGpu::hardware_kms("emu-monitor").console_at(0x02, 0x00));
    let _headless = screen.attach_gpu(EmuGpu::new("emu-headless").compute_at(0x03, 0x00));
    drm::build_gpu_nodes();

    let from_the_driver = drm::driver_for_minor(0)
        .and_then(|d| d.pci_bdf())
        .map(|(_, b, d, f)| alloc::format!("0000:{:02x}:{:02x}.{:x}", b, d, f));
    let from_sysfs = drm_card0_pci_index().and_then(pci_bdf_name);
    assert_eq!(
        from_sysfs, from_the_driver,
        "the node's sysfs identity and the GPU serving its ioctls name different cards"
    );
}

/// With no node table -- a UEFI GOP framebuffer, which is the ordinary QEMU
/// box -- every minor is the primary's, and sysfs has to say the same. This is
/// the path that comes up today, so it must answer exactly as it did.
#[test]
fn with_no_node_table_card0_is_the_primary_as_before() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _bus = test_bus::with(test_bus::a_real_machine());
    let _monitor = screen.attach_gpu(EmuGpu::hardware_kms("emu-monitor").console_at(0x02, 0x00));
    let _headless = screen.attach_gpu(EmuGpu::new("emu-headless").compute_at(0x03, 0x00));
    drm::clear_gpu_nodes_for_test();

    assert!(drm::driver_for_minor(0).is_none(), "no table, no node");
    assert_eq!(
        drm_card0_pci_index(),
        Some(HEADLESS_CARD.0),
        "without a table the pre-table rule stands: card0 is the primary driver"
    );
}

/// A card the sysfs PCI scan does not list at all falls back to the
/// display-class scan instead of answering nothing: a `card0` with no sysfs
/// identity dangles the `/sys/class/drm/card0` symlink, `drmGetDevices2`
/// identifies no device, and the compositor finds no card to open.
#[test]
fn a_card_the_bus_does_not_list_falls_back_instead_of_vanishing() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    // The bus has the two RTX cards; the registered card is at neither BDF.
    let _bus = test_bus::with(test_bus::a_real_machine());
    let _odd = screen.attach_gpu(EmuGpu::hardware_kms("emu-odd").console_at(0x7f, 0x00));
    drm::build_gpu_nodes();

    assert_eq!(
        drm_card0_pci_index(),
        Some(MONITOR_CARD.0),
        "the display-class scan is the fallback, and it finds the first NVIDIA GPU"
    );
}

/// And with no PCI bus at all, `card0` still has an identity as long as
/// something is registered, because `/dev/dri/card0` exists on that condition.
#[test]
fn an_empty_bus_still_leaves_card0_with_an_index() {
    let screen = kms_emu::attach(64, 16);
    let _table = NodeTable;
    let _bus = test_bus::with(vec![]);
    let _monitor = screen.attach_gpu(EmuGpu::hardware_kms("emu-monitor").console_at(0x02, 0x00));
    drm::build_gpu_nodes();

    assert_eq!(drm_card0_pci_index(), Some(0));
}
