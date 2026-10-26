//! Which CRTC the panel state describes.
//!
//! The panel state of this DRM core -- the framebuffer the panel carries
//! ([`super::PANEL_FB`]) and the band hashes of its pixels
//! ([`super::panel_bands_reset`]) -- is **one panel's worth**: a single `fb_id`
//! and a single row of hashes for the whole machine. That was exactly true
//! while there was exactly one CRTC to present to.
//!
//! It stopped being true on 26-oct-2026, when each DRM node started answering
//! `GETRESOURCES` for its own card and every card started advertising a CRTC id
//! of its own. A box whose cards both report hardware KMS now has two CRTCs a
//! compositor can present to, and the two share that one row of state. The two
//! ways that goes wrong both put wrong pixels on a monitor:
//!
//! - A damage box presented to CRTC B is honoured because `PANEL_FB` says the
//!   panel already carries that framebuffer -- but what carries it is CRTC A.
//!   Everything the box leaves alone on B is another monitor's frame.
//! - The band skip declines to copy a band whose hash matches -- against hashes
//!   taken off the *other* panel.
//!
//! So the state carries the CRTC it describes, and a present to any other CRTC
//! starts over instead of inheriting it. One panel's state, used for one panel
//! at a time, which is what it has always been able to hold.
//!
//! This is not the panel-per-card refactor: it does not give the second CRTC a
//! panel of its own, it stops the second CRTC from being told things about the
//! first one's. Presenting alternately to two CRTCs therefore costs a full
//! frame every time instead of a damage box -- correct, and slow, and visibly
//! better than correct-looking pixels from the wrong frame. The fast path for
//! two live panels is state per panel, which is its own batch.

use core::sync::atomic::{AtomicU32, Ordering};

/// The CRTC the panel state currently describes, or 0 for none.
///
/// Separate from the state itself so that every piece of it -- the framebuffer
/// id and the band hashes -- is covered by one answer. Taking it over resets
/// all of them together, because they describe the same pixels and a reset of
/// half of them is the bug above with extra steps.
static PANEL_CRTC: AtomicU32 = AtomicU32::new(0);

/// What a present to `crtc_id` may do with the panel state it finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelState {
    /// The state describes this CRTC: it may be read and trusted.
    Describes,
    /// The state describes another CRTC (or none): it has been reset, and this
    /// present must copy a whole frame.
    TakenOver,
}

/// Claim the panel state for `crtc_id`, resetting it when it described another
/// CRTC.
///
/// Called once per present, before the state is read. `crtc_id == 0` is not a
/// CRTC: a present that names no pipe is left reading whatever is there, which
/// is the pre-26-oct behaviour and the one the software-KMS path still wants --
/// under software KMS there is one synthetic CRTC for the whole machine, so
/// nothing can take the state away from anything.
pub fn claim_panel_for(crtc_id: u32) -> PanelState {
    if crtc_id == 0 {
        return PanelState::Describes;
    }
    if PANEL_CRTC.swap(crtc_id, Ordering::SeqCst) == crtc_id {
        return PanelState::Describes;
    }
    // Both halves, together, and WITHOUT going through `set_panel_fb(0)`: that
    // one also calls [`forget_panel_crtc`], which would undo the swap above and
    // leave every present reading `TakenOver` forever.
    panel_state_reset();
    PanelState::TakenOver
}

/// Forget which CRTC the state describes, so the next present to any CRTC
/// starts over.
///
/// Called wherever the state itself is thrown away for a reason that is not a
/// present -- a blank, a console VT taking the panel, a retired framebuffer.
/// Leaving the CRTC behind there would let the next present to that same CRTC
/// read `Describes` over state that has been cleared, which is harmless today
/// (cleared state honours no damage box) and is a lie either way.
pub fn forget_panel_crtc() {
    PANEL_CRTC.store(0, Ordering::SeqCst);
}

/// The CRTC the state describes, for tests and for the one-shot present line.
pub fn panel_crtc() -> u32 {
    PANEL_CRTC.load(Ordering::SeqCst)
}

