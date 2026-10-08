//! The band arithmetic and the hash behind `drm.present_skip`.
//!
//! Everything here decides whether a band of the panel keeps the pixels it
//! has. Each one of these functions can be wrong in a way that shows up as
//! stale pixels on screen and in nothing else, which is the defect the whole
//! present path has been chasing -- so they are pinned here, away from a
//! display, and the end-to-end behaviour is in `drm_scheme`'s
//! `kms_scanout_tests`.

use super::*;

// --- how many bands, and which rows are in them ---

/// A window of no rows has no bands, rather than one empty one: a band that
/// describes no pixels would compare two hashes of nothing and answer
/// "unchanged" for rows that were never looked at.
#[test]
fn a_window_of_no_rows_has_no_bands() {
    assert_eq!(skip_band_count(0), None);
}

/// Anything up to a full band is one band, and one row past it is two: the
/// tail band is short, never dropped. A dropped tail is a strip at the
/// bottom of the screen that stops being repainted.
#[test]
fn the_last_rows_get_their_own_short_band() {
    assert_eq!(skip_band_count(1), Some(1));
    assert_eq!(skip_band_count(SKIP_BAND_ROWS), Some(1));
    assert_eq!(skip_band_count(SKIP_BAND_ROWS + 1), Some(2));
    assert_eq!(skip_band_count(1080), Some(68));
}

/// And the tail band reports its real height, so the copy that follows it
/// does not run past the window.
#[test]
fn the_tail_band_is_as_short_as_it_really_is() {
    assert_eq!(skip_band_span(0, 1080), Some((0, SKIP_BAND_ROWS)));
    assert_eq!(skip_band_span(67, 1080), Some((1072, 8)));
    assert_eq!(skip_band_span(68, 1080), None);
    assert_eq!(skip_band_span(0, 3), Some((0, 3)));
}

/// A window taller than the state can describe does not take the skip at
/// all. Clamping instead would leave the rows past the last band never
/// compared and never copied.
#[test]
fn a_window_taller_than_the_state_declines_the_skip() {
    let tallest = SKIP_BAND_ROWS * MAX_SKIP_BANDS as u32;
    assert_eq!(skip_band_count(tallest), Some(MAX_SKIP_BANDS));
    assert_eq!(skip_band_count(tallest + 1), None);
}

// --- which bands a write on top of the frame dirties ---

/// One row dirties the band that holds it, and a row range that straddles a
/// boundary dirties both. The cursor is the caller, and a boundary it
/// straddles with half its height is the ordinary case.
#[test]
fn a_row_range_dirties_every_band_it_touches() {
    assert_eq!(bands_covering_rows(0, 1), Some((0, 1)));
    assert_eq!(
        bands_covering_rows(SKIP_BAND_ROWS - 1, 2),
        Some((0, 2)),
        "a range crossing the boundary owns both bands"
    );
    assert_eq!(
        bands_covering_rows(SKIP_BAND_ROWS, SKIP_BAND_ROWS),
        Some((1, 2))
    );
    assert_eq!(bands_covering_rows(0, SKIP_BAND_ROWS + 1), Some((0, 2)));
}

/// A write of no rows dirties nothing, and one entirely past the bands
/// dirties nothing either -- but a range that merely ENDS past them clamps
/// instead of vanishing, because its first rows are on the panel.
#[test]
fn a_range_outside_the_bands_dirties_nothing_but_one_that_leaves_them_clamps() {
    assert_eq!(bands_covering_rows(0, 0), None);
    assert_eq!(
        bands_covering_rows(SKIP_BAND_ROWS * MAX_SKIP_BANDS as u32, 4),
        None
    );
    assert_eq!(
        bands_covering_rows(0, u32::MAX),
        Some((0, MAX_SKIP_BANDS)),
        "a range past the end still dirties every band it does cover"
    );
}

// --- the geometry key ---

