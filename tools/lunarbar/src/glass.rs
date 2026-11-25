//! The shared ink of the clients drawn for `tecno` alone: their colours by
//! role ([`Ink`]) and the few shapes they all draw the same way.
//!
//! It held the `abyss` look's warm-black glass too, until the Abyss session
//! left the repository with the `desktop/` submodule.

use crate::draw::{Canvas, Rgb};

/// The colours of the clients that are drawn for `tecno` alone (lunarmon,
/// lunarnet, lunarnotes, lunardesk, lunarshot, lunarpanel), by role.
#[derive(Clone, Debug, PartialEq)]
pub struct Ink {
    /// The window ground.
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
    /// `tecno`'s signal cyan (or the accent chosen in the panel).
    pub cyan: Rgb,
    /// The crescent gold.
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

/// `tecno`: these clients' colours.
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

/// This process's [`Ink`].
pub fn ink() -> &'static Ink {
    // The accent chosen in the panel takes the place of tecno's cyan.
    static TECNO: std::sync::OnceLock<Ink> = std::sync::OnceLock::new();
    crate::acento::tenida(&TECNO, &INK_TECNO, |i: &mut Ink| {
        i.cyan = crate::acento::tinta(i.cyan)
    })
}

/// A filled, square cell.
pub fn cell(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, c: Rgb, a: f32) {
    cv.fill_rect_a(x, y, w, h, c, a);
}

/// The one-pixel outline that goes round a [`cell`].
pub fn outline(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
    for (a, b, rw, rh) in [
        (x, y, w, 1),
        (x, y + h - 1, w, 1),
        (x, y, 1, h),
        (x + w - 1, y, 1, h),
    ] {
        cv.fill_rect_a(a, b, rw, rh, c, 1.0);
    }
}

/// A client's main panel: a square plate.
pub fn panel(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
    cv.fill_rect_a(x, y, w, h, c, 1.0);
}
