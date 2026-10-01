//! Choosing the graphic mode to boot with: EDID parsing and the policy that
//! turns `resolution=` plus the firmware's mode list into one mode.
//!
//! None of this talks to the firmware, so it is exactly the part that can be
//! tested on a host with no GOP: `init_graphic` collects `(width, height)`
//! from the GOP and asks [`choose_mode`] which one to set.

use crate::config::Resolution;

/// Auto (and an oversized EDID) refuse GOP modes larger than this many pixels.
///
/// VirtualBox EFI GOP lists VRAM-filling "modes" (8K = 7680×4320) that are
/// not a real panel. The kernel shadows each VT at `width×height×4` bytes;
/// seven 8K consoles (~882 MiB) OOM a 512 MiB heap. 4K (3840×2160) is the
/// largest desktop panel we still fit. `resolution=WxH` is uncapped.
pub const AUTO_MAX_PIXELS: usize = 3840 * 2160;

/// Whether a mode is small enough for [`Resolution::Auto`] to pick it.
pub fn mode_fits_auto_cap(w: usize, h: usize) -> bool {
    w > 0 && h > 0 && w.saturating_mul(h) <= AUTO_MAX_PIXELS
}

/// Whether a buffer starts with the EDID 1.x magic `00 FF FF FF FF FF FF 00`.
pub fn edid_header_ok(b: &[u8]) -> bool {
    b.len() >= 8 && b[0] == 0x00 && b[7] == 0x00 && b[1..7].iter().all(|&x| x == 0xFF)
}

/// Parse the display's EDID-preferred resolution from the first detailed
/// timing descriptor (EDID 1.x, bytes 54..71): a non-zero pixel clock marks a
/// timing descriptor, whose active pixels are 12-bit fields split across
/// low-byte + high-nibble. Returns `None` for a missing/invalid EDID or an
/// implausible timing.
///
/// The header check is not decoration. `read_active_edid` hands back the
/// first *non-empty* capture when no source had a valid header, so that
/// `/proc` can dump it for diagnosis -- and that same buffer arrives here.
/// Without this guard, 18 bytes of firmware garbage that happen to land in
/// the plausible range become the mode the machine boots at.
pub fn edid_preferred_resolution(edid: &[u8; 128], edid_size: u32) -> Option<(usize, usize)> {
    if edid_size < 72 {
        return None;
    }
    if !edid_header_ok(&edid[..8]) {
        return None;
    }
    let d = &edid[54..72];
    let pixel_clock = u16::from_le_bytes([d[0], d[1]]);
    if pixel_clock == 0 {
        return None; // not a timing descriptor
    }
    let h = d[2] as usize | ((d[4] as usize & 0xF0) << 4);
    let v = d[5] as usize | ((d[7] as usize & 0xF0) << 4);
    if !(256..=7680).contains(&h) || !(144..=4320).contains(&v) {
        return None;
    }
    Some((h, v))
}

/// One mode the firmware offers, as [`choose_mode`] needs to see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: usize,
    pub height: usize,
    /// Whether this mode has a linear framebuffer we can store into
    /// (`crate::fb::is_direct` of its pixel format).
    ///
    /// Choosing a `BltOnly` mode is worse than not changing the mode at all:
    /// its framebuffer base is null, so rboot draws no splash *and* the kernel
    /// is handed `fb_addr = 0`, logs "no framebuffer from bootloader" and comes
    /// up with no graphic console. A resolution can be listed more than once
    /// with different pixel formats, so which of the two we pick matters.
    pub direct: bool,
}

impl Mode {
    pub fn new(width: usize, height: usize, direct: bool) -> Self {
        Mode {
            width,
            height,
            direct,
        }
    }

    fn pixels(&self) -> usize {
        self.width.saturating_mul(self.height)
    }
}

