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

/// One 18-byte detailed timing descriptor, decoded whole.
///
/// The monitor states its native mode here to the pixel and the kilohertz, and
/// until now only the two active counts were read: everything downstream then
/// advertised a **nominal 60 Hz** mode, because that was the only refresh the
/// kernel could name. On a 60 Hz panel that is right by accident; on a 75, 120
/// or 144 Hz one the kernel tells the compositor 60, the compositor paces its
/// repaints and its `WAIT_VBLANK` sleeps to 16.7 ms, and better than half of
/// the panel's scanouts show a frame that was already on screen.
///
/// Field names and the bit arithmetic follow Linux's `drm_mode_detailed`, so a
/// mode decoded here is the same mode `libdrm` would report for that monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetailedTiming {
    /// Pixel clock in kHz. EDID stores it in units of 10 kHz, and 0 is the
    /// marker for "this is a display descriptor, not a timing".
    pub clock_khz: u32,
    pub hdisplay: u32,
    pub hsync_start: u32,
    pub hsync_end: u32,
    pub htotal: u32,
    pub vdisplay: u32,
    pub vsync_start: u32,
    pub vsync_end: u32,
    pub vtotal: u32,
    /// Whether the panel scans two interleaved fields per frame. The vertical
    /// numbers above are already the whole frame's (the descriptor states them
    /// per field and the decoder doubles them, as Linux does), and the refresh
    /// below is the **field** rate, which is what "1080i60" names.
    pub interlaced: bool,
    /// Whether the descriptor states digital separate sync. The two polarities
    /// below mean nothing unless it does, exactly as in Linux: a descriptor
    /// using analogue or composite sync leaves them unstated.
    pub separate_sync: bool,
    pub hsync_positive: bool,
    pub vsync_positive: bool,
}

impl DetailedTiming {
    /// Whether this is a mode a CRTC could be asked to drive, by the same
    /// rules as Linux's `drm_mode_validate_basic`.
    ///
    /// Not a formality: a mode whose sync pulse ends past its total is
    /// `MODE_H_ILLEGAL`, and wlroots that is handed one drops the output
    /// rather than picking another. So a monitor whose descriptor is line
    /// noise has to be refused here, where the answer is "fall back to the
    /// nominal mode", instead of downstream where it is "no desktop".
    ///
    /// The comparisons are non-strict, like Linux's: a real panel is allowed
    /// a zero front porch or no back porch at all, and refusing those would
    /// throw away a perfectly good timing to go back to guessing.
    pub fn is_valid(&self) -> bool {
        self.clock_khz > 0
            && self.hdisplay > 0
            && self.hsync_start >= self.hdisplay
            && self.hsync_end >= self.hsync_start
            && self.htotal >= self.hsync_end
            && self.vdisplay > 0
            && self.vsync_start >= self.vdisplay
            && self.vsync_end >= self.vsync_start
            && self.vtotal >= self.vsync_end
    }

    /// Refresh rate in millihertz.
    ///
    /// Millihertz because that is the unit wlroots and Mesa work in, and
    /// because a pixel clock is stored in whole kHz and so cannot express an
    /// exact 60.000 Hz for most timings. Rounds to nearest, like Linux's
    /// `DIV_ROUND_CLOSEST`: truncating here is what once turned 59.9995 Hz
    /// into "59" and paced every frame 1.7 % slow.
    ///
    /// 0 when the timing is unusable, which is the caller's cue to fall back.
    pub fn refresh_mhz(&self) -> u32 {
        let den = self.htotal as u64 * self.vtotal as u64;
        if den == 0 {
            return 0;
        }
        // An interlaced mode scans a field per vertical period, so it refreshes
        // twice per frame -- the same doubling `drm_mode_vrefresh` applies.
        let num = self.clock_khz as u64 * 1_000_000 * if self.interlaced { 2 } else { 1 };
        // `min` rather than a cast: a monitor is free to state a 655 MHz clock,
        // and a wrapping cast would turn a fast panel into a slow one.
        ((num + den / 2) / den).min(u32::MAX as u64) as u32
    }

