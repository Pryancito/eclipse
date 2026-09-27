//! The Eclipse OS animated cosmic background, ported from the original
//! smithay compositor (eclipse-old: `sidewind/src/ui.rs`).
//!
//! Static base (rendered once per size):
//! - vertical cosmic gradient, COSMIC_DEEP -> COSMIC_MID, with a soft cyan
//!   nebula glow behind the logo;
//! - deterministic starfield scaled to the output area;
//! - the 48 px blueprint grid, rgb(18,28,55).
//!
//! Animated logo (redrawn every frame inside [`Layout::region`]):
//! - the eclipse crescent: a golden sun disc masked by an offset moon circle
//!   (mask offset `(r/4, -r/5)`, moon radius `9r/10` — the mask shows the
//!   cosmic background through it);
//! - the orbiting text ring "ECLIPSE-SYSTEM-KERNEL-…" (upright characters
//!   with a dark outline, as in the original);
//! - three tech arcs rotating at different speeds and directions;
//! - five pulsing concentric rings;
//! - technical ticks every 5° (major every 30°) with shimmering brightness;
//! - the "ECLIPSE OS" wordmark under the crescent.
//!
//! All radii come from the original design (crescent 140, text ring 165,
//! arcs 145/180/195, ticks 230..255, rings 240..280 — on a 280 px logo) and
//! scale with the output via the original sizing rule
//! `clamp(min(w,h)/2 - 120, 120, 280)` (in logical pixels; an integer HiDPI
//! `scale` multiplies the whole layout so the physical size is unchanged).
//!
//! Performance: circles (rings, the crescent and its moon mask) are rendered
//! by scanline spans — each row visits only the few pixels around the curve
//! crossings instead of the whole bounding box, cutting per-frame cost by an
//! order of magnitude while producing byte-identical output. Animation phases
//! are accumulated in f64 and reduced to each element's own period before the
//! trig call, so a wallpaper left running for weeks never turns steppy from
//! f32 precision loss.

// ---------------------------------------------------------------- palette

const COSMIC_DEEP: Rgb = (2.0 / 255.0, 2.0 / 255.0, 8.0 / 255.0);
const COSMIC_MID: Rgb = (8.0 / 255.0, 15.0 / 255.0, 35.0 / 255.0);
const NEBULA_CYAN: Rgb = (0.0, 70.0 / 255.0, 110.0 / 255.0);
const GRID_BLUE: Rgb = (18.0 / 255.0, 28.0 / 255.0, 55.0 / 255.0);
const ACCENT_CYAN: Rgb = (0.0, 229.0 / 255.0, 1.0);
const ACCENT_VIOLET: Rgb = (180.0 / 255.0, 140.0 / 255.0, 1.0);
const GLOW_HI: Rgb = ACCENT_CYAN;
const GLOW_MID: Rgb = (0.0, 128.0 / 255.0, 160.0 / 255.0);
const GLOW_DIM: Rgb = (0.0, 64.0 / 255.0, 80.0 / 255.0);
const SUN_FILL: Rgb = (1.0, 220.0 / 255.0, 80.0 / 255.0);
const SUN_EDGE: Rgb = (1.0, 200.0 / 255.0, 50.0 / 255.0);

const TEXT_RING: &str = "ECLIPSE-SYSTEM-KERNEL-6.X-STABLE-LINK-ACTIVE-";

type Rgb = (f32, f32, f32);

// ---------------------------------------------------------------- layout

/// Placement of the animated logo on an output.
pub struct Layout {
    pub cx: f32,
    pub cy: f32,
    /// Scale relative to the original 280 px design.
    pub s: f32,
    /// Horizontal squeeze so circles LOOK circular on the monitor.
    ///
    /// The mode the driver sets (synthetic KMS / GOP, or the NVIDIA KMS
    /// driver) is often NOT the panel's native aspect (e.g. a 4:3 1024x768
    /// mode on a 16:9 panel); the panel then stretches the framebuffer and
    /// every circle shows as an ellipse. We pre-squeeze the logo by
    /// fb_aspect/monitor_aspect so the panel's stretch cancels out.
    ///
    /// The monitor aspect is taken, in order of preference, from: the panel's
    /// physical size reported in `wl_output.geometry` when no CLI/env override
    /// was supplied by the caller. Packaging sets `LUNARBG_ASPECT` so Eclipse's
    /// fabricated DRM millimetres cannot cancel the real panel aspect.
    pub sx: f32,
    /// Device-scale factor for stroke weights: line thickness, dot radii,
    /// outline offsets and the crescent's edge band are DESIGN units, so on a
    /// HiDPI output they multiply by the integer scale (radii already do, via
    /// `s`) — otherwise every stroke would render at 1/scale of the physical
    /// weight the scale-1 look defines. Exactly 1.0 on scale-1 outputs.
    pub px: f32,
    /// (x, y, w, h) of the rect that the animation redraws each frame.
    pub region: (usize, usize, usize, usize),
}

/// Parse an aspect spec: `"16:9"`, `"16:10"` or a decimal like `"1.778"`.
pub fn parse_aspect(v: &str) -> Option<f32> {
    let v = v.trim();
    let aspect = if let Some((a, b)) = v.split_once(':') {
        a.trim().parse::<f32>().ok()? / b.trim().parse::<f32>().ok()?
    } else {
        v.parse::<f32>().ok()?
    };
    (aspect.is_finite() && aspect > 0.1).then_some(aspect)
}

/// Read `LUNARBG_ASPECT` if set and valid. Callers load this into an override
/// that must win over fabricated `wl_output.geometry` (Eclipse DRM often invents
/// mm from the mode at ~96 DPI, which would otherwise cancel the packaging env).
pub fn aspect_from_env() -> Option<f32> {
    parse_aspect(&std::env::var("LUNARBG_ASPECT").ok()?)
}

/// `w`/`h` are BUFFER pixels; `scale` is the output's integer HiDPI scale.
/// The design is sized in logical pixels (`w/scale` x `h/scale`) and then
/// multiplied back up, so a 2x output shows the same physical layout with
/// twice the detail.
///
/// `monitor_aspect` is the **effective** panel aspect already chosen by the
/// caller (CLI / env override, else geometry). This function does not re-read
/// the environment — that would let a fabricated geometry hide `LUNARBG_ASPECT`
/// when callers passed `Some(geometry)` first.
pub fn layout(w: usize, h: usize, monitor_aspect: Option<f32>, scale: u32) -> Layout {
    let sc = scale.max(1) as f32;
    let (lw, lh) = (w as f32 / sc, h as f32 / sc);
    let logo_r = ((lw.min(lh) / 2.0) - 120.0).clamp(120.0, 280.0);
    let s = logo_r / 280.0 * sc;
    let cx = w as f32 * 0.5;
    let cy = h as f32 * 0.46;
    let fb_aspect = w as f32 / h as f32;
    let sx = monitor_aspect
        .filter(|a| a.is_finite() && *a > 0.1)
        .map(|mon| (fb_aspect / mon).clamp(0.5, 1.5))
        .unwrap_or(1.0);
    // Outermost animated element: ring 280 + 5 px oscillation, plus the
    // wordmark below at 170 + text height. Take a comfortable margin.
    let reach = (300.0 * s).max(215.0 * s + 40.0 * sc) + 8.0 * sc;
    let x0 = ((cx - reach * sx).floor().max(0.0)) as usize;
    let y0 = ((cy - reach).floor().max(0.0)) as usize;
    let x1 = ((cx + reach * sx).ceil() as usize).min(w);
    let y1 = ((cy + reach).ceil() as usize).min(h);
    Layout {
        cx,
        cy,
        s,
        sx,
        px: sc,
        region: (x0, y0, x1 - x0, y1 - y0),
    }
}

