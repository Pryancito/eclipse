//! The `GEM_FLINK` / `GEM_OPEN` arms: the uAPI structs and the two
//! `DRM_AUTH` handlers over [`drm::gem_flink`] / [`drm::gem_open`]. Split
//! out of `drm_scheme.rs` for the size ratchet.

use super::*;

/// `struct drm_gem_flink` (8 bytes): `handle` in, `name` out.
#[repr(C)]
pub(super) struct DrmGemFlink {
    pub(super) handle: u32,
    pub(super) name: u32,
}

/// `struct drm_gem_open` (16 bytes): `name` in, `handle` and `size` out.
#[repr(C)]
pub(super) struct DrmGemOpen {
    pub(super) name: u32,
    pub(super) handle: u32,
    pub(super) size: u64,
}

const _: () = assert!(core::mem::size_of::<DrmGemFlink>() == 8);
const _: () = assert!(core::mem::size_of::<DrmGemOpen>() == 16);

/// `drm_gem_flink_ioctl`, `DRM_AUTH`: ENOENT for a handle the caller does
/// not hold; otherwise the object's name, the same one every time.
#[allow(unsafe_code)]
pub(super) fn gem_flink(dev: &DrmDev, data: usize) -> Result<usize> {
    dev.legacy_permit(false)?;
    let req = unsafe { &mut *(data as *mut DrmGemFlink) };
    req.name = drm::gem_flink(req.handle).ok_or(FsError::EntryNotFound)?;
    Ok(0)
}

/// `drm_gem_open_ioctl`, `DRM_AUTH`: ENOENT for a name nobody minted or
/// whose object is gone; otherwise a handle of the caller's and the
/// object's size.
#[allow(unsafe_code)]
pub(super) fn gem_open(dev: &DrmDev, data: usize) -> Result<usize> {
    dev.legacy_permit(false)?;
    let req = unsafe { &mut *(data as *mut DrmGemOpen) };
    let (handle, size) = drm::gem_open(req.name).ok_or(FsError::EntryNotFound)?;
    req.handle = handle;
    req.size = size;
    Ok(0)
}
