//! What closing a DRM master does: `drm_master_release`, and handing the
//! screen back to the kernel text console when a bare KMS
//! client is done with it (`drm_lastclose`, and the `SETCRTC` that puts the
//! console's config back).

use super::*;

/// `drm_master_release` + the file's magic: everything the device kept
/// about a closed open. `true` when the file was its device's master.
pub(crate) fn master_release(minor: u32, file: usize) -> bool {
    let mut state = DRM_STATE.lock();
    let before = state.masters.len();
    state.masters.retain(|&(m, f)| !(m == minor && f == file));
    let was_master = state.masters.len() != before;
    if was_master {
        state.busid.retain(|&m| m != minor);
    }
    state.magics.retain(|&(_, _, f)| f != file);
    state
        .authenticated
        .retain(|&(m, f)| !(m == minor && f == file));
    was_master
}

/// Whether closing the master of `minor` must hand the screen back to the
/// text console: a primary node (a render node is never master), and no seat
/// that restores the console itself through its VT handshake.
pub(crate) fn console_restore_on_master_close(minor: u32, seat_owned: bool) -> bool {
    minor < RENDER_MINOR_BASE && !seat_owned
}

/// `drm_lastclose` -> `drm_client_dev_restore`: when the master's file
/// closes, Linux puts fbcon back on the screen. Here that is what
/// `DROP_MASTER` already did and a plain close did not: the VT the client's
/// first present stamped `KD_GRAPHICS` stayed graphics and kept the DRM VT
/// ownership, so the kernel console never drew on it again. A bare KMS
/// client that exits without `DROP_MASTER` -- `glmark2-drm` from a tty,
/// whose restore `SETCRTC` failed -- left that tty with no picture.
pub(crate) fn restore_console_after_master(minor: u32) {
    hand_screen_to_console(minor, "master closed without DROP_MASTER");
}

/// Give the screen of `minor` back to the kernel text console: the DRM VT
/// owner is forgotten, its VT goes back to `KD_TEXT`, and the console is
/// repainted (an `RMFB` of the shown fb painted the panel black, and a VT
/// that was never stamped graphics gets no repaint from `KD_TEXT`). Nothing
/// when a seat owns the VT: logind/seatd hand it back through their own VT
/// handshake.
pub(crate) fn hand_screen_to_console(minor: u32, why: &str) {
    let seat_owned = crate::fs::stdio::graphics_vt_seat_owned();
    if !console_restore_on_master_close(minor, seat_owned) {
        return;
    }
    // Not under `cfg(test)`: every test that closes a master file would
    // otherwise clear the DRM VT owner another test is holding its lock on.
    #[cfg(not(test))]
    {
        use kernel_hal::console::{
            active_vt, kd_mode_vt, redraw_active_console, set_kd_mode_vt, KD_GRAPHICS, KD_TEXT,
        };
        let owner = DRM_STATE.lock().graphics_vt.take();
        let vt = owner.unwrap_or_else(active_vt);
        kernel_hal::klog_info!(
            "[drm] minor {}: {} -- text console back on VT {}",
            minor,
            why,
            vt
        );
        if kd_mode_vt(vt) == KD_GRAPHICS {
            // Repaints the VT itself when it is the active one.
            set_kd_mode_vt(vt, KD_TEXT);
        } else if vt == active_vt() {
            redraw_active_console();
        }
    }
    #[cfg(test)]
    let _ = why;
}
