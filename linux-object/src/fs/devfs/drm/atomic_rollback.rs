//! Undoing an atomic commit whose present failed. Out of `drm.rs` for its
//! size ceiling.

use super::*;

/// Put back the state [`atomic_commit`] saved before its commit phase, so a
/// commit that fails at the present is all-or-nothing the way Linux's is.
pub(super) fn restore_atomic_state(saved: (AtomicKmsState, u32, Option<(u32, Vec<u8>)>)) {
    let (atomic, crtc_fb, blob) = saved;
    let mut state = DRM_STATE.lock();
    state.atomic = atomic;
    state.crtc_fb.set(PANEL_CARD, crtc_fb);
    if let Some((id, data)) = blob {
        if let Some(existing) = state.blobs.iter_mut().find(|b| b.id == id) {
            existing.data = data;
        }
    }
    drop(state);
    reset_vblank_period();
    if atomic.mode_blob_id != 0 {
        if let Some(data) = get_blob(atomic.mode_blob_id) {
            set_vblank_period_from_modeinfo(&data);
        }
    }
}
