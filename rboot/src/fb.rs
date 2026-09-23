//! Shared GOP pixel addressing for the splash / progress bar.
//!
//! `FB_ROT180` / `FB_MIRROR_X` on the kernel command line remap every store.
//! They are opt-in (odd panels); they are not implied by the hypervisor.

use core::sync::atomic::{AtomicBool, Ordering};
use uefi::proto::console::gop::{ModeInfo, PixelFormat};

static ROT180: AtomicBool = AtomicBool::new(false);
static MIRROR_X: AtomicBool = AtomicBool::new(false);

pub fn set_rot180(enable: bool) {
    ROT180.store(enable, Ordering::SeqCst);
}

pub fn set_mirror_x(enable: bool) {
    MIRROR_X.store(enable, Ordering::SeqCst);
}

/// Whether this pixel format has a framebuffer we may store into.
///
/// `BltOnly` means the firmware exposes no linear framebuffer at all: its
/// `FrameBuffer` base is null, so drawing "into it" is a store to address 0.
/// Every drawing entry point has to ask this one question in the same place:
/// the splash and the progress bar used to answer it differently, and
/// `Bitmask` (what NVIDIA GOPs report) was drawable for the bar and skipped
/// for the logo, while `BltOnly` was the exact opposite.
pub fn is_direct(fmt: PixelFormat) -> bool {
    match fmt {
        // 32bpp with a linear framebuffer. We only ever store pure white
        // (0x00FF_FFFF) and pure black (0x0000_0000), which are the same
        // under any channel order, so `Bitmask` needs no special handling.
        PixelFormat::Rgb | PixelFormat::Bgr | PixelFormat::Bitmask => true,
        PixelFormat::BltOnly => false,
    }
}

#[inline]
pub fn map_xy(x: usize, y: usize, sw: usize, sh: usize) -> (usize, usize) {
    map_xy_with(
        x,
        y,
        sw,
        sh,
        ROT180.load(Ordering::SeqCst),
        MIRROR_X.load(Ordering::SeqCst),
    )
}

/// The orientation mapping itself, with the flags passed in.
///
/// 180° rotation flips both axes; the extra X mirror on top of it leaves a
/// vertical flip, which is how a panel mounted upside down is corrected.
#[inline]
pub fn map_xy_with(
    x: usize,
    y: usize,
    sw: usize,
    sh: usize,
    rot180: bool,
    mirror_x: bool,
) -> (usize, usize) {
    let (mut x, mut y) = (x, y);
    if rot180 {
        x = sw.saturating_sub(1).saturating_sub(x);
        y = sh.saturating_sub(1).saturating_sub(y);
    }
    if mirror_x {
        x = sw.saturating_sub(1).saturating_sub(x);
    }
    (x, y)
}

/// A framebuffer we are allowed to store into.
///
/// Bundling the four parameters every drawing primitive needs is not only
/// tidier: it is the one place that knows a pixel outside `sw × sh` is not
/// ours to write, and that a scanline is `stride` pixels wide, not `sw`.
#[derive(Clone, Copy)]
pub struct Screen {
    fb: *mut u32,
    stride: usize,
    sw: usize,
    sh: usize,
}

impl Screen {
    /// `None` when there is nothing to draw on: a null base (which is what a
    /// `BltOnly` GOP hands out) or an empty mode.
    ///
    /// # Safety
    /// `fb_addr` must point at `stride * sh` writable `u32`s.
    pub unsafe fn new(fb_addr: u64, stride: usize, sw: usize, sh: usize) -> Option<Self> {
        if fb_addr == 0 || stride == 0 || sw == 0 || sh == 0 {
            return None;
        }
        Some(Screen {
            fb: fb_addr as *mut u32,
            stride,
            sw,
            sh,
        })
    }

    /// The same, with the geometry read out of a GOP mode. `None` for a mode
    /// with no linear framebuffer (see [`is_direct`]).
    ///
    /// # Safety
    /// `fb_addr` must be the framebuffer base this `mode` describes.
    pub unsafe fn from_mode(mode: ModeInfo, fb_addr: u64) -> Option<Self> {
        if !is_direct(mode.pixel_format()) {
            return None;
        }
        let (sw, sh) = mode.resolution();
        // SAFETY: the caller guarantees `fb_addr` belongs to `mode`.
        unsafe { Self::new(fb_addr, mode.stride(), sw, sh) }
    }

