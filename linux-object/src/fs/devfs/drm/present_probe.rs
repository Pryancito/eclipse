//! The present probe: per-band checksums of the window the kernel just
//! blitted, read before and after, and where the source was already black.
//! A child of `drm` with no logic change: the parent is at its size ceiling
//! (`xtask tamano`); the parent imports every item so `present_probe_tests`
//! keeps seeing them through `use super::*`.

/// Rows the probe samples: every 4th one, every pixel within it.
///
/// Along a row it reads every pixel, because a single changed pixel is a real
/// answer and a strided read could step over a one-pixel-wide comb. Across rows
/// it samples, because a torn frame is never one row -- a rasteriser hands over
/// tiles, so the smallest thing this is looking for is tens of rows tall.
pub(super) const PROBE_ROW_STEP: usize = 4;

/// Width in pixels of one probe band.
///
/// Both software rasterisers under this desktop hand over rectangular tiles 64
/// pixels wide, so a band this wide either *is* a tile column or is a whole
/// number of them. That is what makes the mask in the report worth printing: a
/// comb of stale tiles lights up a scattered *subset* of bands, while a plain
/// tear -- the compositor overwriting the frame it already handed over -- lights
/// up a contiguous run. A scalar checksum cannot tell those two apart, and they
/// do not have the same cause or the same fix.
pub(super) const PROBE_BAND_PX: usize = 64;

/// Bands the mask tracks: 32 * 64 = 2048 px, past both 1920- and 1600-wide
/// panels. A window wider than that folds its right-hand columns into the last
/// band instead of dropping them -- a mask that silently stopped describing part
/// of the window would be worse than a coarse last band, because the reader
/// cannot see which of the two they are looking at.
pub(super) const PROBE_MAX_BANDS: usize = 32;

/// FNV-1a's 64-bit basis and prime. Wrapping on purpose: this is a hash, and a
/// debug build must not panic on the overflow that is the whole mechanism.
pub(super) const PROBE_FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
pub(super) const PROBE_FNV_PRIME: u64 = 0x100_0000_01b3;

/// How many bands a window `w` pixels wide covers, never zero and never past
/// [`PROBE_MAX_BANDS`].
pub(super) fn probe_band_count(w: u32) -> usize {
    ((w as usize).saturating_add(PROBE_BAND_PX - 1) / PROBE_BAND_PX).clamp(1, PROBE_MAX_BANDS)
}

/// Where the source buffer is fully transparent black, gathered in the same
/// pass as the band checksums.
///
/// This exists for the one question a photograph of the screen cannot answer.
/// Black rectangles on the desktop have two completely different causes, and
/// they call for work in opposite places: either the kernel is failing to copy
/// pixels that the compositor did draw, or the compositor handed over a buffer
/// it never finished drawing and the kernel copied the zeros faithfully. The
/// band mask does not separate them -- a region that is black in BOTH reads
/// never differs, so it does not set a bit -- and neither does the cost line.
/// The count and the box do: if the black on screen sits where the source was
/// already zero, the kernel is exonerated and the search moves upstream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ZeroExtent {
    /// Sampled pixels that were exactly `0x0000_0000`.
    pub(super) zeros: usize,
    /// Sampled pixels in total, so `zeros` can be read as a fraction.
    pub(super) sampled: usize,
    /// Window-relative bounding box of the zero pixels. Meaningless while
    /// `zeros` is 0, which is why [`ZeroExtent::bbox`] is the only reader.
    pub(super) min_x: u32,
    pub(super) max_x: u32,
    pub(super) min_y: u32,
    pub(super) max_y: u32,
}

impl ZeroExtent {
    /// Nothing seen yet. The box starts inverted so the first `note` sets both
    /// ends of both axes without a special case.
    pub(super) fn empty() -> Self {
        Self {
            zeros: 0,
            sampled: 0,
            min_x: u32::MAX,
            max_x: 0,
            min_y: u32::MAX,
            max_y: 0,
        }
    }

    /// Record one zero pixel at window-relative `(x, y)`.
    pub(super) fn note(&mut self, x: u32, y: u32) {
        self.zeros = self.zeros.saturating_add(1);
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }

    /// `(x, y, w, h)` of the zero pixels, window-relative, or `None` when there
    /// were none. The box is inclusive of both ends, hence the `+ 1`: a single
    /// zero pixel is a 1x1 box, not a 0x0 one.
    pub(super) fn bbox(&self) -> Option<(u32, u32, u32, u32)> {
        (self.zeros > 0).then(|| {
            (
                self.min_x,
                self.min_y,
                self.max_x.saturating_sub(self.min_x).saturating_add(1),
                self.max_y.saturating_sub(self.min_y).saturating_add(1),
            )
        })
    }
}

