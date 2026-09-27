//! Defensive ceilings against a buggy or hostile compositor.
//!
//! Every value a Wayland server hands this client (output count, configure
//! sizes, HiDPI scales) flows into allocation or render-loop math. These caps
//! bound the damage of an absurd value: the wallpaper skips the offending
//! output instead of allocating gigabytes, wrapping size arithmetic, or
//! pinning a core rendering a preposterous buffer.
//!
//! (Reconstructed: the original module was referenced by `mod fill_guard;`
//! but its file was never committed, leaving the crate unbuildable.)

/// Most `wl_output` globals tracked. A server announcing more is either
/// broken or adversarial; extra outputs are ignored with a log line.
pub const MAX_OUTPUTS: usize = 16;

/// Most simultaneous background surfaces (at most one per output).
pub const MAX_BACKGROUNDS: usize = 16;

/// Largest buffer side, in pixels, after applying the HiDPI scale. Covers a
/// 16K panel; anything past it is skipped (and the scale is walked back down
/// before giving up — see `build_frames`).
pub const MAX_BUFFER_DIM: u32 = 16384;

/// Largest buffer area this software renderer will paint (64 Mpx — an 8K
/// frame is ~33 Mpx). Past this the per-frame CPU cost stops being a
/// wallpaper and starts being a space heater; the output is skipped.
pub const MAX_BUFFER_PIXELS: usize = 64 * 1024 * 1024;

/// Why a buffer size was refused, so the caller can say which ceiling it hit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TooBig {
    /// A side is past [`MAX_BUFFER_DIM`].
    Dim,
    /// The area is past [`MAX_BUFFER_PIXELS`].
    Pixels,
}

/// Both buffer ceilings in one place. `w`/`h` are BUFFER pixels (logical size
/// already multiplied by the HiDPI scale).
///
/// Every caller that allocates or paints a buffer goes through this, so the
/// two ceilings cannot drift apart between the compositor path and `--dump`.
pub fn check_buffer(w: usize, h: usize) -> Result<(), TooBig> {
    if w > MAX_BUFFER_DIM as usize || h > MAX_BUFFER_DIM as usize {
        return Err(TooBig::Dim);
    }
    // The side ceiling has already bounded both factors by 16384, so this
    // product cannot wrap; `saturating_mul` is there for the day someone
    // reorders the two checks. See the test that pins the bound.
    if w.saturating_mul(h) > MAX_BUFFER_PIXELS {
        return Err(TooBig::Pixels);
    }
    Ok(())
}