    /// Refresh rate in whole hertz, rounded to nearest, which is what the
    /// `vrefresh` field of a `drm_mode_modeinfo` carries.
    pub fn refresh_hz(&self) -> u32 {
        let den = self.htotal as u64 * self.vtotal as u64;
        if den == 0 {
            return 0;
        }
        let num = self.clock_khz as u64 * 1_000 * if self.interlaced { 2 } else { 1 };
        // `min` rather than a cast: a monitor is free to state a 655 MHz clock,
        // and a wrapping cast would turn a fast panel into a slow one.
        ((num + den / 2) / den).min(u32::MAX as u64) as u32
    }
}

/// Decode one 18-byte detailed timing descriptor.
///
/// Every field but the two active counts spills its high bits into a shared
/// byte further down the descriptor, which is the whole difficulty: a wrong
/// shift produces a plausible number rather than an obvious one.
fn decode_timing(d: &[u8]) -> Option<DetailedTiming> {
    if d.len() < 18 {
        return None;
    }
    let clock_khz = u16::from_le_bytes([d[0], d[1]]) as u32 * 10;
    // Byte 4 shares the high nibbles of hactive (7:4) and hblank (3:0);
    // byte 7 does the same for the vertical pair.
    let hactive = d[2] as u32 | ((d[4] as u32 & 0xF0) << 4);
    let hblank = d[3] as u32 | ((d[4] as u32 & 0x0F) << 8);
    let vactive = d[5] as u32 | ((d[7] as u32 & 0xF0) << 4);
    let vblank = d[6] as u32 | ((d[7] as u32 & 0x0F) << 8);
    // Byte 11 holds two high bits for each of the four sync numbers, in the
    // order hsync offset (7:6), hsync width (5:4), vsync offset (3:2), vsync
    // width (1:0); the vertical pair's low bits share nibbles of byte 10.
    let hsync_offset = d[8] as u32 | ((d[11] as u32 & 0xC0) << 2);
    let hsync_width = d[9] as u32 | ((d[11] as u32 & 0x30) << 4);
    let vsync_offset = (d[10] as u32 >> 4) | ((d[11] as u32 & 0x0C) << 2);
    let vsync_width = (d[10] as u32 & 0x0F) | ((d[11] as u32 & 0x03) << 4);
    let misc = d[17];
    let interlaced = misc & 0x80 != 0;
    // An interlaced descriptor counts lines PER FIELD, so the mode it describes
    // has twice as many. Linux doubles all four vertical numbers here and sets
    // the low bit of the total (an interlaced frame has an odd line count, the
    // half-line that makes the fields alternate), and a mode that did not would
    // report 1920x540 for 1080i and a 30 Hz refresh for a 60-field panel.
    let (vdisplay, vsync_start, vsync_end, vtotal) = {
        let (d0, s0, e0, t0) = (
            vactive,
            vactive + vsync_offset,
            vactive + vsync_offset + vsync_width,
            vactive + vblank,
        );
        if interlaced {
            (d0 * 2, s0 * 2, e0 * 2, (t0 * 2) | 1)
        } else {
            (d0, s0, e0, t0)
        }
    };
    Some(DetailedTiming {
        clock_khz,
        hdisplay: hactive,
        hsync_start: hactive + hsync_offset,
        hsync_end: hactive + hsync_offset + hsync_width,
        htotal: hactive + hblank,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        interlaced,
        // Bits 4:3 are the sync scheme; 0b11 is digital separate, the only one
        // that states a polarity per axis.
        separate_sync: (misc >> 3) & 0x3 == 0x3,
        hsync_positive: misc & 0x02 != 0,
        vsync_positive: misc & 0x04 != 0,
    })
}

/// The panel's native timing: the first detailed timing, whole, and only if it
/// describes a mode a CRTC could drive.
///
/// The first descriptor is the preferred timing by definition, so this is the
/// mode the panel actually has and the one firmware will have programmed.
pub fn preferred_timing(block: &[u8]) -> Option<DetailedTiming> {
    if !block_valid(block) {
        return None;
    }
    let t = decode_timing(detailed_timings(block).next()?)?;
    t.is_valid().then_some(t)
}

