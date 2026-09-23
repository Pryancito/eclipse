//! Boot logo drawing (UEFI GOP framebuffer).
//!
//! The logo is an 800x250 raw BGRA image (32bpp), same as the userspace
//! `display_service` asset.

use uefi::proto::console::gop::ModeInfo;

use crate::fb::Screen;

pub const LOGO_WIDTH: usize = 800;
pub const LOGO_HEIGHT: usize = 250;

// NOTE: absolute path per request. Consider vendoring into `rboot/src/` later.
const LOGO_DATA: &[u8] = include_bytes!("logo.raw");

const WHITE: u32 = 0x00FF_FFFF;

pub fn draw_centered(mode: ModeInfo, fb_addr: u64) {
    // One question, asked in one place (`fb::is_direct`, via `Screen`). This
    // used to be an inline list of formats that disagreed with the progress
    // bar's: it skipped `Bitmask` -- what an NVIDIA GOP reports -- and
    // accepted `BltOnly`, whose framebuffer base is null.
    //
    // SAFETY: the kernel only calls this with the base of `mode`'s own GOP
    // framebuffer, which is `stride * height` pixels.
    if let Some(s) = unsafe { Screen::from_mode(mode, fb_addr) } {
        draw(&s);
    }
}

/// Clear the screen to white and draw the logo centered on it.
pub fn draw_centered_raw(fb_addr: u64, stride: usize, sw: usize, sh: usize) {
    // SAFETY: as above.
    if let Some(s) = unsafe { Screen::new(fb_addr, stride, sw, sh) } {
        draw(&s);
    }
}

