//! EDID decoding: validity, physical size and the preferred timing.
//!
//! Everything a monitor tells the kernel about itself arrives as these 128
//! bytes, and QEMU sends none of them — the emulated display has no EDID at
//! all, so every path below is dead in a VM and live on a real machine. That
//! is the whole reason this is a separate module of pure functions: it is the
//! only way to exercise it without a monitor plugged in.
//!
//! Layout (EDID 1.3/1.4, one 128-byte block):
//!
//! | bytes    | meaning                                              |
//! | -------- | ---------------------------------------------------- |
//! | 0..8     | fixed header `00 FF FF FF FF FF FF 00`               |
//! | 21, 22   | max image size in **cm**, 0 = undefined              |
//! | 54..126  | four 18-byte descriptors                             |
//! | 127      | checksum: the 128 bytes must sum to 0 mod 256        |
//!
//! A descriptor whose first two bytes are zero is a *display* descriptor
//! (monitor name, range limits); anything else is a detailed timing, whose
//! first two bytes are the pixel clock.

/// One EDID block. Extension blocks (CEA-861 and friends) are the same size
/// and are not decoded here.
pub const BLOCK_LEN: usize = 128;

const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// Whether `block` is a whole, self-consistent EDID block.
///
/// Firmware hands over whatever it last read off the DDC line, and a flaky
/// line, an unpowered sink or a GOP that never filled its buffer all produce
/// something that *looks* like an EDID. Taking those bytes at face value gives
/// a client a physical size and a native mode invented out of line noise, so
/// they are refused here the way Linux's `drm_edid_block_valid` refuses them:
/// the fixed header, then the checksum.
pub fn block_valid(block: &[u8]) -> bool {
    if block.len() < BLOCK_LEN {
        return false;
    }
    if block[..8] != HEADER {
        return false;
    }
    // The spec defines the last byte so the whole block sums to zero.
    block[..BLOCK_LEN]
        .iter()
        .fold(0u8, |sum, b| sum.wrapping_add(*b))
        == 0
}

/// The four 18-byte descriptors, in order.
fn descriptors(block: &[u8]) -> impl Iterator<Item = &[u8]> {
    (0..4).filter_map(move |i| {
        let start = 54 + i * 18;
        block.get(start..start + 18)
    })
}

/// A detailed timing descriptor is one whose pixel clock is non-zero; a zero
/// there marks a display descriptor instead.
fn detailed_timings(block: &[u8]) -> impl Iterator<Item = &[u8]> {
    descriptors(block).filter(|d| u16::from_le_bytes([d[0], d[1]]) != 0)
}

/// The screen's physical size in millimetres, best source first.
///
/// A detailed timing carries the size to the millimetre (bytes 12/13/14 of the
/// descriptor); bytes 21/22 of the block carry it only to the centimetre and
/// are 0 when the display declines to say. Both dimensions have to be present:
/// half an answer is worse than none, because a client told the screen is
/// `600x0` mm computes an infinite DPI, where one told nothing falls back to a
/// sane guess.
///
/// Returns `None` for a projector or a TV that reports no size at all, which
/// the spec allows and which is the caller's cue to estimate.
pub fn physical_size_mm(block: &[u8]) -> Option<(u32, u32)> {
    if !block_valid(block) {
        return None;
    }
    // Precise: the first detailed timing that states a size. Observed on a
    // 32" TV that reports 885x497 mm here and nothing in the cm bytes -- the
    // estimate it fell back to was 270x203, which skewed every DPI-aware
    // client on the machine.
    for d in detailed_timings(block) {
        let w = d[12] as u32 | ((d[14] as u32 & 0xF0) << 4);
        let h = d[13] as u32 | ((d[14] as u32 & 0x0F) << 8);
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }
    // Coarse: whole centimetres, or 0 for "not stated".
    let (w_cm, h_cm) = (block[21] as u32, block[22] as u32);
    if w_cm > 0 && h_cm > 0 {
        Some((w_cm * 10, h_cm * 10))
    } else {
        None
    }
}

