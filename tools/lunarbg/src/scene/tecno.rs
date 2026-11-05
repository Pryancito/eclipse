//! The `tecno` wallpaper: mockup A's ("Orbital HUD") instrument face.
//!
//! Flat near-black ground with a wide teal wash behind the face, no stars and
//! no grid. On it sits a gold eclipse (a sun disc covered by a solid dark
//! moon offset up and to the right, so only a low-left crescent shows), with
//! hairline rings around it: a faint outer ring, a solid one, a dotted one
//! that turns very slowly, and two short instrument arcs (gold inside, cyan
//! outside) sweeping in opposite directions. Under it, the spaced caption
//! `E C L I P S E · S Y S T E M · K E R N E L · Z C O R E`.
//!
//! Radii are design units on the same 280 px logo as the cosmic scene, so
//! the face scales with the output exactly like the original does.

use super::{Layout, PixBuf, Rgb, Style};

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

/// Ground gradient ends (A's `ground` and `ground-2`).
pub(super) const GROUND_TOP: Rgb = rgb(0x04, 0x07, 0x0d);
pub(super) const GROUND_BOTTOM: Rgb = rgb(0x06, 0x0b, 0x14);
/// What the wash ADDS at the centre of the face (full strength there).
pub(super) const GLOW: Rgb = rgb(0x07, 0x22, 0x2b);
pub(super) const GLOW_R: f32 = 620.0;

const CYAN: Rgb = rgb(0x38, 0xe1, 0xff);
const GOLD: Rgb = rgb(0xf2, 0xc1, 0x4e);
const MUTED: Rgb = rgb(0x8a, 0xa4, 0xc2);
/// The moon: darker than the ground, so the eclipse reads as a body.
const VOID: Rgb = rgb(0x02, 0x04, 0x08);

const SUN_R: f32 = 100.0;
/// Moon centre relative to the face centre; the sun sits a little LOW so
/// the pair stays centred on the rings.
const MOON_DX: f32 = 30.0;
const SUN_DY: f32 = 15.0;
const R_INNER: f32 = 165.0;
const R_DOTS: f32 = 215.0;
const R_SOLID: f32 = 255.0;
const R_OUTER: f32 = 330.0;
const CAPTION_DY: f32 = 293.0;
const CAPTION: &str = "ECLIPSE · SYSTEM · KERNEL · ZCORE";
const CAPTION_ADV: f32 = 13.5;

/// Outermost thing drawn (the outer ring, its stroke and the caption's
/// half-width all fit inside), in design units.
pub(super) const REACH: f32 = 340.0;

/// Which wallpaper a look gets: `$ECLIPSE_LOOK` (a launcher override) wins,
/// then the `look=` line of `/etc/eclipse/look`, then the image default,
/// which is tecno. Only `tecno` gets this face; every other look keeps the
/// cosmic original. A name that is no look at all is ignored, as lunarbar
/// ignores it, so a typo cannot switch the wallpaper on its own.
pub fn style_from(env: Option<&str>, file: Option<&str>) -> Style {
    let known = |n: &str| {
        let n = n.trim().to_ascii_lowercase();
        matches!(n.as_str(), "eclipse" | "kde" | "win11" | "gnome" | "tecno").then_some(n)
    };
    let from_file = || {
        file?.lines().find_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            (k.trim() == "look").then(|| known(v)).flatten()
        })
    };
    match env.and_then(known).or_else(from_file).as_deref() {
        Some("tecno") | None => Style::Tecno,
        Some(_) => Style::Cosmic,
    }
}