/// Pick the index in `modes` of the GOP mode to set, or `None` to keep the
/// mode the firmware already selected.
///
/// - [`Resolution::Keep`]: never changes the mode.
/// - [`Resolution::Exact`]: that resolution, or keep the current one. (The old
///   behaviour panicked with "graphic mode not found", bricking boot over a
///   config value the firmware happens not to offer.)
/// - [`Resolution::Auto`]: the EDID-preferred timing if the firmware offers
///   it *and* it fits [`AUTO_MAX_PIXELS`]; otherwise the largest offered mode
///   within that cap. If every offered mode is over the cap, the smallest one
///   -- some VMs only list huge modes and we would rather boot at 8K than not
///   boot at all.
///
/// Modes with no linear framebuffer are skipped in both policies (see
/// [`Mode::direct`]); only if *nothing* on offer is drawable do we fall back to
/// considering them, because on such a machine there is no better mode to be
/// had and today's behaviour at least boots.
pub fn choose_mode(
    resolution: Resolution,
    preferred: Option<(usize, usize)>,
    modes: &[Mode],
) -> Option<usize> {
    let target = match resolution {
        Resolution::Keep => None,
        Resolution::Exact(x, y) => Some((x, y)),
        Resolution::Auto => preferred.filter(|&(w, h)| mode_fits_auto_cap(w, h)),
    };
    if let Some((w, h)) = target {
        let matches = |m: &Mode| m.width == w && m.height == h;
        if let Some(i) = modes.iter().position(|m| matches(m) && m.direct) {
            return Some(i);
        }
        if resolution != Resolution::Auto {
            // `Exact` never picks a different resolution behind the user's
            // back, and a listing that only offers this one as `BltOnly` is
            // not usable, so keep whatever the firmware already set.
            return if modes.iter().any(|m| m.direct) {
                None
            } else {
                modes.iter().position(matches)
            };
        }
    }
    if resolution != Resolution::Auto {
        return None;
    }
    auto_pick(modes, true).or_else(|| auto_pick(modes, false))
}