/// The display's preferred (native) resolution in pixels.
///
/// The first descriptor is the preferred timing by definition, so this is what
/// the panel actually has; anything else is scaled by the monitor.
pub fn preferred_mode(block: &[u8]) -> Option<(u32, u32)> {
    if !block_valid(block) {
        return None;
    }
    let d = detailed_timings(block).next()?;
    let w = d[2] as u32 | ((d[4] as u32 & 0xF0) << 4);
    let h = d[5] as u32 | ((d[7] as u32 & 0xF0) << 4);
    (w > 0 && h > 0).then_some((w, h))
}

/// An estimate from the mode alone, for a display that states no size.
///
/// ~96 DPI is what X and Wayland compositors assume when they are told
/// nothing, so producing it here keeps one answer instead of several.
pub fn estimated_size_mm(width_px: u32, height_px: u32) -> (u32, u32) {
    // 1 inch = 25.4 mm at 96 px/inch, as 254/960 to stay in integers.
    (
        (width_px * 254 / 960).max(1),
        (height_px * 254 / 960).max(1),
    )
}

#[cfg(test)]
mod tests {
    //! Host tests for the EDID decoder.
    //!
    //! Every case here is one a real monitor produces and QEMU never does, so
    //! this file is the only place the code runs outside Moebius's machine.

    use super::*;
    use alloc::vec::Vec;

    /// Build a block with a correct header and checksum. `f` fills in the
    /// interesting bytes before the checksum is computed over the result.
    fn edid(f: impl FnOnce(&mut [u8; BLOCK_LEN])) -> [u8; BLOCK_LEN] {
        let mut b = [0u8; BLOCK_LEN];
        b[..8].copy_from_slice(&HEADER);
        b[18] = 1; // version 1
        b[19] = 4; // revision 4
        f(&mut b);
        let sum = b[..BLOCK_LEN - 1]
            .iter()
            .fold(0u8, |s, x| s.wrapping_add(*x));
        b[BLOCK_LEN - 1] = sum.wrapping_neg();
        b
    }

    /// Write a detailed timing descriptor into slot `slot` (0..4).
    fn timing(b: &mut [u8; BLOCK_LEN], slot: usize, w_px: u32, h_px: u32, w_mm: u32, h_mm: u32) {
        let o = 54 + slot * 18;
        b[o] = 0x01; // any non-zero pixel clock marks it as a timing
        b[o + 1] = 0x02;
        b[o + 2] = (w_px & 0xFF) as u8;
        b[o + 4] = ((w_px >> 4) & 0xF0) as u8;
        b[o + 5] = (h_px & 0xFF) as u8;
        b[o + 7] = ((h_px >> 4) & 0xF0) as u8;
        b[o + 12] = (w_mm & 0xFF) as u8;
        b[o + 13] = (h_mm & 0xFF) as u8;
        b[o + 14] = (((w_mm >> 4) & 0xF0) | ((h_mm >> 8) & 0x0F)) as u8;
    }

    /// A display descriptor (monitor name), which is not a timing.
    fn name_descriptor(b: &mut [u8; BLOCK_LEN], slot: usize) {
        let o = 54 + slot * 18;
        b[o] = 0;
        b[o + 1] = 0;
        b[o + 3] = 0xFC; // "display product name"
        b[o + 5..o + 18].copy_from_slice(b"MOEBIUS-32TV\n");
    }

    #[test]
    fn a_well_formed_block_is_accepted() {
        let b = edid(|b| timing(b, 0, 1920, 1080, 885, 497));
        assert!(block_valid(&b));
    }

    #[test]
    fn line_noise_is_refused_rather_than_decoded() {
        // A GOP that never filled its buffer: all zeros. The header alone
        // rejects it, before anything reads a size out of it.
        assert!(!block_valid(&[0u8; BLOCK_LEN]));
        assert_eq!(physical_size_mm(&[0u8; BLOCK_LEN]), None);
        assert_eq!(preferred_mode(&[0u8; BLOCK_LEN]), None);

        // A block that is one bit off on the DDC line: header intact,
        // checksum no longer agrees. This is the case that matters, because
        // the bytes look plausible and decode to a plausible lie.
        let mut b = edid(|b| timing(b, 0, 1920, 1080, 885, 497));
        b[60] ^= 0x04;
        assert!(!block_valid(&b), "a corrupt block passed the checksum");
        assert_eq!(physical_size_mm(&b), None);
        assert_eq!(preferred_mode(&b), None);

        // And a truncated read is not a block at all.
        let short = edid(|b| timing(b, 0, 1920, 1080, 885, 497));
        assert!(!block_valid(&short[..127]));
    }