pub(super) fn frame(pb: &mut PixBuf, lay: &Layout, t_ms: u64) {
    let (cx, cy, s, px) = (lay.cx, lay.cy, lay.s, lay.px);
    // Degrees, folded to a period before use (see render_frame's note).
    let deg = |per_s: f64| ((t_ms as f64 / 1000.0 * per_s) % 360.0) as f32;
    let hair = 1.0 * px;

    pb.ring(cx, cy, R_OUTER * s, hair, CYAN, 0.07);
    pb.ring(cx, cy, R_SOLID * s, hair, CYAN, 0.20);
    pb.ring(cx, cy, R_INNER * s, hair, CYAN, 0.06);

    // Dotted ring, one dot every 2 degrees, a full turn every 30 minutes.
    let spin = deg(0.2);
    for k in 0..180 {
        let a = (k as f32 * 2.0 + spin).to_radians();
        let (sin, cos) = a.sin_cos();
        pb.dot(
            cx + cos * R_DOTS * s * pb.sx,
            cy + sin * R_DOTS * s,
            0.7 * px,
            CYAN,
            0.30,
        );
    }

    // Instrument arcs: gold on the inner ring, cyan on the solid one.
    pb.sweep(cx, cy, R_INNER * s, 20.0 + deg(3.0), 42.0, 1.6 * px, GOLD, 0.55);
    pb.sweep(cx, cy, R_SOLID * s, 200.0 - deg(2.0), 26.0, 1.6 * px, CYAN, 0.55);

    // The eclipse: the gold sun, then the dark moon over it, with a faint
    // corona on the moon's limb where it stands out against the ground.
    let sun_r = SUN_R * s;
    pb.disc(cx, cy + SUN_DY * s, sun_r, GOLD, 1.0);
    let (mx, my) = (cx + MOON_DX * s * pb.sx, cy);
    pb.disc(mx, my, sun_r, VOID, 1.0);
    pb.ring(mx, my, sun_r + 1.5 * px, hair, CYAN, 0.10);

    // Spaced caption: ' ' is an empty cell, '·' a dot.
    let adv = CAPTION_ADV * s;
    let n = CAPTION.chars().count() as f32;
    let left = cx - (n - 1.0) * adv / 2.0;
    let ty = cy + CAPTION_DY * s;
    let scale = (s.round() as usize).max(1);
    for (i, ch) in CAPTION.chars().enumerate() {
        let gx = left + i as f32 * adv;
        match ch {
            ' ' => {}
            '·' => pb.dot(gx, ty, 1.0 * px, MUTED, 0.6),
            _ => pb.glyph(ch, gx, ty, scale, MUTED, 0.75),
        }
    }
}

