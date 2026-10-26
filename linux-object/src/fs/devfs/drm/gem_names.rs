//! `GEM_FLINK` / `GEM_OPEN`: the global name table of `drm_gem.c`
//! (`dev->object_name_idr`), split out of `drm.rs` for the size ratchet.
//!
//! A flink name is a small integer any authenticated open of the device
//! can turn back into a handle of its own, which is why `drm-uapi.rst`
//! calls the interface insecure and points new clients at PRIME. It is
//! still what every pre-PRIME DRI2 client, the Xorg `modesetting` driver's
//! `drmModeAddFB` path on old servers and `glamor`'s pixmap sharing on a
//! single-GPU X server speak, and both ioctls were ENOTTY here.
//!
//! The rules are `drm_gem_flink_ioctl` / `drm_gem_open_ioctl`, verbatim:
//! FLINK on a handle the caller does not hold is ENOENT, a second FLINK
//! of the same object answers the name it already has, OPEN of an unknown
//! name is ENOENT, and OPEN by a process that holds the object already
//! gives it its existing handle (as `drm_gem_handle_create_tail` would, a
//! second handle on the same `drm_gem_object` -- this tree keys handles by
//! `(object, pid)`, so the one it has is the one it gets). The name lives
//! exactly as long as the object: `drm_gem_object_handle_free` removes it
//! when the LAST handle goes, so a name is never a reference of its own.
//! Here that is [`prune_gem_names`] on the generic table after `GEM_CLOSE`
//! and at process exit, and for a nouveau object (tracked in `gem_mmap`,
//! not here) the name is dropped when its object is found gone: in the
//! `GEM_CLOSE` arm that freed it, or by the OPEN that misses it.

use super::*;
use zcore_drivers::scheme::gem_mmap;

/// What a flink name stands for.
#[derive(Clone)]
pub(super) enum GemNamed {
    /// A CREATE_DUMB / PRIME-imported object of linux-object's own table,
    /// identified by its backing VMO (every handle over the same frames
    /// shares that `Arc`, see `import_dmabuf_for`).
    Generic {
        vmo: Arc<VmObject>,
        phys_addr: u64,
        size: usize,
    },
    /// A nouveau `GEM_NEW` object, identified by its global handle.
    Driver(u32),
}

impl GemNamed {
    fn is(&self, other: &GemNamed) -> bool {
        match (self, other) {
            (GemNamed::Generic { vmo: a, .. }, GemNamed::Generic { vmo: b, .. }) => {
                Arc::ptr_eq(a, b)
            }
            (GemNamed::Driver(a), GemNamed::Driver(b)) => a == b,
            _ => false,
        }
    }
}

/// `drm_gem_flink_ioctl`: the name of `handle`, minted on the first call
/// and answered again on the next; `None` (ENOENT) for a handle the caller
/// does not hold.
pub fn gem_flink(handle: u32) -> Option<u32> {
    gem_flink_for(handle, current_pid())
}

pub(super) fn gem_flink_for(handle: u32, pid: u64) -> Option<u32> {
    // Decide what the handle is BEFORE taking `DRM_STATE`: `lookup_for`
    // takes `gem_mmap`'s own lock and logs a refusal.
    let driver =
        handle >= gem_mmap::DRIVER_HANDLE_BASE && gem_mmap::lookup_for(handle, pid).is_some();
    let mut state = DRM_STATE.lock();
    let named = if let Some((h, vmo, _)) = state
        .handles
        .iter()
        .find(|(h, _, owner)| h.id == handle && owned_by(*owner, pid))
    {
        GemNamed::Generic {
            vmo: vmo.clone(),
            phys_addr: h.phys_addr,
            size: h.size,
        }
    } else if driver {
        GemNamed::Driver(handle)
    } else {
        return None;
    };
    if let Some((name, _)) = state.names.iter().find(|(_, n)| n.is(&named)) {
        return Some(*name);
    }
    let name = state.next_name;
    state.next_name += 1;
    state.names.push((name, named));
    Some(name)
}

/// `drm_gem_open_ioctl`: `(handle, size)` of the object called `name` for
/// the caller -- the handle it already has over that object, or a new one
/// -- or `None` (ENOENT) for a name nobody minted, or whose object is gone.
pub fn gem_open(name: u32) -> Option<(u32, u64)> {
    gem_open_for(name, current_pid())
}

pub(super) fn gem_open_for(name: u32, pid: u64) -> Option<(u32, u64)> {
    let named = DRM_STATE
        .lock()
        .names
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, named)| named.clone())?;
    match named {
        GemNamed::Generic {
            vmo,
            phys_addr,
            size,
        } => Some((import_dmabuf_for(phys_addr, size, vmo, pid), size as u64)),
        GemNamed::Driver(handle) => {
            // As `GETFB` hands out a nouveau handle (`fb_handle`): the
            // caller's existing reference if it holds one, else a new one.
            let Some((_, size)) = gem_mmap::lookup(handle) else {
                // `nouveau_release_process` freed it behind the name.
                forget_gem_name_of_driver_handle(handle);
                return None;
            };
            if !gem_mmap::holds(handle, pid) {
                gem_mmap::add_ref(handle, pid)?;
            }
            Some((handle, size))
        }
    }
}

/// Drops every name whose generic object no handle refers to any more:
/// `drm_gem_object_handle_free`, run after the handle table shrank.
pub(super) fn prune_gem_names(state: &mut DrmState) {
    let handles = &state.handles;
    state.names.retain(|(_, named)| match named {
        GemNamed::Generic { vmo, .. } => handles.iter().any(|(_, held, _)| Arc::ptr_eq(held, vmo)),
        GemNamed::Driver(_) => true,
    });
}

/// Drops the name of nouveau object `handle`, once the object is gone
/// (`gem_mmap` no longer tracks it). `true` if it had one.
pub fn forget_gem_name_of_driver_handle(handle: u32) -> bool {
    let mut state = DRM_STATE.lock();
    let before = state.names.len();
    state
        .names
        .retain(|(_, named)| !matches!(named, GemNamed::Driver(h) if *h == handle));
    state.names.len() != before
}
