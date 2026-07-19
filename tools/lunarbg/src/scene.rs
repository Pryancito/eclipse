//! The Eclipse OS night scene, rendered procedurally at the output's native
//! resolution — no image files, no decoders.
//!
//! Composition (back to front):
//! - vertical night gradient (deep navy -> violet) with a warm horizon glow;
//! - deterministic starfield (kept clear of the logo);
//! - subtle blueprint grid, 48 px spacing, rgb(18,28,55) — carried over from
//!   the original Eclipse OS smithay compositor's cosmic background;
//! - crescent moon;
//! - two mountain silhouette layers;
//! - the Eclipse disc (halo, rim, three white stripes), radius scaled to the
//!   output using the original compositor's formula
//!   `clamp(min(w,h)/2 - 120, 120, 280)`;
//! - "Eclipse OS" wordmark in a 5x7 pixel font.
//!
//! Output is little-endian XRGB8888 (B, G, R, X per pixel), ready for wl_shm.

/// Render the scene and return one XRGB8888 row-major buffer.
pub fn render_xrgb(w: usize, h: usize) -> Vec<u8> {
    let mut buf = vec![0f32; w * h * 3];

    let fw = w as f32;
    let fh = h as f32;
    let cx = fw * 0.5;
    let cy = fh * 0.43;
    // Logo radius: the original smithay compositor's sizing rule.
    let radius = ((fw.min(fh) / 2.0) - 120.0).clamp(120.0, 280.0);

    // --- sky gradient + horizon glow ---
    for y in 0..h {
        let t = y as f32 / fh;
        let (r, g, b) = sky_color(t);
        let glow = (-((y as f32 - fh * 0.78) / (fh * 0.10)).powi(2)).exp() * 0.30;
        for x in 0..w {
            let i = (y * w + x) * 3;
            buf[i] = r + glow * 0.75;
            buf[i + 1] = g + glow * 0.35;
            buf[i + 2] = b + glow * 0.60;
        }
    }

    draw_stars(&mut buf, w, h, cx, cy, radius);
    draw_grid(&mut buf, w, h);
    draw_crescent_moon(&mut buf, w, h, fw * 0.83, fh * 0.16, fh * 0.05);
    draw_mountains(&mut buf, w, h);
    draw_logo(&mut buf, w, h, cx, cy, radius);
    draw_wordmark(&mut buf, w, h, cx, cy + radius + fh * 0.075);

    // Quantise with a hair of deterministic noise so the smooth gradients do
    // not band at 8 bits, packing straight into XRGB8888.
    let mut out = vec![0u8; w * h * 4];
    for px in 0..w * h {
        let i = px * 3;
        let o = px * 4;
        let n = (hash2(i as u32, 0x9e37_79b9) as f32 / u32::MAX as f32 - 0.5) * 1.5;
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + n).round().clamp(0.0, 255.0) as u8;
        out[o] = q(buf[i + 2]); // B
        out[o + 1] = q(buf[i + 1]); // G
        out[o + 2] = q(buf[i]); // R
        out[o + 3] = 0xff; // X
    }
    out
}

fn sky_color(t: f32) -> (f32, f32, f32) {
    let a = (0.051, 0.043, 0.118); // #0d0b1e
    let b = (0.102, 0.078, 0.251); // #1a1440
    let c = (0.239, 0.165, 0.388); // #3d2a63
    if t < 0.55 {
        lerp3(a, b, t / 0.55)
    } else {
        lerp3(b, c, (t - 0.55) / 0.45)
    }
}