    pub fn width(&self) -> usize {
        self.sw
    }

    pub fn height(&self) -> usize {
        self.sh
    }

    /// Store one pixel, with the screen's orientation applied. Coordinates
    /// outside the visible area are dropped, not wrapped.
    #[inline]
    pub fn put(&self, x: usize, y: usize, pixel: u32) {
        if x >= self.sw || y >= self.sh {
            return;
        }
        let (x, y) = map_xy(x, y, self.sw, self.sh);
        // SAFETY: `Screen::new` promised `stride * sh` writable pixels, and
        // `map_xy` keeps `x < sw <= stride` and `y < sh`.
        unsafe { core::ptr::write_volatile(self.fb.add(y * self.stride + x), pixel) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_no_flag_is_set() {
        assert_eq!(map_xy_with(0, 0, 1920, 1080, false, false), (0, 0));
        assert_eq!(map_xy_with(7, 9, 1920, 1080, false, false), (7, 9));
        assert_eq!(
            map_xy_with(1919, 1079, 1920, 1080, false, false),
            (1919, 1079)
        );
    }

    #[test]
    fn rot180_flips_both_axes() {
        assert_eq!(map_xy_with(0, 0, 1920, 1080, true, false), (1919, 1079));
        assert_eq!(map_xy_with(1919, 1079, 1920, 1080, true, false), (0, 0));
        assert_eq!(map_xy_with(10, 20, 100, 50, true, false), (89, 29));
    }

    #[test]
    fn mirror_x_flips_only_x() {
        assert_eq!(map_xy_with(0, 0, 1920, 1080, false, true), (1919, 0));
        assert_eq!(map_xy_with(10, 20, 100, 50, false, true), (89, 20));
    }

    #[test]
    fn rot180_and_mirror_x_together_are_a_vertical_flip() {
        for (x, y) in [(0usize, 0usize), (10, 20), (99, 49)] {
            assert_eq!(map_xy_with(x, y, 100, 50, true, true), (x, 49 - y));
        }
    }

    #[test]
    fn every_mapping_stays_inside_the_screen() {
        for &(sw, sh) in &[(1usize, 1usize), (7, 3), (640, 480), (1920, 1080)] {
            for rot in [false, true] {
                for mir in [false, true] {
                    for y in 0..sh {
                        for x in 0..sw {
                            let (mx, my) = map_xy_with(x, y, sw, sh, rot, mir);
                            assert!(mx < sw && my < sh, "{x},{y} -> {mx},{my} on {sw}x{sh}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_zero_sized_screen_does_not_underflow() {
        // `saturating_sub`, not `- 1`: a 0-wide mode would wrap to usize::MAX
        // and store far outside the framebuffer.
        assert_eq!(map_xy_with(0, 0, 0, 0, true, true), (0, 0));
    }

    #[test]
    fn the_global_flags_reach_map_xy() {
        let _g = crate::testing::ScreenGuard::acquire();
        set_rot180(false);
        set_mirror_x(false);
        assert_eq!(map_xy(10, 20, 100, 50), (10, 20));
        set_rot180(true);
        assert_eq!(map_xy(10, 20, 100, 50), (89, 29));
        set_mirror_x(true);
        assert_eq!(map_xy(10, 20, 100, 50), (10, 29));
        set_rot180(false);
        set_mirror_x(false);
    }

    #[test]
    fn bltonly_is_the_only_format_without_a_framebuffer() {
        assert!(is_direct(PixelFormat::Rgb));
        assert!(is_direct(PixelFormat::Bgr));
        // NVIDIA GOPs report Bitmask; the splash used to refuse to draw on it.
        assert!(is_direct(PixelFormat::Bitmask));
        // And the splash used to happily store into its null framebuffer.
        assert!(!is_direct(PixelFormat::BltOnly));
    }
}
