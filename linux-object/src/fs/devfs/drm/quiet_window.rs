//! What used to paint the whole panel while the scanout was paused.
//!
//! The pause is the console GPU's bring-up window. The panel's framebuffer
//! is that GPU's BAR1, which its state load binds again, and the hardware
//! asks for no access to it until the bind is over: one that lands in it
//! leaves BAR1 faulted for the rest of the boot, and the monitor on the
//! last thing written. So presents are acknowledged and dropped meanwhile
//! (`present_now_checked`) and the console's writes are held
//! (`kernel_hal::console::hold_presents_for`).
//!
//! Two things painted regardless: a CRTC turned off (black, the whole
//! panel) and the frame put back after a VT switch. Neither happens while
//! the session is held back until the GPU is up. Both do with a session
//! already running, and a compositor that gives up on its renderer and
//! starts again does each of them once per attempt.
//!
//! And one thing was not painted that should have been: the window itself
//! paints the console over a compositor's frame, and only a present the
//! pause dropped put the frame back afterwards ([`pause_began`]).
//!
//! Its own file because `drm.rs` sits on the size ratchet.

use super::*;

/// The CRTC was turned off while the scanout was paused: the panel is owed
/// its black.
static BLACK_OWED: AtomicBool = AtomicBool::new(false);

/// The pause begins. The window it is for paints the kernel's console over
/// whatever is on the panel, on a compositor's VT too (the deferred
/// bring-up turns `console.overgraphics` on for its length). A frame a
/// compositor has there is owed back when the pause lifts, as one the pause
/// drops is ([`SCANOUT_STALE`]): an idle compositor presents nothing in the
/// window, so nothing was dropped, nothing put its frame back, and its
/// desktop stayed behind an empty console until its next whole frame.
pub(super) fn pause_began() {
    let a_compositors_frame_is_up = {
        let st = DRM_STATE.lock();
        st.graphics_vt == Some(kernel_hal::console::active_vt()) && st.crtc_fb.panel() != 0
    };
    if a_compositors_frame_is_up {
        SCANOUT_STALE.store(true, Ordering::SeqCst);
    }
}

/// The CRTC is off: black on the panel, now, or when the pause lifts.
pub(super) fn crtc_went_off() {
    if scanout_paused() {
        BLACK_OWED.store(true, Ordering::SeqCst);
        return;
    }
    paint_black();
}

/// The CRTC is on again: the frame that turned it on covers what was owed.
pub(super) fn crtc_came_on() {
    BLACK_OWED.store(false, Ordering::SeqCst);
}

/// The pause is over: a CRTC turned off during it, and still off, gets the
/// black it was owed.
pub(super) fn pause_lifted() {
    if BLACK_OWED.swap(false, Ordering::SeqCst) {
        paint_black();
    }
}

fn paint_black() {
    if let Some(display) = primary_display() {
        display.clear(zcore_drivers::prelude::RgbColor::new(0, 0, 0));
        let _ = display.flush();
    }
}

#[cfg(test)]
pub(super) fn black_is_owed() -> bool {
    BLACK_OWED.load(Ordering::SeqCst)
}

/// Re-blit the compositor's last frame IF its VT is the active one. Called
/// right after a VT switch so returning to the compositor (Ctrl+Alt+F1) shows
/// its last frame immediately instead of a stale/blank screen; a no-op when the
/// switch was TO a text console (which the kernel repaints itself).
///
/// While the scanout is paused the frame is owed instead, as a present the
/// pause dropped is ([`SCANOUT_STALE`]): the resume puts it back.
pub fn represent_if_owner_active() -> bool {
    let fb = {
        let st = DRM_STATE.lock();
        if st.graphics_vt != Some(kernel_hal::console::active_vt()) {
            return false;
        }
        st.crtc_fb.panel()
    };
    if fb == 0 {
        return false;
    }
    if scanout_paused() {
        SCANOUT_STALE.store(true, Ordering::SeqCst);
        return false;
    }
    scanout(fb)
}