fn lerp3(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

/// Blueprint grid from the original Eclipse compositor: 48 px spacing,
/// rgb(18,28,55), blended gently so it reads as texture, not lines.
fn draw_grid(buf: &mut [f32], w: usize, h: usize) {
    const SPACING: usize = 48;
    const COLOR: (f32, f32, f32) = (18.0 / 255.0, 28.0 / 255.0, 55.0 / 255.0);
    const ALPHA: f32 = 0.38;
    for y in (0..h).step_by(SPACING) {
        for x in 0..w {
            blend_px(buf, w, x, y, COLOR, ALPHA);
        }
    }
    for x in (0..w).step_by(SPACING) {
        for y in 0..h {
            if y % SPACING != 0 {
                blend_px(buf, w, x, y, COLOR, ALPHA);
            }
        }
    }
}

fn draw_stars(buf: &mut [f32], w: usize, h: usize, cx: f32, cy: f32, radius: f32) {
    // Star count scales with area so 4K doesn't look sparse.
    let count = ((w * h) as f32 / 6000.0) as u32;
    for i in 0..count {
        let x = (hash2(i, 1) % w as u32) as f32;
        let y = (hash2(i, 2) % (h as u32 * 6 / 10)) as f32;
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        if d < radius + 70.0 {
            continue;
        }
        let bright = 0.25 + (hash2(i, 3) % 1000) as f32 / 1000.0 * 0.75;
        add_px(buf, w, h, x as i32, y as i32, (bright, bright, bright * 0.95));
        if bright > 0.85 {
            let half = bright * 0.35;
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                add_px(buf, w, h, x as i32 + dx, y as i32 + dy, (half, half, half));
            }
        }
    }
}

fn draw_crescent_moon(buf: &mut [f32], w: usize, h: usize, mx: f32, my: f32, r: f32) {
    let bite_x = mx + r * 0.45;
    let bite_y = my - r * 0.18;
    let (x0, x1) = span(mx, r + 2.0, w);
    let (y0, y1) = span(my, r + 2.0, h);
    for y in y0..y1 {
        for x in x0..x1 {
            let d = dist(x as f32, y as f32, mx, my);
            let db = dist(x as f32, y as f32, bite_x, bite_y);
            let cover = coverage(r, d) * (1.0 - coverage(r * 0.92, db));
            if cover > 0.0 {
                blend_px(buf, w, x, y, (0.85, 0.83, 0.94), cover * 0.9);
            }
        }
    }
}

fn draw_mountains(buf: &mut [f32], w: usize, h: usize) {
    let fh = h as f32;
    let layers: [(f32, [f32; 6], (f32, f32, f32)); 2] = [
        (
            fh * 0.780,
            [0.0040, 1.7, 0.011, 0.4, 0.027, 2.2],
            (0.133, 0.102, 0.220), // #221a38
        ),
        (
            fh * 0.845,
            [0.0060, 4.1, 0.015, 1.1, 0.033, 0.0],
            (0.090, 0.067, 0.161), // #171129
        ),
    ];
    for (base, p, color) in layers {
        for x in 0..w {
            let fx = x as f32;
            let ridge = base
                + (fx * p[0] + p[1]).sin() * fh * 0.055
                + (fx * p[2] + p[3]).sin() * fh * 0.028
                + (fx * p[4] + p[5]).sin() * fh * 0.012;
            let start = ridge.max(0.0) as usize;
            for y in start..h {
                let cover = if y == start {
                    1.0 - (ridge - ridge.floor())
                } else {
                    1.0
                };
                blend_px(buf, w, x, y, color, cover);
            }
        }
    }
}

