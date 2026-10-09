extern crate std;

use self::std::vec;
use self::std::vec::Vec;
use super::*;

/// Put the process-global latches back however the body leaves them, and
/// serialise against every other test that touches them.
struct Restored {
    _serialised: self::std::sync::MutexGuard<'static, ()>,
}

impl Drop for Restored {
    fn drop(&mut self) {
        reset_output_state_for_test();
    }
}

fn serialised() -> Restored {
    let g = Restored {
        _serialised: super::test_globals::lock(),
    };
    reset_output_state_for_test();
    g
}

/// A buffer whose every pixel is distinct, so any change the checksum does
/// not notice is the checksum's fault and not a collision of equal values.
fn buf(stride_px: usize, rows: usize) -> Vec<u32> {
    (0..stride_px * rows)
        .map(|n| 0xFF00_0000 | n as u32)
        .collect()
}

fn sum(pixels: &[u32], stride: usize, x: u32, y: u32, w: u32, h: u32) -> Option<u64> {
    bands(pixels, stride, x, y, w, h).map(|b| b.fold())
}

fn bands(pixels: &[u32], stride: usize, x: u32, y: u32, w: u32, h: u32) -> Option<ProbeBands> {
    probe_bands(pixels, stride, x, y, w, h, PROBE_ROW_STEP)
}

// --- where the source is already black (ZeroExtent) ---

/// `buf` paints every pixel opaque, so the answer must be "none". This is
/// the reading that exonerates the compositor: black on screen with no zero
/// pixels in the source means the kernel lost them.
#[test]
fn a_source_with_no_black_reports_no_zero_pixels() {
    let p = buf(64, 32);
    let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("a window to read");
    assert_eq!(b.zero.zeros, 0);
    assert_eq!(b.zero.bbox(), None);
}

/// The reading that convicts it: a black rectangle in the buffer the client
/// handed over comes back as its own box, in window coordinates.
#[test]
fn a_black_rectangle_in_the_source_is_located_by_its_box() {
    let mut p = buf(64, 32);
    // A 10x6 hole at +20+8 of the buffer, zeroed the way an unrasterised
    // tile is: fully transparent black.
    for y in 8..14 {
        for x in 20..30 {
            p[y * 64 + x] = 0;
        }
    }
    let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("a window to read");
    assert_eq!(b.zero.zeros, 10 * 6);
    assert_eq!(b.zero.bbox(), Some((20, 8, 10, 6)));
}

/// The box is window-relative, not buffer-relative, because that is what
/// lines up with what the eye sees: the window's own origin is already
/// printed beside it.
#[test]
fn the_box_is_relative_to_the_window_not_the_buffer() {
    let mut p = buf(64, 32);
    p[10 * 64 + 30] = 0;
    // Window starts at +25+8, so the pixel is at +5+2 inside it.
    let b = probe_bands(&p, 64, 25, 8, 20, 10, 1).expect("a window to read");
    assert_eq!(b.zero.bbox(), Some((5, 2, 1, 1)));
}

/// A single zero pixel is a 1x1 box. Reported inclusively on both ends, so
/// without the `+ 1` it would come out 0x0 and read as "no black at all" --
/// the wrong answer in the direction that sends the search to the wrong side.
#[test]
fn one_black_pixel_is_a_one_by_one_box() {
    let mut p = buf(64, 32);
    p[3 * 64 + 7] = 0;
    let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("a window to read");
    assert_eq!(b.zero.zeros, 1);
    assert_eq!(b.zero.bbox(), Some((7, 3, 1, 1)));
}

/// Black outside the window is none of this window's business: a present
/// that scans out part of a buffer must not be blamed for the rest of it.
#[test]
fn black_outside_the_window_is_not_counted() {
    let mut p = buf(64, 32);
    p[0] = 0; // +0+0 of the buffer, outside the window below
    let b = probe_bands(&p, 64, 10, 10, 20, 10, 1).expect("a window to read");
    assert_eq!(b.zero.zeros, 0);
    assert_eq!(b.zero.bbox(), None);
}

/// `sampled` counts PIXELS, not rows, or the fraction in the klog line would
/// be off by the window's width and the number would mean nothing.
#[test]
fn the_sampled_count_is_pixels_not_rows() {
    let p = buf(64, 32);
    // 20 rows at a step of 4 samples rows 0,4,8,12,16 -- five of them.
    let b = probe_bands(&p, 64, 0, 0, 40, 20, 4).expect("a window to read");
    assert_eq!(b.zero.sampled, 40 * 5);
}

