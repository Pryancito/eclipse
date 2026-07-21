//! Software drawing for lunarbar: an XRGB8888 pixel buffer, a 5x7 bitmap font
//! (integer-scaled), simple filled rects, and a small crescent-moon launcher
//! glyph. No external font/graphics libraries — the whole point is zero heavy
//! dependencies (see the crate doc in main.rs).

pub type Rgb = (u8, u8, u8);

/// A mutable XRGB8888 framebuffer with clipped alpha blending.
pub struct Canvas<'a> {
    pub data: &'a mut [u8],
    pub w: usize,
    pub h: usize,
}

impl Canvas<'_> {
    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 4;
        let a = a.clamp(0.0, 1.0);
        let mix = |old: u8, new: u8| -> u8 {
            (old as f32 * (1.0 - a) + new as f32 * a).round() as u8
        };
        // XRGB8888 little-endian: byte order B,G,R,X.
        self.data[i] = mix(self.data[i], c.2);
        self.data[i + 1] = mix(self.data[i + 1], c.1);
        self.data[i + 2] = mix(self.data[i + 2], c.0);
        self.data[i + 3] = 0xff;
    }

    /// Fill the whole canvas with a solid colour.
    pub fn clear(&mut self, c: Rgb) {
        for px in 0..self.w * self.h {
            let i = px * 4;
            self.data[i] = c.2;
            self.data[i + 1] = c.1;
            self.data[i + 2] = c.0;
            self.data[i + 3] = 0xff;
        }
    }

    /// Filled rectangle (opaque).
    pub fn fill_rect(&mut self, x0: i32, y0: i32, rw: i32, rh: i32, c: Rgb) {
        for dy in 0..rh {
            for dx in 0..rw {
                self.blend(x0 + dx, y0 + dy, c, 1.0);
            }
        }
    }

    /// Horizontal 1px line (used for the bar's top accent rule).
    pub fn hline(&mut self, x0: i32, y: i32, len: i32, c: Rgb, a: f32) {
        for dx in 0..len {
            self.blend(x0 + dx, y, c, a);
        }
    }

    /// Draw one glyph at (x,y) top-left, scaled by `s`. Returns the advance
    /// in pixels (glyph width * s + 1*s spacing).
    pub fn glyph(&mut self, ch: char, x: i32, y: i32, s: i32, c: Rgb) -> i32 {
        let rows = font5x7(ch);
        for (ry, bits) in rows.iter().enumerate() {
            for rx in 0..5 {
                if bits & (1 << (4 - rx)) != 0 {
                    // filled 5x7 cell, scaled to an s*s block
                    for sy in 0..s {
                        for sx in 0..s {
                            self.blend(
                                x + rx as i32 * s + sx,
                                y + ry as i32 * s + sy,
                                c,
                                1.0,
                            );
                        }
                    }
                }
            }
        }
        6 * s
    }

    /// Draw a left-aligned string, returning the total advance in pixels.
    pub fn text(&mut self, s_str: &str, x: i32, y: i32, scale: i32, c: Rgb) -> i32 {
        let mut cx = x;
        for ch in s_str.chars() {
            if ch == ' ' {
                cx += 3 * scale;
            } else {
                cx += self.glyph(ch, cx, y, scale, c);
            }
        }
        cx - x
    }

    /// Pixel width a string will occupy at the given scale.
    pub fn text_width(s_str: &str, scale: i32) -> i32 {
        let mut w = 0;
        for ch in s_str.chars() {
            w += if ch == ' ' { 3 * scale } else { 6 * scale };
        }
        w
    }

    /// A mini horizontal gauge: a solid dark track filled `frac` (0..1) of its
    /// width. The fill colour lerps cool→warm (foot-palette green→amber→red)
    /// as it rises, so a busy metric reads at a glance — a visual waybar never
    /// gives you for free.
    pub fn gauge(&mut self, x: i32, y: i32, gw: i32, gh: i32, frac: f32, track: Rgb) {
        let frac = frac.clamp(0.0, 1.0);
        for dy in 0..gh {
            for dx in 0..gw {
                self.blend(x + dx, y + dy, track, 1.0);
            }
        }
        // Fill ramp uses the foot terminal palette so the bars and the
        // terminal share an accent language.
        let fill = lerp3(
            (0x8f, 0xd1, 0x8a), // green (regular2)
            (0xe0, 0xc0, 0x7a), // amber (regular3)
            (0xe0, 0x7a, 0x7a), // red   (regular1)
            frac,
        );
        let filled = ((gw as f32) * frac).round() as i32;
        for dy in 0..gh {
            for dx in 0..filled {
                self.blend(x + dx, y + dy, fill, 1.0);
            }
        }
    }

    /// A small solid triangle in an `s`x`s` box at (x,y). `up=true` points up
    /// (tip at top → upload), else down (tip at bottom → download).
    pub fn triangle(&mut self, x: i32, y: i32, s: i32, up: bool, c: Rgb) {
        let cx = x + s / 2;
        let denom = (s - 1).max(1) as f32;
        for r in 0..s {
            // frac: 0 at the triangle's base row, 1 at its tip row.
            let frac = if up {
                (s - 1 - r) as f32 / denom
            } else {
                r as f32 / denom
            };
            let half = ((1.0 - frac) * (s as f32 / 2.0)).round() as i32;
            for dx in -half..=half {
                self.blend(cx + dx, y + r, c, 1.0);
            }
        }
    }

    /// Pseudo-bold text: the string drawn twice with a 1px horizontal offset,
    /// thickening every stroke. Same advance as `text`. Matches waybar's
    /// `font-weight: bold` clock.
    pub fn text_bold(&mut self, s_str: &str, x: i32, y: i32, scale: i32, c: Rgb) -> i32 {
        let w = self.text(s_str, x, y, scale, c);
        self.text(s_str, x + 1, y, scale, c);
        w
    }

    /// A filled rounded rectangle (opaque), corner radius `r`. Used for the
    /// clock pill and the active taskbar button, matching waybar's
    /// `border-radius: 6px`.
    pub fn round_rect(&mut self, x: i32, y: i32, rw: i32, rh: i32, r: i32, c: Rgb) {
        let r = r.max(0).min(rw / 2).min(rh / 2);
        for dy in 0..rh {
            for dx in 0..rw {
                let mut draw = true;
                // Clip the four corners to a quarter-circle of radius r.
                let corner = |ex: i32, ey: i32| ex * ex + ey * ey > r * r;
                if dx < r && dy < r {
                    draw = !corner(r - 1 - dx, r - 1 - dy);
                } else if dx >= rw - r && dy < r {
                    draw = !corner(dx - (rw - r), r - 1 - dy);
                } else if dx < r && dy >= rh - r {
                    draw = !corner(r - 1 - dx, dy - (rh - r));
                } else if dx >= rw - r && dy >= rh - r {
                    draw = !corner(dx - (rw - r), dy - (rh - r));
                }
                if draw {
                    self.blend(x + dx, y + dy, c, 1.0);
                }
            }
        }
    }

    /// A faint vertical separator line, centred in a bar of height `h`, ~half
    /// the bar tall. Cleaner than a dot for grouping modules.
    pub fn vrule(&mut self, x: i32, h: i32, c: Rgb) {
        let y0 = h / 4;
        let y1 = h - h / 4;
        for y in y0..y1 {
            self.blend(x, y, c, 0.30);
        }
    }

    /// A half-filled disc launcher (Unicode ◑): a full ring with the right
    /// half filled. Matches the waybar `custom/launcher` glyph, centred in a
    /// `d`x`d` box at (x,y).
    pub fn disc_half(&mut self, x: i32, y: i32, d: i32, c: Rgb) {
        let r = d as f32 / 2.0;
        let cx = x as f32 + r;
        let cy = y as f32 + r;
        for dy in 0..d {
            for dx in 0..d {
                let px = x as f32 + dx as f32 + 0.5;
                let py = y as f32 + dy as f32 + 0.5;
                let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let cov = (r - dist + 0.5).clamp(0.0, 1.0); // AA disc coverage
                if cov <= 0.0 {
                    continue;
                }
                // Right half: solid. Left half: only the outer ring.
                let a = if px >= cx {
                    cov
                } else {
                    // ring where we're within ~1.4px of the edge
                    let ring = (dist - (r - 1.4)).clamp(0.0, 1.0);
                    cov * ring
                };
                if a > 0.0 {
                    self.blend(x + dx, y + dy, c, a);
                }
            }
        }
    }

    /// A small crescent-moon launcher glyph: a filled disc masked by an
    /// offset disc, centred in a `d`x`d` box at (x,y). Mirrors lunarbg's
    /// eclipse crescent so the bar's launcher matches the wallpaper.
    pub fn crescent(&mut self, x: i32, y: i32, d: i32, c: Rgb) {
        let r = d as f32 / 2.0;
        let cx = x as f32 + r;
        let cy = y as f32 + r;
        // Mask disc: same radius, shifted right+up so a crescent remains.
        let mx = cx + r * 0.42;
        let my = cy - r * 0.10;
        let mr = r * 0.92;
        for dy in 0..d {
            for dx in 0..d {
                let px = x as f32 + dx as f32 + 0.5;
                let py = y as f32 + dy as f32 + 0.5;
                let dsun = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let inside = r - dsun + 0.5; // AA coverage of the sun disc
                if inside <= 0.0 {
                    continue;
                }
                let dmoon = ((px - mx).powi(2) + (py - my).powi(2)).sqrt();
                let masked = (mr - dmoon + 0.5).clamp(0.0, 1.0); // inside mask -> hidden
                let a = inside.clamp(0.0, 1.0) * (1.0 - masked);
                if a > 0.0 {
                    self.blend(x + dx, y + dy, c, a);
                }
            }
        }
    }
}

