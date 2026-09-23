//! Boot progress bar drawing (UEFI GOP framebuffer).
//!
//! Minimal, no_std-friendly: draws a centered progress bar directly into GOP fb.
//!
//! Everything below the [`bar`] entry point works on plain `(fb_addr, stride,
//! width, height)` so it can be pointed at ordinary memory and checked pixel
//! by pixel without a GOP.

use uefi::proto::console::gop::ModeInfo;

use crate::fb::Screen;

// 8x8 font for ASCII 0x20..0x7F (same data format as kernel-hal).
const FONT8X8: [[u8; 8]; 96] = include!("font8x8_basic.in");

const WHITE: u32 = 0x00FF_FFFF;
const BLACK: u32 = 0x0000_0000;

/// Bar geometry, in pixels.
pub const BAR_W: usize = 400;
pub const BAR_H: usize = 20;
/// Glyph cell of the percentage text (the 8x8 font is drawn at 2x vertically).
pub const CHAR_W: usize = 8;
pub const CHAR_H: usize = 16;

fn draw_char_8x16(s: &Screen, x: usize, y: usize, c: u8, fg: u32, bg: u32) {
    let c = if (0x20..0x80).contains(&c) { c } else { b'?' };
    let glyph = &FONT8X8[(c - 0x20) as usize];
    for (gy, bits) in glyph.iter().copied().enumerate() {
        let py0 = y + gy * 2;
        if py0 >= s.height() {
            break;
        }
        // gx = 0 is the left of the cell; bit 7 of the VGA row is that pixel.
        for gx in 0..8 {
            let px = x + gx;
            if px >= s.width() {
                break;
            }
            let on = (bits & (1 << (7 - gx))) != 0;
            let color = if on { fg } else { bg };
            s.put(px, py0, color);
            s.put(px, py0 + 1, color);
        }
    }
}

fn draw_text_8x16(s: &Screen, x: usize, y: usize, text: &[u8], fg: u32, bg: u32) {
    let mut cx = x;
    for &ch in text {
        draw_char_8x16(s, cx, y, ch, fg, bg);
        cx = cx.saturating_add(CHAR_W);
        if cx >= s.width() {
            break;
        }
    }
}

fn fill_rect(s: &Screen, x: usize, y: usize, w: usize, h: usize, pixel: u32) {
    let x1 = x.saturating_add(w).min(s.width());
    let y1 = y.saturating_add(h).min(s.height());
    for yy in y..y1 {
        for xx in x..x1 {
            s.put(xx, yy, pixel);
        }
    }
}

fn stroke_rect(s: &Screen, x: usize, y: usize, w: usize, h: usize, t: usize, pixel: u32) {
    if w == 0 || h == 0 || t == 0 {
        return;
    }
    fill_rect(s, x, y, w, t, pixel);
    if h > t {
        fill_rect(s, x, y + h - t, w, t, pixel);
    }
    if h > t * 2 {
        fill_rect(s, x, y + t, t, h - t * 2, pixel);
        if w > t {
            fill_rect(s, x + w - t, y + t, t, h - t * 2, pixel);
        }
    }
}

/// Top-left corner of the progress bar on a `sw × sh` screen.
///
/// The bar sits under the centered logo: the logo's bottom edge is
/// `sh/2 + LOGO_HEIGHT/2`, plus 35px of padding.
pub fn bar_origin(sw: usize, sh: usize) -> (usize, usize) {
    let x = sw.saturating_sub(BAR_W) / 2;
    let y = (sh / 2)
        .saturating_add(crate::logo::LOGO_HEIGHT / 2)
        .saturating_add(35);
    (x, y)
}

/// The percentage label, right-aligned in a fixed 4-character field so the
/// text never changes width (and so never leaves a stale digit behind).
pub fn format_pct(progress: u32) -> [u8; 4] {
    let p = progress.min(100);
    let mut buf = [b' '; 4];
    buf[3] = b'%';
    if p == 100 {
        buf[0] = b'1';
        buf[1] = b'0';
        buf[2] = b'0';
    } else if p >= 10 {
        buf[1] = b'0' + (p / 10) as u8;
        buf[2] = b'0' + (p % 10) as u8;
    } else {
        buf[2] = b'0' + p as u8;
    }
    buf
}

