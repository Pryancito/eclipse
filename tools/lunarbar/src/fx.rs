//! The menus' effects: a translucent panel and a short computed entrance.
//!
//! Every popup the bar opens (launcher, calendar, window and session menus,
//! hunter's) used to appear at once, fully opaque. Now it rises into place
//! from a few pixels off its bar while it fades in, over [`OPEN`], on an
//! ease-out curve, and in tecno its panel lets the desktop show through.
//!
//! What it costs, the "sigma" of the effect: the entrance repaints the popup
//! once per display frame for [`OPEN`] (about 9 frames at 60 Hz, of a surface
//! the size of the menu, not of the output), then never again; the
//! translucency costs nothing, the surface was ARGB already. `LUNAR_FX=off`
//! switches both off, for a slow machine or a taste for stillness.
//!
//! A child of `main.rs` because that file is at its size ceiling (see
//! `xtask/src/tamano.rs`).

use std::time::Duration;

/// How long a menu takes to open.
pub const OPEN: Duration = Duration::from_millis(150);

/// How far (px) the panel travels while it opens.
const RISE: f32 = 8.0;

/// The panel's opacity in tecno: enough of the desktop shows through to
/// read as glass, not so much that text over a busy wallpaper suffers.
pub const GLASS: f32 = 0.86;

/// Whether effects are on (`LUNAR_FX=off` switches them off).
pub fn on() -> bool {
    !std::env::var("LUNAR_FX").is_ok_and(|v| v == "off" || v == "0")
}

/// Where the entrance is `elapsed` after the menu first drew: `(opacity,
/// vertical offset)`, or `None` once it has finished (or effects are off).
/// A menu that hangs below a top bar drops into place; one above a bottom
/// bar rises.
pub fn opening(elapsed: Duration, at_top: bool) -> Option<(f32, i32)> {
    if !on() || elapsed >= OPEN {
        return None;
    }
    let t = elapsed.as_secs_f32() / OPEN.as_secs_f32();
    // Ease-out cubic: quick start, soft landing.
    let e = 1.0 - (1.0 - t).powi(3);
    let off = ((1.0 - e) * RISE).round() as i32;
    Some((e.max(0.05), if at_top { -off } else { off }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entrance_starts_faint_and_off_its_place_and_lands_exactly() {
        if !on() {
            assert!(opening(Duration::ZERO, false).is_none(), "LUNAR_FX=off is still");
            return;
        }
        let (k0, dy0) = opening(Duration::ZERO, false).unwrap();
        assert!(k0 < 0.1 && dy0 == RISE as i32);
        // From a top bar it comes from above instead.
        assert_eq!(opening(Duration::ZERO, true).unwrap().1, -(RISE as i32));
        // Monotonic: never brighter-then-dimmer, never overshooting.
        let mut last = (0.0, i32::MAX);
        for ms in (0..150).step_by(10) {
            let (k, dy) = opening(Duration::from_millis(ms), false).unwrap();
            assert!(k >= last.0 && dy <= last.1 && dy >= 0, "{ms} ms");
            last = (k, dy);
        }
        assert!(opening(OPEN, false).is_none());
    }
}
