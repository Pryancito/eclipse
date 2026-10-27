//! The framebuffer on each card's CRTC.
//!
//! In Linux every `drm_device` has its own `crtc->primary->state->fb`, so a
//! `SETCRTC` on `card1` never changes what `GETCRTC` on `card0` reports. Here
//! it was one `u32` for the whole machine: a client on the second RTX 2060
//! SUPER binding its framebuffer made `card0`'s `GETCRTC` answer with an id
//! from another card (and `RMFB` of that id turn the panel off).
//!
//! Keyed by card index, the `n` of `card{n}`. The panel is card 0 by
//! construction (see [`super::build_gpu_nodes`]), which is what lets the
//! panel's paths keep reading [`PANEL_CARD`] without a lookup under
//! `DRM_STATE`.

use alloc::vec::Vec;

/// The card the panel (the software-KMS output, the boot display) is on.
pub const PANEL_CARD: u32 = 0;

/// `(card, fb_id)` for every card with a framebuffer bound; a card that is not
/// listed has none.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CardFbs(Vec<(u32, u32)>);

impl CardFbs {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// The framebuffer on `card`'s CRTC, 0 for none.
    pub fn get(&self, card: u32) -> u32 {
        self.0
            .iter()
            .find(|(c, _)| *c == card)
            .map_or(0, |(_, fb)| *fb)
    }

    /// The panel's framebuffer.
    pub fn panel(&self) -> u32 {
        self.get(PANEL_CARD)
    }

    /// Bind `fb_id` on `card`'s CRTC; 0 unbinds it.
    pub fn set(&mut self, card: u32, fb_id: u32) {
        self.0.retain(|(c, _)| *c != card);
        if fb_id != 0 {
            self.0.push((card, fb_id));
        }
    }

    /// Unbind `fb_id` from every CRTC that shows it, returning whether the
    /// panel was one of them -- the one case `RMFB` has to turn a pipe off for.
    pub fn unbind(&mut self, fb_id: u32) -> bool {
        let on_panel = fb_id != 0 && self.panel() == fb_id;
        self.0.retain(|(_, fb)| *fb != fb_id);
        on_panel
    }

    /// Unbind every framebuffer `live` says is gone.
    pub fn retain_live(&mut self, live: impl Fn(u32) -> bool) {
        self.0.retain(|(_, fb)| live(*fb));
    }

    /// Unbind everything.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_card_keeps_its_own_framebuffer() {
        let mut fbs = CardFbs::new();
        fbs.set(0, 7);
        fbs.set(1, 9);
        assert_eq!((fbs.get(0), fbs.get(1), fbs.get(2)), (7, 9, 0));
        fbs.set(1, 0);
        assert_eq!((fbs.get(0), fbs.get(1)), (7, 0));
    }

    #[test]
    fn unbind_reports_only_the_panel() {
        let mut fbs = CardFbs::new();
        fbs.set(1, 9);
        assert!(!fbs.unbind(9), "card1's fb going is not the panel's");
        assert_eq!(fbs.get(1), 0);
        fbs.set(PANEL_CARD, 7);
        assert!(fbs.unbind(7));
        assert!(!fbs.unbind(0), "0 is no framebuffer, not the empty panel");
    }

    #[test]
    fn retain_live_drops_dead_ids_on_every_card() {
        let mut fbs = CardFbs::new();
        fbs.set(0, 7);
        fbs.set(1, 9);
        fbs.retain_live(|fb| fb == 9);
        assert_eq!((fbs.get(0), fbs.get(1)), (0, 9));
    }
}
