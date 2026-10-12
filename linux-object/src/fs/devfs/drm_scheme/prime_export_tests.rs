//! `drm_gem_prime_handle_to_dmabuf`: the handle is looked up in the file
//! first (ENOENT), then the driver exports it, and nouveau refuses a
//! `NO_SHARE` object (EPERM). The export answered EINVAL to the first and
//! knew nothing of the second.
use super::gl_client_sequence_tests::Client;
use super::*;
use crate::error::LxError;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use drm::ExportRefusal;
use zcore_drivers::scheme::gem_mmap;
use zircon_object::vm::PAGE_SIZE;

/// Nouveau-range handles no GPU slice hands out in these tests. Another
/// process's object cannot be seen from here: the tests run as pid 0,
/// which holds every handle (`gem_mmap::holds`); `lookup_for` has its
/// own tests for that side.
const MINE: u32 = 0xbfff_0010;
const PRIVATE: u32 = 0xbfff_0011;
const UNTRACKED: u32 = 0xbfff_0012;

#[test]
fn an_export_is_enoent_for_a_handle_the_file_does_not_hold_and_eperm_for_a_no_share_one() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    let buf = c.create_dumb(16, 16);
    assert!(
        drm::export_handle(buf.handle, 0).is_ok(),
        "a dumb buffer of the file"
    );
    assert_eq!(
        drm::export_handle(buf.handle + 0x100, 0).err(),
        Some(ExportRefusal::NotFound),
        "a dumb handle nobody created"
    );
    assert_eq!(
        drm::export_handle(MINE, 0).err(),
        Some(ExportRefusal::NotFound)
    );
    let me = drm::current_pid();
    gem_mmap::register(MINE, 0x1000_0000, 2 * PAGE_SIZE as u64, me);
    gem_mmap::register(PRIVATE, 0x1002_0000, PAGE_SIZE as u64, me);
    gem_mmap::mark_no_share(PRIVATE, me);
    assert_eq!(
        drm::export_handle(MINE, 0).map(|(phys, size, _)| (phys, size)),
        Ok((0x1000_0000, 2 * PAGE_SIZE))
    );
    assert_eq!(
        drm::export_handle(PRIVATE, 0).err(),
        Some(ExportRefusal::NotShareable)
    );
    assert!(!gem_mmap::no_share(UNTRACKED, me));
    for h in [MINE, PRIVATE] {
        assert!(gem_mmap::unregister(h));
    }
    assert_eq!(
        drm::export_handle(PRIVATE, 0).err(),
        Some(ExportRefusal::NotShareable),
        "the record is the object's, not the mapping's"
    );
    assert!(gem_mmap::clear_no_share(PRIVATE));
    assert_eq!(
        drm::export_handle(PRIVATE, 0).err(),
        Some(ExportRefusal::NotFound)
    );
    assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
    assert_eq!(
        drm::export_handle(buf.handle, 0).err(),
        Some(ExportRefusal::NotFound)
    );
}

/// A VRAM-only object has no host address, so no mapping the export could
/// build a dma-buf over, and the lookup alone called it ENOENT; it exists,
/// and the driver says so (`gem_aperture`). It is refused on its own terms
/// (EINVAL, what every export got before), and a NO_SHARE one is EPERM like
/// any other, mapping or not. Nothing of the kind is an object of nobody's.
#[test]
fn a_vram_object_exists_and_is_refused_for_its_own_reason() {
    const VRAM: u32 = 0xbfff_0020;
    const VRAM_PRIVATE: u32 = 0xbfff_0021;
    const NOBODYS: u32 = 0xbfff_0022;
    let screen = kms_emu::attach(16, 16);
    let _gpu = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-gpu")
            .with_vram_object(VRAM)
            .with_vram_object(VRAM_PRIVATE),
    );
    let me = drm::current_pid();
    gem_mmap::mark_no_share(VRAM_PRIVATE, me);
    assert_eq!(
        drm::export_handle(VRAM, 0).err(),
        Some(ExportRefusal::NoHostAddress),
        "exists, no host address"
    );
    assert_eq!(
        drm::export_handle(VRAM_PRIVATE, 0).err(),
        Some(ExportRefusal::NotShareable),
        "NO_SHARE without a mapping"
    );
    assert_eq!(
        drm::export_handle(NOBODYS, 0).err(),
        Some(ExportRefusal::NotFound),
        "no object"
    );
    assert!(gem_mmap::clear_no_share(VRAM_PRIVATE));
    assert_eq!(
        drm::export_handle(VRAM_PRIVATE, 0).err(),
        Some(ExportRefusal::NoHostAddress)
    );
}