/// The framebuffer whose pixels the panel carries **in full**, or 0 for none.
///
/// Not `crtc_fb`, which is bookkeeping for `GETCRTC` and is bound by every
/// present that returns `Ok` -- including one that only copied a damage box, and
/// one a pause acknowledged without drawing anything at all. This is the
/// narrower fact, and it is the only fact a damage box is meaningful against.
///
/// A damage box says "only these pixels changed **in the frame that is already
/// on the panel**". A compositor with a swapchain presents a *different*
/// framebuffer almost every frame, so the pixels outside the box come from the
/// buffer presented before -- and a recycled swapchain buffer holds whatever
/// frame it was last drawn into, which is not this one. Honouring the box then
/// leaves the panel a collage of two frames, and the collage is invisible until
/// something reads the panel back: [`repaint_for_cursor`] restores its two
/// ~64x64 windows *from* `crtc_fb`, so a pointer move over a region the box did
/// not touch pastes the new buffer's older content into the frame still up.
/// That is garbage in a ring around the cursor, appearing exactly when a popup
/// opens -- which is when a fresh buffer is presented with a box around the
/// popup and nothing else.
///
/// This is the same rule Linux applies in `drm_atomic_helper_damage_iter_init`,
/// which throws the clips away and declares a full update when
/// `state->fb != old_state->fb`.
///
/// Cleared, not just rebound, wherever something OTHER than a present writes the
/// panel: blanking paints it black, and a text VT in the foreground puts console
/// output on it. An id can be reused after `RMFB`, so a retired framebuffer
/// clears it too rather than letting a new one inherit "already on screen".
static PANEL_FB: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The framebuffer the panel carries in full, or 0.
pub(super) fn panel_fb() -> u32 {
    PANEL_FB.load(Ordering::SeqCst)
}

/// Record that the panel now carries `fb_id` in full; 0 means it carries no
/// framebuffer (blanked, or a console VT has written over it).
pub(super) fn set_panel_fb(fb_id: u32) {
    PANEL_FB.store(fb_id, Ordering::SeqCst);
    // Every caller passing 0 is something OTHER than a present having written
    // the panel -- a blank painting it black, a console VT printing over it --
    // so the band skip's idea of what is up there stops being true at the same
    // moment, and for the same reason. See [`PRESENT_SKIP`].
    if fb_id == 0 {
        super::panel_bands_reset();
        // And the state no longer describes the pipe it described, so the next
        // present to that same pipe starts over rather than reading
        // `Describes` over state somebody else cleared. See [`self`].
        forget_panel_crtc();
    }
}

/// Clear the panel state without touching which CRTC it describes: for
/// [`claim_panel_for`], which has just set that CRTC and must not undo it.
///
/// Both halves together, because they describe the same pixels -- see
/// [`self`] for what clearing one of them alone leaves on a monitor.
pub(super) fn panel_state_reset() {
    PANEL_FB.store(0, Ordering::SeqCst);
    super::panel_bands_reset();
}

/// [`PANEL_FB`] cleared for a framebuffer that no longer exists, so a reused id
/// cannot inherit "the panel already carries this".
pub(super) fn forget_panel_fb(fb_id: u32) {
    if PANEL_FB
        .compare_exchange(fb_id, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // The framebuffer whose pixels the band hashes describe is gone, and its
        // id can come back on a different buffer. Same rule as [`PANEL_FB`]'s
        // own: a retired id inherits nothing.
        super::panel_bands_reset();
        forget_panel_crtc();
    }
}

#[cfg(test)]
pub(crate) fn panel_fb_for_test() -> u32 {
    panel_fb()
}

/// Whether the band hashes claim to describe anything at all.
///
/// Exists so [`self`]'s tests can assert that taking the panel state
/// over for another CRTC clears the bands and not just the framebuffer id: the
/// two describe the same pixels, and bands left behind are the band skip
/// declining to copy rows it hashed off a different monitor.
#[cfg(test)]
pub(crate) fn panel_bands_known_for_test() -> bool {
    super::PANEL_BAND_GEOM.load(Ordering::Relaxed) != 0
}

/// Put a geometry on the band hashes without presenting a frame, for the test
/// [`panel_bands_known_for_test`] documents. A present is the only producer in
/// production, and reaching one from a unit test would need a display, a
/// framebuffer and a CRTC to say something about one `u64`.
#[cfg(test)]
pub(crate) fn note_panel_bands_for_test(w: u32, h: u32) {
    super::PANEL_BAND_GEOM.store(
        super::pack_panel_geom(0, 0, w, h).expect("a test geometry fits in 16 bits"),
        Ordering::Relaxed,
    );
}

#[cfg(test)]
mod tests;