impl PixBuf<'_> {
    /// Solid anti-aliased disc (an ellipse on a squeezed output, like every
    /// circle here), by scanline span.
    fn disc(&mut self, cx: f32, cy: f32, r: f32, c: Rgb, alpha: f32) {
        let (y0, y1) = self.clip_span_y(cy, r + 1.0);
        for y in y0..y1 {
            let dy = y as f32 - cy;
            let s2 = (r + 0.5) * (r + 0.5) - dy * dy;
            if s2 <= 0.0 {
                continue;
            }
            let half = s2.sqrt() * self.sx + 1.0;
            let (x0, x1) = self.clip_x_range(cx - half, cx + half);
            for x in x0..x1 {
                let d = self.edist(x as f32, y as f32, cx, cy);
                let cover = (r - d + 0.5).clamp(0.0, 1.0);
                if cover > 0.0 {
                    self.blend(x, y, c, alpha * cover);
                }
            }
        }
    }

    /// An arc without the cosmic scene's glowing end dots: a plain stroke.
    #[allow(clippy::too_many_arguments)]
    fn sweep(&mut self, cx: f32, cy: f32, r: f32, start: f32, span: f32, t: f32, c: Rgb, a: f32) {
        const SEGS: usize = 24;
        let sx = self.sx;
        let at = |k: usize| {
            let (sin, cos) = (start + span * k as f32 / SEGS as f32).to_radians().sin_cos();
            (cx + cos * r * sx, cy + sin * r)
        };
        for k in 0..SEGS {
            let (p, q) = (at(k), at(k + 1));
            self.line(p.0, p.1, q.0, q.1, t, c, a);
        }
    }

    /// A capital the height of a 5x7 glyph at `scale`, centred at (gx, gy),
    /// from DejaVu Sans Mono's outline with real coverage at its edges. With
    /// no face (or a character it lacks) it is the 5x7 glyph.
    fn glyph(&mut self, ch: char, gx: f32, gy: f32, scale: usize, c: Rgb, alpha: f32) {
        if !cfg!(test) && self.glyph_vector(ch, gx, gy, scale, c, alpha) {
            return;
        }
        self.glyph5x7(ch, gx, gy, scale, c, alpha);
    }

    #[allow(clippy::too_many_arguments)]
    fn glyph_vector(&mut self, ch: char, gx: f32, gy: f32, scale: usize, c: Rgb, alpha: f32) -> bool {
        // DejaVu's capitals are 0.729 em tall: an em that makes them exactly
        // the 5x7 glyph's 7 cells, on a baseline one pixel under them.
        let cap = (7 * scale) as f32;
        let em = cap / 0.729;
        let cell = lunartext::Cell {
            width: (em * 0.602).ceil() + 2.0,
            em,
            baseline: cap + 1.0,
        };
        let h = (cap + 2.0 + em * 0.25).ceil() as u32;
        let Some((w, h, mask)) = lunartext::coverage(ch, lunartext::Weight::Regular, cell, h) else {
            return false;
        };
        let left = (gx - w as f32 / 2.0).round() as i32;
        let top = (gy - cap / 2.0 - 1.0).round() as i32;
        for row in 0..h as i32 {
            for col in 0..w as i32 {
                let cov = mask[(row * w as i32 + col) as usize];
                let (x, y) = (left + col, top + row);
                if cov > 0 && x >= 0 && y >= 0 {
                    self.blend(x as usize, y as usize, c, alpha * cov as f32 / 255.0);
                }
            }
        }
        true
    }

    /// The 5x7 glyph centred at (gx, gy) with no outline: the caption sits on
    /// a flat ground and an outline would only thicken it.
    fn glyph5x7(&mut self, ch: char, gx: f32, gy: f32, scale: usize, c: Rgb, alpha: f32) {
        let left = (gx - (5 * scale) as f32 / 2.0) as i32;
        let top = (gy - (7 * scale) as f32 / 2.0) as i32;
        for (row, bits) in super::glyph5x7(ch).iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = left + (col * scale + sx) as i32;
                        let y = top + (row * scale + sy) as i32;
                        if x >= 0 && y >= 0 {
                            self.blend(x as usize, y as usize, c, alpha);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{layout_styled, render_base_styled, render_frame};
    use super::*;

    #[test]
    fn the_look_picks_the_face_and_tecno_is_the_default() {
        assert_eq!(style_from(None, None), Style::Tecno);
        assert_eq!(style_from(None, Some("look=eclipse\n")), Style::Cosmic);
        assert_eq!(style_from(None, Some("# x\nlook = tecno\n")), Style::Tecno);
        assert_eq!(style_from(Some("KDE"), Some("look=tecno")), Style::Cosmic);
        assert_eq!(style_from(Some("tecno"), Some("look=win11")), Style::Tecno);
        // Not a look: ignored, the file still decides.
        assert_eq!(style_from(Some("nope"), Some("look=gnome")), Style::Cosmic);
        assert_eq!(style_from(None, Some("look=nope")), Style::Tecno);
    }

    fn pixel(buf: &[u8], w: usize, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * w + x) * 4;
        (buf[i + 2], buf[i + 1], buf[i])
    }

    #[test]
    fn the_face_is_a_gold_crescent_low_left_of_a_dark_moon() {
        let (w, h) = (1440, 900);
        let lay = layout_styled(w, h, None, 1, Style::Tecno);
        let base = render_base_styled(w, h, None, 1, Style::Tecno).expect("base");
        let mut f = base.clone();
        render_frame(&mut f, w, &base, &lay, 0);
        let (cx, cy, s) = (lay.cx, lay.cy, lay.s);
        // Low-left inside the sun, outside the moon: gold.
        let (r, g, b) = pixel(&f, w, (cx - 80.0 * s) as usize, (cy + 50.0 * s) as usize);
        assert!(r > 200 && g > 150 && b < 120, "crescent {r},{g},{b}");
        // The moon's centre: darker than the ground around the face.
        let moon = pixel(&f, w, (cx + MOON_DX * s) as usize, cy as usize);
        let ground = pixel(&f, w, 4, 4);
        assert!(moon.2 <= ground.2 && moon.0 <= 8, "moon {moon:?} ground {ground:?}");
    }

    #[test]
    fn the_ground_has_no_stars_and_a_teal_wash_at_the_centre() {
        let (w, h) = (640, 400);
        let lay = layout_styled(w, h, None, 1, Style::Tecno);
        let base = render_base_styled(w, h, None, 1, Style::Tecno).expect("base");
        let brightest = base.chunks(4).map(|p| p[0].max(p[1]).max(p[2])).max().unwrap();
        assert!(brightest < 80, "a star or grid line in the tecno ground: {brightest}");
        let (_, g, b) = pixel(&base, w, lay.cx as usize, lay.cy as usize);
        let (_, g0, b0) = pixel(&base, w, 0, 0);
        assert!(g > g0 + 15 && b > b0 + 15, "no wash: centre {g},{b} corner {g0},{b0}");
    }

    #[test]
    fn everything_the_face_draws_stays_inside_its_region() {
        let (w, h) = (1920, 1080);
        let lay = layout_styled(w, h, None, 1, Style::Tecno);
        let base = render_base_styled(w, h, None, 1, Style::Tecno).expect("base");
        let mut f = base.clone();
        render_frame(&mut f, w, &base, &lay, 123_456);
        let (rx, ry, rw, rh) = lay.region;
        // The caption's last letter is the widest point; it must be well
        // inside the region, not clipped by it.
        let half = (CAPTION.chars().count() as f32 - 1.0) * CAPTION_ADV * lay.s / 2.0 + 4.0;
        assert!(lay.cx - half > rx as f32 && lay.cx + half < (rx + rw) as f32);
        assert!(lay.cy + (CAPTION_DY + 5.0) * lay.s < (ry + rh) as f32);
        assert!(lay.cy - (R_OUTER + 2.0) * lay.s > ry as f32);
        // And outside it nothing changed.
        for y in (0..h).step_by(7) {
            for x in (0..w).step_by(7) {
                if x < rx || x >= rx + rw || y < ry || y >= ry + rh {
                    let i = (y * w + x) * 4;
                    assert_eq!(f[i..i + 4], base[i..i + 4], "({x},{y})");
                }
            }
        }
    }
}