/// `drm_gem_object_lookup(file_priv, handle)`: a handle names an object of
/// ONE file on ONE card. The VRAM fallback asked the primary card, by handle
/// alone, so another process exporting someone's VRAM handle was told EINVAL
/// (the object exists) where it must be ENOENT, and an object that only the
/// second card has was ENOENT through that card's own node.
#[test]
fn a_vram_object_is_its_owners_and_its_cards() {
    const THEIRS: u32 = 0xbfff_0030;
    const ON_THE_OTHER_CARD: u32 = 0xbfff_0031;
    struct NodeTable;
    impl Drop for NodeTable {
        fn drop(&mut self) {
            drm::clear_gpu_nodes_for_test();
        }
    }
    let screen = kms_emu::attach(16, 16);
    let _table = NodeTable;
    let console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_vram_object_of(THEIRS, 4242),
    );
    let compute = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-compute")
            .compute_at(0x65, 0x00)
            .with_vram_object(ON_THE_OTHER_CARD),
    );
    drm::build_gpu_nodes();
    let card_of = |gpu: &kms_emu::Gpu| {
        drm::gpu_nodes()
            .iter()
            .find(|n| gpu.is_driver(&n.driver))
            .map(|n| n.card_minor())
            .expect("a node of its own")
    };
    let (card_console, card_compute) = (card_of(&console), card_of(&compute));
    assert_ne!(card_console, card_compute);

    assert_eq!(
        drm::export_handle_for(THEIRS, card_console, 4242).err(),
        Some(ExportRefusal::NoHostAddress),
        "its owner: exists, no host address"
    );
    assert_eq!(
        drm::export_handle_for(THEIRS, card_console, 7).err(),
        Some(ExportRefusal::NotFound),
        "another process: not a handle of its file"
    );
    assert_eq!(
        drm::export_handle_for(ON_THE_OTHER_CARD, card_compute, 0).err(),
        Some(ExportRefusal::NoHostAddress),
        "through its own card's node"
    );
    assert_eq!(
        drm::export_handle_for(ON_THE_OTHER_CARD, card_console, 0).err(),
        Some(ExportRefusal::NotFound),
        "the other card has no such object"
    );
    // NO_SHARE does not make the object the other card's: the lookup in the
    // file comes first and says ENOENT there, EPERM only through its own
    // card's node (the record used to be asked before the node, so the
    // handle was EPERM through any node).
    gem_mmap::mark_no_share(THEIRS, 4242);
    assert_eq!(
        drm::export_handle_for(THEIRS, card_console, 4242).err(),
        Some(ExportRefusal::NotShareable),
        "its owner, through its card: the driver's refusal"
    );
    assert_eq!(
        drm::export_handle_for(THEIRS, card_compute, 4242).err(),
        Some(ExportRefusal::NotFound),
        "its owner, through the other card: not a handle of that file"
    );
    assert_eq!(
        drm::export_handle_for(THEIRS, card_console, 7).err(),
        Some(ExportRefusal::NotFound),
        "another process: still not its handle"
    );
    assert!(gem_mmap::clear_no_share(THEIRS));
}

/// The errno each refusal becomes at the ioctl.
#[test]
fn the_refusals_are_enoent_eperm_and_einval() {
    assert_eq!(LxError::from(ExportRefusal::NotFound), LxError::ENOENT);
    assert_eq!(LxError::from(ExportRefusal::NotShareable), LxError::EPERM);
    assert_eq!(LxError::from(ExportRefusal::NoHostAddress), LxError::EINVAL);
}
