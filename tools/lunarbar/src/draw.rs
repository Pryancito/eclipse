//! Software drawing for lunarbar, built on two pure-Rust crates that keep the
//! binary a static musl executable with zero system dependencies:
//!
//! - **tiny-skia** rasterises every shape with real anti-aliasing: the rounded
//!   pills, the ◑/☾ launcher discs (vector paths + mask, no hand-rolled
//!   coverage math), the ▼/▲ triangles, and the load gauges — which get a
//!   green→amber→red linear gradient for free.
//! - **embedded-graphics** supplies the text: mature ISO-8859-1 bitmap fonts
//!   (FONT_9X15 + real bold), so lowercase, accents and eñes in window titles
//!   render properly. tiny-skia has no text support; e-g has no AA shapes —
//!   together they cover each other's blind spot.
//!
//! `Canvas` owns an RGBA tiny-skia `Pixmap`; bars draw into it and then
//! `blit_xrgb` swizzles the finished frame into the wl_shm XRGB8888 buffer.

use embedded_graphics::{
    mono_font::{
        iso_8859_1::{FONT_9X15, FONT_9X15_BOLD},
        MonoTextStyle,
    },
    pixelcolor::Rgb888,
    prelude::*,
    text::{Baseline, Text},
};
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Mask, Paint, Path, PathBuilder, Pixmap,
    PixmapPaint, Rect, SpreadMode, Stroke, Transform,
};

pub type Rgb = (u8, u8, u8);

/// Glyph cell height of the bar font (FONT_9X15).
pub const GLYPH_H: i32 = 15;
/// Glyph advance (cell width) of the bar font.
pub const GLYPH_W: i32 = 9;

/// Cubic-Bézier circle constant (approximates a 90° arc).
const K: f32 = 0.552_284_8;

