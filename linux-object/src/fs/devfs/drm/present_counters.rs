//! Boot-long counters for the present path's source probe, and the numbers
//! `/proc/present` prints. Split out of `drm.rs` for the size ratchet; a
//! child of `drm`, so the probe's private statics above are in scope.

use super::*;

/// Where on screen the probe keeps finding the source moving, counted per
/// 64-pixel column band ([`PROBE_BAND_PX`]), and the repair work that followed.
///
/// The klog lines above are budgeted -- twelve of them and the probe goes quiet
/// for the rest of the boot -- which is right for a line a human reads and
/// wrong for a fault that is intermittent. "It happened twelve times somewhere
/// near the start" cannot tell a desktop that races on every frame from one
/// that raced while the compositor came up, and that is the whole question a
/// flicker nobody can reproduce asks. These counters run for the life of the
/// boot and `/proc/present` prints them; [`PROBE_REPORTS`] over
/// [`ZERO_REPORTS`] is the rate they belong to.
///
/// The band breakdown is the part that LOCALISES the fault, and nothing else
/// here can. A present blits the whole frame, so the window in the report is
/// always the screen and says nothing about which client is at fault; the bands
/// are column ranges of that screen, so a contiguous run of hot bands with the
/// rest cold names the columns one window occupies, and bands hot all the way
/// across say the whole frame is handed over unfinished. Those are different
/// faults with different fixes, and today they produce the same log line.
static PROBE_BAND_HITS: [AtomicU32; PROBE_MAX_BANDS] =
    [const { AtomicU32::new(0) }; PROBE_MAX_BANDS];

/// Presents where at least one repair round ran, and the rounds they ran in
/// total. Both, because [`MAX_REPAIR_ROUNDS`] caps the second: rounds equal to
/// twice the presents means every repair spent its whole budget and the span it
/// was chasing never settled, which is the case where repairing is not enough
/// and the fix has to be upstream.
static REPAIRED_PRESENTS: AtomicU64 = AtomicU64::new(0);
static REPAIR_ROUNDS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// The last mismatch, as `(fb_id, window x, window width, band mask)`. The
/// counters say how often and where; this says what the most recent one looked
/// like, so a reader who catches the flicker and `cat`s straight away sees the
/// frame they just watched rather than only a boot-long average.
static LAST_PROBE_EVENT: Mutex<Option<(u32, u32, u32, u32)>> = Mutex::new(None);

/// Record one probe mismatch: which bands moved, and the frame it happened on.
///
/// `mask` is [`ProbeBands::diff_mask`]'s answer, so bit `i` is band `i` of the
/// window -- source columns `x + 64*i ..`. A mask of 0 (the second read
/// produced no answer at all) still counts as a mismatch, because
/// [`probe_says_changed`] said so; it simply lands in no band, and the band
/// total being below the mismatch count is how a reader sees that happened.
pub(super) fn note_probe_mismatch(fb_id: u32, blit_x: u32, blit_w: u32, mask: u32) {
    bump_bands(mask, &PROBE_BAND_HITS);
    *LAST_PROBE_EVENT.lock() = Some((fb_id, blit_x, blit_w, mask));
}

/// Add one to every band `mask` names. The counter array is a parameter so a
/// test drives the real function over its own array instead of racing the
/// boot-long statics: the production call passes [`PROBE_BAND_HITS`], and the
/// bit-to-band mapping under test is the one the panel actually uses.
///
/// Bits past the array are ignored rather than folded into the last band.
/// [`ProbeBands::diff_mask`] only ever sets bits below [`PROBE_MAX_BANDS`], so
/// folding would be unreachable code standing in for a case that cannot arrive,
/// and a band whose count included columns it does not cover would read as a
/// wider hot run than the screen really has -- the one thing this histogram is
/// for.
pub(super) fn bump_bands(mask: u32, hits: &[AtomicU32]) {
    for (i, h) in hits.iter().enumerate() {
        if mask & (1u32 << i) != 0 {
            h.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Record the repair that followed one mismatch. `rounds` is 0 when the repair
/// is switched off or found no span to redo, and such a present is not a
/// repaired one.
pub(super) fn note_repair(rounds: u32) {
    if rounds == 0 {
        return;
    }
    REPAIRED_PRESENTS.fetch_add(1, Ordering::Relaxed);
    REPAIR_ROUNDS_TOTAL.fetch_add(u64::from(rounds), Ordering::Relaxed);
}

/// The band hit counts, lowest band first. Band `i` covers source columns
/// `64*i .. 64*i + 63` of the presented window.
pub fn probe_band_hits() -> Vec<u32> {
    PROBE_BAND_HITS
        .iter()
        .map(|h| h.load(Ordering::Relaxed))
        .collect()
}

/// Everything `/proc/present` needs from the probe: the reads, the mismatches,
/// the repair work, and the last mismatch.
pub struct PresentProbeCounters {
    /// Probe reads that happened, i.e. CPU-blit presents. The denominator.
    pub reads: u32,
    /// Reads where the source had moved under the copy.
    pub mismatches: u32,
    /// Reads that found the source already fully transparent black somewhere.
    pub black_source: u32,
    /// Reads where black appeared only after the copy had started.
    pub black_grew: u32,
    /// Presents that ran at least one repair round, and the rounds in total.
    pub repaired: u64,
    pub repair_rounds: u64,
    /// `(fb_id, window x, window width, band mask)` of the last mismatch.
    pub last: Option<(u32, u32, u32, u32)>,
    /// Whether the repair is armed at all (`drm.present_repair`).
    pub repair_on: bool,
    /// Whether the probe is armed on its own (`drm.present_probe`); the repair
    /// arms it too, which is why a reader needs both flags to know why the
    /// numbers are moving.
    pub probe_on: bool,
}

/// Read the probe counters. Not a consistent snapshot -- each counter is read
/// on its own and a present can land between two of them -- and it does not
/// need to be: these are rates read by eye, and a frame's worth of skew in one
/// of them changes no answer. Taking a lock over all of them would put the
/// present path behind a reader instead.
pub fn present_probe_counters() -> PresentProbeCounters {
    PresentProbeCounters {
        reads: ZERO_REPORTS.load(Ordering::Relaxed),
        mismatches: PROBE_REPORTS.load(Ordering::Relaxed),
        black_source: ZERO_FOUND_REPORTS.load(Ordering::Relaxed),
        black_grew: ZERO_GREW_REPORTS.load(Ordering::Relaxed),
        repaired: REPAIRED_PRESENTS.load(Ordering::Relaxed),
        repair_rounds: REPAIR_ROUNDS_TOTAL.load(Ordering::Relaxed),
        last: *LAST_PROBE_EVENT.lock(),
        repair_on: present_repair_enabled(),
        probe_on: present_probe_enabled(),
    }
}

/// The width in pixels of one band of the mask, for a reader turning a band
/// number into a column range.
pub fn probe_band_px() -> u32 {
    PROBE_BAND_PX as u32
}
