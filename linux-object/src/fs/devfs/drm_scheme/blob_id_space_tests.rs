//! The property-blob id space, which has three tenants and no referee.
//!
//! `GETPROPBLOB` takes an id and nothing else -- libdrm identifies a blob
//! purely by id -- and resolves it against the blob store first, then the
//! range reserved for connector EDIDs. The synthetic KMS objects and the
//! framebuffers number from 1 upwards in the same space.
//!
//! Until now the only thing keeping the three apart was a comment and the
//! literal `20000` written out at both ends of the EDID encoding, in two
//! files. Now the bases are named, the encoding is one pair of functions,
//! and the gap between them is a compile-time assertion.

use super::gl_client_sequence_tests::Client;
use super::*;

#[test]
fn an_edid_blob_id_round_trips_for_every_connector() {
    for connector in [0u32, 1, 2, 3, 16, 255, 4096] {
        let id = edid_blob_id(connector);
        assert_eq!(
            connector_of_edid_blob(id),
            Some(connector),
            "connector {} does not survive its blob id",
            connector,
        );
    }
}

/// The store's ids and the EDID range must not meet. They do not today by
/// a margin of ten thousand, and that margin is the number of connectors
/// the encoding can name -- far more than a machine has, but write it
/// down, because the failure would be a connector's EDID silently shadowed
/// by somebody's MODE_ID blob.
#[test]
fn the_edid_range_ends_before_the_blob_store_begins() {
    // The gap itself is a `const _: () = assert!(...)` beside the
    // constant, so it is a build error rather than a test failure. What is
    // left here is that the decoder honours it.
    let last = drm::BLOB_ID_BASE - EDID_BLOB_BASE - 1;
    assert_eq!(connector_of_edid_blob(edid_blob_id(last)), Some(last));
    // One past the end belongs to the store, not to a connector.
    assert_eq!(connector_of_edid_blob(drm::BLOB_ID_BASE), None);
    assert_eq!(connector_of_edid_blob(drm::BLOB_ID_BASE + 1), None);
    assert_eq!(connector_of_edid_blob(u32::MAX), None);
}

/// And nothing below the range is an EDID either: the synthetic KMS object
/// ids and the framebuffer ids live down there.
#[test]
fn the_low_ids_belong_to_objects_and_framebuffers() {
    for id in [
        0,
        drm::SYNTH_CRTC_ID,
        drm::SYNTH_ENCODER_ID,
        drm::SYNTH_PLANE_ID,
        1000,
        EDID_BLOB_BASE - 1,
    ] {
        assert_eq!(
            connector_of_edid_blob(id),
            None,
            "id {} is not an EDID blob",
            id,
        );
    }
    assert_eq!(connector_of_edid_blob(EDID_BLOB_BASE), Some(0));
}

/// Ids the store hands out are unique, land where they are supposed to,
/// and keep landing there after a destroy -- an id is never reused, which
/// is what stops a client that freed a blob from reading a later one
/// through the same number.
#[test]
fn the_store_numbers_its_blobs_above_the_reserved_range_and_never_reuses_one() {
    let _serialised = drm::test_globals::lock();
    let a = drm::create_blob(alloc::vec![1u8, 2, 3], true);
    let b = drm::create_blob(alloc::vec![4u8], true);
    assert!(
        a >= drm::BLOB_ID_BASE,
        "blob {} is inside the EDID range",
        a
    );
    assert!(b > a, "ids must not repeat");
    assert_eq!(drm::get_blob(a).as_deref(), Some(&[1u8, 2, 3][..]));

    assert!(matches!(
        drm::destroy_blob(a, 0),
        drm::BlobDestroy::Destroyed
    ));
    assert_eq!(drm::get_blob(a), None);
    let c = drm::create_blob(alloc::vec![5u8], true);
    assert!(c > b, "a freed id came back: {} after {}", c, b);

    assert!(matches!(
        drm::destroy_blob(b, 0),
        drm::BlobDestroy::Destroyed
    ));
    assert!(matches!(
        drm::destroy_blob(c, 0),
        drm::BlobDestroy::Destroyed
    ));
}

/// Linux splits `DESTROYPROPBLOB`'s refusals: ENOENT for an id that names
/// nothing, EPERM for a blob the caller did not create. The kernel's own
/// current-mode blob is the second case, and a client that could free it
/// would take `MODE_ID` readback down with it.
#[test]
fn only_the_creator_may_destroy_a_blob() {
    let _serialised = drm::test_globals::lock();
    let kernel = drm::create_blob(alloc::vec![0u8; 68], false);
    assert!(
        matches!(drm::destroy_blob(kernel, 0), drm::BlobDestroy::KernelOwned),
        "a kernel-owned blob must answer EPERM, not vanish",
    );
    assert!(
        drm::get_blob(kernel).is_some(),
        "and it must still be there afterwards",
    );

    assert!(matches!(
        drm::destroy_blob(drm::BLOB_ID_BASE - 1, 0),
        drm::BlobDestroy::NotFound
    ));
    assert!(matches!(
        drm::destroy_blob(edid_blob_id(2), 0),
        drm::BlobDestroy::NotFound
    ));
}
/// `drm_mode_destroyblob_ioctl` frees a blob only for the file that
/// created it ("ensure the property was actually created by this user",
/// EPERM otherwise), and `drm_release` frees what a file leaves behind.
/// Any client could destroy any other's -- the compositor's MODE_ID blob
/// going away under it makes its next commit fail with ENOENT -- and a
/// closed client's blobs were kept for ever.
#[test]
fn a_blob_belongs_to_the_file_that_created_it_and_dies_with_it() {
    let _serialised = drm::test_globals::lock();
    let owner = Client::open(0);
    let other = Client::open(0);
    let create = |c: &Client| {
        let bytes = [7u8; 68];
        let mut blob = DrmModeCreateBlob {
            data: bytes.as_ptr() as u64,
            length: bytes.len() as u32,
            blob_id: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
            .expect("CREATEPROPBLOB");
        blob.blob_id
    };
    let destroy = |c: &Client, id: u32| {
        let mut id = id;
        c.ioctl(DRM_IOCTL_MODE_DESTROYPROPBLOB, &mut id)
    };

    let id = create(&owner);
    assert_eq!(
        destroy(&other, id),
        Err(FsError::NotPermitted),
        "another file's blob"
    );
    // Readable by anyone: the lookup has no owner check.
    let mut get = DrmModeGetBlob {
        blob_id: id,
        length: 0,
        data: 0,
    };
    assert_eq!(other.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut get), Ok(0));
    assert_eq!(get.length, 68);
    assert_eq!(destroy(&owner, id), Ok(0));
    assert_eq!(destroy(&owner, id), Err(FsError::EntryNotFound), "twice");

    let kernel = drm::create_blob(alloc::vec![0u8; 68], false);
    assert_eq!(
        destroy(&owner, kernel),
        Err(FsError::NotPermitted),
        "the kernel's own"
    );

    let left_behind = create(&owner);
    let kept = create(&other);
    drop(owner);
    assert!(
        drm::get_blob(left_behind).is_none(),
        "a closed file's blob outlived it"
    );
    assert!(
        drm::get_blob(kept).is_some(),
        "and took a stranger's with it"
    );
    assert_eq!(destroy(&other, left_behind), Err(FsError::EntryNotFound));
    assert_eq!(destroy(&other, kept), Ok(0));
}
