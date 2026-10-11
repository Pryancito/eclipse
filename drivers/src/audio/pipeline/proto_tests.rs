//! What the generated SRC prototype table must satisfy.
//!
//! `proto.rs` is machine-written (`tools/audio-src-coef/generate.py`) and is
//! the one file in the pipeline nobody reads: 4096 numbers. The resampler's
//! own tests in `src.rs` check that it resamples, which a table with the
//! wrong GAIN still does -- every sample comes out quietly scaled, and the
//! only symptom is that playback through a converted rate is a little softer
//! than playback at 48 kHz. So the table's invariants are checked here,
//! separately from the file, which may be regenerated at any time.
//!
//! These tests live in their own file on purpose: the generator prints the
//! whole of `proto.rs`, so a `#[cfg(test)] mod` appended there would be
//! deleted the next time anyone runs it.

use super::proto::{SRC_COEF_Q, SRC_HALF_TAPS, SRC_PHASES, SRC_PROTO};

/// One unit of the fixed-point format the coefficients are in.
const ONE: i64 = 1 << SRC_COEF_Q;

/// The DC gain of the filter at phase `p`: the right wing's taps plus the
/// mirrored left wing's, which is how the resampler walks the prototype
/// (`coef()` in `src.rs` indexes it continuously, the left wing being the
/// mirror of the right).
fn dc_gain(p: usize) -> i64 {
    let n = SRC_PROTO.len();
    let mut total = 0i64;
    for tap in 0..SRC_HALF_TAPS {
        let right = tap * SRC_PHASES + p;
        let left = if p == 0 {
            (tap + 1) * SRC_PHASES
        } else {
            tap * SRC_PHASES + (SRC_PHASES - p)
        };
        if right < n {
            total += SRC_PROTO[right] as i64;
        }
        if left > 0 && left < n {
            total += SRC_PROTO[left] as i64;
        }
    }
    total
}

/// The table is indexed as `phase + tap * SRC_PHASES`, so its length is the
/// product and nothing else: a truncated or doubled paste reads another tap's
/// coefficient (or past the end) at every phase.
#[test]
fn the_table_is_exactly_one_coefficient_per_phase_and_tap() {
    assert_eq!(SRC_PROTO.len(), SRC_PHASES * SRC_HALF_TAPS);
    assert_eq!(SRC_PROTO.len(), 4096);
}

/// EVERY phase must have unity gain, not just on average. A phase that is
/// quieter than its neighbours is a per-output-sample gain wobble, which is
/// audible as distortion rather than as low volume -- and the generator
/// normalises by the MEAN, so a bad window shape hides inside a correct mean.
#[test]
fn every_phase_has_unity_dc_gain() {
    // 1e-4 of full scale. The table as generated is inside 1.1e-5; the margin
    // is for a re-generation at a different rounding, not for a new window.
    let tolerance = ONE / 10_000;
    for p in 0..SRC_PHASES {
        let g = dc_gain(p);
        assert!(
            (g - ONE).abs() <= tolerance,
            "phase {} has gain {} ({:.6} of unity), outside +-1e-4",
            p,
            g,
            g as f64 / ONE as f64
        );
    }
}

/// And the mean gain is unity to the count, because that is exactly what the
/// generator divides by. A mean that drifted would mean the normalisation
/// step did not run.
#[test]
fn the_mean_gain_is_unity_to_within_a_rounding_error() {
    let total: i64 = (0..SRC_PHASES).map(dc_gain).sum();
    let mean = total / SRC_PHASES as i64;
    assert!(
        (mean - ONE).abs() <= 16,
        "the mean gain is {} against {}",
        mean,
        ONE
    );
}

/// The prototype is a low-pass centred on tap 0, so its peak is the FIRST
/// coefficient and the main lobe falls away monotonically from it. A table
/// written out of order, or one with the wings swapped, breaks this while
/// keeping every other property checked here.
#[test]
fn the_peak_is_the_first_coefficient_and_the_main_lobe_only_falls() {
    let peak = SRC_PROTO[0];
    assert_eq!(
        peak,
        *SRC_PROTO.iter().max().unwrap(),
        "the peak is not at tap 0"
    );
    assert!(peak > 0);
    for i in 0..SRC_PHASES - 1 {
        assert!(
            SRC_PROTO[i] > SRC_PROTO[i + 1],
            "the main lobe rises between {} and {}",
            i,
            i + 1
        );
    }
}