fn draw_logo(buf: &mut [f32], w: usize, h: usize, cx: f32, cy: f32, r: f32) {
    let halo = r * 0.55;
    let (x0, x1) = span(cx, r + halo, w);
    let (y0, y1) = span(cy, r + halo, h);
    for y in y0..y1 {
        for x in x0..x1 {
            let d = dist(x as f32, y as f32, cx, cy);
            if d > r && d < r + halo {
                let t = 1.0 - (d - r) / halo;
                let a = t * t * 0.45;
                add_px(
                    buf,
                    w,
                    h,
                    x as i32,
                    y as i32,
                    (0.55 * a, 0.47 * a, 0.86 * a),
                );
            }
        }
    }
    for y in y0..y1 {
        for x in x0..x1 {
            let d = dist(x as f32, y as f32, cx, cy);
            let cover = coverage(r, d);
            if cover > 0.0 {
                let shade = 0.5 + (y as f32 - cy) / (2.0 * r);
                let base = lerp3((0.110, 0.086, 0.208), (0.055, 0.043, 0.118), shade);
                blend_px(buf, w, x, y, base, cover);
            }
            let rim = (1.0 - ((d - r).abs() / 3.0)).clamp(0.0, 1.0);
            if rim > 0.0 {
                blend_px(buf, w, x, y, (0.66, 0.60, 0.91), rim * 0.9);
            }
        }
    }
    for (off_frac, bar_frac) in [(-0.36f32, 0.085f32), (-0.02, 0.085), (0.32, 0.085)] {
        let yb = cy + r * off_frac;
        let bar_r = r * bar_frac;
        let dy = yb - cy;
        let chord = (r * r - dy * dy).max(0.0).sqrt();
        let half = chord - r * 0.16;
        if half <= 0.0 {
            continue;
        }
        let (bx0, bx1) = span(cx, half + bar_r + 2.0, w);
        let (by0, by1) = span(yb, bar_r + 2.0, h);
        for y in by0..by1 {
            for x in bx0..bx1 {
                let d = capsule_dist(x as f32, y as f32, cx - half, yb, cx + half, yb);
                let cover = coverage(bar_r, d);
                if cover > 0.0 {
                    blend_px(buf, w, x, y, (0.95, 0.94, 0.98), cover);
                }
            }
        }
    }
}

fn draw_wordmark(buf: &mut [f32], w: usize, h: usize, cx: f32, cy: f32) {
    let text = "Eclipse OS";
    let scale = (h as f32 * 0.008).round().max(3.0) as usize;
    let advance = (6 * scale) as i32;
    let total = text.len() as i32 * advance - scale as i32;
    let left = cx as i32 - total / 2;
    let top = cy as i32;
    for (ci, ch) in text.chars().enumerate() {
        let glyph = glyph5x7(ch);
        let gx = left + ci as i32 * advance;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = gx + (col * scale + sx) as i32;
                        let y = top + (row * scale + sy) as i32;
                        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                            blend_px(buf, w, x as usize, y as usize, (0.85, 0.82, 0.94), 0.95);
                        }
                    }
                }
            }
        }
    }
}

fn glyph5x7(c: char) -> [u8; 7] {
    match c {
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'c' => [0b00000, 0b00000, 0b01111, 0b10000, 0b10000, 0b10000, 0b01111],
        'l' => [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'i' => [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110],
        'p' => [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b11110, 0b10000],
        's' => [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
        'e' => [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01111],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        _ => [0; 7],
    }
}

fn dist(x: f32, y: f32, cx: f32, cy: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()
}

fn coverage(r: f32, d: f32) -> f32 {
    (r - d + 0.5).clamp(0.0, 1.0)
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

fn add_px(buf: &mut [f32], w: usize, h: usize, x: i32, y: i32, c: (f32, f32, f32)) {
    if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
        return;
    }
    let i = (y as usize * w + x as usize) * 3;
    buf[i] += c.0;
    buf[i + 1] += c.1;
    buf[i + 2] += c.2;
}

fn blend_px(buf: &mut [f32], w: usize, x: usize, y: usize, c: (f32, f32, f32), a: f32) {
    let i = (y * w + x) * 3;
    buf[i] = buf[i] * (1.0 - a) + c.0 * a;
    buf[i + 1] = buf[i + 1] * (1.0 - a) + c.1 * a;
    buf[i + 2] = buf[i + 2] * (1.0 - a) + c.2 * a;
}

fn hash2(a: u32, b: u32) -> u32 {
    let mut x = a.wrapping_mul(0x85eb_ca6b) ^ b.wrapping_mul(0xc2b2_ae35);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}