/// A row the sampling step skips over cannot contribute, which is the honest
/// limit of the number: it describes the rows the probe actually read.
#[test]
fn a_black_row_the_step_skips_is_not_seen() {
    let mut p = buf(64, 32);
    for x in 0..64 {
        p[1 * 64 + x] = 0; // row 1, which a step of 4 never reads
    }
    let b = probe_bands(&p, 64, 0, 0, 64, 32, 4).expect("a window to read");
    assert_eq!(b.zero.zeros, 0);
}

/// Black in both reads differs in neither, so the band mask says nothing
/// about it. This is exactly why the zero report is a separate line with its
/// own budget rather than a field on the mismatch one.
#[test]
fn a_static_black_region_sets_no_band_bit_but_is_still_reported() {
    let mut p = buf(64, 32);
    for y in 0..32 {
        for x in 8..16 {
            p[y * 64 + x] = 0;
        }
    }
    let a = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("first read");
    let b = probe_bands(&p, 64, 0, 0, 64, 32, 1).expect("second read");
    assert_eq!(a.diff_mask(&b), 0, "nothing moved, so no band differs");
    assert!(!probe_says_changed(a.fold(), Some(b.fold())));
    assert_eq!(a.zero.bbox(), Some((8, 0, 8, 32)), "but the black is found");
}

// --- what it must notice ---

/// The baseline every other test leans on: read the same window twice with
/// nothing touching it in between and the two answers match. Without this
/// the probe would report on every single frame and the log would carry no
/// information at all.
#[test]
fn the_same_window_read_twice_is_the_same_answer() {
    let p = buf(64, 32);
    assert_eq!(sum(&p, 64, 4, 4, 40, 20), sum(&p, 64, 4, 4, 40, 20));
    assert!(sum(&p, 64, 4, 4, 40, 20).is_some());
}

/// The whole point. One pixel rewritten inside a row the probe samples, and
/// the second read says so.
#[test]
fn one_pixel_changed_inside_a_sampled_row_is_caught() {
    let mut p = buf(64, 32);
    let before = sum(&p, 64, 0, 0, 64, 32);
    // Row 8 is a multiple of the step, so it is one the probe reads.
    assert_eq!(8 % PROBE_ROW_STEP, 0);
    p[8 * 64 + 17] ^= 0x00FF_00FF;
    assert_ne!(before, sum(&p, 64, 0, 0, 64, 32));
}

/// Every pixel of a sampled row is read, not a stride of them -- so the
/// last column counts too. A comb of stale pixels can be one column wide,
/// and a probe that stepped along the row would walk straight past it.
#[test]
fn the_last_column_of_a_sampled_row_counts() {
    let mut p = buf(64, 32);
    let before = sum(&p, 64, 8, 0, 40, 12);
    // x + w - 1, the rightmost pixel the window covers, on row 0.
    p[47] ^= 0x0000_00FF;
    assert_ne!(before, sum(&p, 64, 8, 0, 40, 12));
}

/// Two pixels swapped within a row: the same pixels, in different places.
/// A sum or an XOR would call that unchanged, which is why each pixel is
/// folded with the position it was read from. A scrolled list or a window
/// dragged by one pixel is exactly this shape.
#[test]
fn the_same_pixels_in_a_different_order_are_not_the_same_answer() {
    let mut p = buf(64, 32);
    let before = sum(&p, 64, 0, 0, 64, 8);
    p.swap(3, 40);
    assert_ne!(before, sum(&p, 64, 0, 0, 64, 8));
}

/// Two pixels swapped between two sampled ROWS, not just within one. The
/// row is no longer folded into the hash, so this is the case that says the
/// sequence itself is what distinguishes them.
#[test]
fn the_same_pixels_swapped_between_rows_are_not_the_same_answer() {
    let mut p = buf(64, 32);
    let before = sum(&p, 64, 0, 0, 64, 32);
    p.swap(0 * 64 + 9, 4 * 64 + 9);
    assert_ne!(before, sum(&p, 64, 0, 0, 64, 32));
}