/// The display's preferred (native) resolution in pixels.
///
/// The first descriptor is the preferred timing by definition, so this is what
/// the panel actually has; anything else is scaled by the monitor. Looser than
/// [`preferred_timing`] on purpose: a resolution is still worth reporting from
/// a descriptor whose porches are nonsense, where a *mode* would not be.
pub fn preferred_mode(block: &[u8]) -> Option<(u32, u32)> {
    if !block_valid(block) {
        return None;
    }
    let t = decode_timing(detailed_timings(block).next()?)?;
    (t.hdisplay > 0 && t.vdisplay > 0).then_some((t.hdisplay, t.vdisplay))
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

    /// A whole detailed timing descriptor: the pixel clock, both actives, both
    /// blankings and all four sync numbers, each spilling its high bits into
    /// the shared byte the way a monitor writes them.
    #[allow(clippy::too_many_arguments)]
    fn full_timing(
        b: &mut [u8; BLOCK_LEN],
        slot: usize,
        clock_khz: u32,
        (hactive, hblank, hs_off, hs_w): (u32, u32, u32, u32),
        (vactive, vblank, vs_off, vs_w): (u32, u32, u32, u32),
        misc: u8,
    ) {
        let o = 54 + slot * 18;
        let ten_khz = (clock_khz / 10) as u16;
        b[o..o + 2].copy_from_slice(&ten_khz.to_le_bytes());
        b[o + 2] = (hactive & 0xFF) as u8;
        b[o + 3] = (hblank & 0xFF) as u8;
        b[o + 4] = (((hactive >> 4) & 0xF0) | ((hblank >> 8) & 0x0F)) as u8;
        b[o + 5] = (vactive & 0xFF) as u8;
        b[o + 6] = (vblank & 0xFF) as u8;
        b[o + 7] = (((vactive >> 4) & 0xF0) | ((vblank >> 8) & 0x0F)) as u8;
        b[o + 8] = (hs_off & 0xFF) as u8;
        b[o + 9] = (hs_w & 0xFF) as u8;
        b[o + 10] = (((vs_off & 0x0F) << 4) | (vs_w & 0x0F)) as u8;
        b[o + 11] = ((((hs_off >> 8) & 0x3) << 6)
            | (((hs_w >> 8) & 0x3) << 4)
            | (((vs_off >> 4) & 0x3) << 2)
            | ((vs_w >> 4) & 0x3)) as u8;
        b[o + 17] = misc;
    }

    /// Digital separate sync, negative h, positive v: what almost every
    /// modern monitor states.
    const SEPARATE_NH_PV: u8 = 0b0001_1100;

    /// `1920x1080@60`, the DMT timing, to the number. 148500 kHz over a
    /// 2200x1125 total is exactly 60.000 Hz, which makes it the one mode where
    /// a rounding mistake cannot hide.
    fn dmt_1080p60(b: &mut [u8; BLOCK_LEN], slot: usize) {
        full_timing(
            b,
            slot,
            148_500,
            (1920, 280, 88, 44),
            (1080, 45, 4, 5),
            SEPARATE_NH_PV,
        );
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

    #[test]
    fn a_panels_own_timing_decodes_to_the_mode_libdrm_would_report() {
        // Every field of the DMT 1080p60 timing, so a wrong shift anywhere
        // shows up as one wrong number rather than a plausible mode.
        let b = edid(|b| dmt_1080p60(b, 0));
        let t = preferred_timing(&b).expect("a valid timing was refused");
        assert_eq!(t.clock_khz, 148_500, "the clock is 10 kHz units, not kHz");
        assert_eq!((t.hdisplay, t.vdisplay), (1920, 1080));
        assert_eq!((t.hsync_start, t.hsync_end, t.htotal), (2008, 2052, 2200));
        assert_eq!((t.vsync_start, t.vsync_end, t.vtotal), (1084, 1089, 1125));
        assert!(!t.interlaced);
        assert!(t.separate_sync);
        assert!(!t.hsync_positive && t.vsync_positive);
        // And the whole point of decoding it: the refresh, exact.
        assert_eq!(t.refresh_mhz(), 60_000);
        assert_eq!(t.refresh_hz(), 60);
    }

    #[test]
    fn a_hundred_and_forty_four_hertz_panel_is_not_reported_as_sixty() {
        // The reason this module grew a timing decoder. Same 1080p geometry,
        // 2.4x the pixel clock: a kernel that reads only the two active counts
        // has no way to tell these two monitors apart, and calls both 60 Hz.
        let sixty = edid(|b| dmt_1080p60(b, 0));
        let fast = edid(|b| {
            full_timing(
                b,
                0,
                356_400, // 144 * 2200 * 1125 / 1000, exactly
                (1920, 280, 88, 44),
                (1080, 45, 4, 5),
                SEPARATE_NH_PV,
            )
        });
        // Indistinguishable by resolution...
        assert_eq!(preferred_mode(&sixty), preferred_mode(&fast));
        // ...and not by refresh.
        assert_eq!(preferred_timing(&sixty).unwrap().refresh_hz(), 60);
        assert_eq!(preferred_timing(&fast).unwrap().refresh_hz(), 144);
        assert_eq!(preferred_timing(&fast).unwrap().refresh_mhz(), 144_000);
    }

    #[test]
    fn a_refresh_the_pixel_clock_cannot_express_rounds_to_nearest() {
        // 75 Hz at 2200x1125 wants 185625 kHz, and EDID stores the clock in
        // whole 10 kHz units, so the closest a monitor can state is 185620 --
        // 74.9979 Hz. Truncating calls that 74, and the synthetic vblank then
        // paces every frame 1.3 % slow. This is the 59.9995 -> 59 bug in
        // `refresh_hz_from_modeinfo`, one module upstream of it.
        let b = edid(|b| {
            full_timing(
                b,
                0,
                185_620,
                (1920, 280, 88, 44),
                (1080, 45, 4, 5),
                SEPARATE_NH_PV,
            )
        });
        let t = preferred_timing(&b).unwrap();
        assert_eq!(t.refresh_mhz(), 74_998);
        assert_eq!(t.refresh_hz(), 75, "a truncating divide would say 74");
    }

    #[test]
    fn an_interlaced_descriptor_counts_lines_per_field_and_the_mode_per_frame() {
        // 1080i60: the descriptor says 540 lines and 74.25 MHz. The mode has
        // 1080, the total is odd, and the refresh is the FIELD rate -- 60,
        // which is what "1080i60" names. Reporting the descriptor as it stands
        // gives a 1920x540 panel at 30 Hz: wrong size and wrong pace.
        let b = edid(|b| {
            full_timing(
                b,
                0,
                74_250,
                (1920, 280, 88, 44),
                (540, 22, 2, 5),
                SEPARATE_NH_PV | 0x80,
            )
        });
        let t = preferred_timing(&b).unwrap();
        assert!(t.interlaced);
        assert_eq!(t.vdisplay, 1080, "the frame is both fields");
        assert_eq!(t.vtotal, 1125, "562 doubled, plus the odd half-line");
        assert_eq!(t.vtotal % 2, 1);
        assert_eq!((t.vsync_start, t.vsync_end), (1084, 1094));
        assert_eq!(t.refresh_hz(), 60);
        assert_eq!(t.refresh_mhz(), 60_000);
        // The progressive mode of the same geometry has half the clock-per-line
        // and must NOT be doubled.
        let p = edid(|b| dmt_1080p60(b, 0));
        assert_eq!(preferred_timing(&p).unwrap().vtotal, 1125);
        assert!(!preferred_timing(&p).unwrap().interlaced);
    }

    #[test]
    fn each_sync_number_reads_its_own_two_high_bits_and_not_a_neighbours() {
        // Byte 11 packs four 2-bit fields, in the order hsync offset, hsync
        // width, vsync offset, vsync width. Getting the order wrong swaps a
        // front porch for a pulse width and still yields a mode that looks
        // plausible, so each field is given a value only it can produce.
        let b = edid(|b| {
            full_timing(
                b,
                0,
                148_500,
                (1920, 999, 0x100 | 1, 0x200 | 2), // hs off 257, hs width 514
                (1080, 99, 0x10 | 3, 0x20 | 4),    // vs off 19, vs width 36
                SEPARATE_NH_PV,
            )
        });
        let t = preferred_timing(&b).unwrap();
        assert_eq!(t.htotal, 1920 + 999);
        assert_eq!(t.hsync_start, 1920 + 257);
        assert_eq!(t.hsync_end, 1920 + 257 + 514);
        assert_eq!(t.vtotal, 1080 + 99);
        assert_eq!(t.vsync_start, 1080 + 19);
        assert_eq!(t.vsync_end, 1080 + 19 + 36);
    }

    #[test]
    fn a_sync_polarity_is_only_claimed_when_the_descriptor_states_one() {
        // Bits 4:3 are the sync scheme and only 0b11 is digital separate; a
        // descriptor using analogue or composite sync says nothing about
        // polarity, and reading bits 2:1 anyway invents an answer. Linux's
        // own test here is `& (3 << 3)`, which is truthy for ONE of the two
        // bits -- so 0b01 and 0b10 are where the two disagree.
        for (scheme, separate) in [(0b00u8, false), (0b01, false), (0b10, false), (0b11, true)] {
            let misc = (scheme << 3) | 0b110; // both polarities positive
            let b =
                edid(|b| full_timing(b, 0, 148_500, (1920, 280, 88, 44), (1080, 45, 4, 5), misc));
            let t = preferred_timing(&b).unwrap();
            assert_eq!(t.separate_sync, separate, "scheme {scheme:#b}");
        }
        // And both polarities are read from their own bit.
        for (bits, hp, vp) in [
            (0b000u8, false, false),
            (0b010, true, false),
            (0b100, false, true),
            (0b110, true, true),
        ] {
            let b = edid(|b| {
                full_timing(
                    b,
                    0,
                    148_500,
                    (1920, 280, 88, 44),
                    (1080, 45, 4, 5),
                    0b0001_1000 | bits,
                )
            });
            let t = preferred_timing(&b).unwrap();
            assert_eq!((t.hsync_positive, t.vsync_positive), (hp, vp));
        }
    }

    #[test]
    fn a_mode_whose_sync_runs_past_its_total_is_refused_not_advertised() {
        // The case this validity check exists for. A sync pulse ending past
        // the total is MODE_H_ILLEGAL, and wlroots handed one drops the output
        // instead of picking another mode -- the desktop falls back to the
        // text console. Refusing it here costs the real refresh and nothing
        // else, because the caller falls back to its nominal mode.
        let past_htotal = edid(|b| {
            // 300 of blanking, but a 200+200 sync inside it.
            full_timing(
                b,
                0,
                148_500,
                (1920, 300, 200, 200),
                (1080, 45, 4, 5),
                SEPARATE_NH_PV,
            )
        });
        assert!(block_valid(&past_htotal));
        assert_eq!(preferred_timing(&past_htotal), None);
        // The resolution is still readable: that much of the descriptor is
        // sound, and a caller that only wants the size should still get it.
        assert_eq!(preferred_mode(&past_htotal), Some((1920, 1080)));

        let past_vtotal = edid(|b| {
            full_timing(
                b,
                0,
                148_500,
                (1920, 280, 88, 44),
                (1080, 10, 8, 8),
                SEPARATE_NH_PV,
            )
        });
        assert_eq!(preferred_timing(&past_vtotal), None);

        // A zero pixel clock is not a timing at all -- it is the marker for a
        // display descriptor -- so there is no preferred timing to find.
        let no_clock = edid(|b| {
            dmt_1080p60(b, 0);
            b[54] = 0;
            b[55] = 0;
        });
        assert_eq!(preferred_timing(&no_clock), None);
    }

    #[test]
    fn a_zero_front_porch_is_a_mode_and_not_an_error() {
        // Reduced-blanking timings put the sync right at the end of the active
        // area, and a CVT-RB panel with no back porch at all is legal by
        // Linux's own rule. Being stricter than Linux here throws away a real
        // panel's real refresh to go back to guessing 60.
        let b = edid(|b| {
            full_timing(
                b,
                0,
                148_500,
                (1920, 80, 0, 80), // sync starts at hdisplay, ends at htotal
                (1080, 45, 0, 45),
                0b0001_1010, // CVT-RB: +hsync (bit 1), -vsync (bit 2 clear)
            )
        });
        let t = preferred_timing(&b).expect("a zero-porch mode was refused");
        assert_eq!(
            (t.hdisplay, t.hsync_start, t.hsync_end, t.htotal),
            (1920, 1920, 2000, 2000)
        );
        assert!(t.hsync_positive && !t.vsync_positive);
        assert!(t.refresh_hz() > 0);
    }

    #[test]
    fn a_corrupt_block_has_no_timing_however_good_the_descriptor_looks() {
        // The checksum gate is the same one the size and the mode go through:
        // a timing decoded out of line noise is a pixel clock invented for a
        // monitor that may not even be plugged in.
        let mut b = edid(|b| dmt_1080p60(b, 0));
        b[60] ^= 0x04;
        assert!(!block_valid(&b));
        assert_eq!(preferred_timing(&b), None);
        assert_eq!(preferred_timing(&[0u8; BLOCK_LEN]), None);
        assert_eq!(preferred_timing(&[]), None);
    }

    #[test]
    fn the_preferred_timing_is_the_first_descriptor_even_with_faster_ones_after() {
        // Monitors list their native mode first and alternates after it,
        // often at higher refresh for a lower resolution. Picking the fastest,
        // or the last, drives the panel at a mode it is not scanning.
        let b = edid(|b| {
            dmt_1080p60(b, 0);
            full_timing(
                b,
                1,
                356_400,
                (1280, 200, 48, 32),
                (720, 30, 3, 5),
                SEPARATE_NH_PV,
            );
        });
        let t = preferred_timing(&b).unwrap();
        assert_eq!((t.hdisplay, t.vdisplay), (1920, 1080));
        assert_eq!(t.refresh_hz(), 60);
    }

    #[test]
    fn a_name_before_the_timing_does_not_become_the_preferred_mode() {
        // A display descriptor has a zero pixel clock, which is exactly how it
        // is told apart from a timing; a decoder that took descriptor 0 on
        // faith would read the monitor's name as a pixel clock and porches.
        let b = edid(|b| {
            name_descriptor(b, 0);
            dmt_1080p60(b, 1);
        });
        let t = preferred_timing(&b).unwrap();
        assert_eq!((t.hdisplay, t.vdisplay), (1920, 1080));
        assert_eq!(t.refresh_hz(), 60);
    }

    #[test]
    fn the_refresh_of_an_impossible_timing_is_zero_and_not_a_panic() {
        // `refresh_*` are public and take a `DetailedTiming` the caller may
        // have built itself, so they cannot assume `is_valid`. A zero total
        // is a divide by zero, and a clock of 0xFFFF*10 kHz over a 1x1 total
        // overflows a u32 of millihertz.
        let zero = DetailedTiming {
            clock_khz: 148_500,
            hdisplay: 1920,
            hsync_start: 1920,
            hsync_end: 1920,
            htotal: 0,
            vdisplay: 1080,
            vsync_start: 1080,
            vsync_end: 1080,
            vtotal: 0,
            interlaced: false,
            separate_sync: true,
            hsync_positive: false,
            vsync_positive: true,
        };
        assert!(!zero.is_valid());
        assert_eq!(zero.refresh_mhz(), 0);
        assert_eq!(zero.refresh_hz(), 0);
        let huge = DetailedTiming {
            clock_khz: u32::MAX,
            htotal: 1,
            vtotal: 1,
            ..zero
        };
        assert_eq!(
            huge.refresh_mhz(),
            u32::MAX,
            "saturates instead of wrapping"
        );
    }
}