    #[test]
    fn a_wrong_header_is_refused_even_with_a_valid_checksum() {
        let mut b = edid(|b| timing(b, 0, 1920, 1080, 885, 497));
        b[0] = 0x01;
        // Re-checksum so only the header is wrong.
        let sum = b[..BLOCK_LEN - 1]
            .iter()
            .fold(0u8, |s, x| s.wrapping_add(*x));
        b[BLOCK_LEN - 1] = sum.wrapping_neg();
        assert!(!block_valid(&b));
    }

    #[test]
    fn the_millimetre_size_wins_over_the_centimetre_one() {
        // The 32" TV: 885x497 mm in the detailed timing, and the coarse bytes
        // rounded to whole centimetres. Taking the coarse pair loses 5 mm on
        // each axis; taking neither loses 600 mm.
        let b = edid(|b| {
            b[21] = 88;
            b[22] = 49;
            timing(b, 0, 1920, 1080, 885, 497);
        });
        assert_eq!(physical_size_mm(&b), Some((885, 497)));
    }

    #[test]
    fn the_centimetre_size_is_used_when_no_timing_states_one() {
        let b = edid(|b| {
            b[21] = 60;
            b[22] = 34;
            // A timing that states no physical size at all, which is legal.
            timing(b, 0, 1920, 1080, 0, 0);
        });
        assert_eq!(physical_size_mm(&b), Some((600, 340)));
    }

    #[test]
    fn a_display_that_states_no_size_says_so_instead_of_guessing() {
        // Projectors and some TVs leave both out. The honest answer is None —
        // the caller then estimates once, rather than each caller inventing a
        // different number.
        let b = edid(|b| timing(b, 0, 1920, 1080, 0, 0));
        assert_eq!(physical_size_mm(&b), None);
    }

    #[test]
    fn half_a_size_is_no_size() {
        // A client told the screen is 600x0 mm computes an infinite DPI and
        // scales everything off the screen; one told nothing falls back to a
        // sane guess. So one axis missing has to discard the pair, in the
        // detailed timing...
        let b = edid(|b| timing(b, 0, 1920, 1080, 885, 0));
        assert_eq!(physical_size_mm(&b), None);
        let b = edid(|b| timing(b, 0, 1920, 1080, 0, 497));
        assert_eq!(physical_size_mm(&b), None);
        // ...and in the centimetre bytes.
        let b = edid(|b| {
            b[21] = 60;
            b[22] = 0;
        });
        assert_eq!(physical_size_mm(&b), None);
        let b = edid(|b| {
            b[21] = 0;
            b[22] = 34;
        });
        assert_eq!(physical_size_mm(&b), None);
    }

    #[test]
    fn a_size_is_found_in_a_later_descriptor_too() {
        // Slot 0 is often the monitor's name on a TV, with the timing after
        // it. Stopping at slot 0 would report no size for those.
        let b = edid(|b| {
            name_descriptor(b, 0);
            timing(b, 1, 3840, 2160, 1428, 804);
        });
        assert_eq!(physical_size_mm(&b), Some((1428, 804)));
        assert_eq!(preferred_mode(&b), Some((3840, 2160)));
    }

    #[test]
    fn a_later_timing_supplies_the_size_the_preferred_one_omits() {
        // Monitors list several detailed timings and do not always repeat the
        // physical size in every one. Stopping at the first timing then falls
        // all the way through to the centimetre bytes -- or, when those are
        // blank too, to a guess -- while the answer was sitting in the next
        // descriptor.
        let b = edid(|b| {
            timing(b, 0, 1920, 1080, 0, 0);
            timing(b, 1, 1280, 720, 597, 336);
        });
        assert_eq!(physical_size_mm(&b), Some((597, 336)));
        // The preferred mode is still the first one: only the size came from
        // further down.
        assert_eq!(preferred_mode(&b), Some((1920, 1080)));
    }