// ---------------------------------------------------------------- base

/// Render the static cosmic base as XRGB8888.
///
/// Returns `None` on overflow or allocation failure instead of aborting via a
/// giant `vec!` under `panic=abort`.
pub fn render_base(w: usize, h: usize, monitor_aspect: Option<f32>, scale: u32) -> Option<Vec<u8>> {
    let lay = layout(w, h, monitor_aspect, scale);
    let sc = scale.max(1) as f32;
    let n3 = w.checked_mul(h)?.checked_mul(3)?;
    let n4 = w.checked_mul(h)?.checked_mul(4)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(n3).ok()?;
    buf.resize(n3, 0f32);

    // Cosmic vertical gradient + nebula glow behind the logo.
    //
    // The gradient is a full-surface pass (one lerp per pixel), so it is split
    // across CPUs by horizontal band. Each band owns disjoint rows and writes
    // only its own slice; the output is identical to the serial loop.
    let fh = h as f32;
    crate::par::par_rows(&mut buf, h, w * 3, |y0, band| {
        for (ry, row) in band.chunks_mut(w * 3).enumerate() {
            let t = (y0 + ry) as f32 / fh;
            let (r, g, b) = lerp3(COSMIC_DEEP, COSMIC_MID, t);
            for x in 0..w {
                row[x * 3] = r;
                row[x * 3 + 1] = g;
                row[x * 3 + 2] = b;
            }
        }
    });
    // Soft radial nebula centred on the logo (squeezed like the logo so the
    // glow stays concentric with it on a stretching panel). Also a large
    // per-pixel pass (~1M px at 1080p), so it is band-split like the gradient:
    // each band writes disjoint rows and the result is byte-identical.
    let neb_r = 420.0 * lay.s + 120.0 * sc;
    let (nx0, nx1) = span(lay.cx, neb_r * lay.sx, w);
    let (ny0, ny1) = span(lay.cy, neb_r, h);
    let stride3 = w * 3;
    crate::par::par_rows(
        &mut buf[ny0 * stride3..ny1 * stride3],
        ny1 - ny0,
        stride3,
        |y0, band| {
            for (ry, row) in band.chunks_mut(stride3).enumerate() {
                let y = ny0 + y0 + ry;
                for x in nx0..nx1 {
                    let d = dist(
                        (x as f32 - lay.cx) / lay.sx + lay.cx,
                        y as f32,
                        lay.cx,
                        lay.cy,
                    );
                    if d < neb_r {
                        let t = 1.0 - d / neb_r;
                        let a = t * t * 0.22;
                        let i = x * 3;
                        row[i] += NEBULA_CYAN.0 * a;
                        row[i + 1] += NEBULA_CYAN.1 * a;
                        row[i + 2] += NEBULA_CYAN.2 * a;
                    }
                }
            }
        },
    );

    // Starfield on the LOGICAL grid: same count and positions at every
    // scale, each star stamped as a scale x scale block, so the sky's density
    // and the stars' physical size match the scale-1 look exactly.
    let scu = scale.max(1) as usize;
    let (lw, lh) = (w / scu, h / scu);
    let count = ((lw * lh) as f32 / 6000.0) as u32;
    for i in 0..count {
        let x = (hash2(i, 1) % lw.max(1) as u32) as i32 * scu as i32;
        let y = (hash2(i, 2) % lh.max(1) as u32) as i32 * scu as i32;
        let bright = 0.25 + (hash2(i, 3) % 1000) as f32 / 1000.0 * 0.75;
        star_block(&mut buf, w, h, x, y, scu, (bright, bright, bright * 0.95));
        if bright > 0.85 {
            let half = bright * 0.35;
            for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                star_block(
                    &mut buf,
                    w,
                    h,
                    x + dx * scu as i32,
                    y + dy * scu as i32,
                    scu,
                    (half, half, half),
                );
            }
        }
    }

    // Blueprint grid, 48 logical px; each line is `scale` px wide so its
    // physical weight matches the scale-1 look.
    grid_pass(&mut buf, w, h, 48 * scu, scu);

    // Quantise to XRGB8888 with light dithering noise. Also a full-surface
    // pass: split `out` by band, read the float buffer by absolute pixel index.
    // The dither noise is a per-pixel hash of the byte offset, so bands compute
    // exactly the bytes the serial loop would — the image is identical.
    let mut out = Vec::new();
    out.try_reserve_exact(n4).ok()?;
    out.resize(n4, 0u8);
    let src: &[f32] = &buf;
    crate::par::par_rows(&mut out, h, w * 4, |y0, band| {
        for (ry, orow) in band.chunks_mut(w * 4).enumerate() {
            let y = y0 + ry;
            for x in 0..w {
                let px = y * w + x;
                let i = px * 3;
                let o = x * 4;
                let n = (hash2(i as u32, 0x9e37_79b9) as f32 / u32::MAX as f32 - 0.5) * 1.5;
                let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + n).round().clamp(0.0, 255.0) as u8;
                orow[o] = q(src[i + 2]);
                orow[o + 1] = q(src[i + 1]);
                orow[o + 2] = q(src[i]);
                orow[o + 3] = 0xff;
            }
        }
    });
    Some(out)
}

// ---------------------------------------------------------------- frame