/// Three-stop colour ramp: `a`→`b` over the first half of `t`, `b`→`c` over
/// the second. Used to tint gauges by load.
fn lerp3(a: Rgb, b: Rgb, c: Rgb, t: f32) -> Rgb {
    let lerp = |x: u8, y: u8, f: f32| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    if t <= 0.5 {
        let f = t / 0.5;
        (lerp(a.0, b.0, f), lerp(a.1, b.1, f), lerp(a.2, b.2, f))
    } else {
        let f = (t - 0.5) / 0.5;
        (lerp(b.0, c.0, f), lerp(b.1, c.1, f), lerp(b.2, c.2, f))
    }
}

/// 5-wide x 7-tall bitmap font covering the glyphs lunarbar renders: digits,
/// A-Z, a-z (real lowercase shapes, HD44780-style, so the bar can match
/// waybar's lowercase labels), and module punctuation. Bit 4 (0b10000) is the
/// leftmost column. Unknown chars render blank.
fn font5x7(c: char) -> [u8; 7] {
    match c {
        'a' => [0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111],
        'b' => [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b11110],
        'c' => [0b00000, 0b00000, 0b01110, 0b10000, 0b10000, 0b10001, 0b01110],
        'd' => [0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111],
        'e' => [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110],
        'f' => [0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000],
        'g' => [0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b01110],
        'h' => [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001],
        'i' => [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110],
        'j' => [0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100],
        'k' => [0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010],
        'l' => [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'm' => [0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10101, 0b10101],
        'n' => [0b00000, 0b00000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001],
        'o' => [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110],
        'p' => [0b00000, 0b00000, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000],
        'q' => [0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b00001],
        'r' => [0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000],
        's' => [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
        't' => [0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110],
        'u' => [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101],
        'v' => [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'w' => [0b00000, 0b00000, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        'x' => [0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001],
        'y' => [0b00000, 0b00000, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110],
        'z' => [0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111],
        _ => font5x7_upper(c),
    }
}

/// Uppercase / digit / punctuation half of the font.
fn font5x7_upper(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01111, 0b10000, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        ':' => [0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100],
        '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        _ => [0; 7],
    }
}