/// The hardest case for a value-only hash: every pixel identical, so only
/// how many of them there were can tell the two windows apart. FNV-1a folds
/// each one in turn even when they are equal, so it does -- which is why
/// dropping the position fold cost nothing. A flat wallpaper is exactly this
/// buffer.
#[test]
fn a_window_of_all_equal_pixels_still_depends_on_how_many_there_were() {
    let p = vec![0xFF80_8080u32; 64 * 32];
    assert_ne!(sum(&p, 64, 0, 0, 64, 32), sum(&p, 64, 0, 0, 64, 16));
    assert_ne!(sum(&p, 64, 0, 0, 64, 8), sum(&p, 64, 0, 0, 32, 8));
    // And the same window twice is still the same answer.
    assert_eq!(sum(&p, 64, 0, 0, 64, 32), sum(&p, 64, 0, 0, 64, 32));
}

// --- what it must NOT notice ---

/// A change outside the window is not this present's business. The probe
/// names a rectangle in its report, so it has to be reporting on that
/// rectangle: a checksum that covered the whole buffer would fire on every
/// frame of an animated clock in another corner and the log would be noise.
#[test]
fn a_change_outside_the_window_is_not_reported() {
    let mut p = buf(64, 32);
    let before = sum(&p, 64, 8, 8, 16, 16);
    // Left of the window, right of it, above it and below it.
    p[8 * 64 + 7] ^= 0xFFFF_FFFF;
    p[8 * 64 + 24] ^= 0xFFFF_FFFF;
    p[7 * 64 + 12] ^= 0xFFFF_FFFF;
    p[24 * 64 + 12] ^= 0xFFFF_FFFF;
    assert_eq!(before, sum(&p, 64, 8, 8, 16, 16));
}

/// The blind spot, written down rather than discovered later. Rows are
/// sampled, so a change confined to the rows in between is invisible. That
/// is the trade: a torn frame comes from a rasteriser handing over tiles,
/// which is tens of rows tall, and reading every row would double the cost
/// of the present the probe is measuring.
#[test]
fn a_change_only_in_the_rows_the_step_skips_is_missed() {
    let mut p = buf(64, 32);
    let before = sum(&p, 64, 0, 0, 64, 32);
    for r in 0..32 {
        if r % PROBE_ROW_STEP != 0 {
            p[r * 64 + 5] ^= 0xFFFF_FFFF;
        }
    }
    assert_eq!(
        before,
        sum(&p, 64, 0, 0, 64, 32),
        "the row step is what it is; if this starts failing the step changed"
    );
    // Pinned by value, not by the constant. Read through `PROBE_ROW_STEP`
    // the loop above selects nothing at all when the step is 1, so the test
    // would keep passing while the trade it describes silently went away --
    // and a step of 1 doubles the cost of every present the probe measures.
    assert_eq!(
        PROBE_ROW_STEP, 4,
        "the step changed: re-read what the blind spot above now covers, and \
         what the second read now costs per frame"
    );
}

// --- "I cannot tell" is its own answer ---

/// `None` is never equal to a checksum, so a window the probe could not
/// read does not come back as "unchanged". The call site compares
/// `after != Some(before)`, so a `None` after a `Some` reports -- which is
/// the safe direction for something whose job is to find a defect.
#[test]
fn nothing_to_compare_is_not_the_same_as_a_match() {
    let p = buf(64, 8);
    let real = sum(&p, 64, 0, 0, 8, 4);
    assert!(real.is_some());
    // Degenerate in each of the four ways.
    assert!(probe_bands(&p, 0, 0, 0, 8, 4, PROBE_ROW_STEP).is_none());
    assert!(probe_bands(&p, 64, 0, 0, 0, 4, PROBE_ROW_STEP).is_none());
    assert!(probe_bands(&p, 64, 0, 0, 8, 0, PROBE_ROW_STEP).is_none());
    assert!(probe_bands(&p, 64, 0, 0, 8, 4, 0).is_none());
    assert_ne!(real, None);
}

/// A window whose very first row is already past the end of the buffer has
/// nothing to checksum at all.
#[test]
fn a_window_past_the_end_of_the_buffer_has_no_answer() {
    let p = buf(64, 8);
    assert_eq!(sum(&p, 64, 0, 64, 64, 4), None);
    // And one whose first row starts inside the buffer but runs off its end.
    assert_eq!(sum(&p, 64, 32, 7, 64, 4), None);
}