/// Largest HiDPI scale in `1..=scale` whose buffer clears [`check_buffer`],
/// for a surface of `lw` x `lh` LOGICAL pixels.
///
/// The walk-down used to test [`MAX_BUFFER_DIM`] only, so an output whose
/// sides fit but whose AREA does not was skipped outright instead of being
/// painted at a lower scale: a 3840x2160 panel at scale 3 is 11520x6480 —
/// both sides under 16384, but 74 Mpx, past the 64 Mpx render ceiling — so
/// the wallpaper gave up on it, leaving that monitor black, when scale 2
/// (33 Mpx) would have painted it. Walking both ceilings is what this
/// module's `MAX_BUFFER_DIM` doc already promised ("the scale is walked back
/// down before giving up").
///
/// Returns 1 when even scale 1 does not fit; the caller still has to run
/// [`check_buffer`] on the result and skip the output if it fails, since no
/// scale can shrink a logical size that is itself too big.
pub fn fit_scale(lw: u32, lh: u32, scale: u32) -> u32 {
    let mut scale = scale.max(1);
    while scale > 1 {
        let w = (lw as usize).saturating_mul(scale as usize);
        let h = (lh as usize).saturating_mul(scale as usize);
        if check_buffer(w, h).is_ok() {
            break;
        }
        scale -= 1;
    }
    scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceilings_refuse_the_side_and_the_area_separately() {
        assert_eq!(check_buffer(1920, 1080), Ok(()));
        assert_eq!(check_buffer(MAX_BUFFER_DIM as usize, 1), Ok(()));
        assert_eq!(
            check_buffer(MAX_BUFFER_DIM as usize + 1, 1),
            Err(TooBig::Dim)
        );
        assert_eq!(
            check_buffer(1, MAX_BUFFER_DIM as usize + 1),
            Err(TooBig::Dim)
        );
        // 16384x16384 is 268 Mpx: sides legal, area not.
        assert_eq!(
            check_buffer(MAX_BUFFER_DIM as usize, MAX_BUFFER_DIM as usize),
            Err(TooBig::Pixels)
        );
        // Exactly at the area ceiling still passes: 8192x8192 == 64 Mpx.
        assert_eq!(check_buffer(8192, 8192), Ok(()));
        assert_eq!(check_buffer(8192, 8193), Err(TooBig::Pixels));
    }

    #[test]
    fn the_side_ceiling_is_what_keeps_the_area_from_wrapping() {
        // Why the area multiply cannot overflow once the side check has run:
        // both factors are at most MAX_BUFFER_DIM. If the side ceiling ever
        // grows past the square root of usize::MAX, or the two checks are
        // reordered, this is the line that has to be looked at.
        let d = MAX_BUFFER_DIM as usize;
        assert!(d.checked_mul(d).is_some());
        assert!(d.checked_mul(d).unwrap() > MAX_BUFFER_PIXELS);
    }

    #[test]
    fn a_size_that_would_wrap_the_area_is_refused_not_wrapped() {
        // usize::MAX/2 * 4 wraps in release builds; the ceiling must still
        // catch it (saturating_mul, and the Dim check fires first anyway).
        assert_eq!(check_buffer(usize::MAX, usize::MAX), Err(TooBig::Dim));
    }

    #[test]
    fn the_scale_walks_down_past_the_area_ceiling_not_only_the_side_one() {
        // The regression: 4K at scale 3 clears both sides but not the area.
        assert_eq!(fit_scale(3840, 2160, 3), 2);
        assert!(check_buffer(3840 * 2, 2160 * 2).is_ok());
        // 5K at scale 3 is 15360x8640: sides legal, 132 Mpx.
        assert_eq!(fit_scale(5120, 2880, 3), 2);
        // And the side ceiling it always honoured: 4K at scale 8 is 30720
        // wide, so it must come down at least to 4 (15360 <= 16384) and then
        // further still, because the area rules 4 and 3 out too.
        assert_eq!(fit_scale(3840, 2160, 8), 2);
    }

    #[test]
    fn a_scale_that_already_fits_is_left_alone() {
        assert_eq!(fit_scale(1920, 1080, 1), 1);
        assert_eq!(fit_scale(1920, 1080, 4), 4); // 7680x4320 == 33 Mpx
        assert_eq!(fit_scale(1280, 720, 8), 8); // 10240x5760 == 59 Mpx
    }

    #[test]
    fn the_walk_down_never_returns_zero_and_never_raises_the_scale() {
        // A logical size no scale can rescue still returns 1 (the caller
        // rejects it with check_buffer), never 0: a 0 scale would divide by
        // zero in `layout` and multiply the buffer size to nothing.
        assert_eq!(fit_scale(20000, 20000, 8), 1);
        assert_eq!(fit_scale(0, 0, 0), 1);
        for scale in 0..=8 {
            for (lw, lh) in [(1u32, 1u32), (1920, 1080), (3840, 2160), (16384, 16384)] {
                let got = fit_scale(lw, lh, scale);
                assert!(got >= 1, "fit_scale({lw},{lh},{scale}) = {got}");
                assert!(
                    got <= scale.max(1),
                    "fit_scale({lw},{lh},{scale}) = {got}, above what was asked"
                );
            }
        }
    }

    #[test]
    fn whatever_the_walk_down_returns_above_one_actually_fits() {
        for scale in 1..=8u32 {
            for (lw, lh) in [
                (640u32, 480u32),
                (1920, 1080),
                (2560, 1440),
                (3840, 2160),
                (5120, 2880),
                (7680, 4320),
            ] {
                let got = fit_scale(lw, lh, scale);
                if got > 1 {
                    let (w, h) = (lw as usize * got as usize, lh as usize * got as usize);
                    assert!(
                        check_buffer(w, h).is_ok(),
                        "fit_scale({lw},{lh},{scale}) = {got} gives {w}x{h}, which does not fit"
                    );
                }
            }
        }
    }
}
