//! `GEM_FLINK` / `GEM_OPEN` through the ioctl entry point: the permit,
//! the errnos, the name's stability and its lifetime.

use super::gem_name::{DrmGemFlink, DrmGemOpen};
use super::gl_client_sequence_tests::Client;
use super::*;

fn flink(c: &Client, handle: u32) -> Result<u32> {
    let mut req = DrmGemFlink { handle, name: 0 };
    c.ioctl(DRM_IOCTL_GEM_FLINK, &mut req)?;
    Ok(req.name)
}

fn open(c: &Client, name: u32) -> Result<(u32, u64)> {
    let mut req = DrmGemOpen {
        name,
        handle: 0,
        size: 0,
    };
    c.ioctl(DRM_IOCTL_GEM_OPEN, &mut req)?;
    Ok((req.handle, req.size))
}

/// `drm_gem_flink_ioctl` / `drm_gem_open_ioctl`: both `DRM_AUTH` (EACCES
/// for an open nobody authenticated), ENOENT for a handle the caller does
/// not hold and for a name nobody minted, the same name on a second FLINK,
/// OPEN giving the caller the handle it already has over the object, and
/// the name dying with the object's last handle
/// (`drm_gem_object_handle_free`). Both were ENOTTY.
#[test]
fn a_flinked_object_opens_by_name_until_its_last_handle_goes() {
    let _serialised = drm::test_globals::lock();
    drm::set_test_root(false);
    // A node of its own: the first open takes its master and is
    // authenticated; the second is neither.
    let master = Client::open(87);
    let client = Client::open(87);
    let buf = master.create_dumb(16, 16);

    assert_eq!(
        flink(&client, buf.handle),
        Err(FsError::NoPermission),
        "FLINK is DRM_AUTH"
    );
    assert_eq!(open(&client, 1), Err(FsError::NoPermission), "so is OPEN");
    assert_eq!(
        flink(&master, buf.handle + 1000),
        Err(FsError::EntryNotFound),
        "a handle nobody holds"
    );

    let name = flink(&master, buf.handle).expect("FLINK of its own buffer");
    assert_ne!(name, 0, "names start at 1 (idr_alloc(.., 1, 0, ..))");
    assert_eq!(flink(&master, buf.handle), Ok(name), "the same name again");
    assert_eq!(
        open(&master, name),
        Ok((buf.handle, buf.size)),
        "the handle it already has, and the object's size"
    );
    assert_eq!(
        open(&master, name + 1000),
        Err(FsError::EntryNotFound),
        "a name nobody minted"
    );

    // Two live objects, two names.
    let other = master.create_dumb(16, 16);
    let other_name = flink(&master, other.handle).unwrap();
    assert_ne!(other_name, name, "one name per object");
    assert_eq!(open(&master, other_name), Ok((other.handle, other.size)));

    // `drm_authmagic` lets the second open in: it is the same (test) pid,
    // so it too is handed the handle that pid already holds.
    let mut magic = 0u32;
    client.ioctl(DRM_IOCTL_GET_MAGIC, &mut magic).unwrap();
    master.ioctl(DRM_IOCTL_AUTH_MAGIC, &mut magic).unwrap();
    assert_eq!(open(&client, name), Ok((buf.handle, buf.size)));

    // The last handle goes: so does the name, and the handle is unknown
    // to FLINK too.
    let mut h = buf.handle;
    assert_eq!(master.ioctl(DRM_IOCTL_GEM_CLOSE, &mut h), Ok(0));
    assert_eq!(open(&master, name), Err(FsError::EntryNotFound));
    assert_eq!(flink(&master, buf.handle), Err(FsError::EntryNotFound));

    // A new object is a new name, never a reused one.
    let next = master.create_dumb(16, 16);
    let next_name = flink(&master, next.handle).unwrap();
    assert!(next_name > name, "{} after {}", next_name, name);
    assert_eq!(
        open(&master, other_name),
        Ok((other.handle, other.size)),
        "the other object's name stands"
    );
    for mut h in [next.handle, other.handle] {
        assert_eq!(master.ioctl(DRM_IOCTL_GEM_CLOSE, &mut h), Ok(0));
    }
}
