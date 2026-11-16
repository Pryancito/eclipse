//! The `abyss` look: the Abyss session's warm-black glass, for every Lunar
//! client.
//!
//! The values come from Abyss's published style guide (`desktop/docs/STYLE.md`:
//! colours, alphas and radii). They are numbers, not code: nothing here is
//! taken from the AGPL sources.
//!
//! The material itself is the compositor's. Abyss blurs, saturates and rims
//! whatever is behind a surface drawn below full opacity, so a client only has
//! to hand it a translucent ground. The clients draw exactly as before, on an
//! opaque [`GROUND`], and [`frost`] turns that ground into the window tint
//! afterwards: a pixel of the ground colour gets [`TINT`] alpha, and a pixel
//! further from it (a card, a hairline, text) ramps back to opaque. Text stays
//! solid; plates a shade lighter than the ground read as glass a step clearer,
//! which is what the guide asks of a card.
//!
//! Without blur (labwc, or Abyss with `blur off`) the same buffer is a dark
//! veil at .70 over the wallpaper, still readable.

use crate::draw::{Canvas, Rgb};
use crate::look::Look;

/// The window base, warm near-black (never blue-grey).
pub const GROUND: Rgb = (0x0b, 0x09, 0x06);
/// The window tint's alpha over the compositor's blur.
pub const TINT: f32 = 0.70;
/// A card: white at .035 over the ground.
pub const CARD: Rgb = (0x14, 0x12, 0x0f);
/// An inset well (a field, a list): a shade under the card.
pub const WELL: Rgb = (0x10, 0x0e, 0x0a);
/// Hover on a cell: white at .08.
pub const HOVER: Rgb = (0x1d, 0x1b, 0x18);
/// Selected or focused cell: white at .12, under a [`RIM`].
pub const SELECT: Rgb = (0x28, 0x26, 0x23);
/// A hairline, rows and separators: white at .12.
pub const HAIRLINE: Rgb = (0x28, 0x27, 0x24);
/// The crisp rim of a selected cell: white at .20.
pub const RIM: Rgb = (0x3c, 0x3a, 0x38);
/// Primary text.
pub const TEXT: Rgb = (0xff, 0xff, 0xff);
/// Secondary text: white at .64.
pub const MUTED: Rgb = (0xa7, 0xa6, 0xa5);
/// Tertiary text, still enabled text (5:1 on the ground): white at .48.
pub const DIM: Rgb = (0x80, 0x7f, 0x7d);
/// The accent: marks, knobs, the current value of a meter.
pub const GOLD: Rgb = (0xf2, 0xc3, 0x3c);
/// The accent as text: the selected row's label.
pub const GOLD_TEXT: Rgb = (0xf5, 0xcf, 0x5c);
/// Danger: warm coral, never a red ground.
pub const DANGER: Rgb = (0xf5, 0x8a, 0x72);
/// Healthy / running.
pub const OK: Rgb = (0x8f, 0xd6, 0x9c);
/// A filled meter's run that is not the live value: white at .62.
pub const METER: Rgb = (0xa3, 0xa2, 0xa0);

/// Corner radii, logical px: window, card, inset cell.
pub const R_WINDOW: i32 = 16;
pub const R_CARD: i32 = 13;
pub const R_CELL: i32 = 10;

/// Whether this process is wearing the glass. Calls
/// [`lunartext::use_family`] on the way, so a client that asks once at start
/// also gets the look's data face (JetBrains Mono) before its first glyph.
pub fn enter(look: Look) -> bool {
    if look == Look::Abyss {
        lunartext::use_family(lunartext::Family::JetBrains);
        true
    } else {
        false
    }
}

/// How far, per channel, a pixel may sit from the ground and still be ramped
/// toward the tint; at this distance it is opaque again.
const RAMP: f32 = 40.0;

/// Turn the opaque [`GROUND`] of a finished frame into the window tint (see
/// the module comment). Pixels already translucent (a scrim, the transparent
/// outside of a rounded panel) are left as they are.
pub fn frost(cv: &mut Canvas) {
    cv.map_opaque(|r, g, b| {
        let d = (r as i32 - GROUND.0 as i32)
            .abs()
            .max((g as i32 - GROUND.1 as i32).abs())
            .max((b as i32 - GROUND.2 as i32).abs()) as f32;
        let t = (d / RAMP).min(1.0);
        TINT + (1.0 - TINT) * t
    });
}

/// The colours of the clients that were drawn for `tecno` alone (lunarmon,
/// lunarnet, lunarnotes, lunardesk, lunarshot), by role, in the two looks
/// they wear: `tecno` everywhere but the Abyss session, and the glass in it.
#[derive(Debug, PartialEq)]
pub struct Ink {
    /// The window ground; under `abyss`, what [`frost`] makes the tint.
    pub ground: Rgb,
    /// A plate or card on the ground.
    pub surface: Rgb,
    /// A well sunk into a plate: fields, tracks.
    pub well: Rgb,
    pub hairline: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    /// Decoration and disabled text.
    pub dim: Rgb,
    /// `tecno`'s signal cyan; under `abyss` the near-white of a lit value.
    pub cyan: Rgb,
    /// The crescent gold; under `abyss` the selected label's gold.
    pub gold: Rgb,
    pub warn: Rgb,
    pub ok: Rgb,
    /// An attention colour stronger than `warn` (lunarnotes' urgent).
    pub amber: Rgb,
    /// A selected row's plate.
    pub sel: Rgb,
    /// The alpha of a full-screen veil drawn in `ground` behind a panel.
    pub veil: f32,
}

