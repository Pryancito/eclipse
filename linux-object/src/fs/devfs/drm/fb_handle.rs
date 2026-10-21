//! The GEM handle `GETFB`/`GETFB2` hand back for a framebuffer: what
//! `drm_framebuffer_funcs::create_handle` (`drm_gem_fb_create_handle`, so
//! `drm_gem_handle_create(file_priv, obj)`) answers, a handle of the
//! CALLER's for the object behind the framebuffer. Out of `drm.rs` for its
//! size ceiling.

use super::*;

/// A handle of `pid`'s for the object behind `fb`.
///
/// Linux makes a fresh handle in the caller's file for every call; the
/// tables here are per pid, so the handle `pid` already holds is that
/// handle: its own dumb buffer, or a nouveau object it holds (the kernel's
/// objects, owner 0, are everyone's). Otherwise one of its own over the same
/// backing: for a dumb buffer a second entry sharing the `VmObject`, as a
/// PRIME self-import makes, so `DESTROY_DUMB` of either leaves the other's
/// frames alive; for a nouveau object the same global handle with `pid`
/// added as a holder, as `PRIME_FD_TO_HANDLE` does, so the caller's
/// `GEM_CLOSE` takes that reference and not the creator's. 0 for an object
/// no table knows any more (a framebuffer whose buffer was destroyed under
/// it), which is what a client reads as "no handle".
pub fn fb_gem_handle_for(fb: &DrmFramebuffer, pid: u64) -> u32 {
    let handle = fb.gem_handle_id;
    let dumb = {
        let state = DRM_STATE.lock();
        state
            .handles
            .iter()
            .find(|(h, _, _)| h.id == handle)
            .map(|(h, vmo, owner)| (h.phys_addr, h.size, vmo.clone(), *owner))
    };
    if let Some((phys_addr, size, vmo, owner)) = dumb {
        if owned_by(owner, pid) {
            return handle;
        }
        return import_dmabuf_for(phys_addr, size, vmo, pid);
    }
    use zcore_drivers::scheme::gem_mmap;
    if gem_mmap::ref_count(handle).is_none() {
        return 0;
    }
    if gem_mmap::holds(handle, pid) {
        return handle;
    }
    gem_mmap::add_ref(handle, pid).map_or(0, |_| handle)
}

/// Record `fb` as created by `owner`, for a test: the framebuffers and
/// buffers a test makes are the kernel's (pid 0), which every caller owns,
/// so the alias paths above are reachable only by lending them an owner.
/// `true` when the framebuffer exists.
#[cfg(test)]
pub(crate) fn set_fb_owner_for_test(fb_id: u32, owner: u64) -> bool {
    let mut state = DRM_STATE.lock();
    match state.framebuffers.iter_mut().find(|f| f.id == fb_id) {
        Some(f) => {
            f.owner = owner;
            true
        }
        None => false,
    }
}

/// Record the dumb buffer `handle_id` as `owner`'s, for a test (see
/// [`set_fb_owner_for_test`]). `true` when the handle exists.
#[cfg(test)]
pub(crate) fn set_handle_owner_for_test(handle_id: u32, owner: u64) -> bool {
    let mut state = DRM_STATE.lock();
    match state.handles.iter_mut().find(|(h, _, _)| h.id == handle_id) {
        Some(entry) => {
            entry.2 = owner;
            true
        }
        None => false,
    }
}