/// The peak is the cutoff as a fraction of unity: 0.92 of input Nyquist, as
/// the file's own comment says. This is the one number that says the window
/// was generated with the parameters it claims.
#[test]
fn the_peak_is_the_declared_cutoff() {
    let ratio = SRC_PROTO[0] as f64 / ONE as f64;
    assert!(
        (0.915..=0.925).contains(&ratio),
        "the peak is {:.4} of unity, not the declared 0.92 cutoff",
        ratio
    );
}

/// The far tail is the stopband, and it must be deep: a tail that is not
/// small is aliasing folded back into the audible band. The generated table
/// is at 3.5e-5 of the peak; anything above 1e-4 is a different window.
#[test]
fn the_stopband_tail_is_deep() {
    let peak = SRC_PROTO[0] as i64;
    let tail = &SRC_PROTO[SRC_PROTO.len() - SRC_PHASES..];
    let worst = tail.iter().map(|c| (*c as i64).abs()).max().unwrap();
    assert!(
        worst * 10_000 < peak,
        "the last tap's wing reaches {}, which is {:.6} of the peak",
        worst,
        worst as f64 / peak as f64
    );
}

/// Every coefficient must fit the `i32` the table is declared as (the
/// generator clamps, so a saturated entry is a silently wrong filter), and
/// the whole filter applied to full-scale `i16` input must not come near
/// overflowing the `i64` accumulator the resampler sums into.
#[test]
fn the_accumulator_cannot_overflow_on_full_scale_input() {
    let worst_abs_sum: i64 = (0..SRC_PHASES)
        .map(|p| {
            let n = SRC_PROTO.len();
            let mut a = 0i64;
            for tap in 0..SRC_HALF_TAPS {
                let right = tap * SRC_PHASES + p;
                let left = if p == 0 {
                    (tap + 1) * SRC_PHASES
                } else {
                    tap * SRC_PHASES + (SRC_PHASES - p)
                };
                if right < n {
                    a += (SRC_PROTO[right] as i64).abs();
                }
                if left > 0 && left < n {
                    a += (SRC_PROTO[left] as i64).abs();
                }
            }
            a
        })
        .max()
        .unwrap();

    // No entry was clamped to the edge of the type.
    for (i, c) in SRC_PROTO.iter().enumerate() {
        assert!(
            *c != i32::MIN && *c != i32::MAX,
            "coefficient {} is saturated at the edge of i32",
            i
        );
    }

    // Full-scale input through the widest window the resampler uses, with the
    // 3x tap allowance of a 1/3 downsample.
    let worst = worst_abs_sum
        .checked_mul(i16::MIN as i64)
        .and_then(|v| v.checked_mul(3))
        .expect("the worst-case accumulation overflows i64");
    assert!(
        worst.abs() < i64::MAX / 1024,
        "the worst-case accumulation leaves less than 10 bits of headroom"
    );

    // And the sum of magnitudes is bounded: the filter is an interpolator, so
    // a little over 2.0 (the two wings at the busiest phase) is expected, and
    // a much larger figure means the table is not a windowed sinc.
    assert!(
        worst_abs_sum < 3 * ONE,
        "the magnitudes sum to {:.3} of unity",
        worst_abs_sum as f64 / ONE as f64
    );
}

/// The fixed-point format is part of the resampler's arithmetic (`>> Q` after
/// every multiply), so the exponent is pinned here rather than left to
/// whatever the generator last printed.
#[test]
fn the_fixed_point_format_is_q30() {
    assert_eq!(SRC_COEF_Q, 30);
    assert_eq!(SRC_PHASES, 256);
    assert_eq!(SRC_HALF_TAPS, 16);
    // Q30 leaves one bit of headroom in an i32 for the peak, which is what
    // lets a coefficient slightly above unity exist at all.
    assert!((SRC_PROTO[0] as i64) < i32::MAX as i64);
}