/// Draw one animation frame: restore the logo region from `base`, then paint
/// the animated logo. `t_ms` is a monotonic millisecond clock; the original
/// compositor advanced `counter` once per ~60 Hz frame, so `counter =
/// t_ms * 0.06` reproduces its speeds.
pub fn render_frame(frame: &mut [u8], w: usize, base: &[u8], lay: &Layout, t_ms: u64) {
    if w == 0 {
        return;
    }
    let stride = w.saturating_mul(4);
    if stride == 0 || frame.len() < stride || base.len() != frame.len() {
        return;
    }
    let h = frame.len() / stride;
    let (rx, ry, rw, rh) = lay.region;
    if rw == 0 || rh == 0 || rx.saturating_add(rw) > w || ry.saturating_add(rh) > h {
        return;
    }
    for row in 0..rh {
        let off = ((ry + row) * w + rx) * 4;
        let end = off + rw * 4;
        if end > frame.len() || end > base.len() {
            return;
        }
        frame[off..end].copy_from_slice(&base[off..end]);
    }

    let mut pb = PixBuf {
        data: frame,
        w,
        clip: (rx, ry, rx + rw, ry + rh),
        sx: lay.sx,
        px: lay.px,
    };
    // Accumulate the phase in f64 and fold each element to its own period
    // right before the trig call: after days of uptime an f32 phase loses
    // sub-frame resolution and the animation turns visibly steppy.
    let counter = t_ms as f64 * 0.06;
    const TAU64: f64 = std::f64::consts::TAU;
    let (cx, cy, s) = (lay.cx, lay.cy, lay.s);
    // Stroke weights are design units: x device scale (see Layout::px).
    let px = lay.px;

    // --- five pulsing concentric rings (backmost) ---
    for (i, base_r) in [280.0f32, 275.0, 260.0, 255.0, 240.0].iter().enumerate() {
        let osc = ((counter * (0.01 + i as f64 * 0.005)) % TAU64).sin() as f32 * 5.0;
        let r = (base_r + osc) * s;
        let color = if i % 2 == 0 { GLOW_DIM } else { ACCENT_VIOLET };
        let alpha = if i % 2 == 0 { 0.55 } else { 0.18 };
        pb.ring(cx, cy, r, 1.4 * px, color, alpha);
    }

    // --- technical ticks every 5°, major every 30°, slow shimmer+drift ---
    let tick_phase = ((counter * 0.05) % 360.0) as f32; // degrees
    let shim_phase = ((counter * 0.02) % TAU64) as f32;
    for angle in (0..360).step_by(5) {
        let is_major = angle % 30 == 0;
        let a = (angle as f32 + tick_phase).to_radians();
        let (r0, r1) = if is_major {
            (230.0 * s, 255.0 * s)
        } else {
            (235.0 * s, 250.0 * s)
        };
        let shimmer = (a * 2.0 + shim_phase).sin().abs();
        let (color, alpha) = if is_major {
            (ACCENT_CYAN, 0.25 + 0.45 * shimmer)
        } else {
            (GLOW_MID, 0.15 + 0.25 * shimmer)
        };
        let (sin, cos) = a.sin_cos();
        pb.line(
            cx + cos * r0 * pb.sx,
            cy + sin * r0,
            cx + cos * r1 * pb.sx,
            cy + sin * r1,
            1.2 * px,
            color,
            alpha,
        );
    }

    // --- three tech arcs at different speeds/directions ---
    // 3600° is a common period of the three arc speeds (x1.5 / x0.8 / x1.2),
    // so the fold is seamless for all of them.
    let arc_rot = ((counter * 0.5) % 3600.0) as f32; // degrees
    pb.arc(
        cx,
        cy,
        180.0 * s,
        -arc_rot * 1.5,
        60.0,
        2.0 * px,
        GLOW_HI,
        0.9,
    );
    pb.arc(
        cx,
        cy,
        195.0 * s,
        arc_rot * 0.8 + 180.0,
        30.0,
        2.0 * px,
        ACCENT_VIOLET,
        0.9,
    );
    pb.arc(
        cx,
        cy,
        145.0 * s,
        arc_rot * 1.2,
        45.0,
        2.0 * px,
        ACCENT_CYAN,
        0.9,
    );

    // --- orbiting text ring (upright chars, dark outline) ---
    let n = TEXT_RING.chars().count() as f32;
    let rot_phase = ((counter * 0.12) % 360.0) as f32; // degrees
    let text_r = 165.0 * s;
    let scale = ((2.0 * s).round() as usize).max(1);
    for (i, ch) in TEXT_RING.chars().enumerate() {
        let a = ((i as f32 * 360.0 / n) + rot_phase).to_radians();
        let (sin, cos) = a.sin_cos();
        let gx = cx + cos * text_r * pb.sx;
        let gy = cy + sin * text_r;
        pb.glyph_outlined(ch, gx, gy, scale, GLOW_HI, 0.85, COSMIC_DEEP);
    }

    // --- the eclipse crescent core ---
    // Scanline spans: each row visits only the pixels inside the sun disc,
    // and the moon mask's fully-transparent interior (where a == 0 anyway)
    // is skipped without being evaluated. Byte-identical to the full scan.
    let sun_r = 140.0 * s;
    let moon_r = sun_r * 9.0 / 10.0;
    // Edge tint band: 6 design px wide, so x device scale.
    let edge_band = 6.0 * px;
    // The moon-mask centre offset lives in the round pre-stretch space, so
    // its X component squeezes with everything else.
    let (mx, my) = (cx + sun_r / 4.0 * pb.sx, cy - sun_r / 5.0);
    let (sy0, sy1) = pb.clip_span_y(cy, sun_r + 2.0);
    for y in sy0..sy1 {
        let dy = y as f32 - cy;
        // cover > 0 needs d < sun_r + 0.5; solve the row's x extent (+1 px
        // of safety margin) instead of scanning the whole bounding box.
        let s2 = (sun_r + 0.5) * (sun_r + 0.5) - dy * dy;
        if s2 <= 0.0 {
            continue;
        }
        let half = s2.sqrt() * pb.sx + 1.0;
        let (x0, x1) = pb.clip_x_range(cx - half, cx + half);
        // Fully-masked moon interior: dm <= moon_r - 0.5 gives mask == 1 and
        // a == 0, so those pixels can be skipped. Shrink by 1 px so boundary
        // pixels are still evaluated exactly as before.
        let dmy = y as f32 - my;
        let m2 = (moon_r - 0.5) * (moon_r - 0.5) - dmy * dmy;
        let hx = if m2 > 0.0 {
            m2.sqrt() * pb.sx - 1.0
        } else {
            0.0
        };
        let (a1, b0) = if hx > 1.0 {
            let lo = ((mx - hx).ceil().max(x0 as f32) as usize).min(x1);
            let hi = (((mx + hx).floor().max(0.0) as usize) + 1).clamp(lo, x1);
            (lo, hi)
        } else {
            (x1, x1)
        };
        for (xa, xb) in [(x0, a1), (b0, x1)] {
            for x in xa..xb {
                let d = pb.edist(x as f32, y as f32, cx, cy);
                let cover = (sun_r - d + 0.5).clamp(0.0, 1.0);
                if cover <= 0.0 {
                    continue;
                }
                // Moon mask: transparent, the cosmic base shows through.
                let dm = pb.edist(x as f32, y as f32, mx, my);
                let mask = (moon_r - dm + 0.5).clamp(0.0, 1.0);
                let a = cover * (1.0 - mask);
                if a <= 0.0 {
                    continue;
                }
                // Edge tint on the outer 6 (design) px of the sun.
                let edge = ((sun_r - d) / edge_band).clamp(0.0, 1.0);
                let color = lerp3(SUN_EDGE, SUN_FILL, edge);
                pb.blend(x, y, color, a);
            }
        }
    }

    // --- "ECLIPSE OS" wordmark under the crescent ---
    let scale = ((3.0 * s).round() as usize).max(2);
    let text = "ECLIPSE OS";
    let advance = (6 * scale) as f32;
    let total = text.len() as f32 * advance - scale as f32;
    // Below the text ring (165) so the wordmark never collides with the
    // orbiting characters. The original drew it at +170, overlapping.
    let ty = cy + 215.0 * s;
    for (i, ch) in text.chars().enumerate() {
        let gx = cx - total / 2.0 + i as f32 * advance + advance / 2.0;
        pb.glyph_outlined(ch, gx, ty, scale, (0.90, 0.96, 1.0), 0.95, COSMIC_DEEP);
    }
}