/// `tecno`: these clients' colours as they were.
pub const INK_TECNO: Ink = Ink {
    ground: (0x04, 0x07, 0x0d),
    surface: (0x0a, 0x14, 0x23),
    well: (0x06, 0x0d, 0x18),
    hairline: (0x1d, 0x35, 0x50),
    text: (0xe6, 0xf1, 0xff),
    muted: (0x8a, 0xa4, 0xc2),
    dim: (0x4a, 0x60, 0x7a),
    cyan: (0x38, 0xe1, 0xff),
    gold: (0xf2, 0xc1, 0x4e),
    warn: (0xff, 0x8a, 0x5c),
    ok: (0x6e, 0xe7, 0xa8),
    amber: (0xf6, 0x74, 0x00),
    sel: (0x0c, 0x2a, 0x3a),
    veil: 0.9,
};

/// `abyss`: the same roles in the glass.
pub const INK_ABYSS: Ink = Ink {
    ground: GROUND,
    surface: CARD,
    well: WELL,
    hairline: HAIRLINE,
    text: TEXT,
    muted: MUTED,
    dim: (0x5c, 0x5a, 0x57),
    cyan: (0xe8, 0xe6, 0xe3),
    gold: GOLD_TEXT,
    warn: DANGER,
    ok: OK,
    amber: (0xf2, 0x9a, 0x3c),
    sel: SELECT,
    veil: 0.45,
};

/// Read the look ONCE for a client that has no palette of its own.
fn this_look() -> Look {
    static LOOK: std::sync::OnceLock<Look> = std::sync::OnceLock::new();
    *LOOK.get_or_init(|| {
        let l = Look::current();
        enter(l);
        l
    })
}

/// Whether this process wears the glass (read once, see [`Look::current`]).
pub fn on() -> bool {
    this_look() == Look::Abyss
}

/// This process's [`Ink`].
pub fn ink() -> &'static Ink {
    if on() {
        &INK_ABYSS
    } else {
        &INK_TECNO
    }
}

/// A filled cell for an [`Ink`] client: rounded under the glass, square under
/// `tecno`, as the client drew it before.
pub fn cell(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, c: Rgb, a: f32) {
    if on() {
        cv.round_rect_a(x, y, w, h, R_CELL.min(h / 2), c, a);
    } else {
        cv.fill_rect_a(x, y, w, h, c, a);
    }
}

/// The outline that goes round a [`cell`].
pub fn outline(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
    if on() {
        cv.round_frame(x, y, w, h, R_CELL.min(h / 2), c);
    } else {
        for (a, b, rw, rh) in [
            (x, y, w, 1),
            (x, y + h - 1, w, 1),
            (x, y, 1, h),
            (x + w - 1, y, 1, h),
        ] {
            cv.fill_rect_a(a, b, rw, rh, c, 1.0);
        }
    }
}

/// `tecno`'s corner brackets; nothing under the glass, where focus is a
/// clearer cell and not a mark round it.
#[allow(clippy::too_many_arguments)]
pub fn brackets(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, len: i32, thick: i32, c: Rgb) {
    if !on() {
        cv.brackets(x, y, w, h, len, thick, c);
    }
}

/// A client's main panel: a square plate under `tecno`; under the glass a
/// rounded window of [`GROUND`] (frosted into the tint) inside a crisp rim.
pub fn panel(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
    if on() {
        cv.round_rect(x - 1, y - 1, w + 2, h + 2, R_WINDOW + 1, RIM);
        cv.round_rect(x, y, w, h, R_WINDOW, GROUND);
    } else {
        cv.fill_rect_a(x, y, w, h, c, 1.0);
    }
}

/// [`frost`], when this process wears the glass.
pub fn finish(cv: &mut Canvas) {
    if on() {
        frost(cv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(cv: &Canvas, x: usize) -> [u8; 4] {
        let mut buf = vec![0u8; cv.width() as usize * cv.height() as usize * 4];
        assert!(cv.blit_argb(&mut buf));
        [buf[x * 4], buf[x * 4 + 1], buf[x * 4 + 2], buf[x * 4 + 3]]
    }

    #[test]
    fn the_ground_becomes_the_tint_and_text_stays_solid() {
        let mut cv = Canvas::new(3, 1);
        cv.clear(GROUND);
        cv.fill_rect_a(1, 0, 1, 1, TEXT, 1.0);
        cv.fill_rect_a(2, 0, 1, 1, CARD, 1.0);
        frost(&mut cv);
        let ground = px(&cv, 0);
        assert_eq!(ground[3], (TINT * 255.0).round() as u8, "ground alpha");
        // Premultiplied: the colour is scaled with its alpha.
        assert!(ground[2] <= GROUND.0, "{ground:?}");
        assert_eq!(px(&cv, 1), [0xff, 0xff, 0xff, 0xff], "text is opaque");
        let card = px(&cv, 2)[3];
        assert!(
            card > ground[3] && card < 0xff,
            "a card is clearer glass: {card}"
        );
    }

    #[test]
    fn a_scrim_keeps_its_own_alpha() {
        let mut cv = Canvas::new(1, 1);
        cv.fill_rect_a(0, 0, 1, 1, GROUND, 0.45);
        let before = px(&cv, 0);
        frost(&mut cv);
        assert_eq!(px(&cv, 0), before);
    }

    #[test]
    fn only_the_abyss_look_wears_it() {
        assert!(!enter(Look::Tecno));
        assert!(!enter(Look::Eclipse));
        assert!(!enter(Look::Kde));
        assert!(!enter(Look::Win11));
    }
}