/// A row is `w` pixels from where it starts, the way `blit_from` reads it --
/// so a window wider than the stride runs into the next row rather than
/// being clipped or refused. Worth pinning because it looks like a bug: it
/// is not reachable from the present path (`expand_x_for_wc` caps the right
/// edge at the pitch), and the probe has to read exactly what the blit read,
/// not what a tidier rule would have read.
#[test]
fn a_window_wider_than_the_stride_reads_into_the_next_row() {
    let p = buf(64, 8);
    assert!(sum(&p, 64, 60, 0, 64, 4).is_some());
    // Which means a pixel two rows down, at the far end of that run, counts.
    let mut q = p.clone();
    q[64 + 20] ^= 0xFFFF_FFFF;
    assert_ne!(sum(&p, 64, 60, 0, 64, 4), sum(&q, 64, 60, 0, 64, 4));
}

/// A window that starts inside the buffer and runs off the bottom
/// checksums the rows that fit instead of giving up on all of them, so a
/// present whose last rows fall outside the mapping is still measured.
#[test]
fn a_window_that_runs_off_the_bottom_measures_the_rows_that_fit() {
    let p = buf(64, 10);
    let partial = sum(&p, 64, 0, 4, 64, 40);
    assert!(partial.is_some());
    // Rows 4 and 8 fit, row 12 does not -- so this is the same read as a
    // window two sampled rows tall, and a different one from a window that
    // only covers the first of them.
    assert_eq!(partial, sum(&p, 64, 0, 4, 64, 8));
    assert_ne!(partial, sum(&p, 64, 0, 4, 64, 4));
}

/// The number of rows that were read is part of the answer, so a window
/// that shrank between the two reads does not pass as unchanged.
#[test]
fn a_window_of_a_different_height_is_a_different_answer() {
    let p = buf(64, 32);
    assert_ne!(sum(&p, 64, 0, 0, 64, 32), sum(&p, 64, 0, 0, 64, 16));
}

/// Same pixels, read through a different stride: a different set of
/// pixels. The probe is handed the framebuffer's own pitch, and a
/// mismatched one would silently be measuring a diagonal.
#[test]
fn the_stride_is_part_of_what_is_being_read() {
    let p = buf(64, 32);
    assert_ne!(sum(&p, 64, 0, 0, 16, 16), sum(&p, 32, 0, 0, 16, 16));
}

// --- the rule that turns two reads into a verdict ---

/// Two equal reads: nobody touched it, no report.
#[test]
fn two_equal_reads_are_not_a_report() {
    assert!(!probe_says_changed(0x1234, Some(0x1234)));
}

/// Two different reads: somebody wrote the window while the kernel was
/// copying it, which is the entire finding.
#[test]
fn two_different_reads_are_a_report() {
    assert!(probe_says_changed(0x1234, Some(0x1235)));
}

/// And a second read that could not produce an answer reports too. The
/// probe exists to decide who is responsible for a wrong pixel, so the one
/// direction it must never fall in is quietly clearing the compositor.
#[test]
fn a_second_read_with_no_answer_reports_rather_than_clearing_anyone() {
    assert!(probe_says_changed(0x1234, None));
}

// --- the report budget ---

/// A compositor that tears every frame must not turn the klog into the
/// bottleneck: `klog` writes synchronously to the UART, slower than the
/// frame it is describing. Twelve reports, then silence.
#[test]
fn the_report_budget_runs_out_and_says_so_on_the_way_past() {
    assert_eq!(probe_report_decision(0), (true, false));
    assert_eq!(
        probe_report_decision(MAX_PROBE_REPORTS - 2),
        (true, false),
        "the one before the last is not the last"
    );
    assert_eq!(
        probe_report_decision(MAX_PROBE_REPORTS - 1),
        (true, true),
        "the last report has to say it is the last, or the log looks truncated"
    );
    assert_eq!(probe_report_decision(MAX_PROBE_REPORTS), (false, false));
    assert_eq!(probe_report_decision(u32::MAX), (false, false));
}

// --- the latch ---

/// Off unless a boot asks for it. The probe reads the damage box a second
/// time on every present, so a default of on would be a permanent tax on
/// the frame rate for a measurement nobody requested.
#[test]
fn the_probe_is_off_unless_the_cmdline_arms_it() {
    let _g = serialised();
    assert!(!present_probe_enabled());
    set_present_probe_enabled(true);
    assert!(present_probe_enabled());
}