// ------------------------------------------------------------- draw utils

struct PixBuf<'a> {
    data: &'a mut [u8],
    w: usize,
    /// (x0, y0, x1, y1) — drawing outside is discarded.
    clip: (usize, usize, usize, usize),
    /// Horizontal squeeze (see [`Layout::sx`]): circles are drawn as ellipses
    /// with X semi-axis `r * sx` so a stretching monitor shows them round.
    sx: f32,
    /// Device-scale factor for stroke weights (see [`Layout::px`]).
    px: f32,
}

impl PixBuf<'_> {
    fn blend(&mut self, x: usize, y: usize, c: Rgb, a: f32) {
        if x < self.clip.0 || y < self.clip.1 || x >= self.clip.2 || y >= self.clip.3 {
            return;
        }
        let i = (y * self.w + x) * 4;
        let a = a.clamp(0.0, 1.0);
        let mix = |old: u8, new: f32| -> u8 {
            (old as f32 * (1.0 - a) + new * 255.0 * a)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        let Some(px) = self.data.get_mut(i..i + 3) else {
            return;
        };
        px[0] = mix(px[0], c.2);
        px[1] = mix(px[1], c.1);
        px[2] = mix(px[2], c.0);
    }

    fn clip_span_x(&self, c: f32, r: f32) -> (usize, usize) {
        let lo = (c - r).floor().max(self.clip.0 as f32) as usize;
        let hi = ((c + r).ceil() as usize).saturating_add(1).min(self.clip.2);
        (lo, hi)
    }

    fn clip_span_y(&self, c: f32, r: f32) -> (usize, usize) {
        let lo = (c - r).floor().max(self.clip.1 as f32) as usize;
        let hi = ((c + r).ceil() as usize).saturating_add(1).min(self.clip.3);
        (lo, hi)
    }

    /// Clamp a floating x interval to the clip window, as `lo..hi` pixels.
    fn clip_x_range(&self, lo: f32, hi: f32) -> (usize, usize) {
        let a = lo.floor().max(self.clip.0 as f32) as usize;
        let b = (hi.ceil().max(0.0) as usize)
            .saturating_add(1)
            .min(self.clip.2);
        (a, b)
    }

    /// Undo the horizontal squeeze: distance is measured in the round,
    /// pre-stretch space so an on-screen ellipse reads as a circle.
    fn edist(&self, x: f32, y: f32, cx: f32, cy: f32) -> f32 {
        dist((x - cx) / self.sx + cx, y, cx, cy)
    }

    /// Thin anti-aliased ring, rendered by scanline spans: each row visits
    /// only the few pixels around the annulus' two crossings instead of the
    /// whole bounding disc. The rings dominated per-frame cost before this
    /// (~1.6M distance evaluations/frame at 1080p); output is byte-identical.
    fn ring(&mut self, cx: f32, cy: f32, r: f32, thick: f32, c: Rgb, alpha: f32) {
        let r_out = r + thick + 1.0;
        let r_in = (r - thick - 1.0).max(0.0);
        let (y0, y1) = self.clip_span_y(cy, r_out);
        for y in y0..y1 {
            let dy = y as f32 - cy;
            let out2 = r_out * r_out - dy * dy;
            if out2 <= 0.0 {
                continue;
            }
            // Screen-space half-widths of the outer/inner circle crossings,
            // with 1 px of safety margin each so anti-aliased edge pixels are
            // evaluated exactly as the full scan would.
            let half_out = out2.sqrt() * self.sx + 1.0;
            let in2 = r_in * r_in - dy * dy;
            let half_in = if in2 > 0.0 {
                in2.sqrt() * self.sx - 1.0
            } else {
                0.0
            };
            if half_in > 1.0 {
                let (lx0, lx1) = self.clip_x_range(cx - half_out, cx - half_in);
                let (rx0, rx1) = self.clip_x_range(cx + half_in, cx + half_out);
                self.ring_span(y, lx0, lx1, cx, cy, r, thick, c, alpha);
                // rx0 clamps to lx1 so touching spans never blend a pixel twice.
                self.ring_span(y, rx0.max(lx1), rx1, cx, cy, r, thick, c, alpha);
            } else {
                let (x0, x1) = self.clip_x_range(cx - half_out, cx + half_out);
                self.ring_span(y, x0, x1, cx, cy, r, thick, c, alpha);
            }
        }
    }

    /// One row segment of [`PixBuf::ring`]: the original per-pixel math.
    #[allow(clippy::too_many_arguments)]
    fn ring_span(
        &mut self,
        y: usize,
        x0: usize,
        x1: usize,
        cx: f32,
        cy: f32,
        r: f32,
        thick: f32,
        c: Rgb,
        alpha: f32,
    ) {
        for x in x0..x1 {
            let d = self.edist(x as f32, y as f32, cx, cy);
            let cover = (thick / 2.0 - (d - (r - thick / 2.0)).abs() + 0.5).clamp(0.0, 1.0);
            if cover > 0.0 {
                self.blend(x, y, c, alpha * cover);
            }
        }
    }

    /// Anti-aliased thick line (capsule).
    fn line(&mut self, ax: f32, ay: f32, bx: f32, by: f32, thick: f32, c: Rgb, alpha: f32) {
        let half = thick / 2.0;
        let x0 = (ax.min(bx) - half - 1.0).floor().max(self.clip.0 as f32) as usize;
        let x1 = (((ax.max(bx) + half + 1.0).ceil() as usize) + 1).min(self.clip.2);
        let y0 = (ay.min(by) - half - 1.0).floor().max(self.clip.1 as f32) as usize;
        let y1 = (((ay.max(by) + half + 1.0).ceil() as usize) + 1).min(self.clip.3);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = capsule_dist(x as f32, y as f32, ax, ay, bx, by);
                let cover = (half - d + 0.5).clamp(0.0, 1.0);
                if cover > 0.0 {
                    self.blend(x, y, c, alpha * cover);
                }
            }
        }
    }

    /// Arc as in the original: the span walked in 20 line segments, with
    /// glowing endpoint dots.
    #[allow(clippy::too_many_arguments)]
    fn arc(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        start_deg: f32,
        span_deg: f32,
        thick: f32,
        c: Rgb,
        alpha: f32,
    ) {
        const SEGS: usize = 20;
        let mut prev: Option<(f32, f32)> = None;
        for k in 0..=SEGS {
            let a = (start_deg + span_deg * k as f32 / SEGS as f32).to_radians();
            let (sin, cos) = a.sin_cos();
            let p = (cx + cos * r * self.sx, cy + sin * r);
            if let Some(q) = prev {
                self.line(q.0, q.1, p.0, p.1, thick, c, alpha);
            }
            prev = Some(p);
        }
        for k in [0usize, SEGS] {
            let a = (start_deg + span_deg * k as f32 / SEGS as f32).to_radians();
            let (sin, cos) = a.sin_cos();
            self.dot(
                cx + cos * r * self.sx,
                cy + sin * r,
                2.5 * self.px,
                c,
                alpha,
            );
        }
    }

    fn dot(&mut self, cx: f32, cy: f32, r: f32, c: Rgb, alpha: f32) {
        let (x0, x1) = self.clip_span_x(cx, r + 1.0);
        let (y0, y1) = self.clip_span_y(cy, r + 1.0);
        for y in y0..y1 {
            for x in x0..x1 {
                // Use edist so dots stay round on the same ellipse as rings.
                let d = self.edist(x as f32, y as f32, cx, cy);
                let cover = (r - d + 0.5).clamp(0.0, 1.0);
                if cover > 0.0 {
                    self.blend(x, y, c, alpha * cover);
                }
            }
        }
    }

    /// A 5x7 glyph centred at (gx, gy), upright, with a 1-design-px dark
    /// outline (four offset passes, as the original text ring does; the
    /// offset is x device scale so the outline keeps its physical weight).
    fn glyph_outlined(
        &mut self,
        ch: char,
        gx: f32,
        gy: f32,
        scale: usize,
        c: Rgb,
        alpha: f32,
        outline: Rgb,
    ) {
        let gw = (5 * scale) as f32;
        let gh = (7 * scale) as f32;
        let left = (gx - gw / 2.0) as i32;
        let top = (gy - gh / 2.0) as i32;
        let glyph = glyph5x7(ch);
        let o = (self.px.round() as i32).max(1);
        for pass in 0..5 {
            let (dx, dy, color, a) = match pass {
                0 => (-o, 0i32, outline, alpha * 0.9),
                1 => (o, 0, outline, alpha * 0.9),
                2 => (0, -o, outline, alpha * 0.9),
                3 => (0, o, outline, alpha * 0.9),
                _ => (0, 0, c, alpha),
            };
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..5 {
                    if bits & (0b10000 >> col) == 0 {
                        continue;
                    }
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let x = left + (col * scale + sx) as i32 + dx;
                            let y = top + (row * scale + sy) as i32 + dy;
                            if x >= 0 && y >= 0 {
                                self.blend(x as usize, y as usize, color, a);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn glyph5x7(c: char) -> [u8; 7] {
    match c {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        _ => [0; 7],
    }
}

// --------------------------------------------------------------- helpers

fn lerp3(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

fn dist(x: f32, y: f32, cx: f32, cy: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
}

fn capsule_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    dist(px, py, ax + dx * t, ay + dy * t)
}

fn span(c: f32, r: f32, limit: usize) -> (usize, usize) {
    let lo = (c - r).floor().max(0.0) as usize;
    let hi = ((c + r).ceil() as usize + 1).min(limit);
    (lo, hi)
}

/// Stamp a star as a `scu` x `scu` block (a single pixel at scale 1).
fn star_block(buf: &mut [f32], w: usize, h: usize, x: i32, y: i32, scu: usize, c: Rgb) {
    for dy in 0..scu as i32 {
        for dx in 0..scu as i32 {
            add_px_f(buf, w, h, x + dx, y + dy, c);
        }
    }
}

fn add_px_f(buf: &mut [f32], w: usize, h: usize, x: i32, y: i32, c: Rgb) {
    if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
        return;
    }
    let i = (y as usize * w + x as usize) * 3;
    buf[i] += c.0;
    buf[i + 1] += c.1;
    buf[i + 2] += c.2;
}

/// Paint the blueprint grid: full rows every `spacing` pixels and full columns
/// every `spacing` pixels, each line `line` pixels thick.
///
/// **Every pixel is blended AT MOST ONCE.** The blend is not idempotent
/// (`v*(1-a) + c*a` applied twice lands twice as far towards the grid colour),
/// so a pixel caught by both passes would come out darker than the lines
/// crossing it: a visibly wrong dot at every intersection of the grid. The
/// column pass therefore skips the rows the row pass already painted, which is
/// exactly `y % spacing < line`. Extracted from `render_base` so that
/// invariant has somewhere to be tested.
fn grid_pass(buf: &mut [f32], w: usize, h: usize, spacing: usize, line: usize) {
    // `.min(h)` / `.min(w)` on the line bands below are an optimisation, not a
    // guard: `blend_px_f` discards a coordinate past the buffer, so dropping
    // them changes no pixel. Kept so the loops do not walk rows that cannot
    // exist.
    let (spacing, line) = (spacing.max(1), line.max(1));
    for y in (0..h).step_by(spacing) {
        for yy in y..(y + line).min(h) {
            for x in 0..w {
                blend_px_f(buf, w, h, x, yy, GRID_BLUE, 0.38);
            }
        }
    }
    for x in (0..w).step_by(spacing) {
        for xx in x..(x + line).min(w) {
            for y in 0..h {
                // Skip the rows the row pass already painted.
                if y % spacing >= line {
                    blend_px_f(buf, w, h, xx, y, GRID_BLUE, 0.38);
                }
            }
        }
    }
}

fn blend_px_f(buf: &mut [f32], w: usize, h: usize, x: usize, y: usize, c: Rgb, a: f32) {
    // Bounds-check like `add_px_f`, which means the X check too: this used to
    // rely on `get_mut` alone, and a row-major index hides an x overflow
    // instead of catching it. With x == w the slice offset lands on pixel 0 of
    // the NEXT row, inside the buffer, so `get_mut` succeeds and the write
    // silently lands on the wrong pixel -- a stray dot one row down, in the
    // one helper written to make a coordinate bug harmless. Only x == w on the
    // last row is caught by the slice, so the discard the comment promised was
    // there for a single pixel of the whole buffer.
    if x >= w || y >= h {
        return;
    }
    let i = (y * w + x) * 3;
    let Some(px) = buf.get_mut(i..i + 3) else {
        return;
    };
    px[0] = px[0] * (1.0 - a) + c.0 * a;
    px[1] = px[1] * (1.0 - a) + c.1 * a;
    px[2] = px[2] * (1.0 - a) + c.2 * a;
}

fn hash2(a: u32, b: u32) -> u32 {
    let mut x = a.wrapping_mul(0x85eb_ca6b) ^ b.wrapping_mul(0xc2b2_ae35);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alpha byte of every pixel: the wallpaper is committed as XRGB8888
    /// and declared fully opaque, so a 0 here is a transparent hole in a
    /// surface the compositor was told it may cull everything beneath.
    fn every_alpha_is_opaque(buf: &[u8]) -> bool {
        buf.as_chunks::<4>().0.iter().all(|px| px[3] == 0xff)
    }

    #[test]
    fn an_aspect_is_read_as_a_ratio_or_a_number_and_nonsense_is_refused() {
        assert_eq!(parse_aspect("16:9"), Some(16.0 / 9.0));
        assert_eq!(parse_aspect(" 16 : 9 "), Some(16.0 / 9.0));
        assert_eq!(parse_aspect("1.7777778"), Some(1.7777778));
        assert_eq!(parse_aspect("4:3"), Some(4.0 / 3.0));
        // Refused, each for its own reason.
        assert_eq!(parse_aspect(""), None);
        assert_eq!(parse_aspect("abc"), None);
        assert_eq!(parse_aspect("16:"), None);
        assert_eq!(parse_aspect(":9"), None);
        assert_eq!(parse_aspect("-2"), None); // negative
        assert_eq!(parse_aspect("0"), None); // zero
        assert_eq!(parse_aspect("0.05"), None); // past the 0.1 floor
        assert_eq!(parse_aspect("16:0"), None); // divide by zero -> inf
        assert_eq!(parse_aspect("nan"), None);
        assert_eq!(parse_aspect("inf"), None);
    }

    #[test]
    fn the_animated_region_always_sits_inside_the_buffer() {
        // `render_frame` restores this rect from the base and clips every
        // stroke to it; a region past the buffer is an early return, so the
        // whole logo silently stops being drawn.
        for scale in 1..=4u32 {
            for (w, h) in [
                (1usize, 1usize),
                (2, 3),
                (16, 16),
                (320, 240),
                (640, 480),
                (1280, 720),
                (1920, 1080),
                (3840, 2160),
                (1080, 1920), // portrait
                (3840, 600),  // ultrawide
            ] {
                let lay = layout(w, h, None, scale);
                let (rx, ry, rw, rh) = lay.region;
                assert!(
                    rx + rw <= w && ry + rh <= h,
                    "{w}x{h} scale {scale}: region {:?} leaves the buffer",
                    lay.region
                );
            }
        }
    }

    #[test]
    fn a_stretching_panel_squeezes_the_logo_and_a_bogus_aspect_does_not() {
        // 1920x1080 buffer shown on a 16:10 panel: circles must be drawn as
        // ellipses so they read round, i.e. sx != 1.
        let stretched = layout(1920, 1080, Some(16.0 / 10.0), 1);
        assert!(stretched.sx > 1.0, "sx = {}", stretched.sx);
        // Same aspect as the buffer: nothing to correct.
        let square = layout(1920, 1080, Some(16.0 / 9.0), 1);
        assert!((square.sx - 1.0).abs() < 1e-5, "sx = {}", square.sx);
        // No aspect, or one that cannot be believed: no squeeze at all,
        // never a NaN that would poison every coordinate downstream.
        for a in [
            None,
            Some(f32::NAN),
            Some(0.0),
            Some(-3.0),
            Some(f32::INFINITY),
        ] {
            let l = layout(1920, 1080, a, 1);
            assert_eq!(l.sx, 1.0, "aspect {a:?} should not squeeze");
        }
        // And the squeeze is clamped, so an absurd panel aspect cannot fling
        // the logo off the surface.
        for a in [0.2f32, 0.5, 100.0] {
            let l = layout(1920, 1080, Some(a), 1);
            assert!((0.5..=1.5).contains(&l.sx), "aspect {a}: sx = {}", l.sx);
        }
    }

    #[test]
    fn the_hidpi_scale_multiplies_the_design_not_the_layout() {
        // The design is sized in logical pixels and multiplied back up, so a
        // 2x buffer of the same logical size must place the logo at twice the
        // coordinates with twice the stroke weight.
        let one = layout(1920, 1080, None, 1);
        let two = layout(3840, 2160, None, 2);
        assert!((two.cx - one.cx * 2.0).abs() < 1e-3);
        assert!((two.cy - one.cy * 2.0).abs() < 1e-3);
        assert!((two.s - one.s * 2.0).abs() < 1e-3);
        assert_eq!(two.px, 2.0);
        assert_eq!(one.px, 1.0);
        // A 0 scale must not divide by zero: it is treated as 1.
        let zero = layout(1920, 1080, None, 0);
        assert_eq!(zero.px, 1.0);
        assert!(zero.s.is_finite() && zero.s > 0.0);
    }

    #[test]
    fn the_base_scene_is_opaque_the_right_size_and_reproducible() {
        let (w, h) = (97usize, 61usize); // deliberately not multiples of 48
        let a = render_base(w, h, None, 1).expect("base");
        assert_eq!(a.len(), w * h * 4);
        assert!(every_alpha_is_opaque(&a));
        // Two builds of the same rootfs must give the same wallpaper: the
        // dither noise is a hash of the pixel offset, not a random number.
        let b = render_base(w, h, None, 1).expect("base");
        assert_eq!(a, b, "the base scene is not reproducible");
    }

    #[test]
    fn a_size_that_would_overflow_the_allocation_returns_none() {
        // Instead of aborting inside a giant `vec!` under panic=abort.
        assert!(render_base(usize::MAX, 2, None, 1).is_none());
        assert!(render_base(usize::MAX / 3, 4, None, 1).is_none());
    }

    #[test]
    fn a_frame_repaints_only_the_region_it_declared() {
        // The commit damages the whole buffer, but the RENDER only restores
        // and repaints `region`. A stroke escaping it would leave a trail
        // that no later frame ever cleans up, because nothing outside the
        // region is ever restored from the base again.
        let (w, h) = (400usize, 300usize);
        let base = render_base(w, h, None, 1).expect("base");
        let lay = layout(w, h, None, 1);
        let (rx, ry, rw, rh) = lay.region;
        let mut frame = base.clone();
        render_frame(&mut frame, w, &base, &lay, 1234);
        assert_ne!(frame, base, "the frame painted nothing at all");
        for y in 0..h {
            for x in 0..w {
                let inside = x >= rx && x < rx + rw && y >= ry && y < ry + rh;
                if inside {
                    continue;
                }
                let i = (y * w + x) * 4;
                assert_eq!(
                    frame[i..i + 4],
                    base[i..i + 4],
                    "pixel ({x},{y}) outside region {:?} was touched",
                    lay.region
                );
            }
        }
        assert!(every_alpha_is_opaque(&frame));
    }

    #[test]
    fn a_frame_starts_from_the_base_so_the_previous_one_cannot_accumulate() {
        // Buffers alternate, so the one being drawn carries the logo from two
        // frames ago; `render_frame` restores the region from the base first.
        // Without that restore the anti-aliased strokes pile up into a smear.
        let (w, h) = (300usize, 220usize);
        let base = render_base(w, h, None, 1).expect("base");
        let lay = layout(w, h, None, 1);
        let mut once = base.clone();
        render_frame(&mut once, w, &base, &lay, 900);
        // Same clock, but on top of a frame that already holds another one.
        let mut twice = base.clone();
        render_frame(&mut twice, w, &base, &lay, 4321);
        render_frame(&mut twice, w, &base, &lay, 900);
        assert_eq!(once, twice, "an earlier frame survived into this one");
    }

    #[test]
    fn a_frame_refuses_a_buffer_that_does_not_match_instead_of_panicking() {
        let (w, h) = (64usize, 48usize);
        let base = render_base(w, h, None, 1).expect("base");
        let lay = layout(w, h, None, 1);
        // Zero width, a shorter frame than one row, and a base of a different
        // size are all early returns: under panic=abort a slice panic here
        // would kill the wallpaper outright.
        let mut f = base.clone();
        render_frame(&mut f, 0, &base, &lay, 10);
        assert_eq!(f, base);
        let mut short = vec![0u8; 4];
        render_frame(&mut short, w, &base, &lay, 10);
        assert_eq!(short, vec![0u8; 4]);
        let mut f = base.clone();
        let other = render_base(32, 24, None, 1).expect("base");
        render_frame(&mut f, w, &other, &lay, 10);
        assert_eq!(f, base, "a mismatched base must be refused, not blitted");
        // And a region bigger than the buffer (a layout from another size).
        // Checked on a buffer that does NOT already hold the base, because a
        // partial restore of base onto base is invisible: the rows are copied
        // and the run only stops at the first row that would leave the slice,
        // after which the strokes are clipped to a window wider than the
        // buffer, so `blend` indexes row y+1 for every x past the edge.
        let big = layout(1920, 1080, None, 1);
        let mut blank = vec![0u8; base.len()];
        render_frame(&mut blank, w, &base, &big, 10);
        assert!(
            blank.iter().all(|b| *b == 0),
            "a region from another size painted into the buffer anyway"
        );
        let mut f = base.clone();
        render_frame(&mut f, w, &base, &big, 10);
        assert_eq!(f, base);

        // The case the per-row length check alone does NOT catch: a region
        // that fits inside the buffer as a flat slice but runs off the end of
        // its ROW. `off..end` for every row stays in bounds, so the restore
        // happily copies across the row boundary, and the clip then admits
        // x >= w so every stroke past the edge lands one row down. This is why
        // the region is validated against w and h and not just against len.
        let escaping = Layout {
            region: (w - 4, 0, 10, 4),
            ..layout(w, h, None, 1)
        };
        assert!(escaping.region.0 + escaping.region.2 > w);
        let mut blank = vec![0u8; base.len()];
        render_frame(&mut blank, w, &base, &escaping, 10);
        assert!(
            blank.iter().all(|b| *b == 0),
            "a region running off the end of its row was drawn anyway"
        );
        // Same for one that runs off the bottom.
        let tall = Layout {
            region: (0, h - 2, 4, 9),
            ..layout(w, h, None, 1)
        };
        let mut blank = vec![0u8; base.len()];
        render_frame(&mut blank, w, &base, &tall, 10);
        assert!(
            blank.iter().all(|b| *b == 0),
            "a region running off the bottom was drawn anyway"
        );
    }

    #[test]
    fn an_x_past_the_row_is_discarded_and_not_written_to_the_next_row() {
        // `blend_px_f` bounds-checked with `get_mut` alone, which cannot see an
        // x overflow: a row-major index with x == w lands on pixel 0 of the
        // NEXT row, still inside the buffer, so the write landed on the wrong
        // pixel instead of being dropped.
        let (w, h) = (4usize, 3usize);
        let mut buf = vec![0f32; w * h * 3];
        blend_px_f(&mut buf, w, h, w, 0, (1.0, 1.0, 1.0), 1.0);
        assert!(
            buf.iter().all(|v| *v == 0.0),
            "x == w wrote somewhere: {buf:?}"
        );
        blend_px_f(&mut buf, w, h, w + 7, 1, (1.0, 1.0, 1.0), 1.0);
        assert!(buf.iter().all(|v| *v == 0.0), "x past w wrote somewhere");
        blend_px_f(&mut buf, w, h, 0, h, (1.0, 1.0, 1.0), 1.0);
        assert!(buf.iter().all(|v| *v == 0.0), "y == h wrote somewhere");
        // A y big enough to WRAP `y * w` is why the explicit check has to be
        // there and not just the slice bound: the wrapped product can land
        // back inside the buffer (and in a debug build it panics outright,
        // which under panic=abort is the wallpaper gone).
        blend_px_f(&mut buf, w, h, 0, usize::MAX, (1.0, 1.0, 1.0), 1.0);
        blend_px_f(&mut buf, w, h, 0, usize::MAX / w + 1, (1.0, 1.0, 1.0), 1.0);
        assert!(buf.iter().all(|v| *v == 0.0), "a wrapped y wrote somewhere");
        // A coordinate inside still paints, at the pixel asked for.
        blend_px_f(&mut buf, w, h, 3, 1, (1.0, 0.5, 0.25), 1.0);
        let i = (w + 3) * 3;
        assert_eq!((buf[i], buf[i + 1], buf[i + 2]), (1.0, 0.5, 0.25));
    }

    #[test]
    fn the_two_pixel_helpers_agree_on_what_is_out_of_bounds() {
        let (w, h) = (5usize, 4usize);
        for y in 0..h + 2 {
            for x in 0..w + 2 {
                let mut add = vec![0f32; w * h * 3];
                let mut blend = vec![0f32; w * h * 3];
                add_px_f(&mut add, w, h, x as i32, y as i32, (1.0, 1.0, 1.0));
                blend_px_f(&mut blend, w, h, x, y, (1.0, 1.0, 1.0), 1.0);
                assert_eq!(
                    add.iter().any(|v| *v != 0.0),
                    blend.iter().any(|v| *v != 0.0),
                    "({x},{y}) in a {w}x{h} buffer: the two helpers disagree"
                );
            }
        }
        // Negative coordinates only `add_px_f` can be handed (it takes i32).
        let mut add = vec![0f32; w * h * 3];
        add_px_f(&mut add, w, h, -1, 0, (1.0, 1.0, 1.0));
        add_px_f(&mut add, w, h, 0, -1, (1.0, 1.0, 1.0));
        assert!(add.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn a_star_block_off_the_edge_is_clipped_not_wrapped() {
        let (w, h) = (6usize, 5usize);
        let mut buf = vec![0f32; w * h * 3];
        // A 3x3 block anchored one pixel inside the right edge: two of its
        // three columns fall off and must not appear on the next row.
        star_block(&mut buf, w, h, w as i32 - 1, 0, 3, (1.0, 1.0, 1.0));
        for y in 0..h {
            for x in 0..w {
                let lit = buf[(y * w + x) * 3] != 0.0;
                let want = x == w - 1 && y < 3;
                assert_eq!(lit, want, "({x},{y}) lit={lit} want={want}");
            }
        }
    }

    #[test]
    fn the_grid_blends_every_pixel_at_most_once() {
        // The blend is not idempotent, so a pixel caught by both passes comes
        // out darker than the lines crossing it: a wrong dot at every
        // intersection. Painting a 1.0 white over a 0.0 buffer at a=0.38
        // leaves exactly 0.38; twice leaves 0.6156.
        for (w, h, spacing, line) in [
            (200usize, 150usize, 48usize, 1usize),
            (200, 150, 48, 2),
            (200, 150, 96, 2),
            (49, 49, 48, 1),
            (200, 150, 7, 3),
            (10, 10, 1, 1),
        ] {
            let mut buf = vec![0f32; w * h * 3];
            grid_pass(&mut buf, w, h, spacing, line);
            let once = 0.38f32;
            for (i, v) in buf.iter().enumerate() {
                let (px, ch) = (i / 3, i % 3);
                let (x, y) = (px % w, px / w);
                let expect = if y % spacing < line || x % spacing < line {
                    grid_blue_ch(ch) * once
                } else {
                    0.0
                };
                assert!(
                    (v - expect).abs() < 1e-5,
                    "{w}x{h} spacing {spacing} line {line}: ({x},{y}) ch {ch} \
                     is {v}, expected {expect} (blended twice?)"
                );
            }
        }
    }

    fn grid_blue_ch(ch: usize) -> f32 {
        match ch {
            0 => GRID_BLUE.0,
            1 => GRID_BLUE.1,
            _ => GRID_BLUE.2,
        }
    }

    #[test]
    fn a_zero_spacing_or_line_does_not_divide_by_zero_or_hang() {
        // `step_by(0)` panics, and under panic=abort that is a dead wallpaper.
        let (w, h) = (8usize, 6usize);
        let mut buf = vec![0f32; w * h * 3];
        grid_pass(&mut buf, w, h, 0, 0);
        // Treated as 1/1: every pixel painted once.
        assert!(buf.iter().all(|v| *v > 0.0));
    }

    #[test]
    fn every_character_the_scene_draws_has_a_glyph() {
        // An unknown character falls back to a blank 5x7 cell, so a typo in
        // the ring text or the wordmark deletes a letter from the wallpaper
        // without any complaint. The space is the one deliberate blank.
        for ch in TEXT_RING.chars().chain("ECLIPSE OS".chars()) {
            let bits = glyph5x7(ch);
            if ch == ' ' {
                assert_eq!(bits, [0; 7], "the space should stay blank");
                continue;
            }
            assert_ne!(bits, [0; 7], "'{ch}' draws a blank cell");
        }
    }

    #[test]
    fn a_glyph_never_sets_a_bit_outside_its_five_columns() {
        // The renderer walks columns 0..5 with the mask 0b10000 >> col, so a
        // row with bit 5 or above set silently loses that pixel.
        for ch in TEXT_RING.chars().chain("ECLIPSE OS".chars()) {
            for (row, bits) in glyph5x7(ch).iter().enumerate() {
                assert_eq!(
                    bits & !0b11111,
                    0,
                    "'{ch}' row {row} = {bits:#07b} sets a bit past column 5"
                );
            }
        }
    }

    #[test]
    fn a_span_never_leaves_the_buffer_and_never_runs_backwards() {
        for limit in [0usize, 1, 7, 1920] {
            for c in [-1000.0f32, -1.0, 0.0, 0.5, 100.0, 1e9] {
                for r in [0.0f32, 0.4, 3.0, 5000.0] {
                    let (lo, hi) = span(c, r, limit);
                    assert!(hi <= limit, "span({c},{r},{limit}) = {lo}..{hi}");
                    // An empty range is fine; a reversed one would panic in a
                    // `for` loop over `lo..hi`... it does not, but a reversed
                    // range silently draws nothing, which is worse to debug.
                    if lo > hi {
                        assert!(
                            lo >= limit,
                            "span({c},{r},{limit}) = {lo}..{hi} runs backwards \
                             inside the buffer"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_distance_helpers_behave_at_their_edges() {
        assert_eq!(dist(3.0, 4.0, 0.0, 0.0), 5.0);
        assert_eq!(dist(1.0, 1.0, 1.0, 1.0), 0.0);
        // A capsule with both ends at the same point degenerates to a circle
        // instead of dividing by zero.
        assert_eq!(capsule_dist(3.0, 4.0, 0.0, 0.0, 0.0, 0.0), 5.0);
        // On the segment, off the ends, and past the ends (clamped to them).
        assert_eq!(capsule_dist(5.0, 0.0, 0.0, 0.0, 10.0, 0.0), 0.0);
        assert_eq!(capsule_dist(5.0, 2.0, 0.0, 0.0, 10.0, 0.0), 2.0);
        assert_eq!(capsule_dist(-3.0, 0.0, 0.0, 0.0, 10.0, 0.0), 3.0);
        assert_eq!(capsule_dist(14.0, 0.0, 0.0, 0.0, 10.0, 0.0), 4.0);
    }

    #[test]
    fn a_colour_mix_stays_between_its_two_ends() {
        assert_eq!(lerp3(COSMIC_DEEP, COSMIC_MID, 0.0), COSMIC_DEEP);
        assert_eq!(lerp3(COSMIC_DEEP, COSMIC_MID, 1.0), COSMIC_MID);
        // Out-of-range t is clamped, not extrapolated into a negative colour.
        assert_eq!(lerp3(COSMIC_DEEP, COSMIC_MID, -5.0), COSMIC_DEEP);
        assert_eq!(lerp3(COSMIC_DEEP, COSMIC_MID, 5.0), COSMIC_MID);
        let mid = lerp3((0.0, 0.0, 0.0), (1.0, 1.0, 1.0), 0.5);
        assert_eq!(mid, (0.5, 0.5, 0.5));
    }

    #[test]
    fn the_star_hash_spreads_and_is_stable() {
        // It places the stars and the dither noise: identical output for the
        // same input is what makes two builds give the same wallpaper.
        assert_eq!(hash2(7, 9), hash2(7, 9));
        assert_ne!(hash2(7, 9), hash2(9, 7), "the hash is symmetric in a,b");
        // No input collapses it to zero, and the low bits move (the star
        // positions are `hash2(i, 1) % lw`, so a hash with dead low bits
        // would stack every star in the same column).
        let mut lows = std::collections::HashSet::new();
        for i in 0..256u32 {
            let hv = hash2(i, 1);
            assert_ne!(hv, 0, "hash2({i},1) == 0");
            lows.insert(hv % 64);
        }
        assert!(lows.len() > 50, "only {} distinct low values", lows.len());
    }

    #[test]
    fn a_one_pixel_surface_renders_without_panicking() {
        // The smallest thing a compositor can configure. Every span, region
        // and chunk math has to survive it: this is the size a broken
        // wl_output mode produces, and an abort here is a black desktop.
        for (w, h) in [(1usize, 1usize), (1, 100), (100, 1), (2, 2), (3, 7)] {
            let base = render_base(w, h, None, 1).unwrap_or_else(|| panic!("{w}x{h}"));
            assert_eq!(base.len(), w * h * 4);
            assert!(every_alpha_is_opaque(&base));
            let lay = layout(w, h, None, 1);
            let mut frame = base.clone();
            render_frame(&mut frame, w, &base, &lay, 7777);
            assert_eq!(frame.len(), base.len());
            assert!(every_alpha_is_opaque(&frame));
        }
    }

    #[test]
    fn the_scene_survives_a_long_uptime_clock() {
        // The phase is accumulated in f64 and folded per element; an f32
        // phase loses sub-frame resolution after days and the animation turns
        // steppy. Two clocks a frame apart must still differ after 40 days.
        let (w, h) = (200usize, 150usize);
        let base = render_base(w, h, None, 1).expect("base");
        let lay = layout(w, h, None, 1);
        let far = 40 * 24 * 3600 * 1000u64;
        let mut a = base.clone();
        let mut b = base.clone();
        render_frame(&mut a, w, &base, &lay, far);
        render_frame(&mut b, w, &base, &lay, far + 42);
        assert_ne!(a, b, "the animation has stopped moving after 40 days");
    }
}