/// Two different windows pack to two different keys, or a present would read
/// hashes taken from somewhere else on the screen as its own.
#[test]
fn every_window_packs_to_its_own_key() {
    let a = pack_panel_geom(0, 0, 1920, 1080).expect("packs");
    for (x, y, w, h) in [
        (1, 0, 1920, 1080),
        (0, 1, 1920, 1080),
        (0, 0, 1921, 1080),
        (0, 0, 1920, 1081),
    ] {
        assert_ne!(
            a,
            pack_panel_geom(x, y, w, h).expect("packs"),
            "{:?}",
            (x, y, w, h)
        );
    }
}

/// And a packed key is never the `0` that means "nothing known", so a real
/// geometry cannot be mistaken for the absence of one.
#[test]
fn a_real_geometry_never_packs_to_the_empty_key() {
    for (x, y, w, h) in [(0, 0, 1, 1), (0, 0, 1920, 1080), (7, 9, 64, 64)] {
        assert_ne!(pack_panel_geom(x, y, w, h), Some(0));
    }
}

/// A window with no width or no height, and one that does not fit the key,
/// decline instead of aliasing onto some other window's hashes.
#[test]
fn a_window_that_does_not_fit_the_key_declines() {
    assert_eq!(pack_panel_geom(0, 0, 0, 1080), None);
    assert_eq!(pack_panel_geom(0, 0, 1920, 0), None);
    assert_eq!(pack_panel_geom(0, 0, 70_000, 1080), None);
    assert_eq!(pack_panel_geom(70_000, 0, 1920, 1080), None);
}

// --- the hash ---

/// The same pixels hash the same, and one pixel of one row different hashes
/// differently. That second half is the whole claim: a band that changed must
/// not be recognised as already on the panel.
#[test]
fn one_changed_pixel_changes_the_bands_hash() {
    let stride = 8usize;
    let mut px: Vec<u32> = (0..stride * 40).map(|n| n as u32).collect();
    let before = skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8).expect("hashes");
    assert_eq!(
        skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
        Some(before),
        "the same pixels twice"
    );
    px[stride * 9 + 3] ^= 1;
    assert_ne!(
        skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
        Some(before)
    );
}

/// Two pixels swapped between rows hash differently, so the hash is of the
/// band's LAYOUT and not of its multiset of colours: a window dragged by one
/// row is not "the same pixels".
#[test]
fn moving_a_pixel_changes_the_hash() {
    let stride = 8usize;
    let mut px: Vec<u32> = (0..stride * 20).map(|n| 0x1000 + n as u32).collect();
    let before = skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8).expect("hashes");
    px.swap(0, stride);
    assert_ne!(
        skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
        Some(before)
    );
}

/// Only the window's own columns count. The bytes past `w` in a row are the
/// scanline's padding, and a hash that read them would call a band changed
/// because of pixels nobody displays.
#[test]
fn the_padding_after_the_window_is_not_part_of_the_band() {
    let stride = 12usize;
    let mut px: Vec<u32> = (0..stride * 20).map(|n| 0x2000 + n as u32).collect();
    let before = skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8).expect("hashes");
    for r in 0..SKIP_BAND_ROWS as usize {
        px[r * stride + 9] ^= 0xFFFF;
    }
    assert_eq!(
        skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8),
        Some(before)
    );
}

/// A band that does not lie wholly inside the buffer has no answer, and the
/// caller reads `None` as "copy it". Hashing a short last row would compare
/// against a hash of different rows and could answer "unchanged".
#[test]
fn a_band_that_runs_past_the_buffer_has_no_hash() {
    let stride = 8usize;
    let px: Vec<u32> = (0..stride * 8).map(|n| n as u32).collect();
    assert_eq!(skip_band_hash(&px, stride, 0, SKIP_BAND_ROWS, 8), None);
    assert!(skip_band_hash(&px, stride, 0, 8, 8).is_some());
    assert_eq!(skip_band_hash(&px, stride, 4, 8, 8), None);
}

/// A window wider than its own stride is not a window, and a zero stride,
/// width or height describes no pixels: all of them decline rather than hash
/// whatever the arithmetic lands on.
#[test]
fn a_window_that_cannot_be_read_has_no_hash() {
    let px: Vec<u32> = (0..64).collect();
    assert_eq!(skip_band_hash(&px, 8, 0, 4, 9), None);
    assert_eq!(skip_band_hash(&px, 0, 0, 4, 4), None);
    assert_eq!(skip_band_hash(&px, 8, 0, 0, 4), None);
    assert_eq!(skip_band_hash(&px, 8, 0, 4, 0), None);
}