/// One probe read: a checksum per 64-pixel column band of the window.
pub(super) struct ProbeBands {
    /// Bands past `n` are never touched, so they hold [`PROBE_FNV_BASIS`] in
    /// every read and compare equal.
    pub(super) bands: [u64; PROBE_MAX_BANDS],
    /// Bands the window actually covered.
    pub(super) n: usize,
    /// Where this read found the source already black. See [`ZeroExtent`].
    pub(super) zero: ZeroExtent,
}

impl ProbeBands {
    /// One number for the whole window, which is all the changed/unchanged
    /// decision needs.
    ///
    /// `n` is deliberately NOT folded in. It looks like it should be -- two reads
    /// that disagreed about the window's width must not come out equal -- but the
    /// band array already separates them: band `n` holds [`PROBE_FNV_BASIS`] in
    /// the narrower read and a real hash of its pixels in the wider one, and
    /// those differ even when every pixel is black. Folding `n` as well was a
    /// mutation that could not be killed by anything the probe can actually do
    /// (the two reads bracketing a blit are always the same width), so it is
    /// gone, and the property it was standing in for is pinned on the bands
    /// where it really lives.
    pub(super) fn fold(&self) -> u64 {
        let mut acc = PROBE_FNV_BASIS;
        for b in self.bands.iter() {
            acc ^= *b;
            acc = acc.wrapping_mul(PROBE_FNV_PRIME);
        }
        acc
    }

    /// Bit `i` set means band `i` -- source columns `x + 64*i ..= x + 64*i + 63`
    /// -- reads differently in the two passes.
    pub(super) fn diff_mask(&self, other: &ProbeBands) -> u32 {
        let mut mask = 0u32;
        for i in 0..PROBE_MAX_BANDS {
            if self.bands[i] != other.bands[i] {
                mask |= 1u32 << i;
            }
        }
        mask
    }
}

/// Per-band order-dependent checksums of the `(x, y, w, h)` window of `pixels`,
/// sampling every `row_step`-th row and every pixel within it.
///
/// Order-dependent matters: two frames can hold the same pixels in different
/// places (a scrolled list, a window dragged by a pixel) and a sum or an XOR
/// would call those equal. FNV-1a gives that for nothing -- each pixel is
/// multiplied into the accumulator in turn, so the sequence is what is hashed,
/// not the multiset. This used to fold the row and column in as well, which was
/// two extra multiplies per pixel on a per-frame path buying nothing the chain
/// did not already provide; the mutation that removed it could not be killed,
/// and the honest resolution was to remove it here too.
///
/// `stride_px` of 0, an empty window, or a window whose first sampled row is
/// already past the end of `pixels` all give `None`: there is nothing to
/// compare, which is not the same answer as "these two match".
pub(super) fn probe_bands(
    pixels: &[u32],
    stride_px: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    row_step: usize,
) -> Option<ProbeBands> {
    if stride_px == 0 || w == 0 || h == 0 || row_step == 0 {
        return None;
    }
    let n = probe_band_count(w);
    let mut bands = [PROBE_FNV_BASIS; PROBE_MAX_BANDS];
    let mut zero = ZeroExtent::empty();
    // Rows, not pixels -- `zero.sampled` beside it counts pixels, and the two
    // being one word apart with the same name is how a later reader divides by
    // the wrong thing.
    let mut sampled_rows = 0usize;
    let mut r = 0usize;
    while r < h as usize {
        let off = (y as usize)
            .saturating_add(r)
            .saturating_mul(stride_px)
            .saturating_add(x as usize);
        let end = off.saturating_add(w as usize);
        if end > pixels.len() {
            break;
        }
        for (c, px) in pixels[off..end].iter().enumerate() {
            // `min(n - 1)`: the columns past the mask's reach fold into the last
            // band rather than running off the array.
            let b = (c / PROBE_BAND_PX).min(n - 1);
            bands[b] ^= *px as u64;
            bands[b] = bands[b].wrapping_mul(PROBE_FNV_PRIME);
            if *px == 0 {
                // Window-relative, and `r` not `y + r`, because the window's own
                // origin is already in the reported line: a box relative to the
                // window is what lines up with what the eye sees on screen.
                zero.note(c as u32, r as u32);
            }
        }
        zero.sampled = zero.sampled.saturating_add(end.saturating_sub(off));
        sampled_rows += 1;
        r += row_step;
    }
    (sampled_rows > 0).then_some(ProbeBands { bands, n, zero })
}

/// Whether the two reads bracketing a blit say somebody else was writing the
/// window while the kernel copied it.
///
/// `after` is an `Option` because the second read can fail to produce an answer
/// at all -- the framebuffer shrank, the mapping went away -- and that counts as
/// changed. "I could not tell" must never come out as "the compositor is in the
/// clear": this whole flag exists to decide who is responsible, and a wrong
/// acquittal sends the search back to the code that has already been walked.
pub(super) fn probe_says_changed(before: u64, after: Option<u64>) -> bool {
    after != Some(before)
}
