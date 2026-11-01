//! KMS property blobs: `DRM_IOCTL_MODE_CREATEPROPBLOB` and its destroy side.
//!
//! Linux keeps the blob table and its ownership rule in `drm_property.c`,
//! away from the rest of mode setting, because the only thing a blob is is a
//! byte buffer with an id and an owner: who may destroy it is the whole of
//! the logic. The table itself lives in [`super::DRM_STATE`], so this is the
//! same lock as everything else, just a smaller file.

use super::*;
use alloc::vec::Vec;

/// Create a KMS property blob (`DRM_IOCTL_MODE_CREATEPROPBLOB`) and return its
/// id. `user_created` distinguishes client blobs (destroyable by their
/// creator) from kernel-owned ones (current mode), mirroring Linux's
/// ownership rule.
pub fn create_blob(data: Vec<u8>, user_created: bool) -> u32 {
    create_blob_owned(data, user_created, 0)
}

/// [`create_blob`] on behalf of the open file `owner` (see [`DrmBlob::owner`]).
pub fn create_blob_owned(data: Vec<u8>, user_created: bool, owner: usize) -> u32 {
    let mut state = DRM_STATE.lock();
    let id = state.next_object_id;
    state.next_object_id += 1;
    state.blobs.push(DrmBlob {
        id,
        user_created,
        owner,
        data,
    });
    id
}

/// `drm_release`'s `drm_property_destroy_user_blobs`: the blobs the open file
/// `owner` created go with it.
pub fn destroy_blobs_of(owner: usize) {
    DRM_STATE
        .lock()
        .blobs
        .retain(|b| !(b.user_created && b.owner == owner));
}

/// Look up a blob's contents (`DRM_IOCTL_MODE_GETPROPBLOB`).
pub fn get_blob(id: u32) -> Option<Vec<u8>> {
    DRM_STATE
        .lock()
        .blobs
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.data.clone())
}

/// Outcome of `DRM_IOCTL_MODE_DESTROYPROPBLOB` (Linux: ENOENT / EPERM split).
pub enum BlobDestroy {
    /// Blob removed.
    Destroyed,
    /// No blob with that id.
    NotFound,
    /// Kernel-created blob (current mode, …): only the creator may destroy.
    KernelOwned,
    /// Another open file's blob (EPERM, like the kernel's).
    NotOwner,
}

/// Destroy a user-created blob, on behalf of the open file `owner`.
pub fn destroy_blob(id: u32, owner: usize) -> BlobDestroy {
    let mut state = DRM_STATE.lock();
    match state.blobs.iter().position(|b| b.id == id) {
        None => BlobDestroy::NotFound,
        Some(pos) if !state.blobs[pos].user_created => BlobDestroy::KernelOwned,
        Some(pos) if state.blobs[pos].owner != owner => BlobDestroy::NotOwner,
        Some(pos) => {
            state.blobs.remove(pos);
            BlobDestroy::Destroyed
        }
    }
}