/// And a test that armed it does not leave it armed for the next one --
/// which would make every later present test read its window twice and log
/// about a frame nobody was looking at.
#[test]
fn the_reset_between_tests_disarms_a_leaked_probe() {
    let _g = serialised();
    set_present_probe_enabled(true);
    PROBE_REPORTS.store(MAX_PROBE_REPORTS, Ordering::Relaxed);
    reset_output_state_for_test();
    assert!(!present_probe_enabled());
    assert_eq!(
        probe_report_decision(PROBE_REPORTS.load(Ordering::Relaxed)),
        (true, false),
        "the budget has to come back too, or the last test to run finds it spent"
    );
}

/// The buffer helper really does hand out distinct pixels, so
/// "the checksum did not notice" can never be a collision of equal values.
#[test]
fn the_test_buffer_has_no_two_equal_pixels() {
    let p = buf(16, 4);
    let mut seen = vec![];
    for px in &p {
        assert!(!seen.contains(px));
        seen.push(*px);
    }
}

// --- which bands moved ---

/// A window read twice with nothing touching it sets no bit. Without this
/// every mask below would be indistinguishable from "the mask is always
/// full", and a full mask is exactly one of the two answers we are trying
/// to tell apart.
#[test]
fn a_settled_window_sets_no_band() {
    let p = buf(512, 8);
    let a = bands(&p, 512, 0, 0, 512, 8).unwrap();
    let b = bands(&p, 512, 0, 0, 512, 8).unwrap();
    assert_eq!(a.diff_mask(&b), 0);
    assert_eq!(a.n, 8, "512 px is eight 64-px bands");
}

/// The comb: a rasteriser handed over tiles 1, 3 and 5 and left the rest of
/// the frame as it was. That is the shape this mask exists to name, and it
/// must come out as a *scattered* subset -- not as a run, and not as
/// everything.
#[test]
fn stale_tiles_light_up_exactly_their_own_bands() {
    let mut p = buf(512, 8);
    let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
    for band in [1usize, 3, 5] {
        for row in 0..8usize {
            p[row * 512 + band * PROBE_BAND_PX + 7] ^= 0xFF;
        }
    }
    let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
    assert_eq!(
        before.diff_mask(&after),
        (1 << 1) | (1 << 3) | (1 << 5),
        "only the bands whose pixels moved may be set"
    );
}

/// The other shape: the compositor overwrote the frame it had already handed
/// over, so the change is a contiguous run. Same instrument, visibly
/// different answer -- which is the whole reason the mask beats a count.
#[test]
fn a_frame_overwritten_in_place_lights_up_a_contiguous_run() {
    let mut p = buf(512, 8);
    let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
    for row in 0..8usize {
        for col in (2 * PROBE_BAND_PX)..(6 * PROBE_BAND_PX) {
            p[row * 512 + col] ^= 0xFF;
        }
    }
    let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
    let mask = before.diff_mask(&after);
    assert_eq!(mask, 0b0011_1100, "bands 2..5 and nothing else");
    assert_eq!(mask.count_ones(), 4);
}

/// One changed pixel in a band sets that band and no other. Run for every
/// band of the window, because a mask that is right for band 0 and wrong for
/// band 7 is worse than no mask: it would point the search at the wrong
/// columns of the panel.
#[test]
fn one_changed_pixel_sets_its_own_band_and_only_it() {
    for band in 0..8usize {
        let mut p = buf(512, 8);
        let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
        p[band * PROBE_BAND_PX] ^= 0xFF;
        let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
        assert_eq!(
            before.diff_mask(&after),
            1u32 << band,
            "a pixel in band {} must set band {} alone",
            band,
            band
        );
    }
}

/// The last pixel of a band belongs to that band and the first pixel of the
/// next one does not. Off by one here would slide the whole reading of the
/// klog 64 px to the left.
#[test]
fn a_band_ends_where_the_next_one_starts() {
    let mut p = buf(512, 8);
    let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
    p[PROBE_BAND_PX - 1] ^= 0xFF;
    assert_eq!(
        before.diff_mask(&bands(&p, 512, 0, 0, 512, 8).unwrap()),
        1 << 0
    );
    let mut q = buf(512, 8);
    q[PROBE_BAND_PX] ^= 0xFF;
    assert_eq!(
        before.diff_mask(&bands(&q, 512, 0, 0, 512, 8).unwrap()),
        1 << 1
    );
}