/// Draw a centered progress bar (0..=100).
///
/// `pixel` encoding matches existing `logo.rs`: 0x00RRGGBB.
pub fn bar(mode: ModeInfo, fb_addr: u64, progress: u32) {
    // SAFETY: the kernel only calls this with the base of `mode`'s own GOP
    // framebuffer, which is `stride * height` pixels.
    if let Some(s) = unsafe { Screen::from_mode(mode, fb_addr) } {
        draw_bar(&s, progress);
    }
}

/// Draw the same bar using raw framebuffer parameters.
///
/// The fault handler has no `ModeInfo` left to read, so it comes in here.
/// Both entry points draw through [`draw_bar`]: two copies of the geometry is
/// how the bar and the fault-time bar drifted apart before.
pub fn bar_raw(fb_addr: u64, stride: usize, sw: usize, sh: usize, progress: u32) {
    // SAFETY: as above; `idt::init` captured these from the live GOP mode.
    if let Some(s) = unsafe { Screen::new(fb_addr, stride, sw, sh) } {
        draw_bar(&s, progress);
    }
}

fn draw_bar(s: &Screen, progress: u32) {
    let progress = progress.min(100);
    let (x, y) = bar_origin(s.width(), s.height());

    // Border (black) and inner content (black fill / white remainder).
    stroke_rect(
        s,
        x.saturating_sub(2),
        y.saturating_sub(2),
        BAR_W + 4,
        BAR_H + 4,
        1,
        BLACK,
    );
    let fill_w = (BAR_W * progress as usize) / 100;
    if fill_w > 0 {
        fill_rect(s, x, y, fill_w, BAR_H, BLACK);
    }
    if fill_w < BAR_W {
        fill_rect(s, x + fill_w, y, BAR_W - fill_w, BAR_H, WHITE);
    }

    // Fixed-width percentage text (4 chars: "100%" or "  7%"/" 42%").
    let buf = format_pct(progress);
    let text_w = buf.len() * CHAR_W;
    let tx = x + (BAR_W.saturating_sub(text_w)) / 2;
    let ty = y + BAR_H + 15;
    draw_text_8x16(s, tx, ty, &buf, BLACK, WHITE);
}