#[inline]
fn color(c: Rgb, a: f32) -> Color {
    // The clamp is for the reader, not for the cast: `as u8` from `f32`
    // saturates in Rust, so an alpha outside 0..1 lands on the same byte
    // either way. No test can tell the two apart.
    Color::from_rgba8(c.0, c.1, c.2, (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// An RGBA scratch frame with AA vector drawing (tiny-skia) and bitmap text
/// (embedded-graphics), blitted to XRGB8888 when the frame is complete.
pub struct Canvas {
    pix: Pixmap,
}

impl Canvas {
    /// Fallible constructor — OOM / absurd sizes must not abort the panel
    /// (`panic = "abort"` in release).
    pub fn try_new(w: usize, h: usize) -> Option<Self> {
        let pix = Pixmap::new(w.max(1) as u32, h.max(1) as u32)?;
        Some(Self { pix })
    }

    pub fn new(w: usize, h: usize) -> Self {
        Self::try_new(w, h).expect("pixmap alloc")
    }

    #[allow(dead_code)]
    pub fn width(&self) -> u32 {
        self.pix.width()
    }

    #[allow(dead_code)]
    pub fn height(&self) -> u32 {
        self.pix.height()
    }

    fn paint<'a>(c: Rgb, a: f32) -> Paint<'a> {
        let mut p = Paint::default();
        p.set_color(color(c, a));
        p.anti_alias = true;
        p
    }

    fn fill(&mut self, path: &Path, c: Rgb, a: f32) {
        self.pix
            .fill_path(path, &Self::paint(c, a), FillRule::Winding, Transform::identity(), None);
    }

    /// Fill the whole canvas with a solid colour.
    pub fn clear(&mut self, c: Rgb) {
        self.pix.fill(color(c, 1.0));
    }

    /// Horizontal 1px line (used for the bars' accent rules).
    pub fn hline(&mut self, x0: i32, y: i32, len: i32, c: Rgb, a: f32) {
        if let Some(r) = Rect::from_xywh(x0 as f32, y as f32, len.max(0) as f32, 1.0) {
            self.fill(&PathBuilder::from_rect(r), c, a);
        }
    }

    /// A faint vertical separator line, centred in a bar of height `h`, ~half
    /// the bar tall. Cleaner than a dot for grouping modules.
    pub fn vrule(&mut self, x: i32, h: i32, c: Rgb) {
        let y0 = (h / 4) as f32;
        let y1 = (h - h / 4) as f32;
        if let Some(r) = Rect::from_xywh(x as f32, y0, 1.0, y1 - y0) {
            self.fill(&PathBuilder::from_rect(r), c, 0.30);
        }
    }

    /// A filled rounded rectangle, corner radius `rad`. Used for the clock and
    /// date pills and the active taskbar button, matching waybar's
    /// `border-radius: 6px` — now genuinely round thanks to tiny-skia's AA.
    pub fn round_rect(&mut self, x: i32, y: i32, rw: i32, rh: i32, rad: i32, c: Rgb) {
        if let Some(p) = rounded_rect_path(x as f32, y as f32, rw as f32, rh as f32, rad as f32) {
            self.fill(&p, c, 1.0);
        }
    }

    /// A small solid triangle in an `s`x`s` box at (x,y). `up=true` points up
    /// (tip at top → upload), else down (tip at bottom → download).
    pub fn triangle(&mut self, x: i32, y: i32, s: i32, up: bool, c: Rgb) {
        let (x, y, s) = (x as f32, y as f32, s as f32);
        let mut pb = PathBuilder::new();
        if up {
            pb.move_to(x + s / 2.0, y);
            pb.line_to(x + s, y + s);
            pb.line_to(x, y + s);
        } else {
            pb.move_to(x, y);
            pb.line_to(x + s, y);
            pb.line_to(x + s / 2.0, y + s);
        }
        pb.close();
        if let Some(p) = pb.finish() {
            self.fill(&p, c, 1.0);
        }
    }

    /// A small solid triangle pointing left/right in an `s`x`s` box at (x,y) —
    /// the calendar's month-nav arrows.
    pub fn triangle_h(&mut self, x: i32, y: i32, s: i32, left: bool, c: Rgb) {
        let (x, y, s) = (x as f32, y as f32, s as f32);
        let mut pb = PathBuilder::new();
        if left {
            pb.move_to(x + s, y);
            pb.line_to(x + s, y + s);
            pb.line_to(x, y + s / 2.0);
        } else {
            pb.move_to(x, y);
            pb.line_to(x, y + s);
            pb.line_to(x + s, y + s / 2.0);
        }
        pb.close();
        if let Some(p) = pb.finish() {
            self.fill(&p, c, 1.0);
        }
    }

    /// A mini horizontal gauge: a pill-shaped dark track filled `frac` (0..1)
    /// of its width with a green→amber→red gradient (foot-terminal palette),
    /// so a busy metric reads at a glance.
    pub fn gauge(&mut self, x: i32, y: i32, gw: i32, gh: i32, frac: f32, track: Rgb) {
        let frac = frac.clamp(0.0, 1.0);
        // `rounded_rect_path` caps the radius at half the shorter side
        // itself, so passing `gh` instead of `gh / 2` draws the same bar at
        // every even height --- measured, not assumed. On an odd height (the
        // modules draw a seven pixel gauge) the two differ by half a pixel of
        // corner radius, which is a difference in antialiasing alone.
        let rad = gh / 2;
        self.round_rect(x, y, gw, gh, rad, track);
        let filled = (gw as f32 * frac).round();
        if filled < 1.0 {
            // Taking this guard out changes nothing: the only value it
            // catches is zero, and `rounded_rect_path` returns `None` for a
            // width of zero, so the `let ... else` below returns anyway.
            // It is here to say so without building a path first.
            return;
        }
        let Some(p) = rounded_rect_path(x as f32, y as f32, filled, gh as f32, rad as f32) else {
            return;
        };
        // Gradient spans the FULL track, revealed by the fill width, so the
        // visible leading edge carries the colour of the current level.
        let stops = vec![
            GradientStop::new(0.0, Color::from_rgba8(0x8f, 0xd1, 0x8a, 0xff)), // green
            GradientStop::new(0.5, Color::from_rgba8(0xe0, 0xc0, 0x7a, 0xff)), // amber
            GradientStop::new(1.0, Color::from_rgba8(0xe0, 0x7a, 0x7a, 0xff)), // red
        ];
        if let Some(shader) = LinearGradient::new(
            tiny_skia::Point::from_xy(x as f32, y as f32),
            tiny_skia::Point::from_xy((x + gw) as f32, y as f32),
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            let mut paint = Paint::default();
            paint.shader = shader;
            paint.anti_alias = true;
            self.pix
                .fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    /// The ◑ launcher: an outlined circle whose right half is filled. Matches
    /// the waybar `custom/launcher` glyph.
    pub fn disc_half(&mut self, x: i32, y: i32, d: i32, c: Rgb) {
        let r = d as f32 / 2.0;
        let cx = x as f32 + r;
        let cy = y as f32 + r;
        // Outer ring.
        if let Some(circle) = PathBuilder::from_circle(cx, cy, r - 0.75) {
            self.pix.stroke_path(
                &circle,
                &Self::paint(c, 1.0),
                &Stroke {
                    width: 1.5,
                    ..Stroke::default()
                },
                Transform::identity(),
                None,
            );
        }
        // Right-half fill: a semicircle built from two quarter-arc cubics.
        let ri = r - 0.5;
        let mut pb = PathBuilder::new();
        pb.move_to(cx, cy - ri);
        pb.cubic_to(cx + K * ri, cy - ri, cx + ri, cy - K * ri, cx + ri, cy);
        pb.cubic_to(cx + ri, cy + K * ri, cx + K * ri, cy + ri, cx, cy + ri);
        pb.close();
        if let Some(p) = pb.finish() {
            self.fill(&p, c, 1.0);
        }
    }

    /// The ☾ crescent: the sun disc masked by an offset moon disc — done with
    /// a real inverted clip mask instead of hand-rolled coverage math.
    /// Mirrors lunarbg's eclipse crescent so the top bar matches the wallpaper.
    pub fn crescent(&mut self, x: i32, y: i32, d: i32, c: Rgb) {
        if d <= 0 {
            // At zero this is belt and braces: `Pixmap::new(0, 0)` returns
            // `None` and the `let ... else` below returns too. It is a
            // negative `d` that needs catching, before the cast to `u32`.
            return;
        }
        // Rasterise into a d×d scratch pixmap, not against a full-canvas mask:
        // the app menu draws this on an output-sized canvas, where a
        // screen-sized Mask would mean a multi-megabyte alloc plus two
        // full-screen passes (fill + invert) for an 18px glyph, on every
        // repaint.
        let s = d as u32;
        let r = d as f32 / 2.0;
        let (cx, cy) = (r, r);
        let (mx, my, mr) = (cx + r * 0.42, cy - r * 0.10, r * 0.92);
        let (Some(sun), Some(moon)) = (
            PathBuilder::from_circle(cx, cy, r),
            PathBuilder::from_circle(mx, my, mr),
        ) else {
            return;
        };
        let (Some(mut glyph), Some(mut mask)) = (Pixmap::new(s, s), Mask::new(s, s)) else {
            return;
        };
        mask.fill_path(&moon, FillRule::Winding, true, Transform::identity());
        mask.invert();
        glyph.fill_path(
            &sun,
            &Self::paint(c, 1.0),
            FillRule::Winding,
            Transform::identity(),
            Some(&mask),
        );
        self.pixmap(x, y, &glyph);
    }

    /// Bright accent indicator line at the bottom edge of an active taskbar button.
    pub fn active_line(&mut self, x: i32, y: i32, w: i32, c: Rgb) {
        if let Some(p) = rounded_rect_path((x + 4) as f32, y as f32, (w - 8).max(4) as f32, 2.0, 1.0) {
            self.fill(&p, c, 1.0);
        }
    }

    /// Vector power symbol (⏻): circle arc + top vertical line in white.
    pub fn power_icon(&mut self, x: i32, y: i32, s: i32, c: Rgb) {
        let (x, y, s) = (x as f32, y as f32, s as f32);
        let r = s / 2.0 - 1.0;
        let cx = x + s / 2.0;
        let cy = y + s / 2.0;
        let stroke = Stroke { width: 1.8, ..Stroke::default() };
        // Circle arc
        let mut pb = PathBuilder::new();
        pb.move_to(cx + r * 0.5, cy - r * 0.866);
        pb.cubic_to(cx + r * 1.2, cy, cx + r * 0.5, cy + r * 1.1, cx, cy + r);
        pb.cubic_to(cx - r * 0.5, cy + r * 1.1, cx - r * 1.2, cy, cx - r * 0.5, cy - r * 0.866);
        if let Some(p) = pb.finish() {
            self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
        }
        // Top vertical bar
        let mut pb2 = PathBuilder::new();
        pb2.move_to(cx, cy - r * 1.1);
        pb2.line_to(cx, cy - r * 0.1);
        if let Some(p) = pb2.finish() {
            self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
        }
    }

    /// Vector speaker icon (🔊 / 🔇)
    pub fn volume_icon(&mut self, x: i32, y: i32, s: i32, c: Rgb, muted: bool) {
        let (x, y, s) = (x as f32, y as f32, s as f32);
        let stroke = Stroke { width: 1.5, ..Stroke::default() };
        // Speaker box + cone
        let mut pb = PathBuilder::new();
        pb.move_to(x, y + s * 0.35);
        pb.line_to(x + s * 0.25, y + s * 0.35);
        pb.line_to(x + s * 0.5, y + s * 0.15);
        pb.line_to(x + s * 0.5, y + s * 0.85);
        pb.line_to(x + s * 0.25, y + s * 0.65);
        pb.line_to(x, y + s * 0.65);
        pb.close();
        if let Some(p) = pb.finish() {
            self.fill(&p, c, 1.0);
        }
        if muted {
            // X mark
            let mut pb_x = PathBuilder::new();
            pb_x.move_to(x + s * 0.65, y + s * 0.35);
            pb_x.line_to(x + s * 0.95, y + s * 0.65);
            pb_x.move_to(x + s * 0.95, y + s * 0.35);
            pb_x.line_to(x + s * 0.65, y + s * 0.65);
            if let Some(p) = pb_x.finish() {
                self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
            }
        } else {
            // Sound wave arcs
            let mut pb_wave = PathBuilder::new();
            pb_wave.move_to(x + s * 0.65, y + s * 0.35);
            pb_wave.cubic_to(x + s * 0.8, y + s * 0.45, x + s * 0.8, y + s * 0.55, x + s * 0.65, y + s * 0.65);
            if let Some(p) = pb_wave.finish() {
                self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
            }
        }
    }

    /// Vector padlock icon (🔒)
    pub fn lock_icon(&mut self, x: i32, y: i32, s: i32, c: Rgb) {
        let (x, y, s) = (x as f32, y as f32, s as f32);
        let stroke = Stroke { width: 1.5, ..Stroke::default() };
        let mut pb = PathBuilder::new();
        pb.move_to(x + s * 0.3, y + s * 0.45);
        pb.line_to(x + s * 0.3, y + s * 0.25);
        pb.cubic_to(x + s * 0.3, y + s * 0.05, x + s * 0.7, y + s * 0.05, x + s * 0.7, y + s * 0.25);
        pb.line_to(x + s * 0.7, y + s * 0.45);
        if let Some(p) = pb.finish() {
            self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
        }
        if let Some(p) = rounded_rect_path(x + s * 0.2, y + s * 0.45, s * 0.6, s * 0.5, 2.0) {
            self.fill(&p, c, 1.0);
        }
    }

    /// Vector logout icon (🚪 / ➔)
    pub fn exit_icon(&mut self, x: i32, y: i32, s: i32, c: Rgb) {
        let (x, y, s) = (x as f32, y as f32, s as f32);
        let stroke = Stroke { width: 1.5, ..Stroke::default() };
        let mut pb = PathBuilder::new();
        pb.move_to(x + s * 0.5, y + s * 0.15);
        pb.line_to(x + s * 0.15, y + s * 0.15);
        pb.line_to(x + s * 0.15, y + s * 0.85);
        pb.line_to(x + s * 0.5, y + s * 0.85);
        if let Some(p) = pb.finish() {
            self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
        }
        let mut pb_arr = PathBuilder::new();
        pb_arr.move_to(x + s * 0.35, y + s * 0.5);
        pb_arr.line_to(x + s * 0.85, y + s * 0.5);
        pb_arr.move_to(x + s * 0.65, y + s * 0.3);
        pb_arr.line_to(x + s * 0.85, y + s * 0.5);
        pb_arr.line_to(x + s * 0.65, y + s * 0.7);
        if let Some(p) = pb_arr.finish() {
            self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
        }
    }

    /// Vector reboot icon (🔄)
    pub fn reboot_icon(&mut self, x: i32, y: i32, s: i32, c: Rgb) {
        let (x, y, s) = (x as f32, y as f32, s as f32);
        let stroke = Stroke { width: 1.5, ..Stroke::default() };
        let cx = x + s / 2.0;
        let cy = y + s / 2.0;
        let r = s * 0.35;
        let mut pb = PathBuilder::new();
        pb.move_to(cx + r, cy);
        pb.cubic_to(cx + r, cy + r * 1.2, cx - r * 1.2, cy + r, cx - r, cy);
        pb.cubic_to(cx - r, cy - r * 1.2, cx + r * 0.8, cy - r, cx + r * 0.7, cy - r * 0.3);
        if let Some(p) = pb.finish() {
            self.pix.stroke_path(&p, &Self::paint(c, 1.0), &stroke, Transform::identity(), None);
        }
        let mut pb_ah = PathBuilder::new();
        pb_ah.move_to(cx + r * 0.3, cy - r * 0.6);
        pb_ah.line_to(cx + r * 0.7, cy - r * 0.3);
        pb_ah.line_to(cx + r * 0.9, cy - r * 0.7);
        if let Some(p) = pb_ah.finish() {
            self.fill(&p, c, 1.0);
        }
    }

    /// Draw a left-aligned string (FONT_9X15, transparent background) with its
    /// cell top at `y`. Returns the advance in pixels.
    pub fn text(&mut self, s: &str, x: i32, y: i32, c: Rgb) -> i32 {
        let style = MonoTextStyle::new(&FONT_9X15, Rgb888::new(c.0, c.1, c.2));
        Text::with_baseline(s, Point::new(x, y), style, Baseline::Top)
            .draw(self)
            .map(|end| end.x - x)
            .unwrap_or(0)
    }

    /// Bold variant of `text` (FONT_9X15_BOLD, same metrics). Matches waybar's
    /// `font-weight: bold` clock.
    pub fn text_bold(&mut self, s: &str, x: i32, y: i32, c: Rgb) -> i32 {
        let style = MonoTextStyle::new(&FONT_9X15_BOLD, Rgb888::new(c.0, c.1, c.2));
        Text::with_baseline(s, Point::new(x, y), style, Baseline::Top)
            .draw(self)
            .map(|end| end.x - x)
            .unwrap_or(0)
    }

    /// Pixel width a string will occupy (monospace: chars × cell width).
    pub fn text_width(s: &str) -> i32 {
        s.chars().count() as i32 * GLYPH_W
    }

    /// Alpha-blend a pre-scaled icon pixmap at (x,y) — the taskbar/menu icon
    /// slot (icons::IconCache hands out pixmaps already at slot size).
    pub fn pixmap(&mut self, x: i32, y: i32, pm: &Pixmap) {
        self.pix.draw_pixmap(
            x,
            y,
            pm.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }

    /// Letter fallback for a missing icon: a rounded square with the app's
    /// bold initial, so every button/row keeps a consistent icon slot.
    pub fn badge(&mut self, x: i32, y: i32, s: i32, ch: char, bg: Rgb, fg: Rgb) {
        self.round_rect(x, y, s, s, (s / 4).max(3), bg);
        let tx = x + (s - GLYPH_W) / 2 + 1;
        let ty = y + (s - GLYPH_H) / 2;
        let up: String = ch.to_uppercase().take(1).collect();
        self.text_bold(&up, tx, ty, fg);
    }

    /// Swizzle the finished RGBA frame into an XRGB8888 (B,G,R,X) buffer.
    /// `dst` must be at least w*h*4 bytes. Returns false if sizes disagree
    /// (never panics — the panel must stay up under a bad configure).
    pub fn blit_xrgb(&self, dst: &mut [u8]) -> bool {
        let src = self.pix.data();
        let w = self.pix.width() as usize;
        let h = self.pix.height() as usize;
        let n = w * h;
        if dst.len() < n * 4 || src.len() < n * 4 {
            return false;
        }
        // Full-surface swizzle; see blit_argb. The size gate keeps the thin
        // status bars serial and lets the big preview/menu blits fan out.
        crate::par::par_rows(&mut dst[..n * 4], h, w * 4, |y0, band| {
            for (ry, row) in band.chunks_mut(w * 4).enumerate() {
                let base = (y0 + ry) * w * 4;
                for x in 0..w {
                    let o = x * 4;
                    let s = base + o;
                    // Everything drawn is opaque (the bar clears to a solid
                    // ground), so premultiplied RGBA here equals straight RGB.
                    row[o] = src[s + 2]; // B
                    row[o + 1] = src[s + 1]; // G
                    row[o + 2] = src[s]; // R
                    row[o + 3] = 0xff; // X
                }
            }
        });
        true
    }

    /// Nearest-neighbour upscale from the logical canvas into a
    /// `scale`-times-larger XRGB buffer (HiDPI without rewriting every draw).
    pub fn blit_xrgb_scaled(&self, dst: &mut [u8], scale: u32) -> bool {
        let scale = scale.max(1) as usize;
        if scale == 1 {
            return self.blit_xrgb(dst);
        }
        let src = self.pix.data();
        let w = self.pix.width() as usize;
        let h = self.pix.height() as usize;
        let bw = w * scale;
        let bh = h * scale;
        if dst.len() < bw * bh * 4 || src.len() < w * h * 4 {
            return false;
        }
        for y in 0..bh {
            let sy = y / scale;
            for x in 0..bw {
                let sx = x / scale;
                let s = (sy * w + sx) * 4;
                let o = (y * bw + x) * 4;
                dst[o] = src[s + 2];
                dst[o + 1] = src[s + 1];
                dst[o + 2] = src[s];
                dst[o + 3] = 0xff;
            }
        }
        true
    }

    /// Swizzle the finished RGBA frame into an ARGB8888 (B,G,R,A) buffer,
    /// preserving alpha. tiny-skia stores premultiplied RGBA and wl_shm's
    /// Argb8888 also expects premultiplied, so the bytes map straight across.
    /// Used by the translucent menu overlay.
    pub fn blit_argb(&self, dst: &mut [u8]) -> bool {
        let src = self.pix.data();
        let w = self.pix.width() as usize;
        let h = self.pix.height() as usize;
        let n = w * h;
        if dst.len() < n * 4 || src.len() < n * 4 {
            return false;
        }
        // Full-surface swizzle: split by band, read `src` (immutable) by
        // absolute offset. Byte-identical to the serial copy; the size gate in
        // `par_rows` keeps the thin bars serial and only the full-output menu
        // fans out.
        crate::par::par_rows(&mut dst[..n * 4], h, w * 4, |y0, band| {
            for (ry, row) in band.chunks_mut(w * 4).enumerate() {
                let base = (y0 + ry) * w * 4;
                for x in 0..w {
                    let o = x * 4;
                    let s = base + o;
                    row[o] = src[s + 2]; // B
                    row[o + 1] = src[s + 1]; // G
                    row[o + 2] = src[s]; // R
                    row[o + 3] = src[s + 3]; // A
                }
            }
        });
        true
    }

    /// Nearest-neighbour upscale into a `scale`-times-larger ARGB buffer.
    pub fn blit_argb_scaled(&self, dst: &mut [u8], scale: u32) -> bool {
        let scale = scale.max(1) as usize;
        if scale == 1 {
            return self.blit_argb(dst);
        }
        let src = self.pix.data();
        let w = self.pix.width() as usize;
        let h = self.pix.height() as usize;
        let bw = w * scale;
        let bh = h * scale;
        if dst.len() < bw * bh * 4 || src.len() < w * h * 4 {
            return false;
        }
        for y in 0..bh {
            let sy = y / scale;
            for x in 0..bw {
                let sx = x / scale;
                let s = (sy * w + sx) * 4;
                let o = (y * bw + x) * 4;
                dst[o] = src[s + 2];
                dst[o + 1] = src[s + 1];
                dst[o + 2] = src[s];
                dst[o + 3] = src[s + 3];
            }
        }
        true
    }

    /// Draw a filled path at the given RGB with explicit alpha — for the
    /// menu's translucent scrim and hover highlights.
    pub fn fill_rect_a(&mut self, x: i32, y: i32, rw: i32, rh: i32, c: Rgb, a: f32) {
        if let Some(r) = Rect::from_xywh(x as f32, y as f32, rw.max(0) as f32, rh.max(0) as f32) {
            self.fill(&PathBuilder::from_rect(r), c, a);
        }
    }

    /// Filled rounded rect with explicit alpha.
    pub fn round_rect_a(&mut self, x: i32, y: i32, rw: i32, rh: i32, rad: i32, c: Rgb, a: f32) {
        if let Some(p) = rounded_rect_path(x as f32, y as f32, rw as f32, rh as f32, rad as f32) {
            self.fill(&p, c, a);
        }
    }
}

// embedded-graphics draw target: text renders straight into the RGBA pixmap.
impl OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(self.pix.width(), self.pix.height())
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let w = self.pix.width() as i32;
        let h = self.pix.height() as i32;
        let data = self.pix.data_mut();
        for Pixel(p, c) in pixels {
            if p.x >= 0 && p.y >= 0 && p.x < w && p.y < h {
                let i = ((p.y * w + p.x) as usize) * 4;
                // Pixmap is premultiplied RGBA; alpha 255 makes this straight.
                data[i] = c.r();
                data[i + 1] = c.g();
                data[i + 2] = c.b();
                data[i + 3] = 0xff;
            }
        }
        Ok(())
    }
}

/// A rounded-rect path with all corners of radius `r` (quarter-arc cubics).
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    if w <= 0.0 || h <= 0.0 {
        // At exactly zero this is belt and braces as well: the radius then
        // clamps to zero and `Rect::from_xywh` refuses a zero-sized rect.
        return None;
    }
    let r = r.clamp(0.0, (w / 2.0).min(h / 2.0));
    if r <= 0.5 {
        return Rect::from_xywh(x, y, w, h).map(PathBuilder::from_rect);
    }
    let mut pb = PathBuilder::new();
    // Starting at the corner instead of `r` along it would draw the same
    // shape --- the extra piece of the top edge is walked forwards by the
    // `line_to` and backwards by the `close()`, so it cancels. Starting
    // where the first arc ends is what makes the path say that.
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + K * r, y, x + w, y + r - K * r, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + K * r, x + w - r + K * r, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - K * r, y + h, x, y + h - r + K * r, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - K * r, x + r - K * r, y, x + r, y);
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fetch one BGRA pixel out of a blitted buffer.
    fn px(buf: &[u8], w: usize, x: usize, y: usize) -> (u8, u8, u8, u8) {
        let o = (y * w + x) * 4;
        (buf[o], buf[o + 1], buf[o + 2], buf[o + 3])
    }

    /// The scrim is the whole reason the overlay is ARGB: it must reach the
    /// compositor PREMULTIPLIED, because that is what `wl_shm`'s Argb8888
    /// means. A straight-alpha scrim would composite far too bright.
    #[test]
    fn translucent_fill_is_premultiplied() {
        let mut cv = Canvas::try_new(2, 2).unwrap();
        // The app menu's backdrop, exactly: black at 35%.
        cv.fill_rect_a(0, 0, 2, 2, (0, 0, 0), 0.35);
        let mut buf = vec![0u8; 2 * 2 * 4];
        assert!(cv.blit_argb(&mut buf));
        let (b, g, r, a) = px(&buf, 2, 0, 0);
        // Black premultiplied by any alpha is still black; alpha is ~0.35.
        assert_eq!((b, g, r), (0, 0, 0));
        assert!((a as i32 - 89).abs() <= 2, "alpha {a} is not ~0.35*255");

        // A COLOURED translucent fill is where straight alpha would show: the
        // colour channels must already be scaled down by the alpha.
        let mut cv = Canvas::try_new(1, 1).unwrap();
        cv.fill_rect_a(0, 0, 1, 1, (255, 255, 255), 0.5);
        let mut buf = vec![0u8; 4];
        assert!(cv.blit_argb(&mut buf));
        let (b, g, r, a) = px(&buf, 1, 0, 0);
        assert!((a as i32 - 128).abs() <= 2, "alpha {a}");
        for c in [b, g, r] {
            assert!(c <= a, "channel {c} exceeds alpha {a}: not premultiplied");
            assert!((c as i32 - a as i32).abs() <= 2, "channel {c} vs alpha {a}");
        }
    }

    /// Nothing drawn means fully transparent, not opaque black — the overlay
    /// covers the whole output, so an opaque clear would black out the desktop.
    #[test]
    fn untouched_overlay_pixels_are_transparent() {
        let cv = Canvas::try_new(3, 3).unwrap();
        let mut buf = vec![0xabu8; 3 * 3 * 4];
        assert!(cv.blit_argb(&mut buf));
        assert!(buf.iter().all(|&b| b == 0), "a fresh overlay canvas is not clear");
    }

    /// The swizzle is RGBA -> BGRA, alpha carried through untouched.
    #[test]
    fn blit_argb_swizzles_and_keeps_alpha() {
        let mut cv = Canvas::try_new(1, 1).unwrap();
        cv.fill_rect_a(0, 0, 1, 1, (200, 100, 50), 1.0);
        let mut buf = vec![0u8; 4];
        assert!(cv.blit_argb(&mut buf));
        assert_eq!(px(&buf, 1, 0, 0), (50, 100, 200, 255));
    }

    /// The bars' XRGB path forces the alpha byte opaque whatever was drawn.
    #[test]
    fn blit_xrgb_forces_opaque() {
        let mut cv = Canvas::try_new(1, 1).unwrap();
        cv.fill_rect_a(0, 0, 1, 1, (10, 20, 30), 0.25);
        let mut buf = vec![0u8; 4];
        assert!(cv.blit_xrgb(&mut buf));
        assert_eq!(px(&buf, 1, 0, 0).3, 0xff);
    }

    /// HiDPI: the overlay canvas is LOGICAL pixels and the wl_buffer is
    /// `scale` times that, so each logical pixel must land as a scale x scale
    /// block — alpha included, or the scrim would gain a grid of holes.
    #[test]
    fn blit_argb_scaled_replicates_every_pixel() {
        let mut cv = Canvas::try_new(2, 2).unwrap();
        cv.fill_rect_a(0, 0, 1, 1, (255, 0, 0), 1.0);
        cv.fill_rect_a(1, 1, 1, 1, (0, 0, 255), 0.5);
        let (bw, bh) = (4, 4);
        let mut buf = vec![0u8; bw * bh * 4];
        assert!(cv.blit_argb_scaled(&mut buf, 2));
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert_eq!(px(&buf, bw, x, y), (0, 0, 255, 255), "red block at {x},{y}");
        }
        for (x, y) in [(2, 2), (3, 2), (2, 3), (3, 3)] {
            let (b, g, r, a) = px(&buf, bw, x, y);
            assert!((a as i32 - 128).abs() <= 2, "alpha {a} at {x},{y}");
            assert_eq!((g, r), (0, 0));
            assert!((b as i32 - a as i32).abs() <= 2, "blue {b} not premultiplied");
        }
        // The untouched top-right quadrant stays clear.
        assert_eq!(px(&buf, bw, 3, 0), (0, 0, 0, 0));
    }

    /// Scale 1 is the same bytes as the unscaled blit — the popup takes the
    /// scaled path unconditionally now, so the common case must not change.
    #[test]
    fn blit_argb_scaled_one_matches_unscaled() {
        let mut cv = Canvas::try_new(4, 3).unwrap();
        cv.fill_rect_a(0, 0, 4, 3, (0, 0, 0), 0.35);
        cv.round_rect_a(1, 1, 2, 2, 1, (30, 60, 90), 0.98);
        let mut a = vec![0u8; 4 * 3 * 4];
        let mut b = vec![0u8; 4 * 3 * 4];
        assert!(cv.blit_argb(&mut a));
        assert!(cv.blit_argb_scaled(&mut b, 1));
        assert_eq!(a, b);
    }

    /// A short destination must be refused, never panic: a bad configure from
    /// the compositor has to leave the panel up.
    #[test]
    fn blits_refuse_a_short_destination() {
        let cv = Canvas::try_new(4, 4).unwrap();
        let mut small = vec![0u8; 4 * 4 * 4 - 1];
        assert!(!cv.blit_argb(&mut small));
        assert!(!cv.blit_xrgb(&mut small));
        let mut scaled = vec![0u8; 8 * 8 * 4 - 1];
        assert!(!cv.blit_argb_scaled(&mut scaled, 2));
        assert!(!cv.blit_xrgb_scaled(&mut scaled, 2));
    }

    /// Scale 0 would divide by zero in the nearest-neighbour walk.
    #[test]
    fn blit_argb_scaled_clamps_zero_scale() {
        let cv = Canvas::try_new(2, 2).unwrap();
        let mut buf = vec![0u8; 2 * 2 * 4];
        assert!(cv.blit_argb_scaled(&mut buf, 0));
    }

    /// Draw into a `w`x`h` canvas cleared to black and hand back the blitted
    /// BGRA bytes. What landed in the frame is the only thing a drawing
    /// routine can be judged on: every one of these builds a path and gives
    /// it to tiny-skia, so there is no return value to look at.
    fn drawn(w: usize, h: usize, f: impl FnOnce(&mut Canvas)) -> Vec<u8> {
        let mut cv = Canvas::try_new(w, h).unwrap();
        cv.clear((0, 0, 0));
        f(&mut cv);
        let mut buf = vec![0u8; w * h * 4];
        assert!(cv.blit_xrgb(&mut buf));
        buf
    }

    /// How much ink landed on (x,y): 0 for untouched ground, 255 for a pixel
    /// a shape covers whole. Everything between is tiny-skia's antialiasing,
    /// which is the whole point of a rounded corner, so a test about rounding
    /// has to read the level and not just ask whether anything is there.
    fn ink(buf: &[u8], w: usize, x: usize, y: usize) -> u8 {
        let (b, g, r, _) = px(buf, w, x, y);
        b.max(g).max(r)
    }

    /// Is there ink at (x,y)? Anything that is not the black ground. The
    /// threshold is above the faintest antialiased edge so a shape's outline
    /// does not read as a shape of its own.
    fn lit(buf: &[u8], w: usize, x: usize, y: usize) -> bool {
        ink(buf, w, x, y) > 8
    }

    /// How many pixels inside the box at (x0,y0) are inked.
    fn count(buf: &[u8], w: usize, x0: usize, y0: usize, bw: usize, bh: usize) -> usize {
        let mut n = 0;
        for y in y0..y0 + bh {
            for x in x0..x0 + bw {
                if lit(buf, w, x, y) {
                    n += 1;
                }
            }
        }
        n
    }

    /// One drawing routine of the power menu, so the tests below walk the
    /// six of them from one list instead of each keeping its own copy.
    type Draw = fn(&mut Canvas, i32, i32, i32);

    /// Where a shape's ink lands: `(x, y, width, height)`, in pixels. Its
    /// own name rather than the bare tuple, which clippy reads as a type too
    /// complex to repeat --- and `Box` is taken.
    type Inked = (usize, usize, usize, usize);

    /// One row of the icon table: name, how to draw it, and its footprint.
    type Icon = (&'static str, Draw, Inked);

    /// Every icon the power menu draws: its name, how to draw it, and the
    /// footprint its ink has in a twenty-four pixel slot drawn at (4,4) ---
    /// `(x, y, width, height)`, measured. One floor for all six would not
    /// do, because each is a different shape of the box on purpose: the
    /// padlock is narrow and tall, the speaker short and wide, the power
    /// symbol as tall as its slot. The footprints below are what each one's
    /// own fractions of `s` come out to, give or take the pixel a stroke
    /// centred on an edge puts either side of it.
    fn menu_icons() -> [Icon; 6] {
        [
            // The ring spans 0.1..0.9 of the box and the bar runs from
            // 1.1r above the centre, which is the whole height of the slot.
            (
                "power",
                |cv, x, y, s| cv.power_icon(x, y, s, (255, 255, 255)),
                (6, 4, 20, 24),
            ),
            // The cone starts at the very left of the slot; the wave bulges
            // out to 0.8 of it, and both keep to the middle 0.15..0.85 band.
            (
                "volume",
                |cv, x, y, s| cv.volume_icon(x, y, s, (255, 255, 255), false),
                (4, 7, 19, 18),
            ),
            // The cross reaches 0.95, five pixels further right than the
            // wave does, which is the whole width of the slot.
            (
                "muted",
                |cv, x, y, s| cv.volume_icon(x, y, s, (255, 255, 255), true),
                (4, 7, 24, 18),
            ),
            // Body 0.2..0.8 wide, shackle from 0.05 down to the body's foot
            // at 0.95: narrower than its slot and nearly as tall.
            (
                "lock",
                |cv, x, y, s| cv.lock_icon(x, y, s, (255, 255, 255)),
                (8, 5, 16, 22),
            ),
            // Door 0.15..0.5, arrow out to 0.85, both inside 0.15..0.85.
            (
                "exit",
                |cv, x, y, s| cv.exit_icon(x, y, s, (255, 255, 255)),
                (6, 6, 20, 20),
            ),
            // A ring of radius 0.35 about the centre, so 0.7 of the box
            // across, with the arrowhead reaching 0.9r on its right.
            (
                "reboot",
                |cv, x, y, s| cv.reboot_icon(x, y, s, (255, 255, 255)),
                (6, 8, 20, 16),
            ),
        ]
    }

    /// A canvas configured at nothing must not take the panel with it: a
    /// compositor can send a zero configure before it knows the output, and
    /// `panic = "abort"` in release turns an unwrap here into a dead desktop.
    #[test]
    fn a_canvas_configured_at_nothing_is_still_a_canvas() {
        assert!(Canvas::try_new(0, 0).is_some());
        let cv = Canvas::try_new(0, 10).unwrap();
        assert_eq!((cv.width(), cv.height()), (1, 10), "nothing becomes one");
    }

    /// Half of 255 is 127.5, which rounds to 128 and truncates to 127. The
    /// scrim is the one thing the user looks through, and the premultiplied
    /// colour channels are scaled by this same byte, so a truncation here
    /// drags the whole overlay a shade dark.
    #[test]
    fn an_alpha_lands_on_the_byte_it_rounds_to() {
        let mut cv = Canvas::try_new(1, 1).unwrap();
        cv.fill_rect_a(0, 0, 1, 1, (0, 0, 0), 0.5);
        let mut buf = vec![0u8; 4];
        assert!(cv.blit_argb(&mut buf));
        assert_eq!(px(&buf, 1, 0, 0).3, 128, "0.5 of 255 rounds up, not down");
    }

    /// The bar's own two lines: the accent rule is one row and the module
    /// separator is the middle half of the bar, drawn faint. Both are
    /// positioned from numbers nothing checked.
    #[test]
    fn the_bars_own_lines_are_one_pixel_and_sit_where_they_are_told() {
        let buf = drawn(10, 6, |cv| cv.hline(2, 3, 5, (255, 255, 255), 1.0));
        assert_eq!(count(&buf, 10, 0, 3, 10, 1), 5, "the row it was given");
        assert_eq!(count(&buf, 10, 0, 0, 10, 3), 0, "nothing above it");
        assert_eq!(count(&buf, 10, 0, 4, 10, 2), 0, "nothing below it");

        // A bar of 16 gives the separator rows 4..12: the top and bottom
        // quarters stay clear, which is what makes it read as a separator
        // and not as a border.
        let buf = drawn(5, 16, |cv| cv.vrule(2, 16, (255, 255, 255)));
        assert_eq!(count(&buf, 5, 2, 0, 1, 4), 0, "the top quarter is clear");
        assert_eq!(count(&buf, 5, 2, 12, 1, 4), 0, "and the bottom quarter");
        assert_eq!(
            count(&buf, 5, 2, 4, 1, 8),
            8,
            "and the middle half is drawn"
        );
        // Faint, so it groups the modules without competing with them.
        let (_, g, _, _) = px(&buf, 5, 2, 8);
        assert!(
            g > 40 && g < 120,
            "the separator came out at {g}, not faint"
        );
    }

    /// The marker under the focused taskbar button is inset from both ends,
    /// so it reads as belonging to that button rather than as a line between
    /// two of them -- and a button too narrow for the inset still gets one,
    /// because it is the only thing that says which window is focused.
    #[test]
    fn the_active_marker_is_inset_from_both_ends_of_its_button() {
        let buf = drawn(24, 6, |cv| cv.active_line(0, 2, 24, (255, 255, 255)));
        assert_eq!(count(&buf, 24, 0, 0, 4, 6), 0, "clear on the left");
        assert_eq!(count(&buf, 24, 20, 0, 4, 6), 0, "clear on the right");
        assert!(count(&buf, 24, 4, 2, 16, 2) > 20, "and drawn in between");
        assert_eq!(count(&buf, 24, 0, 0, 24, 2), 0, "nothing above it");
        assert_eq!(count(&buf, 24, 0, 4, 24, 2), 0, "nothing below it");

        let narrow = drawn(8, 6, |cv| cv.active_line(0, 2, 6, (255, 255, 255)));
        assert!(
            count(&narrow, 8, 0, 0, 8, 6) > 0,
            "a button narrower than the inset still marks"
        );
    }

    /// Every arrow points the way its name says. A solid triangle's tip is a
    /// pixel or two and the opposite edge is the full width, so counting the
    /// ink in the first and last row -- or column -- says which way it faces.
    #[test]
    fn every_arrow_points_the_way_its_name_says() {
        let n = 12usize;
        for (up, tip_row, base_row) in [(true, 0usize, n - 1), (false, n - 1, 0)] {
            let buf = drawn(n, n, |cv| cv.triangle(0, 0, n as i32, up, (255, 255, 255)));
            let tip = count(&buf, n, 0, tip_row, n, 1);
            let base = count(&buf, n, 0, base_row, n, 1);
            assert!(base > tip * 3, "up={up}: tip row {tip}, base row {base}");
        }
        for (left, tip_col, base_col) in [(true, 0usize, n - 1), (false, n - 1, 0)] {
            let buf = drawn(n, n, |cv| {
                cv.triangle_h(0, 0, n as i32, left, (255, 255, 255))
            });
            let tip = count(&buf, n, tip_col, 0, 1, n);
            let base = count(&buf, n, base_col, 0, 1, n);
            assert!(
                base > tip * 3,
                "left={left}: tip col {tip}, base col {base}"
            );
        }
        // And the horizontal pair's tip is halfway down, not at a corner: a
        // month-nav arrow with its point at the top is a different glyph.
        let buf = drawn(n, n, |cv| {
            cv.triangle_h(0, 0, n as i32, false, (255, 255, 255))
        });
        let top = count(&buf, n, n - 3, 0, 3, n / 3);
        let mid = count(&buf, n, n - 3, n / 3, 3, n / 3);
        assert!(mid > top, "the right arrow's tip: top {top}, middle {mid}");
    }

    /// A gauge fills the fraction it was given, over a track that shows where
    /// the gauge ends, with the gradient spanning the WHOLE track so the
    /// leading edge carries the colour of the current level. None of it had a
    /// test: a gauge is read at a glance and a wrong one still looks like a
    /// gauge.
    #[test]
    fn a_gauge_fills_the_fraction_it_is_given_and_nothing_more() {
        let (w, h) = (40usize, 8usize);
        // A black track, so every lit pixel is fill and nothing else.
        let bar = |frac: f32| {
            drawn(w, h, |cv| {
                cv.gauge(0, 0, w as i32, h as i32, frac, (0, 0, 0))
            })
        };

        let half = bar(0.5);
        assert_eq!(count(&half, w, 2, 3, 16, 2), 32, "the filled part is solid");
        assert_eq!(
            count(&half, w, 22, 0, 18, h),
            0,
            "and it stops at the fraction"
        );

        // Clamped at both ends, rounded to the nearest pixel, and a fraction
        // too small for one pixel draws the track alone.
        assert_eq!(count(&bar(-1.0), w, 0, 0, w, h), 0, "below zero is empty");
        assert_eq!(count(&bar(0.0), w, 0, 0, w, h), 0);
        assert_eq!(count(&bar(0.005), w, 0, 0, w, h), 0, "under half a pixel");
        assert!(
            count(&bar(0.02), w, 0, 0, w, h) > 0,
            "four fifths of a pixel rounds up to one"
        );
        let full = count(&bar(1.0), w, 0, 0, w, h);
        assert!(full > 0, "a full gauge is drawn");
        assert_eq!(
            count(&bar(2.0), w, 0, 0, w, h),
            full,
            "above one is clamped"
        );

        // Green at the left, amber in the middle, red at the right -- across
        // the whole track, not across the filled part, or a gauge at 20%
        // would already look alarming.
        let (_, lg, lr, _) = px(&bar(0.2), w, 6, 4);
        assert!(
            lg > lr,
            "a gauge at a fifth is green at its tip: r{lr} g{lg}"
        );
        let (_, mg, mr, _) = px(&bar(0.5), w, 19, 4);
        assert!(mr >= mg, "halfway along it is amber: r{mr} g{mg}");
        let (_, hg, hr, _) = px(&bar(0.9), w, 34, 4);
        assert!(hr > hg, "and near the end it is red: r{hr} g{hg}");

        // The track is drawn under the fill, so the empty part is the track
        // and not the ground showing through -- and its ends are round.
        let shown = drawn(w, h, |cv| {
            cv.gauge(0, 0, w as i32, h as i32, 0.25, (40, 40, 40))
        });
        assert_eq!(
            px(&shown, w, 34, 4),
            (40, 40, 40, 255),
            "the bare part is the track"
        );
        assert!(!lit(&shown, w, 0, 0), "and the pill's corner is rounded");
    }

    /// The launcher glyph is an outlined circle with its RIGHT half filled.
    /// Mirror it and the panel's first button points the wrong way; drop the
    /// inset and the ring is drawn half outside its own box.
    #[test]
    fn the_launcher_disc_is_filled_on_its_right_half_only() {
        let d = 20usize;
        let buf = drawn(d, d, |cv| cv.disc_half(0, 0, d as i32, (255, 255, 255)));
        let right = count(&buf, d, d / 2 + 2, 4, d / 2 - 4, d - 8);
        let left = count(&buf, d, 2, 4, d / 2 - 4, d - 8);
        assert!(right > left * 4, "right {right} against left {left}");
        assert!(right > 50, "the right half is a solid semicircle: {right}");
        assert!(left > 0, "and the outline still closes round the left");
        for (x, y) in [(0, 0), (d - 1, 0), (0, d - 1), (d - 1, d - 1)] {
            assert!(!lit(&buf, d, x, y), "ink in the corner at {x},{y}");
        }
    }

    /// The crescent is the sun disc with a bite taken out of its upper right,
    /// which is what makes it a crescent and not a disc. The bite comes from
    /// an INVERTED mask: without the inversion the glyph is the bite itself.
    #[test]
    fn the_crescent_is_bitten_out_of_its_upper_right() {
        let d = 24usize;
        let buf = drawn(d, d, |cv| cv.crescent(0, 0, d as i32, (255, 255, 255)));
        // The lower left keeps the sun; the upper right is where the moon is.
        let keep = count(&buf, d, 2, d / 2, d / 2, d / 2 - 2);
        let bite = count(&buf, d, d / 2 + 2, 2, d / 2 - 2, d / 2);
        assert!(keep > 60, "the crescent's own side is solid: {keep}");
        assert!(
            bite * 4 < keep,
            "the bite is on the upper right: {bite} vs {keep}"
        );
        // It is a crescent, so something is left: an inverted mask that took
        // everything would leave an empty glyph.
        assert!(count(&buf, d, 0, 0, d, d) > 100, "the glyph is not empty");
        // And it is drawn at the place it was given, inside the box it was
        // given. The offsets differ on purpose: at the same x as y, a glyph
        // blitted at (y,x) lands exactly where one blitted at (x,y) does.
        let (cw, ch) = (d + 10, d + 6);
        let wide = drawn(cw, ch, |cv| cv.crescent(8, 2, d as i32, (255, 255, 255)));
        assert_eq!(count(&wide, cw, 0, 0, cw, 2), 0, "nothing above it");
        assert_eq!(count(&wide, cw, 0, 0, 8, ch), 0, "nothing left of it");
        assert!(count(&wide, cw, 8, 2, d, d) > 100, "and the glyph is in it");
        // A glyph of no size draws nothing rather than allocating a 0x0 mask.
        let none = drawn(8, 8, |cv| cv.crescent(0, 0, 0, (255, 255, 255)));
        assert_eq!(count(&none, 8, 0, 0, 8, 8), 0);
    }

    /// Every icon in the power menu, drawn. None of them had a test: an icon
    /// is read as a picture, so one drawn wrong still reads as an icon --
    /// the wrong one. Each has to ink a sensible part of its own box, stay
    /// inside it, and look like nothing else in the menu.
    #[test]
    fn every_icon_fills_its_own_box_and_looks_like_nothing_else() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;
        let mut shots: Vec<(&str, Vec<u8>)> = Vec::new();
        for (name, f, _) in menu_icons() {
            let buf = drawn(n, n, |cv| f(cv, pad as i32, pad as i32, s as i32));
            let inside = count(&buf, n, pad, pad, s, s);
            let total = count(&buf, n, 0, 0, n, n);
            // A recognisable glyph, not a blank and not a solid block.
            assert!(
                inside > s * 2 && inside < s * s * 3 / 4,
                "{name} inked {inside} of {}",
                s * s
            );
            // Drawn inside the slot it was given: the menu packs these next to
            // their labels, so an icon that spills writes over the text.
            assert!(
                total <= inside + s * 2,
                "{name} spills out of its box: {total} against {inside} inside"
            );
            shots.push((name, buf));
        }

        // No two of them are the same picture. This is what catches a glyph
        // whose shape collapsed into another's silhouette.
        for i in 0..shots.len() {
            for j in i + 1..shots.len() {
                assert_ne!(
                    shots[i].1, shots[j].1,
                    "{} and {} draw the same picture",
                    shots[i].0, shots[j].0
                );
            }
        }
    }

    /// The icons that have an orientation, in the direction they have it:
    /// the power symbol's bar is at the TOP, the muted speaker is crossed out
    /// where the unmuted one has its waves, and the logout arrow points
    /// right, out of the door it is drawn beside.
    #[test]
    fn the_icons_that_point_somewhere_point_the_right_way() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;

        // The power symbol: the gap in its ring, and the bar through the gap,
        // are both at the top. Below the centre there is only the ring.
        let buf = drawn(n, n, |cv| {
            cv.power_icon(pad as i32, pad as i32, s as i32, (255, 255, 255))
        });
        let col = pad + s / 2;
        let above = count(&buf, n, col - 1, pad, 3, s / 2 - 2);
        let below = count(&buf, n, col - 1, pad + s / 2 + 2, 3, s / 2 - 4);
        assert!(
            above > below,
            "the power bar is at the top: above {above}, below {below}"
        );

        // Muted and unmuted share the speaker and differ to its right.
        let plain = drawn(n, n, |cv| {
            cv.volume_icon(pad as i32, pad as i32, s as i32, (255, 255, 255), false)
        });
        let muted = drawn(n, n, |cv| {
            cv.volume_icon(pad as i32, pad as i32, s as i32, (255, 255, 255), true)
        });
        let cone = (pad, pad + s / 4, s / 2, s / 2);
        assert_eq!(
            count(&plain, n, cone.0, cone.1, cone.2, cone.3),
            count(&muted, n, cone.0, cone.1, cone.2, cone.3),
            "both speakers have the same cone"
        );
        // The cross reaches further up and down than the single wave arc does.
        let right = (pad + s * 3 / 5, pad, s * 2 / 5, s);
        assert!(
            count(&muted, n, right.0, right.1, right.2, right.3)
                > count(&plain, n, right.0, right.1, right.2, right.3),
            "the mute cross is more than the sound wave"
        );

        // The logout arrow points right: its head is at the far end of the
        // shaft, past the door it comes out of.
        let buf = drawn(n, n, |cv| {
            cv.exit_icon(pad as i32, pad as i32, s as i32, (255, 255, 255))
        });
        let row = pad + s / 2;
        let head = count(&buf, n, pad + s * 3 / 4, row - 4, s / 4, 9);
        let tail = count(&buf, n, pad + s / 4, row - 4, s / 4, 9);
        assert!(
            head > tail,
            "the arrowhead is at the right: head {head}, tail {tail}"
        );
        // And the shaft reaches the head, rather than stopping short of it.
        assert!(
            lit(&buf, n, pad + s / 2, row) || lit(&buf, n, pad + s / 2, row - 1),
            "the shaft is drawn across the middle"
        );
    }

    /// Text advances by whole cells of the bar's monospace font, and the
    /// width a caller is promised has to be the width that gets drawn: every
    /// module's layout is `text_width` plus padding, so a disagreement here
    /// is modules written over each other.
    #[test]
    fn text_advances_by_the_cells_it_promises() {
        let mut cv = Canvas::try_new(80, 20).unwrap();
        cv.clear((0, 0, 0));
        // The advance is relative to where the string STARTED, not an
        // absolute x: drawn at 20, "abc" advances 27, not 47.
        assert_eq!(cv.text("abc", 20, 0, (255, 255, 255)), 3 * GLYPH_W);
        assert_eq!(cv.text_bold("abc", 20, 0, (255, 255, 255)), 3 * GLYPH_W);
        assert_eq!(Canvas::text_width("abc"), 3 * GLYPH_W);
        // Counted in CHARACTERS: the clock and the date are written in the
        // panel's own language, and a multi-byte character is one cell wide.
        assert_eq!(Canvas::text_width("mie"), Canvas::text_width("mié"));
        assert_eq!(Canvas::text_width(""), 0);
    }

    /// Text lands in the canvas as opaque colour, in the right channels, and
    /// a glyph that falls off an edge is dropped rather than wrapped round to
    /// the other side.
    #[test]
    fn text_is_drawn_opaque_and_clipped_at_every_edge() {
        let (w, h) = (40usize, 20usize);
        let mut cv = Canvas::try_new(w, h).unwrap();
        cv.clear((0, 0, 0));
        cv.text("I", 2, 2, (255, 0, 0));
        let mut buf = vec![0u8; w * h * 4];
        assert!(cv.blit_argb(&mut buf));
        let mut seen = false;
        for y in 0..h {
            for x in 0..w {
                let (b, g, r, a) = px(&buf, w, x, y);
                if (b, g, r) != (0, 0, 0) {
                    assert_eq!((b, g, r, a), (0, 0, 255, 255), "glyph pixel at {x},{y}");
                    seen = true;
                }
            }
        }
        assert!(seen, "the glyph was drawn at all");

        // Off every edge nothing is drawn at all. One "M" is nine by fifteen,
        // so twenty past an edge puts it wholly outside the frame.
        for (x, y) in [
            (-20i32, 2i32),
            (2, -20),
            (w as i32 + 5, 2),
            (2, h as i32 + 5),
        ] {
            let buf = drawn(w, h, |cv| {
                cv.text("M", x, y, (255, 255, 255));
            });
            assert_eq!(
                count(&buf, w, 0, 0, w, h),
                0,
                "a glyph at {x},{y} landed in the frame"
            );
        }

        // And a string that only half fits is cut at the edge rather than
        // folded over to the far side: "MMMM" is thirty-six wide, so from
        // x = -20 its ink reaches x = 16 and the right of the bar stays clear.
        let buf = drawn(w, h, |cv| {
            cv.text("MMMM", -20, 2, (255, 255, 255));
        });
        assert!(
            count(&buf, w, 0, 0, 20, h) > 0,
            "the half of the string that fits was drawn"
        );
        assert_eq!(
            count(&buf, w, 20, 0, w - 20, h),
            0,
            "and the clipped half did not wrap round"
        );
    }

    /// The letter badge is the fallback for an app with no icon, so it has to
    /// look like an icon: a rounded square with ONE capital letter centred in
    /// it. A lowercase initial, two letters, or a letter in the corner all
    /// read as a bug in the launcher.
    #[test]
    fn a_badge_is_one_capital_letter_centred_in_a_rounded_square() {
        let s = 24usize;
        let bg = (40, 40, 40);
        let fg = (255, 255, 255);
        let badge = |ch: char| drawn(s, s, |cv| cv.badge(0, 0, s as i32, ch, bg, fg));

        // Case does not matter: the same letter draws the same badge.
        assert_eq!(badge('a'), badge('A'), "the initial is capitalised");
        // And a letter whose capital is two characters still draws one.
        assert_eq!(
            count(&badge('\u{df}'), s, 0, 0, s, s),
            count(&badge('S'), s, 0, 0, s, s),
            "only the first character of the capital is drawn"
        );

        // Centred: the glyph's ink is in the middle, with the rounded corners
        // of the square showing the ground through.
        let buf = badge('A');
        let mid = count(&buf, s, s / 2 - 5, s / 2 - 7, 10, 14);
        assert!(mid > 10, "the letter is in the middle: {mid}");
        // Rounded: the very corner of the box is untouched. The radius here
        // is six, so only the corner pixel itself falls outside the arc --- a
        // box three wide already reaches a pixel the arc covers solidly.
        assert_eq!(ink(&buf, s, 0, 0), 0, "the badge's corner is rounded");
        assert_eq!(ink(&buf, s, s - 1, s - 1), 0, "every corner of it");
        // The square itself is drawn, so the slot is never a hole.
        assert!(lit(&buf, s, s / 2, 1), "the badge has a background");
        assert!(lit(&buf, s, 1, s / 2), "down its side as well as across");
    }

    /// A rounded rectangle's radius is capped at half the SHORTER side, and a
    /// radius of half a pixel or less is a plain rectangle. Both ends matter:
    /// the clock pill is wider than it is tall, and a cap taken from the long
    /// side would round it into a lens.
    #[test]
    fn a_rounded_rectangle_rounds_by_at_most_half_its_shorter_side() {
        // A radius of 4 on a 20x20 box rounds the corners away.
        let buf = drawn(20, 20, |cv| cv.round_rect(0, 0, 20, 20, 4, (255, 255, 255)));
        assert!(!lit(&buf, 20, 0, 0), "the corner is rounded off");
        assert!(lit(&buf, 20, 10, 0), "but the top edge is drawn");
        assert!(lit(&buf, 20, 0, 10), "and the left edge");

        // The same radius on a short box is capped, so the pill keeps square
        // sides: capped from the LONG side it would be a lens with no middle.
        let pill = drawn(24, 6, |cv| cv.round_rect(0, 0, 24, 6, 8, (255, 255, 255)));
        assert!(lit(&pill, 24, 12, 0), "a pill keeps its flat top");
        assert!(lit(&pill, 24, 12, 5), "and its flat bottom");

        // And the other way round, which is what pins the cap to the SHORTER
        // side rather than to the width: on a box six wide and twenty-four
        // tall, a radius of eight has to come down to three. Capped at the
        // long side instead it stays at eight, and the straight part of the
        // left edge --- everything below the arc --- disappears.
        let tall = drawn(6, 24, |cv| cv.round_rect(0, 0, 6, 24, 8, (255, 255, 255)));
        assert!(
            lit(&tall, 6, 0, 4),
            "the left edge is drawn below a radius of three"
        );
        assert!(lit(&tall, 6, 0, 19), "and above the bottom corner");
        let corner = ink(&tall, 6, 0, 0);
        assert!(
            corner < 64,
            "while the corner itself is still rounded: {corner}"
        );

        // Half a pixel or less is a plain rectangle, corners and all.
        let square = drawn(10, 10, |cv| cv.round_rect(0, 0, 10, 10, 0, (255, 255, 255)));
        assert_eq!(
            ink(&square, 10, 0, 0),
            255,
            "no radius means a square corner"
        );
        assert_eq!(count(&square, 10, 0, 0, 10, 10), 100, "and a solid block");
        // A radius of three is NOT nothing: it eats most of the corner pixel.
        // Not all of it --- an arc of three passing (3,0) to (0,3) still clips
        // the far corner of that pixel, so what proves the rounding is how
        // little ink is left, against a middle that stays solid.
        let little = drawn(10, 10, |cv| cv.round_rect(0, 0, 10, 10, 3, (255, 255, 255)));
        let corner = ink(&little, 10, 0, 0);
        assert!(corner < 64, "a radius of three rounds the corner: {corner}");
        assert_eq!(ink(&little, 10, 5, 5), 255, "but the middle stays solid");
    }

    /// The box the ink actually landed in: `(x, y, width, height)`, or `None`
    /// for an empty frame. An icon promises a glyph of a given size in a
    /// given place, and this measures exactly that --- which a count of lit
    /// pixels does not, since a shape that lost a side keeps most of its ink.
    fn bounds(buf: &[u8], w: usize, h: usize) -> Option<Inked> {
        let mut b: Option<Inked> = None;
        for y in 0..h {
            for x in 0..w {
                if lit(buf, w, x, y) {
                    b = Some(match b {
                        None => (x, y, x, y),
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                    });
                }
            }
        }
        b.map(|(x0, y0, x1, y1)| (x0, y0, x1 + 1 - x0, y1 + 1 - y0))
    }

    /// Every icon fills the slot it is given, in both directions. A shape
    /// that shrank, or one that lost a side, still looks like a picture and
    /// still passes a count of its pixels; what gives it away is that it no
    /// longer reaches the edges of its box. The menu lays these out on a
    /// fixed grid beside their labels, so an icon drawn at two thirds of the
    /// size reads as a rendering glitch rather than as a bug to report.
    #[test]
    fn every_icon_reaches_the_edges_of_the_slot_it_was_given() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;
        for (name, f, want) in menu_icons() {
            let buf = drawn(n, n, |cv| f(cv, pad as i32, pad as i32, s as i32));
            let got = bounds(&buf, n, n).unwrap_or_else(|| panic!("{name} drew nothing at all"));
            let (bx, by, bw, bh) = got;
            let (wx, wy, ww, wh) = want;
            assert!(
                bx.abs_diff(wx) <= 1
                    && by.abs_diff(wy) <= 1
                    && bw.abs_diff(ww) <= 1
                    && bh.abs_diff(wh) <= 1,
                "{name} inks {got:?}, not {want:?}"
            );
            // And it keeps to its slot, give or take the pixel that a stroke
            // centred on the edge puts either side of it.
            assert!(
                bx + 1 >= pad && by + 1 >= pad && bx + bw <= pad + s + 1 && by + bh <= pad + s + 1,
                "{name} inks {got:?}, outside a slot of {s} at {pad},{pad}"
            );
        }
    }

    /// The launcher disc is a ring one and a half pixels wide drawn INSIDE
    /// its box: the three quarters of a pixel it is inset by is exactly half
    /// that stroke, which is what keeps the ring's outer edge on the edge of
    /// the box instead of three quarters of a pixel past it. The bar packs
    /// this against its neighbour, so a ring that spills writes on it.
    #[test]
    fn the_launcher_disc_is_drawn_inside_the_box_it_was_given() {
        let d = 20usize;
        let pad = 2usize;
        let n = d + pad * 2;
        let buf = drawn(n, n, |cv| {
            cv.disc_half(pad as i32, pad as i32, d as i32, (255, 255, 255))
        });
        assert_eq!(count(&buf, n, 0, 0, pad, n), 0, "ink left of the box");
        assert_eq!(
            count(&buf, n, d + pad, 0, pad, n),
            0,
            "ink right of the box"
        );
        assert_eq!(count(&buf, n, 0, 0, n, pad), 0, "ink above the box");
        assert_eq!(count(&buf, n, 0, d + pad, n, pad), 0, "ink below the box");
        // And the ring really does reach that edge, so the inset is an inset
        // and not a shape drawn a size too small.
        assert!(
            lit(&buf, n, pad, pad + d / 2),
            "the ring reaches the left edge"
        );
        assert!(lit(&buf, n, pad + d - 1, pad + d / 2), "and the right edge");
        // The filled half is inset half a pixel of its own, so it sits UNDER
        // the ring rather than flush with the ring's outer edge: the last
        // pixel of the box comes out just short of solid. Flush, the fill
        // paints over the ring's antialiasing and that pixel saturates.
        let edge = ink(&buf, n, pad + d - 1, pad + d / 2);
        assert!(
            (200..255).contains(&edge),
            "the filled half is flush with the ring's edge: {edge} of 255"
        );
    }

    /// The power symbol's bar has to be a stroke you can see: it is drawn at
    /// 1.8 pixels wide, which inks a pixel nearly whole. Thin it and the
    /// glyph is still there, still the right shape, and invisible on a
    /// screen --- the kind of change no count of lit pixels notices, because
    /// the same pixels are lit, just barely.
    #[test]
    fn the_power_symbol_is_a_stroke_thick_enough_to_see() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;
        let buf = drawn(n, n, |cv| {
            cv.power_icon(pad as i32, pad as i32, s as i32, (255, 255, 255))
        });
        let col = pad + s / 2;
        let best = (pad + 2..pad + s / 2)
            .flat_map(|y| [ink(&buf, n, col - 1, y), ink(&buf, n, col, y)])
            .max()
            .unwrap();
        assert!(
            best > 180,
            "the power bar is only {best} of 255 at its darkest"
        );
        // The ring is drawn inside its box with a pixel of air around it, so
        // the glyph keeps clear of the label beside it. A ring taken out to
        // half the box fills that pixel in.
        assert_eq!(
            count(&buf, n, 0, 0, pad + 1, n),
            0,
            "the power ring touches the left of its slot"
        );
        assert_eq!(
            count(&buf, n, pad + s - 1, 0, pad + 1, n),
            0,
            "and the right of it"
        );
    }

    /// The padlock is a shackle standing on a solid body, and the body is
    /// half the height of the box: it is the part that reads as a padlock at
    /// eighteen pixels. Flatten it to a bar and the icon reads as a dash
    /// under an arch.
    #[test]
    fn the_padlock_is_a_shackle_standing_on_a_solid_body() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;
        let buf = drawn(n, n, |cv| {
            cv.lock_icon(pad as i32, pad as i32, s as i32, (255, 255, 255))
        });
        // The body: a solid block from 0.45 to 0.95 of the box, 0.6 wide.
        let (bx, by) = (pad + s * 25 / 100, pad + s * 55 / 100);
        let (bw, bh) = (s * 50 / 100, s * 35 / 100);
        assert_eq!(
            count(&buf, n, bx, by, bw, bh),
            bw * bh,
            "the padlock's body is not solid"
        );
        // The arch above it is hollow: a shackle, not a filled dome.
        let hollow = count(
            &buf,
            n,
            pad + s * 40 / 100,
            pad + s * 20 / 100,
            s / 5,
            s / 5,
        );
        assert_eq!(hollow, 0, "the shackle is filled in");
    }

    /// The logout icon is a door open on the side its arrow points, with the
    /// arrow coming out through the opening. Both halves matter: a door with
    /// no sides is a line, and an arrow with half a head is a tick.
    #[test]
    fn the_logout_door_is_open_on_the_side_its_arrow_points() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;
        let buf = drawn(n, n, |cv| {
            cv.exit_icon(pad as i32, pad as i32, s as i32, (255, 255, 255))
        });
        // Three sides of the door are drawn: the top, the hinge and the
        // bottom. Its fourth side is the opening the arrow leaves by.
        let hinge = pad + s * 15 / 100;
        assert!(
            count(&buf, n, hinge - 1, pad + s * 25 / 100, 3, s / 2) > s / 3,
            "the door has no hinge side"
        );
        assert!(
            count(&buf, n, hinge + 2, pad + s * 80 / 100, s / 4, 3) > s / 8,
            "the door has no bottom"
        );
        // The arrowhead is a chevron, so it has a stroke above the shaft and
        // a stroke below it. One of the two and the arrow is a tick.
        let hx = pad + s * 70 / 100;
        let row = pad + s / 2;
        assert!(
            count(
                &buf,
                n,
                hx,
                pad + s * 28 / 100,
                s / 5,
                (row - 1) - (pad + s * 28 / 100)
            ) > 0,
            "the arrowhead has no upper stroke"
        );
        assert!(
            count(
                &buf,
                n,
                hx,
                row + 2,
                s / 5,
                (pad + s * 72 / 100) - (row + 2)
            ) > 0,
            "the arrowhead has no lower stroke"
        );
    }

    /// The reboot icon is a ring with an arrowhead on its end, and the head
    /// is a FILLED triangle: a filled path of two points fills nothing, so
    /// losing one of its three corners leaves a broken ring with no arrow on
    /// it at all --- which still reads as an icon, just not as "reboot".
    #[test]
    fn the_reboot_arrow_has_a_head_on_the_end_of_its_ring() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;
        let buf = drawn(n, n, |cv| {
            cv.reboot_icon(pad as i32, pad as i32, s as i32, (255, 255, 255))
        });
        // Its left corner, at 0.3r right of the centre and 0.6r above it.
        // Not its centroid: the ring's own curve comes down through that,
        // and nothing the head does would show. The ring at this height is
        // a good two pixels further right, so this box is the head alone.
        let r = s as f32 * 0.35;
        let c = pad as f32 + s as f32 / 2.0;
        let hx = (c + r * 0.3) as usize;
        let hy = (c - r * 0.6) as usize;
        let head = count(&buf, n, hx, hy + 1, 2, 1);
        assert!(head > 0, "no arrowhead at {hx},{}", hy + 1);
    }

    /// The speaker keeps its cone on the left and its mark on the right,
    /// with clear pixels between them. The cone is a filled polygon whose
    /// top edge runs a quarter of the way across: take that edge to the far
    /// side instead and the speaker becomes a wedge over the whole icon,
    /// which is still one shape of about the right size in the right place.
    #[test]
    fn the_speaker_keeps_its_cone_and_its_mark_on_their_own_sides() {
        let s = 24usize;
        let pad = 4usize;
        let n = s + pad * 2;
        let plain = drawn(n, n, |cv| {
            cv.volume_icon(pad as i32, pad as i32, s as i32, (255, 255, 255), false)
        });
        let muted = drawn(n, n, |cv| {
            cv.volume_icon(pad as i32, pad as i32, s as i32, (255, 255, 255), true)
        });
        let gap = pad + s * 56 / 100;
        for (name, buf) in [("speaker", &plain), ("muted speaker", &muted)] {
            assert_eq!(
                count(buf, n, gap, 0, 1, n),
                0,
                "the {name}'s cone runs into its mark"
            );
        }

        // The mark is a CROSS when muted: two strokes crossing, so there is
        // ink at BOTH top corners of its box. With one stroke it is a slash,
        // which reads as a mute that did not take.
        let (cl, ct) = (pad + s * 65 / 100, pad + s * 33 / 100);
        let cr = pad + s * 95 / 100;
        assert!(
            count(&muted, n, cl, ct, 3, 4) > 0,
            "the mute cross starts at its top left"
        );
        assert!(
            count(&muted, n, cr - 2, ct, 3, 4) > 0,
            "and at its top right"
        );

        // And it is a WAVE when not muted: one arc that leaves the cone's
        // level, bulges out and comes back, so it reaches below the middle
        // of the icon as well as above it.
        let right = pad + s * 60 / 100;
        assert!(
            count(&plain, n, right, pad + s / 2 + 2, s * 2 / 5, s * 3 / 10) > 0,
            "the sound wave does not come back below the middle"
        );
    }

    /// The badge's letter: ONE capital, in the middle of the slot. The
    /// square is drawn black here, so what is left in the frame is the
    /// letter and nothing else --- which is the only way to measure any of
    /// the three things that can go wrong with it, since against the real
    /// background the square's own ink swamps the glyph's.
    #[test]
    fn the_badges_letter_is_one_capital_in_the_middle_of_its_slot() {
        let s = 24usize;
        let letter = |ch: char| {
            drawn(s, s, |cv| {
                cv.badge(0, 0, s as i32, ch, (0, 0, 0), (255, 255, 255))
            })
        };
        // A reference glyph, drawn anywhere: a count of lit pixels does not
        // care where a bitmap font put them, only how many there are.
        let plain = |t: &str| {
            let b = drawn(s, s, |cv| {
                cv.text_bold(t, 2, 4, (255, 255, 255));
            });
            count(&b, s, 0, 0, s, s)
        };

        // Capitalised, not merely case-folded. The font draws the two cases
        // differently, and the badge has to draw the capital either way.
        let cap = plain("A");
        assert_ne!(cap, plain("a"), "the font draws 'A' and 'a' alike");
        assert_eq!(
            count(&letter('a'), s, 0, 0, s, s),
            cap,
            "a lowercase initial is not drawn as its capital"
        );
        assert_eq!(
            count(&letter('A'), s, 0, 0, s, s),
            cap,
            "and a capital is not left alone"
        );
        // And a letter whose capital is TWO characters still draws one: an
        // "ss" spilling out of the slot is what made this a `take(1)`.
        assert_eq!(
            count(&letter('\u{df}'), s, 0, 0, s, s),
            plain("S"),
            "only the first letter of a two-letter capital is drawn"
        );

        // Centred in the slot: a letter in the corner reads as a launcher
        // that failed to load an icon rather than as the fallback for one.
        let (bx, by, bw, bh) = bounds(&letter('A'), s, s).expect("the badge drew no letter");
        assert!(
            (bx + bw / 2).abs_diff(s / 2) <= 2 && (by + bh / 2).abs_diff(s / 2) <= 2,
            "the letter sits at {bx},{by} {bw}x{bh} in a slot of {s}"
        );
    }
}