/// The window's own `x` is the mask's origin: band 0 is the left edge of
/// what was blitted, not of the framebuffer. Anything else and the klog's
/// mask could not be read against the blit rectangle it is printed with.
#[test]
fn band_zero_is_the_windows_left_edge_not_the_buffers() {
    let mut p = buf(512, 8);
    let before = bands(&p, 512, 128, 0, 256, 8).unwrap();
    p[128] ^= 0xFF;
    let after = bands(&p, 512, 128, 0, 256, 8).unwrap();
    assert_eq!(before.diff_mask(&after), 1 << 0);
}

/// A window narrower than one band still has one, and a change in it is
/// reported rather than divided away to nothing.
#[test]
fn a_window_narrower_than_a_band_still_has_one() {
    let mut p = buf(512, 8);
    let before = bands(&p, 512, 0, 0, 7, 8).unwrap();
    assert_eq!(before.n, 1);
    p[3] ^= 0xFF;
    assert_eq!(before.diff_mask(&bands(&p, 512, 0, 0, 7, 8).unwrap()), 1);
}

/// A window wider than the mask can describe folds its right-hand columns
/// into the last band instead of dropping them. A mask that quietly stopped
/// covering part of the window would read as "those columns are clean".
#[test]
fn columns_past_the_masks_reach_fold_into_the_last_band() {
    let wide = PROBE_MAX_BANDS * PROBE_BAND_PX + 300;
    let mut p = buf(wide, 8);
    let before = bands(&p, wide, 0, 0, wide as u32, 8).unwrap();
    assert_eq!(before.n, PROBE_MAX_BANDS);
    p[wide - 1] ^= 0xFF;
    let after = bands(&p, wide, 0, 0, wide as u32, 8).unwrap();
    assert_eq!(
        before.diff_mask(&after),
        1u32 << (PROBE_MAX_BANDS - 1),
        "the far right column has to land in the last band, not nowhere"
    );
}

/// The band count never reaches past the array, and never reads as zero.
#[test]
fn the_band_count_stays_inside_the_mask() {
    assert_eq!(probe_band_count(0), 1);
    assert_eq!(probe_band_count(1), 1);
    assert_eq!(probe_band_count(PROBE_BAND_PX as u32), 1);
    assert_eq!(probe_band_count(PROBE_BAND_PX as u32 + 1), 2);
    assert_eq!(probe_band_count(1920), 30);
    assert_eq!(probe_band_count(u32::MAX), PROBE_MAX_BANDS);
}

/// The scalar answer the report's decision rests on still notices a change
/// in any single band -- including the last one, which is where a fold that
/// stopped early would go quiet.
#[test]
fn the_fold_notices_a_change_in_any_band() {
    for band in 0..8usize {
        let mut p = buf(512, 8);
        let before = sum(&p, 512, 0, 0, 512, 8);
        p[band * PROBE_BAND_PX + 1] ^= 0xFF;
        assert_ne!(
            before,
            sum(&p, 512, 0, 0, 512, 8),
            "a change in band {} has to reach the fold",
            band
        );
    }
}

/// A band of black pixels is not the same answer as a band nobody read. That
/// is what the hash basis buys, and it is also what lets two reads of
/// different widths come out different without the fold having to carry `n`:
/// on an all-black buffer the wider read's extra band is the only thing that
/// separates them.
#[test]
fn an_all_black_band_is_not_a_band_that_was_never_read() {
    let p = vec![0u32; 512 * 8];
    let narrow = bands(&p, 512, 0, 0, 64, 8).unwrap();
    let wide = bands(&p, 512, 0, 0, 128, 8).unwrap();
    assert_eq!((narrow.n, wide.n), (1, 2));
    assert_eq!(
        narrow.bands[1], PROBE_FNV_BASIS,
        "band 1 was never read in the narrow window"
    );
    assert_ne!(
        narrow.bands[1], wide.bands[1],
        "64 black pixels must not hash to the untouched value"
    );
    assert_ne!(narrow.fold(), wide.fold());
    assert_eq!(narrow.diff_mask(&wide), 1 << 1);
}

// --- what a repair round would copy ---

/// Nothing moved, nothing to repair. Without this every span below could be
/// explained by "it always returns a span".
#[test]
fn a_mask_with_no_band_set_repairs_nothing() {
    assert_eq!(repair_span_px(0, 30, 1920), None);
}

