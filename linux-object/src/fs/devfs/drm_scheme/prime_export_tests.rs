//! `drm_gem_prime_handle_to_dmabuf`: the handle is looked up in the file
//! first (ENOENT), then the driver exports it, and nouveau refuses a
//! `NO_SHARE` object (EPERM). The export answered EINVAL to the first and
//! knew nothing of the second.
use super::gl_client_sequence_tests::Client;
use super::*;
use crate::error::LxError;
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
        drm::export_handle(buf.handle).is_ok(),
        "a dumb buffer of the file"
    );
    assert_eq!(
        drm::export_handle(buf.handle + 0x100).err(),
        Some(ExportRefusal::NotFound),
        "a dumb handle nobody created"
    );
    assert_eq!(
        drm::export_handle(MINE).err(),
        Some(ExportRefusal::NotFound)
    );
    let me = drm::current_pid();
    gem_mmap::register(MINE, 0x1000_0000, 2 * PAGE_SIZE as u64, me);
    gem_mmap::register(PRIVATE, 0x1002_0000, PAGE_SIZE as u64, me);
    assert!(gem_mmap::mark_no_share(PRIVATE));
    assert_eq!(
        drm::export_handle(MINE).map(|(phys, size, _)| (phys, size)),
        Ok((0x1000_0000, 2 * PAGE_SIZE))
    );
    assert_eq!(
        drm::export_handle(PRIVATE).err(),
        Some(ExportRefusal::NotShareable)
    );
    assert!(!gem_mmap::mark_no_share(UNTRACKED), "not tracked");
    assert!(!gem_mmap::no_share(UNTRACKED));
    for h in [MINE, PRIVATE] {
        assert!(gem_mmap::unregister(h));
    }
    assert_eq!(
        drm::export_handle(PRIVATE).err(),
        Some(ExportRefusal::NotFound)
    );
    assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
    assert_eq!(
        drm::export_handle(buf.handle).err(),
        Some(ExportRefusal::NotFound)
    );
}

/// The errno each refusal becomes at the ioctl.
#[test]
fn the_refusals_are_enoent_and_eperm() {
    assert_eq!(LxError::from(ExportRefusal::NotFound), LxError::ENOENT);
    assert_eq!(LxError::from(ExportRefusal::NotShareable), LxError::EPERM);
}
