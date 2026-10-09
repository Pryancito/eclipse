//! `MAP_DUMB` names a handle the file holds, and the `mmap` that follows
//! is no longer than the object -- `drm_gem_dumb_map_offset` and
//! `drm_gem_mmap_obj`.
use super::gl_client_sequence_tests::Client;
use super::*;
use zcore_drivers::scheme::gem_mmap;
use zircon_object::vm::PAGE_SIZE;

/// A nouveau-range handle no GPU slice hands out in these tests.
const H: u32 = 0xbfff_0008;

#[test]
fn map_dumb_is_enoent_for_a_handle_the_file_does_not_hold() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    let buf = c.create_dumb(16, 16);
    assert_eq!(c.map_dumb(buf.handle), Ok(mmap_cookie_for(buf.handle)));
    assert_eq!(
        c.map_dumb(buf.handle + 0x100),
        Err(FsError::EntryNotFound),
        "a dumb handle nobody created"
    );
    assert_eq!(
        c.map_dumb(H),
        Err(FsError::EntryNotFound),
        "a nouveau handle nobody registered"
    );
    // A nouveau GEM the process holds maps through the same ioctl, as
    // `drm_gem_object_lookup` finds any GEM object of the file.
    gem_mmap::register(H, 0x1000_0000, 2 * PAGE_SIZE as u64, drm::current_pid());
    assert_eq!(c.map_dumb(H), Ok(mmap_cookie_for(H)));
    assert!(gem_mmap::unregister(H));
    assert_eq!(c.map_dumb(H), Err(FsError::EntryNotFound), "gone again");
    assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
    assert_eq!(
        c.map_dumb(buf.handle),
        Err(FsError::EntryNotFound),
        "a destroyed dumb handle"
    );
}

#[test]
fn an_mmap_longer_than_the_object_is_einval_and_one_that_fits_is_the_object() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    // 64 x 64 x 4 = 16 KiB: four whole pages.
    let buf = c.create_dumb(64, 64);
    assert_eq!(buf.size, 4 * PAGE_SIZE as u64);
    let off = c.map_dumb(buf.handle).unwrap();
    for len in [1usize, PAGE_SIZE, 3 * PAGE_SIZE + 1, 4 * PAGE_SIZE] {
        let vmo = c
            .mmap(off, len)
            .unwrap_or_else(|e| panic!("len {}: {:?}", len, e));
        assert_eq!(vmo.len(), 4 * PAGE_SIZE, "len {}", len);
    }
    for len in [4 * PAGE_SIZE + 1, 5 * PAGE_SIZE, 1 << 30] {
        assert_eq!(
            c.mmap(off, len).err(),
            Some(FsError::InvalidParam),
            "len {}",
            len
        );
    }
    // A 16 x 16 buffer is 1 KiB in a page of its own: the page is the
    // object's, the next one is not.
    let small = c.create_dumb(16, 16);
    let off = c.map_dumb(small.handle).unwrap();
    assert!(c.mmap(off, PAGE_SIZE).is_ok());
    assert_eq!(
        c.mmap(off, PAGE_SIZE + 1).err(),
        Some(FsError::InvalidParam)
    );
    assert_eq!(c.destroy_dumb(small.handle), Ok(0));
    assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
}

#[test]
fn the_nouveau_side_measures_the_object_the_same_way() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    // Two pages and a byte: three pages of object.
    gem_mmap::register(H, 0x1000_0000, 2 * PAGE_SIZE as u64 + 1, drm::current_pid());
    let off = c.map_dumb(H).unwrap();
    assert_eq!(
        c.mmap(off, 3 * PAGE_SIZE + 1).err(),
        Some(FsError::InvalidParam),
        "past the object"
    );
    assert!(
        c.mmap(off, 3 * PAGE_SIZE).is_ok(),
        "the object's three pages"
    );
    assert!(gem_mmap::unregister(H));
    drm::nouveau_cpu_vmo_forget(H);
}
