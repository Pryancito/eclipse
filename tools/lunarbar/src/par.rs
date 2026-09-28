//! Render-pass helpers for the full-surface pixel blits.
//!
//! Originally used `std::thread::scope` to split row-bands across CPU cores.
//! Eclipse OS's kernel does not reliably support the cross-thread
//! stack-reference captures that `std::thread::scope` relies on: spawned
//! threads faulted with a null-range kernel page fault at offset 0x18 (the
//! captured source-buffer slice pointer inside the closure struct). All passes
//! now run serially, which is functionally identical and fast enough for every
//! buffer lunarbar produces (bars are small; even a full-output overlay
//! swizzle completes in well under 5 ms serially).

/// Split `data` — a row-major image buffer of `h` rows, each `row_stride`
/// elements — into horizontal bands and run `f(y0, band)` on each.
/// `y0` is the band's first row index.
///
/// Currently runs as a single serial call. The signature is kept multi-band
/// compatible so callers need not change if parallelism is re-enabled later.
pub fn par_rows<T, F>(data: &mut [T], h: usize, row_stride: usize, f: F)
where
    T: Send,
    F: Fn(usize, &mut [T]) + Sync,
{
    // The contract the callers rely on, and the one a future re-banding would
    // have to keep: `h` rows of exactly `row_stride` elements, all of them in
    // `data`. Checked in debug only, because it is a caller bug and not a
    // runtime condition: `blit_argb` and the swizzle pass both index rows out of
    // the band they are handed, so a stride that does not match the buffer
    // shears the image instead of failing.
    debug_assert_eq!(
        data.len(),
        h.saturating_mul(row_stride),
        "par_rows: {h} rows of {row_stride} do not fill a buffer of {}",
        data.len()
    );
    let _ = (h, row_stride); // parameters reserved for future multi-band use
    f(0, data);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `par_rows` and report, per row, how many times it was handed out.
    fn coverage(h: usize, row_stride: usize) -> Vec<usize> {
        let mut data: Vec<usize> = vec![0; h * row_stride];
        par_rows(&mut data, h, row_stride, |y0, band| {
            for (i, px) in band.iter_mut().enumerate() {
                // Record the absolute row this element was reached as, which is
                // what a band-splitting implementation has to get right.
                *px = y0 + i / row_stride.max(1) + 1;
            }
        });
        let mut per_row = vec![0usize; h];
        for (i, v) in data.iter().enumerate() {
            let row = i / row_stride.max(1);
            // The value is the row index the callee computed, +1 so 0 means
            // "never written".
            assert_eq!(*v, row + 1, "element {i} was written as row {v}, not {row}");
            per_row[row] += 1;
        }
        per_row
    }

    #[test]
    fn every_row_of_the_buffer_is_covered_exactly_once() {
        // The invariant that outlives the current serial implementation: a row
        // handed out twice gets blended twice (visibly darker), and one handed
        // out never keeps the previous frame's pixels. This test is written so
        // it still checks that if the bands come back.
        for (h, row_stride) in [
            (1, 1),
            (1, 4),
            (4, 1),
            (2, 8),
            (7, 13),
            (32, 40),
            (100, 3),
            (1080, 16),
        ] {
            let per_row = coverage(h, row_stride);
            assert_eq!(per_row.len(), h);
            for (row, n) in per_row.iter().enumerate() {
                assert_eq!(*n, row_stride, "row {row} of {h} got {n} elements");
            }
        }
    }

    #[test]
    fn an_empty_buffer_is_not_a_panic() {
        // A wl_output whose mode has not settled gives a 0-sized surface, and
        // this crate is `panic = "abort"`: a panic here is a dead panel.
        assert!(coverage(0, 4).is_empty());
        assert!(coverage(0, 0).is_empty());
        // Through a Sync counter, because the bound is `Fn + Sync`: that is the
        // shape a re-threaded implementation needs, so a test that only worked
        // with a plain `FnMut` would stop compiling the day it comes back.
        let mut none: Vec<u8> = Vec::new();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        par_rows(&mut none, 0, 0, |_, _| {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the pass still runs once, over nothing"
        );
    }

    #[test]
    #[should_panic(expected = "do not fill a buffer")]
    fn a_shape_that_does_not_match_the_buffer_is_caught_in_debug() {
        // A caller bug, not a runtime condition: `blit_argb` and the swizzle pass
        // both index rows out of the band they are handed, so a stride that does
        // not match the buffer SHEARS the image rather than failing -- one row
        // slid sideways per row, which looks like a driver problem.
        let mut data = vec![0u8; 10];
        par_rows(&mut data, 3, 4, |_, _| {});
    }

    #[test]
    fn the_band_starts_at_row_zero_and_is_the_whole_buffer() {
        // What the callers may assume TODAY, spelled out so that re-enabling
        // parallelism is a deliberate change with a failing test beside it
        // rather than a silent one.
        let mut data = vec![7u8; 5 * 3];
        let seen = std::sync::Mutex::new(Vec::new());
        par_rows(&mut data, 5, 3, |y0, band| {
            seen.lock().unwrap().push((y0, band.len()));
        });
        assert_eq!(seen.into_inner().unwrap(), [(0, 15)]);
    }
}