// --- the stored form of a hash ---

/// A stored hash is never the `0` that means "nothing is known about this
/// band". If it could be, a band whose pixels happened to hash to the
/// sentinel would be skipped on the first present after a reset -- when the
/// panel does not hold them yet -- and that is a stale band on screen.
#[test]
fn a_stored_hash_is_never_the_unknown_sentinel() {
    for h in [0u64, 1, u64::MAX, PROBE_FNV_BASIS, 1 << 63] {
        assert_ne!(known_hash(h), 0, "hash {:#x}", h);
    }
}

/// And two different hashes still store differently, so the encoding costs
/// one bit and not the comparison: `known_hash` must not fold hashes together
/// beyond that bit.
#[test]
fn the_stored_form_keeps_different_hashes_apart() {
    assert_ne!(known_hash(1), known_hash(2));
    assert_ne!(known_hash(PROBE_FNV_BASIS), known_hash(PROBE_FNV_PRIME));
    // The one pair it does fold: bit 63 is the sentinel's, and the doc says so.
    assert_eq!(known_hash(0), known_hash(1 << 63));
}

// --- the state ---

/// A reset forgets every band and the geometry, which is what a boot, a
/// blank and a console VT all leave behind.
#[test]
fn a_reset_forgets_the_geometry_and_every_band() {
    let _g = test_globals::lock();
    PANEL_BAND_GEOM.store(7, Ordering::Relaxed);
    PANEL_BAND_STRIDE.store(1920, Ordering::Relaxed);
    for (i, h) in PANEL_BAND_HASH.iter().enumerate() {
        h.store(i as u64 + 1, Ordering::Relaxed);
    }
    panel_bands_reset();
    assert_eq!(PANEL_BAND_GEOM.load(Ordering::Relaxed), 0);
    assert_eq!(PANEL_BAND_STRIDE.load(Ordering::Relaxed), 0);
    assert!(PANEL_BAND_HASH
        .iter()
        .all(|h| h.load(Ordering::Relaxed) == 0));
}

/// Dirtying a row range clears exactly the bands it covers and leaves the
/// rest, which is what makes the cursor cost 16 rows instead of the frame.
#[test]
fn dirtying_rows_leaves_the_bands_it_does_not_cover() {
    let _g = test_globals::lock();
    for h in PANEL_BAND_HASH.iter() {
        h.store(0xABCD, Ordering::Relaxed);
    }
    panel_bands_dirty_rows(SKIP_BAND_ROWS, 1);
    assert_eq!(PANEL_BAND_HASH[0].load(Ordering::Relaxed), 0xABCD);
    assert_eq!(PANEL_BAND_HASH[1].load(Ordering::Relaxed), 0);
    assert_eq!(PANEL_BAND_HASH[2].load(Ordering::Relaxed), 0xABCD);
    panel_bands_reset();
}

/// The skip is off unless the cmdline arms it, like every other diagnostic
/// and mitigation on this path: a boot that says nothing gets exactly the
/// present it got before.
#[test]
fn the_skip_is_off_unless_the_cmdline_arms_it() {
    let _g = test_globals::lock();
    reset_output_state_for_test();
    assert!(!present_skip_enabled());
    set_present_skip_enabled(true);
    assert!(present_skip_enabled());
    reset_output_state_for_test();
    assert!(!present_skip_enabled());
}

/// Arming it forgets whatever was remembered: nothing was maintaining the
/// hashes while it was off, so the first present after arming must copy.
#[test]
fn arming_the_skip_starts_from_nothing_known() {
    let _g = test_globals::lock();
    PANEL_BAND_HASH[3].store(0x1234, Ordering::Relaxed);
    PANEL_BAND_GEOM.store(99, Ordering::Relaxed);
    set_present_skip_enabled(true);
    assert_eq!(PANEL_BAND_HASH[3].load(Ordering::Relaxed), 0);
    assert_eq!(PANEL_BAND_GEOM.load(Ordering::Relaxed), 0);
    reset_output_state_for_test();
}