/// One band is its own 64 columns and not one more. A span that overshot
/// would copy settled pixels on every repair round, which is the cost this
/// whole mechanism is trying to keep proportional.
#[test]
fn one_band_is_its_own_sixty_four_columns() {
    assert_eq!(repair_span_px(1 << 0, 30, 1920), Some((0, 64)));
    assert_eq!(repair_span_px(1 << 1, 30, 1920), Some((64, 64)));
    assert_eq!(repair_span_px(1 << 29, 30, 1920), Some((1856, 64)));
}

/// Scattered bands become ONE span from the first to the last, settled bands
/// in between included: two blits of nearby bands cost more than one blit of
/// both, and a repair round is a blit.
#[test]
fn scattered_bands_become_one_span_from_first_to_last() {
    let mask = (1 << 2) | (1 << 5) | (1 << 9);
    assert_eq!(repair_span_px(mask, 30, 1920), Some((128, (10 - 2) * 64)));
}

/// A mask whose bits all sit past the bands the window covered describes no
/// pixels. Turning that into a copy would be a copy of the wrong columns.
#[test]
fn bands_past_the_window_repair_nothing() {
    assert_eq!(repair_span_px(1 << 20, 4, 256), None);
    assert_eq!(repair_span_px(1 << 31, 30, 1920), None);
}

/// The last band is the folded one: for a window wider than the mask's reach
/// it stands for every column up to the window's right edge, and the span has
/// to reach that far or those columns never get repaired.
#[test]
fn the_last_band_reaches_the_windows_right_edge() {
    let wide = (PROBE_MAX_BANDS * PROBE_BAND_PX + 300) as u32;
    let (x, w) = repair_span_px(1 << (PROBE_MAX_BANDS - 1), PROBE_MAX_BANDS, wide).unwrap();
    assert_eq!(x, ((PROBE_MAX_BANDS - 1) * PROBE_BAND_PX) as u32);
    assert_eq!(
        x + w,
        wide,
        "the folded band owns everything to the right edge"
    );
}

/// A span never reaches past the window, whatever the mask says. A blit that
/// started inside the window and ran past its right edge would walk into the
/// next row.
#[test]
fn a_span_never_reaches_past_the_window() {
    for bit in 0..PROBE_MAX_BANDS {
        if let Some((x, w)) = repair_span_px(1 << bit, PROBE_MAX_BANDS, 100) {
            assert!(
                x + w <= 100,
                "band {} gave {}..{} on a 100-px window",
                bit,
                x,
                x + w
            );
        }
    }
    assert_eq!(repair_span_px(u32::MAX, 30, 1920), Some((0, 1920)));
}

/// A window with no width and a mask that covers no bands are both "nothing
/// to do", not a zero-width blit at the origin.
#[test]
fn a_window_of_no_width_repairs_nothing() {
    assert_eq!(repair_span_px(1, 30, 0), None);
    assert_eq!(repair_span_px(1, 0, 1920), None);
}

/// The repair is on by default (the boot that has no fence for llvmpipe),
/// the cmdline can disarm it, and the reset between tests restores the
/// default -- a leaked OFF would hide the settle-and-recopy pass from every
/// later present test.
#[test]
fn the_repair_is_on_by_default_and_reset_restores_it() {
    let _g = serialised();
    assert!(present_repair_enabled());
    set_present_repair_enabled(false);
    assert!(!present_repair_enabled());
    reset_output_state_for_test();
    assert!(present_repair_enabled());
    assert_eq!(repair_rounds_for_test(), 0);
}

/// Two rounds, and it is a budget: the loop must not be able to run longer
/// than this against a source that never settles.
#[test]
fn the_repair_budget_is_two_rounds() {
    assert_eq!(MAX_REPAIR_ROUNDS, 2);
}

/// A pixel that moves from one band into another changes both of them, so
/// the mask describes a shift rather than hiding it as "nothing moved".
#[test]
fn a_pixel_that_moves_between_bands_marks_both() {
    let mut p = vec![0u32; 512 * 8];
    p[10] = 0xDEAD_BEEF;
    let before = bands(&p, 512, 0, 0, 512, 8).unwrap();
    p[10] = 0;
    p[PROBE_BAND_PX + 10] = 0xDEAD_BEEF;
    let after = bands(&p, 512, 0, 0, 512, 8).unwrap();
    assert_eq!(before.diff_mask(&after), 0b11);
}
