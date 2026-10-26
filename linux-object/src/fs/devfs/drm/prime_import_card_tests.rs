//! Which card a `PRIME_FD_TO_HANDLE` resolves against.
//!
//! The reverse lookup is by physical address, with no card in the key, so it
//! identified the MEMORY correctly and then answered with the handle of
//! whichever card happened to own the object -- through any node. That is
//! the hole under one labwc desktop over
//! `WLR_DRM_DEVICES=/dev/dri/card0:/dev/dri/card1`: the compositor imports
//! the composed frame on each secondary card, and it was handed an id out of
//! the primary card's slice.
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use zcore_drivers::scheme::gem_mmap;
use zircon_object::vm::PAGE_SIZE;

/// Nouveau-range handles no GPU slice in these tests hands out.
const ON_CARD0: u32 = 0xbfff_0040;
const ON_CARD1: u32 = 0xbfff_0041;
const NOBODYS: u32 = 0xbfff_0042;
const PHYS_ON_CARD0: u64 = 0x2000_0000;
const PHYS_ON_CARD1: u64 = 0x2100_0000;
const PHYS_DUMB: u64 = 0x2200_0000;

/// Drops the node table however the test ends.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        clear_gpu_nodes_for_test();
    }
}

/// A two-card box with one sysmem object on each, and the two cards' minors.
/// The objects are in `gem_mmap` too, because that is what the reverse lookup
/// reads: a card knowing an object and the object having a physical address
/// are two separate records, and both are needed for an import.
///
/// Every piece comes back so it lives as long as the test and is dropped
/// with it: a `Gpu` left behind stays in `DRM_STATE.drivers` for the rest of
/// the binary (see `unregister_driver`), which would put every later test of
/// the crate on the hardware-KMS path.
struct TwoCards {
    _screen: kms_emu::Screen,
    _table: NodeTable,
    _console: kms_emu::Gpu,
    _compute: kms_emu::Gpu,
    card0: u32,
    card1: u32,
}

fn two_cards() -> TwoCards {
    let screen = kms_emu::attach(16, 16);
    let table = NodeTable;
    let console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_sysmem_object(ON_CARD0),
    );
    let compute = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-compute")
            .compute_at(0x65, 0x00)
            .with_sysmem_object(ON_CARD1),
    );
    build_gpu_nodes();
    let card_of = |gpu: &kms_emu::Gpu| {
        gpu_nodes()
            .iter()
            .find(|n| gpu.is_driver(&n.driver))
            .map(|n| n.card_minor())
            .expect("a node of its own")
    };
    let (card0, card1) = (card_of(&console), card_of(&compute));
    assert_ne!(card0, card1);
    gem_mmap::register(ON_CARD0, PHYS_ON_CARD0, PAGE_SIZE as u64, 0);
    gem_mmap::register(ON_CARD1, PHYS_ON_CARD1, PAGE_SIZE as u64, 0);
    TwoCards {
        _screen: screen,
        _table: table,
        _console: console,
        _compute: compute,
        card0,
        card1,
    }
}

fn forget_objects() {
    for h in [ON_CARD0, ON_CARD1] {
        gem_mmap::unregister(h);
    }
}

/// A handle names an object of ONE device's driver
/// (`drm_gem_object_lookup(file_priv, handle)`), so an import through one
/// card's node never answers with another card's id. Both directions,
/// because a two-card box has a primary that is NOT the console card: the
/// bug is symmetric and so is the fix.
#[test]
fn an_import_resolves_against_the_card_of_the_node_it_arrives_on() {
    // No `test_globals::lock()` here: `kms_emu::attach` takes it (it owns
    // the `Screen` guard), and it is a plain `std::sync::Mutex`, so taking
    // it twice on one thread deadlocks the suite.
    let box2 = two_cards();
    let (card0, card1) = (box2.card0, box2.card1);

    assert_eq!(
        prime_import_for(PHYS_ON_CARD0, card0),
        PrimeImport::SameCard(ON_CARD0),
        "its own card: the original handle, which is what makes the desktop work"
    );
    assert_eq!(
        prime_import_for(PHYS_ON_CARD0, card1),
        PrimeImport::OtherCard(ON_CARD0),
        "through the other card's node: NOT that card's handle"
    );
    assert_eq!(
        prime_import_for(PHYS_ON_CARD1, card1),
        PrimeImport::SameCard(ON_CARD1)
    );
    assert_eq!(
        prime_import_for(PHYS_ON_CARD1, card0),
        PrimeImport::OtherCard(ON_CARD1)
    );
    forget_objects();
}