    #[test]
    fn sizes_use_all_twelve_bits_of_their_field() {
        // The upper nibbles live in one shared byte, so a display wider than
        // 255 mm or taller than 255 mm is where a wrong shift shows up. 1428
        // and 804 both need theirs; a 65" TV needs the full range.
        let b = edid(|b| timing(b, 0, 1920, 1080, 4095, 4095));
        assert_eq!(physical_size_mm(&b), Some((4095, 4095)));
        let b = edid(|b| timing(b, 0, 1920, 1080, 1428, 804));
        assert_eq!(physical_size_mm(&b), Some((1428, 804)));
        // And a size that fits in one byte must not pick up the other's bits.
        let b = edid(|b| timing(b, 0, 1920, 1080, 255, 1));
        assert_eq!(physical_size_mm(&b), Some((255, 1)));
    }

    #[test]
    fn the_preferred_mode_uses_all_twelve_bits_of_each_axis() {
        // 1080 and 2160 both exceed one byte, so the upper nibble is not
        // optional: dropping it reports 1920x56 for a 1080p panel.
        for (w, h) in [(1920u32, 1080u32), (3840, 2160), (1366, 768), (640, 480)] {
            let b = edid(|b| timing(b, 0, w, h, 600, 340));
            assert_eq!(preferred_mode(&b), Some((w, h)));
        }
    }

    #[test]
    fn a_block_with_no_detailed_timing_has_no_preferred_mode() {
        let b = edid(|b| {
            b[21] = 60;
            b[22] = 34;
            name_descriptor(b, 0);
        });
        assert_eq!(preferred_mode(&b), None);
        // The size is still readable from the coarse bytes.
        assert_eq!(physical_size_mm(&b), Some((600, 340)));
    }

    #[test]
    fn the_estimate_is_ninety_six_dpi_and_never_zero() {
        // 1920 px at 96 DPI is 20 inches, 508 mm.
        assert_eq!(estimated_size_mm(1920, 1080), (508, 285));
        // A tiny or zero mode must still produce a usable number: a client
        // told 0 mm divides by it.
        assert_eq!(estimated_size_mm(1, 1), (1, 1));
        assert_eq!(estimated_size_mm(0, 0), (1, 1));
    }

    #[test]
    fn a_real_monitors_block_decodes_whole() {
        // Assembled the way a monitor does: name in one descriptor, range
        // limits in another, the preferred timing first.
        let b = edid(|b| {
            b[21] = 60;
            b[22] = 34;
            timing(b, 0, 2560, 1440, 597, 336);
            name_descriptor(b, 1);
            let o = 54 + 2 * 18;
            b[o + 3] = 0xFD; // range limits: a display descriptor
        });
        assert!(block_valid(&b));
        assert_eq!(preferred_mode(&b), Some((2560, 1440)));
        assert_eq!(physical_size_mm(&b), Some((597, 336)));
        // Sanity: that is about 109 DPI, so the 96-DPI estimate would have
        // been ~14 % wrong in each axis.
        let (ew, _) = estimated_size_mm(2560, 1440);
        assert!(ew > 597, "the estimate should overshoot a dense panel");
    }

    #[test]
    fn every_descriptor_slot_is_read_and_none_past_the_block() {
        // Four slots, the last ending exactly at byte 125.
        let b = edid(|b| {
            for s in 0..3 {
                name_descriptor(b, s);
            }
            timing(b, 3, 1024, 768, 300, 230);
        });
        assert_eq!(physical_size_mm(&b), Some((300, 230)));
        assert_eq!(preferred_mode(&b), Some((1024, 768)));
        // A block that stops short of the fourth descriptor yields nothing
        // rather than reading past it.
        let mut short: Vec<u8> = b.to_vec();
        short.truncate(120);
        assert_eq!(physical_size_mm(&short), None);
    }
}