/// Draw a small fault marker block at top-left, encoding `tag` and `code` as pixels.
pub fn fault_block_raw(fb_addr: u64, stride: usize, sw: usize, sh: usize, tag: u32, code: u32) {
    const BASE: usize = 8;
    // The block is 48x16 at (8,8) plus its border, so anything smaller than
    // that has nothing to show. `w - 16` on a 4-pixel-wide mode wrapped to
    // `usize::MAX` instead.
    if sw <= BASE * 2 || sh <= BASE * 2 + 4 {
        return;
    }
    // SAFETY: as in `bar_raw`.
    let Some(s) = (unsafe { Screen::new(fb_addr, stride, sw, sh) }) else {
        return;
    };
    let w = sw.min(64) - BASE * 2;
    let h = sh.min(32) - BASE * 2;
    let red: u32 = 0x0000_00FF; // best-effort visible

    // Background.
    fill_rect(&s, BASE, BASE, w, h, BLACK);
    // Border.
    stroke_rect(&s, BASE, BASE, w, h, 1, WHITE);
    // A red stripe.
    fill_rect(&s, BASE + 2, BASE + 2, 8, h - 4, red);

    // Encode tag/code in a few pixels: bits 0..15 on one row, 16..31 on the
    // next. One row of 16 showed only the low half, and the tag -- the whole
    // point of the marker, the only thing that tells a #PF from a #GP on a
    // machine with no console left -- lives in the *top* byte
    // (0xAF00_0000 vs 0xA600_0000), so every fault painted the same picture.
    let t = tag ^ (code.rotate_left(7));
    for i in 0..32usize {
        let bit = (t >> i) & 1;
        let px = BASE + 14 + (i % 16);
        let py = BASE + 4 + (i / 16) * 2;
        s.put(px, py, if bit == 1 { WHITE } else { BLACK });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fb;
    use crate::testing::Canvas;

    const UNTOUCHED: u32 = Canvas::UNTOUCHED;

    #[test]
    fn percentage_text_is_four_columns_wide_at_every_value() {
        for p in 0..=100u32 {
            let s = format_pct(p);
            assert_eq!(s.len(), 4);
            assert_eq!(s[3], b'%');
            let text = core::str::from_utf8(&s).unwrap();
            assert_eq!(text.trim(), alloc::format!("{p}%"), "at {p}");
        }
    }

    #[test]
    fn percentage_text_clamps_above_100() {
        assert_eq!(&format_pct(101), b"100%");
        assert_eq!(&format_pct(u32::MAX), b"100%");
    }

    #[test]
    fn the_bar_fills_left_to_right_and_the_boundary_is_exact() {
        let (sw, sh) = (1024usize, 768usize);
        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        bar_raw(addr, sw, sw, sh, 25);
        let (x, y) = bar_origin(sw, sh);
        let mid = y + BAR_H / 2;
        let fill = BAR_W / 4;
        assert_eq!(c.get(x, mid), BLACK, "first filled column");
        assert_eq!(c.get(x + fill - 1, mid), BLACK, "last filled column");
        assert_eq!(c.get(x + fill, mid), WHITE, "first empty column");
        assert_eq!(c.get(x + BAR_W - 1, mid), WHITE, "last empty column");
        c.assert_no_overrun();
    }

    #[test]
    fn zero_and_one_hundred_are_all_empty_and_all_full() {
        let (sw, sh) = (1024usize, 768usize);
        let (x, y) = bar_origin(sw, sh);
        let mid = y + BAR_H / 2;

        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        bar_raw(addr, sw, sw, sh, 0);
        assert!((0..BAR_W).all(|i| c.get(x + i, mid) == WHITE));

        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        bar_raw(addr, sw, sw, sh, 100);
        assert!((0..BAR_W).all(|i| c.get(x + i, mid) == BLACK));
    }

    #[test]
    fn the_fill_never_goes_backwards() {
        let (sw, sh) = (1024usize, 768usize);
        let (x, y) = bar_origin(sw, sh);
        let mid = y + BAR_H / 2;
        let mut last = 0usize;
        for p in 0..=100u32 {
            let mut c = Canvas::new(sw, sh, sw);
            let addr = c.addr();
            bar_raw(addr, sw, sw, sh, p);
            let filled = (0..BAR_W).filter(|&i| c.get(x + i, mid) == BLACK).count();
            assert!(filled >= last, "{p}% drew less than the previous step");
            assert!(filled <= BAR_W);
            last = filled;
        }
        assert_eq!(last, BAR_W);
    }

    #[test]
    fn the_bar_draws_a_one_pixel_border_around_itself() {
        let (sw, sh) = (1024usize, 768usize);
        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        bar_raw(addr, sw, sw, sh, 0);
        let (x, y) = bar_origin(sw, sh);
        // Border at x-2 / y-2, one pixel thick, 404x24.
        assert_eq!(c.get(x - 2, y - 2), BLACK);
        assert_eq!(c.get(x + BAR_W + 1, y - 2), BLACK);
        assert_eq!(c.get(x - 2, y + BAR_H + 1), BLACK);
        assert_eq!(c.get(x + BAR_W + 1, y + BAR_H + 1), BLACK);
        // The pixel just inside the border is bar, not border.
        assert_eq!(c.get(x - 1, y - 1), UNTOUCHED);
        c.assert_no_overrun();
    }

    #[test]
    fn the_bar_only_touches_its_own_band_of_the_screen() {
        let (sw, sh) = (1024usize, 768usize);
        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        bar_raw(addr, sw, sw, sh, 42);
        let (x, y) = bar_origin(sw, sh);
        let top = y - 2;
        let bottom = y + BAR_H + 15 + CHAR_H;
        for yy in 0..sh {
            for xx in 0..sw {
                if (top..bottom).contains(&yy) && (x - 2..x + BAR_W + 2).contains(&xx) {
                    continue;
                }
                assert_eq!(c.get(xx, yy), UNTOUCHED, "stray store at {xx},{yy}");
            }
        }
        c.assert_no_overrun();
    }

    #[test]
    fn a_stride_wider_than_the_screen_is_respected() {
        // Real GOPs pad each scanline; writing at `y * sw` would shear.
        let (sw, sh, stride) = (800usize, 600usize, 1024usize);
        let mut c = Canvas::new(sw, sh, stride);
        let addr = c.addr();
        bar_raw(addr, stride, sw, sh, 50);
        let (x, y) = bar_origin(sw, sh);
        assert_eq!(c.get(x, y + BAR_H / 2), BLACK);
        // The padding columns are not part of the visible screen.
        for yy in 0..sh {
            for xx in sw..stride {
                assert_eq!(c.raw(yy * stride + xx), UNTOUCHED, "padding at {xx},{yy}");
            }
        }
        c.assert_no_overrun();
    }

    #[test]
    fn a_screen_that_cuts_through_the_last_text_row_clips_the_bottom_half() {
        // The 8x8 font is drawn at 2x vertically, so every glyph row is two
        // scanlines and the second one can be the first row past the screen.
        // At 420 rows the bar lands so that it is: `bar_origin().1 + BAR_H +
        // 15 + 15 == 420`. Without the clip in `Screen::put` that store lands
        // one whole scanline past the framebuffer.
        let (sw, sh) = (1024usize, 420usize);
        let (_, y) = bar_origin(sw, sh);
        assert_eq!(
            y + BAR_H + 15 + CHAR_H - 1,
            sh,
            "the test lost its edge case"
        );
        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        bar_raw(addr, sw, sw, sh, 50);
        c.assert_no_overrun();
    }

    #[test]
    fn a_screen_narrower_than_the_bar_clips_instead_of_overrunning() {
        for &(sw, sh) in &[(320usize, 200usize), (640, 480), (100, 100), (1, 1)] {
            let mut c = Canvas::new(sw, sh, sw);
            let addr = c.addr();
            bar_raw(addr, sw, sw, sh, 73);
            c.assert_no_overrun();
        }
    }

    #[test]
    fn a_null_or_zero_sized_framebuffer_is_not_written_to() {
        // `paint_fault_marker` guards this too, but the guard belongs here:
        // a BltOnly GOP hands out base 0.
        bar_raw(0, 1024, 1024, 768, 50);
        let mut c = Canvas::new(16, 16, 16);
        let addr = c.addr();
        bar_raw(addr, 0, 16, 16, 50);
        bar_raw(addr, 16, 0, 16, 50);
        bar_raw(addr, 16, 16, 0, 50);
        assert!(c.pixels().iter().all(|&p| p == UNTOUCHED));
    }

    #[test]
    fn rot180_draws_the_same_picture_upside_down() {
        let (sw, sh) = (1024usize, 768usize);
        let mut plain = Canvas::new(sw, sh, sw);
        let addr = plain.addr();
        bar_raw(addr, sw, sw, sh, 30);

        let mut flipped = Canvas::new(sw, sh, sw);
        fb::set_rot180(true);
        let addr = flipped.addr();
        bar_raw(addr, sw, sw, sh, 30);
        fb::set_rot180(false);

        for y in 0..sh {
            for x in 0..sw {
                assert_eq!(
                    plain.get(x, y),
                    flipped.get(sw - 1 - x, sh - 1 - y),
                    "at {x},{y}"
                );
            }
        }
        flipped.assert_no_overrun();
    }

    #[test]
    fn the_fault_marker_fits_in_a_tiny_screen() {
        // `sw.min(64) - 16` used to wrap on anything narrower than 16 px.
        for &(sw, sh) in &[
            (1usize, 1usize),
            (8, 8),
            (16, 16),
            (17, 21),
            (64, 32),
            (640, 480),
        ] {
            let mut c = Canvas::new(sw, sh, sw);
            let addr = c.addr();
            fault_block_raw(addr, sw, sw, sh, 0xAF00_0000, 0x1234);
            c.assert_no_overrun();
        }
    }

    #[test]
    fn the_fault_marker_paints_something_visible() {
        let (sw, sh) = (640usize, 480usize);
        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        fault_block_raw(addr, sw, sw, sh, 0xAF00_0000, 0x0000_0004);
        assert!(c.count(0x0000_00FF) > 0, "no red stripe");
        assert!(c.count(WHITE) > 0, "no border");
        c.assert_no_overrun();
    }

    #[test]
    fn different_faults_paint_different_markers() {
        let (sw, sh) = (640usize, 480usize);
        let render = |tag: u32, code: u32| {
            let mut c = Canvas::new(sw, sh, sw);
            let addr = c.addr();
            fault_block_raw(addr, sw, sw, sh, tag, code);
            c.pixels().to_vec()
        };
        // A #PF and a #GP have to be told apart on a screen with no console.
        assert_ne!(render(0xAF00_0000, 4), render(0xA600_0000, 4));
        assert_ne!(render(0xAF00_0000, 4), render(0xAF00_0000, 5));
    }
}