/// A physical address no driver-private object has is not a cross-card
/// import: it is a dumb buffer, or an object freed under the dma-buf. The
/// loud "SELF-IMPORT MISS" line is for that case and only that case.
#[test]
fn an_address_no_card_tracks_is_untracked_not_another_cards() {
    // No `test_globals::lock()` here: `kms_emu::attach` takes it (it owns
    // the `Screen` guard), and it is a plain `std::sync::Mutex`, so taking
    // it twice on one thread deadlocks the suite.
    let box2 = two_cards();
    let (card0, card1) = (box2.card0, box2.card1);

    for card in [card0, card1] {
        assert_eq!(prime_import_for(PHYS_DUMB, card), PrimeImport::Untracked);
    }
    // An object in `gem_mmap` that NO card claims: the memory resolves, so
    // it is not untracked -- it is nobody's handle to hand out through a
    // node, which is the same refusal as another card's.
    gem_mmap::register(NOBODYS, PHYS_DUMB, PAGE_SIZE as u64, 0);
    for card in [card0, card1] {
        assert_eq!(
            prime_import_for(PHYS_DUMB, card),
            PrimeImport::OtherCard(NOBODYS),
            "the memory is tracked but no card owns it"
        );
    }
    assert!(gem_mmap::unregister(NOBODYS));
    forget_objects();
}

/// The owner question is NOT the card question. Linux imports a dma-buf for
/// whoever holds the fd (`dma_buf_get`, then the device's own importer); it
/// never asks whether the importer created the object. Xwayland importing a
/// client's buffer on the same card still resolves to the original handle,
/// which is the chain `prime_import_ref_tests` covers -- so the card is
/// asked as the kernel.
#[test]
fn whose_object_it_is_does_not_decide_which_card_has_it() {
    // The lock comes with the `Screen`; see `two_cards`.
    let screen = kms_emu::attach(16, 16);
    let _table = NodeTable;
    const THEIRS: u32 = 0xbfff_0043;
    const CLIENT: u64 = 77_001;
    let gpu = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_vram_object_of(THEIRS, CLIENT)
            .with_sysmem_object(ON_CARD0),
    );
    build_gpu_nodes();
    let card = gpu_nodes()
        .iter()
        .find(|n| gpu.is_driver(&n.driver))
        .map(|n| n.card_minor())
        .expect("a node of its own");
    gem_mmap::register(THEIRS, PHYS_ON_CARD0, PAGE_SIZE as u64, CLIENT);
    assert_eq!(
        prime_import_for(PHYS_ON_CARD0, card),
        PrimeImport::SameCard(THEIRS),
        "another process's object on this card is still this card's"
    );
    // And what the other pid would have answered, which is the whole reason
    // the question is the kernel's: asked as a stranger, this card does not
    // admit to having the object at all, and the import would mint a second
    // handle over the same memory on the card that already has it.
    assert!(super::prime_import::card_knows_gem_as_for_test(
        card, THEIRS, 0
    ));
    assert!(!super::prime_import::card_knows_gem_as_for_test(
        card,
        THEIRS,
        CLIENT + 1
    ));
    assert!(super::prime_import::card_knows_gem_as_for_test(
        card, THEIRS, CLIENT
    ));
    assert!(gem_mmap::unregister(THEIRS));
}

/// With no node table every minor is the primary's -- the rule the whole DRM
/// core keeps -- so a single-card box answers `SameCard` for everything it
/// tracks, exactly as the phys-only lookup did. This is the test that fails
/// if the new card check ever becomes a gate the old path did not have.
#[test]
fn a_box_with_no_node_table_resolves_every_minor_as_it_always_did() {
    let _serialised = super::test_globals::lock();
    clear_gpu_nodes_for_test();
    gem_mmap::register(ON_CARD0, PHYS_ON_CARD0, PAGE_SIZE as u64, 0);
    for minor in [0, 1, 2, RENDER_MINOR_BASE, RENDER_MINOR_BASE + 1] {
        assert_eq!(
            prime_import_for(PHYS_ON_CARD0, minor),
            PrimeImport::SameCard(ON_CARD0)
        );
        assert_eq!(prime_import_for(PHYS_DUMB, minor), PrimeImport::Untracked);
    }
    assert!(gem_mmap::unregister(ON_CARD0));
}

/// The two indices the cross-card line names, so it says card0 and card1 and
/// not two question marks.
#[test]
fn the_cross_card_line_can_name_both_cards() {
    // No `test_globals::lock()` here: `kms_emu::attach` takes it (it owns
    // the `Screen` guard), and it is a plain `std::sync::Mutex`, so taking
    // it twice on one thread deadlocks the suite.
    let box2 = two_cards();
    let (card0, card1) = (box2.card0, box2.card1);

    assert_eq!(card_index_holding_gem(ON_CARD0), card_index_of_minor(card0));
    assert_eq!(card_index_holding_gem(ON_CARD1), card_index_of_minor(card1));
    assert_ne!(card_index_of_minor(card0), card_index_of_minor(card1));
    assert_eq!(
        card_index_holding_gem(NOBODYS),
        None,
        "no card claims it, so the line says so rather than guessing one"
    );
    forget_objects();
}