/// `Auto`'s fallback: the largest mode under the cap, else the smallest mode
/// at all. With `direct_only`, modes we cannot draw on are not candidates.
fn auto_pick(modes: &[Mode], direct_only: bool) -> Option<usize> {
    let usable = |m: &Mode| m.direct || !direct_only;
    modes
        .iter()
        .enumerate()
        .filter(|(_, m)| usable(m) && mode_fits_auto_cap(m.width, m.height))
        .max_by_key(|(_, m)| m.pixels())
        .or_else(|| {
            modes
                .iter()
                .enumerate()
                .filter(|(_, m)| usable(m))
                .min_by_key(|(_, m)| m.pixels())
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed EDID block 0 whose first detailed timing is `w × h`.
    fn edid_with_dtd(w: usize, h: usize) -> [u8; 128] {
        let mut e = [0u8; 128];
        e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        let d = &mut e[54..72];
        d[0..2].copy_from_slice(&148u16.to_le_bytes()); // pixel clock, non-zero
        d[2] = (w & 0xFF) as u8;
        d[4] = ((w >> 4) & 0xF0) as u8;
        d[5] = (h & 0xFF) as u8;
        d[7] = ((h >> 4) & 0xF0) as u8;
        e
    }

    #[test]
    fn preferred_timing_is_read_from_the_first_dtd() {
        for &(w, h) in &[(1920, 1080), (1366, 768), (3840, 2160), (2560, 1440)] {
            assert_eq!(
                edid_preferred_resolution(&edid_with_dtd(w, h), 128),
                Some((w, h)),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn a_short_edid_is_not_parsed() {
        let e = edid_with_dtd(1920, 1080);
        assert_eq!(edid_preferred_resolution(&e, 0), None);
        assert_eq!(edid_preferred_resolution(&e, 71), None);
        assert_eq!(edid_preferred_resolution(&e, 72), Some((1920, 1080)));
    }

    #[test]
    fn a_zero_pixel_clock_is_not_a_timing_descriptor() {
        let mut e = edid_with_dtd(1920, 1080);
        e[54] = 0;
        e[55] = 0;
        assert_eq!(edid_preferred_resolution(&e, 128), None);
    }

    #[test]
    fn implausible_timings_are_refused() {
        for &(w, h) in &[(0, 1080), (1920, 0), (128, 1080), (1920, 16), (255, 143)] {
            assert_eq!(
                edid_preferred_resolution(&edid_with_dtd(w, h), 128),
                None,
                "{w}x{h} should not be trusted"
            );
        }
    }

    #[test]
    fn a_dtd_cannot_express_more_than_12_bits_of_active_pixels() {
        // So the upper ends of the plausibility range are belt and braces:
        // the largest timing an EDID 1.x DTD can carry is 4095x4095.
        let e = edid_with_dtd(0xFFF, 0xFFF);
        assert_eq!(edid_preferred_resolution(&e, 128), Some((4095, 4095)));
    }

    #[test]
    fn garbage_without_the_edid_header_is_not_a_mode() {
        // `read_active_edid` keeps the first non-empty capture even when no
        // source had a valid header, "so /proc can dump it for diagnosis".
        // That buffer must not be able to choose the boot resolution.
        let mut e = edid_with_dtd(1920, 1080);
        e[1] = 0x00; // magic broken, everything else still plausible
        assert_eq!(edid_preferred_resolution(&e, 128), None);
        assert_eq!(edid_preferred_resolution(&[0u8; 128], 128), None);
        assert_eq!(edid_preferred_resolution(&[0xAAu8; 128], 128), None);
    }

    #[test]
    fn auto_cap_is_4k() {
        assert!(mode_fits_auto_cap(3840, 2160));
        assert!(mode_fits_auto_cap(1920, 1080));
        assert!(!mode_fits_auto_cap(7680, 4320));
        assert!(!mode_fits_auto_cap(0, 0));
        assert!(!mode_fits_auto_cap(usize::MAX, usize::MAX));
    }

    /// Firmware mode lists in these tests are all drawable unless a test says
    /// otherwise, which is the normal case.
    fn direct(modes: &[(usize, usize)]) -> alloc::vec::Vec<Mode> {
        modes.iter().map(|&(w, h)| Mode::new(w, h, true)).collect()
    }

    const RES: &[(usize, usize)] = &[(640, 480), (800, 600), (1024, 768), (1920, 1080)];

    #[test]
    fn keep_never_changes_the_mode() {
        let modes = direct(RES);
        assert_eq!(
            choose_mode(Resolution::Keep, Some((1920, 1080)), &modes),
            None
        );
        assert_eq!(choose_mode(Resolution::Keep, None, &modes), None);
    }

    #[test]
    fn exact_picks_that_mode() {
        assert_eq!(
            choose_mode(Resolution::Exact(1024, 768), None, &direct(RES)),
            Some(2)
        );
    }

    #[test]
    fn exact_that_the_firmware_does_not_offer_keeps_the_current_mode() {
        // Never a panic, and never a different mode behind the user's back.
        assert_eq!(
            choose_mode(Resolution::Exact(1280, 1024), None, &direct(RES)),
            None
        );
    }

    #[test]
    fn exact_is_not_capped() {
        let modes = direct(&[(1024, 768), (7680, 4320)]);
        assert_eq!(
            choose_mode(Resolution::Exact(7680, 4320), None, &modes),
            Some(1)
        );
    }

    #[test]
    fn exact_skips_a_bltonly_listing_of_the_resolution_it_asks_for() {
        // The one that makes `resolution=WxH` "not work" while `auto` does:
        // a mode with no linear framebuffer hands the kernel `fb_addr = 0`, so
        // the machine boots with no splash and no graphic console at all.
        let modes = [
            Mode::new(1920, 1080, false),
            Mode::new(1024, 768, true),
            Mode::new(1920, 1080, true),
        ];
        assert_eq!(
            choose_mode(Resolution::Exact(1920, 1080), None, &modes),
            Some(2)
        );
    }

    #[test]
    fn exact_keeps_the_current_mode_when_its_only_listing_is_bltonly() {
        // Setting it would blank the machine; the mode the firmware already
        // chose is at least on screen.
        let modes = [Mode::new(1920, 1080, false), Mode::new(1024, 768, true)];
        assert_eq!(
            choose_mode(Resolution::Exact(1920, 1080), None, &modes),
            None
        );
    }

    #[test]
    fn a_firmware_with_nothing_drawable_is_not_made_worse() {
        // Nothing to choose between, so behave exactly as before: honour the
        // request, and let `auto` still pick the largest under the cap.
        let modes = [
            Mode::new(1024, 768, false),
            Mode::new(1920, 1080, false),
            Mode::new(7680, 4320, false),
        ];
        assert_eq!(
            choose_mode(Resolution::Exact(1024, 768), None, &modes),
            Some(0)
        );
        assert_eq!(choose_mode(Resolution::Auto, None, &modes), Some(1));
    }

    #[test]
    fn auto_prefers_the_edid_timing_over_the_largest_mode() {
        // The 1366x768 TV: its firmware offers better modes, and stretching a
        // 4:3 1024x768 across a 16:9 panel is what it used to do.
        let modes = direct(&[(640, 480), (1024, 768), (1366, 768), (1920, 1080)]);
        assert_eq!(
            choose_mode(Resolution::Auto, Some((1366, 768)), &modes),
            Some(2)
        );
    }

    #[test]
    fn auto_without_edid_takes_the_largest_mode_under_the_cap() {
        assert_eq!(choose_mode(Resolution::Auto, None, &direct(RES)), Some(3));
    }

    #[test]
    fn auto_skips_modes_it_cannot_draw_on() {
        let modes = [
            Mode::new(1024, 768, true),
            Mode::new(1920, 1080, false),
            Mode::new(1366, 768, false),
        ];
        assert_eq!(choose_mode(Resolution::Auto, None, &modes), Some(0));
        assert_eq!(
            choose_mode(Resolution::Auto, Some((1366, 768)), &modes),
            Some(0)
        );
    }

    #[test]
    fn auto_never_picks_an_8k_virtualbox_mode() {
        // The kernel shadows every VT at width*height*4; 8K OOMs the heap.
        let modes = direct(&[(1024, 768), (1920, 1080), (7680, 4320)]);
        assert_eq!(choose_mode(Resolution::Auto, None, &modes), Some(1));
        // Not even when the (bogus) EDID asks for it.
        assert_eq!(
            choose_mode(Resolution::Auto, Some((7680, 4320)), &modes),
            Some(1)
        );
    }

    #[test]
    fn auto_falls_back_to_the_smallest_when_every_mode_is_over_the_cap() {
        let modes = direct(&[(7680, 4320), (5120, 2880)]);
        assert_eq!(choose_mode(Resolution::Auto, None, &modes), Some(1));
    }

    #[test]
    fn auto_with_an_edid_mode_the_firmware_does_not_offer_falls_back_to_largest() {
        assert_eq!(
            choose_mode(Resolution::Auto, Some((2560, 1440)), &direct(RES)),
            Some(3)
        );
    }

    #[test]
    fn no_modes_at_all_keeps_the_current_one() {
        assert_eq!(choose_mode(Resolution::Auto, Some((1920, 1080)), &[]), None);
        assert_eq!(choose_mode(Resolution::Exact(640, 480), None, &[]), None);
    }

    #[test]
    fn a_zero_sized_mode_is_never_chosen_over_a_real_one() {
        let modes = direct(&[(0, 0), (1024, 768)]);
        assert_eq!(choose_mode(Resolution::Auto, None, &modes), Some(1));
    }
}
