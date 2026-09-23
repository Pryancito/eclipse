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

/// Pick the index in `modes` of the GOP mode to set, or `None` to keep the
/// mode the firmware already selected.
///
/// - [`Resolution::Keep`]: never changes the mode.
/// - [`Resolution::Exact`]: that mode, or keep the current one. (The old
///   behaviour panicked with "graphic mode not found", bricking boot over a
///   config value the firmware happens not to offer.)
/// - [`Resolution::Auto`]: the EDID-preferred timing if the firmware offers
///   it *and* it fits [`AUTO_MAX_PIXELS`]; otherwise the largest offered mode
///   within that cap. If every offered mode is over the cap, the smallest one
///   -- some VMs only list huge modes and we would rather boot at 8K than not
///   boot at all.
pub fn choose_mode(
    resolution: Resolution,
    preferred: Option<(usize, usize)>,
    modes: &[(usize, usize)],
) -> Option<usize> {
    let target = match resolution {
        Resolution::Keep => None,
        Resolution::Exact(x, y) => Some((x, y)),
        Resolution::Auto => preferred.filter(|&(w, h)| mode_fits_auto_cap(w, h)),
    };
    if let Some(want) = target {
        if let Some(i) = modes.iter().position(|&m| m == want) {
            return Some(i);
        }
    }
    if resolution != Resolution::Auto {
        return None;
    }
    modes
        .iter()
        .enumerate()
        .filter(|(_, &(w, h))| mode_fits_auto_cap(w, h))
        .max_by_key(|(_, &(w, h))| w.saturating_mul(h))
        .or_else(|| {
            modes
                .iter()
                .enumerate()
                .min_by_key(|(_, &(w, h))| w.saturating_mul(h))
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

    const MODES: &[(usize, usize)] = &[(640, 480), (800, 600), (1024, 768), (1920, 1080)];

    #[test]
    fn keep_never_changes_the_mode() {
        assert_eq!(
            choose_mode(Resolution::Keep, Some((1920, 1080)), MODES),
            None
        );
        assert_eq!(choose_mode(Resolution::Keep, None, MODES), None);
    }

    #[test]
    fn exact_picks_that_mode() {
        assert_eq!(
            choose_mode(Resolution::Exact(1024, 768), None, MODES),
            Some(2)
        );
    }

    #[test]
    fn exact_that_the_firmware_does_not_offer_keeps_the_current_mode() {
        // Never a panic, and never a different mode behind the user's back.
        assert_eq!(
            choose_mode(Resolution::Exact(1280, 1024), None, MODES),
            None
        );
    }

    #[test]
    fn exact_is_not_capped() {
        let modes = [(1024, 768), (7680, 4320)];
        assert_eq!(
            choose_mode(Resolution::Exact(7680, 4320), None, &modes),
            Some(1)
        );
    }

    #[test]
    fn auto_prefers_the_edid_timing_over_the_largest_mode() {
        // The 1366x768 TV: its firmware offers better modes, and stretching a
        // 4:3 1024x768 across a 16:9 panel is what it used to do.
        let modes = [(640, 480), (1024, 768), (1366, 768), (1920, 1080)];
        assert_eq!(
            choose_mode(Resolution::Auto, Some((1366, 768)), &modes),
            Some(2)
        );
    }

    #[test]
    fn auto_without_edid_takes_the_largest_mode_under_the_cap() {
        assert_eq!(choose_mode(Resolution::Auto, None, MODES), Some(3));
    }

    #[test]
    fn auto_never_picks_an_8k_virtualbox_mode() {
        // The kernel shadows every VT at width*height*4; 8K OOMs the heap.
        let modes = [(1024, 768), (1920, 1080), (7680, 4320)];
        assert_eq!(choose_mode(Resolution::Auto, None, &modes), Some(1));
        // Not even when the (bogus) EDID asks for it.
        assert_eq!(
            choose_mode(Resolution::Auto, Some((7680, 4320)), &modes),
            Some(1)
        );
    }

    #[test]
    fn auto_falls_back_to_the_smallest_when_every_mode_is_over_the_cap() {
        let modes = [(7680, 4320), (5120, 2880)];
        assert_eq!(choose_mode(Resolution::Auto, None, &modes), Some(1));
    }

    #[test]
    fn auto_with_an_edid_mode_the_firmware_does_not_offer_falls_back_to_largest() {
        assert_eq!(
            choose_mode(Resolution::Auto, Some((2560, 1440)), MODES),
            Some(3)
        );
    }

    #[test]
    fn no_modes_at_all_keeps_the_current_one() {
        assert_eq!(choose_mode(Resolution::Auto, Some((1920, 1080)), &[]), None);
        assert_eq!(choose_mode(Resolution::Exact(640, 480), None, &[]), None);
    }
}