fn draw(s: &Screen) {
    let (sw, sh) = (s.width(), s.height());
    let start_x = sw.saturating_sub(LOGO_WIDTH) / 2;
    let start_y = sh.saturating_sub(LOGO_HEIGHT) / 2;

    // Clear to white (match the asset background expectation).
    for y in 0..sh {
        for x in 0..sw {
            s.put(x, y, WHITE);
        }
    }

    // Draw logo. LOGO_DATA is BGRA (b,g,r,a). GOP is typically BGRX/BGRA.
    for y in 0..LOGO_HEIGHT {
        let sy = start_y + y;
        if sy >= sh {
            break;
        }
        for x in 0..LOGO_WIDTH {
            let sx = start_x + x;
            if sx >= sw {
                break;
            }
            let idx = (y * LOGO_WIDTH + x) * 4;
            if idx + 3 >= LOGO_DATA.len() {
                return;
            }
            let b = LOGO_DATA[idx] as u32;
            let g = LOGO_DATA[idx + 1] as u32;
            let r = LOGO_DATA[idx + 2] as u32;
            let a = LOGO_DATA[idx + 3] as u32;
            if a == 0 {
                continue;
            }
            // Write as 0x00RRGGBB; UEFI GOP Bgr generally maps this as B,G,R in low bytes.
            let pixel = (r << 16) | (g << 8) | b;
            s.put(sx, sy, pixel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_mode_info, Canvas};
    use uefi::proto::console::gop::PixelFormat;

    #[test]
    fn the_asset_is_exactly_the_declared_size() {
        // `draw_centered_raw` bails out of the whole draw the moment it runs
        // past the end of the asset, so a short file is a half-drawn logo.
        assert_eq!(LOGO_DATA.len(), LOGO_WIDTH * LOGO_HEIGHT * 4);
    }

    #[test]
    fn the_whole_visible_screen_is_painted() {
        let mut c = Canvas::new(1024, 768, 1024);
        let addr = c.addr();
        draw_centered_raw(addr, 1024, 1024, 768);
        assert_eq!(c.untouched_visible(), 0, "the splash left holes");
        c.assert_no_overrun();
    }

    #[test]
    fn the_logo_is_centered_and_is_not_blank() {
        let (sw, sh) = (1024usize, 768usize);
        let mut c = Canvas::new(sw, sh, sw);
        let addr = c.addr();
        draw_centered_raw(addr, sw, sw, sh);
        let x0 = (sw - LOGO_WIDTH) / 2;
        let y0 = (sh - LOGO_HEIGHT) / 2;
        // Outside the logo rect it is the white background.
        assert_eq!(c.get(0, 0), WHITE);
        assert_eq!(c.get(x0 - 1, y0), WHITE);
        // Inside it, the asset really was copied.
        let drawn = (0..LOGO_HEIGHT)
            .flat_map(|y| (0..LOGO_WIDTH).map(move |x| (x, y)))
            .filter(|&(x, y)| c.get(x0 + x, y0 + y) != WHITE)
            .count();
        assert!(
            drawn > 1000,
            "only {drawn} logo pixels differ from the background"
        );
        c.assert_no_overrun();
    }

    #[test]
    fn a_screen_smaller_than_the_logo_clips_instead_of_overrunning() {
        for &(sw, sh) in &[(640usize, 480usize), (320, 200), (800, 250), (1, 1)] {
            let mut c = Canvas::new(sw, sh, sw);
            let addr = c.addr();
            draw_centered_raw(addr, sw, sw, sh);
            assert_eq!(c.untouched_visible(), 0, "{sw}x{sh} left holes");
            c.assert_no_overrun();
        }
    }

    #[test]
    fn scanline_padding_is_never_written() {
        let (sw, sh, stride) = (800usize, 600usize, 1024usize);
        let mut c = Canvas::new(sw, sh, stride);
        let addr = c.addr();
        draw_centered_raw(addr, stride, sw, sh);
        for y in 0..sh {
            for x in sw..stride {
                assert_eq!(c.raw(y * stride + x), Canvas::UNTOUCHED, "padding {x},{y}");
            }
        }
        c.assert_no_overrun();
    }

    #[test]
    fn a_bltonly_mode_is_never_drawn_into() {
        // BltOnly has no linear framebuffer: `gop.frame_buffer()` hands back
        // base 0, and the splash used to store 800x250 pixels into it while
        // the progress bar (correctly) skipped the mode entirely.
        let mut c = Canvas::new(64, 64, 64);
        let addr = c.addr();
        draw_centered(fake_mode_info(64, 64, 64, PixelFormat::BltOnly), addr);
        assert_eq!(c.untouched_visible(), 64 * 64);
    }

    #[test]
    fn a_bitmask_mode_is_drawn_into() {
        // What an NVIDIA GOP reports. The splash used to skip it, so the
        // machine showed the progress bar on a screen it never cleared.
        for fmt in [PixelFormat::Bgr, PixelFormat::Rgb, PixelFormat::Bitmask] {
            let mut c = Canvas::new(64, 64, 64);
            let addr = c.addr();
            draw_centered(fake_mode_info(64, 64, 64, fmt), addr);
            assert_eq!(c.untouched_visible(), 0, "{fmt:?} was skipped");
        }
    }

    #[test]
    fn the_splash_and_the_progress_bar_agree_on_which_modes_they_draw() {
        // They are drawn one after the other on the same framebuffer, so a
        // format one of them accepts and the other refuses is a half-painted
        // screen at best and a null store at worst.
        for fmt in [
            PixelFormat::Bgr,
            PixelFormat::Rgb,
            PixelFormat::Bitmask,
            PixelFormat::BltOnly,
        ] {
            let mode = fake_mode_info(1024, 768, 1024, fmt);
            let mut logo = Canvas::new(1024, 768, 1024);
            let addr = logo.addr();
            draw_centered(mode, addr);
            let logo_drew = logo.untouched_visible() != 1024 * 768;

            let mut bar = Canvas::new(1024, 768, 1024);
            let addr = bar.addr();
            crate::progress::bar(mode, addr, 50);
            let bar_drew = bar.untouched_visible() != 1024 * 768;

            assert_eq!(logo_drew, bar_drew, "{fmt:?}");
        }
    }
}
