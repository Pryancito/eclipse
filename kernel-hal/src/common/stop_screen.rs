//! Layout for the full-screen kernel stop report, and the cursor that makes it
//! **cumulative**.
//!
//! On a machine with a monitor and no serial capture, the stop screen is the
//! whole crash report: somebody photographs it, and whatever is not on it did
//! not happen as far as anyone can tell. That made the erasing behaviour of
//! the old `panic_banner` expensive. It filled the ENTIRE framebuffer on every
//! call, so the last writer won:
//!
//! * `cpu0` panics, paints `KERNEL PANIC cpu=0 ... <why>`;
//! * `cpu2` takes an unresolved kernel #PF holding the heap lock and reports it
//!   to **serial only**, so nothing of it reaches the screen at all;
//! * eight seconds later the deadlock detector fires, repaints the whole
//!   screen, and the photograph shows a convoy of CPUs stuck on a lock with a
//!   verdict that has to offer AB-BA as a possibility — because the two
//!   reports that named the actual first cause are gone.
//!
//! So the screen is claimed once and then appended to. The first report paints
//! the ground and the title; every later one lands underneath, in the order the
//! reports happened, and the footer says how many did not fit rather than
//! letting a reader take the last line for the end of the story.
//!
//! Everything here is geometry over atomics: no locks, no allocation, nothing
//! that can fault. It has to be, because its callers are the panic handler, the
//! kernel-#PF reporter and the deadlock hook, each of which may be running with
//! the heap lock held by a CPU that is never going to release it.

use core::sync::atomic::{AtomicU32, Ordering};

/// Character cell size of the stop screen's font, in pixels.
pub const CHAR_W: u32 = 8;
/// See [`CHAR_W`].
pub const CHAR_H: u32 = 16;

/// Left and right margin, in character cells.
pub const MARGIN_COLS: u32 = 2;

/// Rows of the title bar, plus the blank row under it.
const TITLE_ROWS: u32 = 2;

/// Rows reserved at the bottom for the footer.
const FOOTER_ROWS: u32 = 2;

/// Next free body row, as a row index from the top of the screen. Zero means
/// the screen has not been claimed yet, which is the state the very first
/// report finds.
static NEXT_ROW: AtomicU32 = AtomicU32::new(0);

/// Reports that found no room left. Counted rather than dropped silently: a
/// reader who cannot see the count has no way to tell a report that ended from
/// one that was cut off.
static CUT_REPORTS: AtomicU32 = AtomicU32::new(0);

/// Where one report's text goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// First body row for this report.
    pub row0: u32,
    /// Rows this report may use, counted from `row0`.
    pub rows: u32,
    /// Columns inside the margins.
    pub cols: u32,
    /// Whether this report claimed the screen (so the caller fills the ground
    /// and draws the title bar) or is appending under an earlier one.
    pub first: bool,
}

/// Usable geometry of a `sw x sh` pixel screen: `(cols, rows)` inside the
/// margins, with the footer's rows already taken out.
///
/// Returns `(0, 0)` for a screen too small to hold a title, one body row and a
/// footer — a caller that gets that draws nothing rather than scribbling over
/// the edges.
pub fn geometry(sw: u32, sh: u32) -> (u32, u32) {
    let cols = sw / CHAR_W;
    let rows = sh / CHAR_H;
    if cols <= MARGIN_COLS * 2 || rows <= TITLE_ROWS + FOOTER_ROWS {
        return (0, 0);
    }
    (cols - MARGIN_COLS * 2, rows - TITLE_ROWS - FOOTER_ROWS)
}

/// Claim room for one report on a `sw x sh` screen.
///
/// The first call takes the screen; later calls append. `None` means the screen
/// is full (or too small to use), and the report is counted as cut.
pub fn claim(sw: u32, sh: u32) -> Option<Placement> {
    let (cols, rows) = geometry(sw, sh);
    if cols == 0 || rows == 0 {
        CUT_REPORTS.fetch_add(1, Ordering::SeqCst);
        return None;
    }
    let body_top = TITLE_ROWS;
    let body_end = TITLE_ROWS + rows;
    loop {
        let cur = NEXT_ROW.load(Ordering::SeqCst);
        let first = cur == 0;
        // A blank row between reports, so two stop reports do not read as one.
        let row0 = if first { body_top } else { cur + 1 };
        if row0 >= body_end {
            CUT_REPORTS.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        // Claim the whole rest of the screen up front and hand back what was
        // not used, so a second CPU reporting at the same moment cannot be
        // given rows this one is about to draw into.
        if NEXT_ROW
            .compare_exchange(cur, body_end, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(Placement {
                row0,
                rows: body_end - row0,
                cols,
                first,
            });
        }
    }
}

/// Give back the rows a report did not use, so the next one starts right under
/// it instead of at the bottom of the screen. `end_row` is what [`lay_out`]
/// returned.
pub fn release(end_row: u32) {
    NEXT_ROW.store(end_row, Ordering::SeqCst);
}

/// Lay `text` out inside `place`, calling `put(col, row, byte)` for every
/// character that fits. Returns the first row after the text.
///
/// `\n` breaks a line and a line longer than `place.cols` wraps, which is what
/// lets a caller hand this a multi-line report without measuring it. Text that
/// runs past the bottom is dropped and the report counted as cut — the same
/// honesty the footer needs.
pub fn lay_out(text: &str, place: &Placement, mut put: impl FnMut(u32, u32, u8)) -> u32 {
    let end = place.row0 + place.rows;
    let mut row = place.row0;
    let mut col = 0u32;
    let mut cut = false;
    for &b in text.as_bytes() {
        if b == b'\r' {
            continue;
        }
        if b == b'\n' {
            row += 1;
            col = 0;
            continue;
        }
        if col >= place.cols {
            row += 1;
            col = 0;
        }
        if row >= end {
            cut = true;
            break;
        }
        put(col, row, b);
        col += 1;
    }
    if cut {
        CUT_REPORTS.fetch_add(1, Ordering::SeqCst);
        return end;
    }
    // A report that ended mid-line still owns that whole row.
    (row + if col > 0 { 1 } else { 0 }).min(end)
}

/// The row the footer occupies on a screen `sh` pixels tall.
///
/// Row arithmetic, and the one place it is done: `sh - 2 * CHAR_H` is NOT the
/// same row on a height that is not a whole number of rows (1080 is 67.5 of
/// them), and the two differ by half a row -- enough for the footer to land on
/// top of the last body line.
pub fn footer_row(sh: u32) -> u32 {
    (sh / CHAR_H).saturating_sub(FOOTER_ROWS)
}

/// How many reports did not fit on the screen, for the footer.
pub fn cut_reports() -> u32 {
    CUT_REPORTS.load(Ordering::SeqCst)
}

/// Forget the screen, so the next report claims it again.
///
/// Called when a fault is CONTAINED and the machine goes on running (see
/// `oops::try_contain`): the screen the fault path painted is void, the
/// compositor repaints over it, and a later stop must claim it from the top
/// again. Never within one crash -- that is the whole point.
pub fn reset() {
    NEXT_ROW.store(0, Ordering::SeqCst);
    CUT_REPORTS.store(0, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    //! The point of every one of these is the same: a photograph of a wedged
    //! machine is a single frame, and what the frame leaves out is gone.

    use super::*;
    use alloc::vec::Vec;

    /// Serialise: the statics are process-wide and every test here claims them.
    static LOCK: spin::Mutex<()> = spin::Mutex::new(());

    /// A 1920x1080 screen, which is what the reports in question were taken on.
    const SW: u32 = 1920;
    const SH: u32 = 1080;

    fn draw(text: &str) -> Option<(Placement, Vec<(u32, u32, u8)>, u32)> {
        let place = claim(SW, SH)?;
        let mut cells = Vec::new();
        let end = lay_out(text, &place, |c, r, b| cells.push((c, r, b)));
        release(end);
        Some((place, cells, end))
    }

    fn row_text(cells: &[(u32, u32, u8)], row: u32) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        for (_, _, b) in cells.iter().filter(|(_, r, _)| *r == row) {
            s.push(*b as char);
        }
        s
    }

    /// The first report claims the screen (the caller fills the ground and
    /// draws the title); the second does not, or it would erase the first.
    /// This is the whole bug: the deadlock banner repainted over the panic
    /// that caused it.
    #[test]
    fn only_the_first_report_claims_the_screen() {
        let _g = LOCK.lock();
        reset();
        let (first, _, _) = draw("KERNEL PANIC cpu=0").unwrap();
        let (second, _, _) = draw("DEADLOCK: spinlock(s) stuck >8s").unwrap();
        assert!(first.first, "the first report paints the ground");
        assert!(
            !second.first,
            "a later report must not repaint the screen -- that is what erased \
             the panic that explained the deadlock"
        );
    }

    /// ... and it lands UNDER the first, in the order the reports happened.
    #[test]
    fn later_reports_land_under_the_earlier_ones() {
        let _g = LOCK.lock();
        reset();
        let (_, first_cells, first_end) = draw("one\ntwo").unwrap();
        let (second, second_cells, _) = draw("three").unwrap();
        let first_last = first_cells.iter().map(|(_, r, _)| *r).max().unwrap();
        assert!(
            second.row0 > first_last,
            "second report started at row {} but the first still had row {}",
            second.row0,
            first_last
        );
        assert_eq!(second.row0, first_end + 1, "one blank row between reports");
        assert_eq!(row_text(&second_cells, second.row0), "three");
    }

    /// A report that ends mid-line still owns that row: the next one must not
    /// overprint its tail.
    #[test]
    fn a_report_ending_mid_line_keeps_its_row() {
        let _g = LOCK.lock();
        reset();
        let (place, _, end) = draw("no trailing newline").unwrap();
        assert_eq!(end, place.row0 + 1);
    }

    /// Long lines wrap instead of running off the right edge, so a wide report
    /// (the deadlock banner's `non-acker` lines) stays readable.
    #[test]
    fn a_line_longer_than_the_box_wraps() {
        let _g = LOCK.lock();
        reset();
        let (cols, _) = geometry(SW, SH);
        let long: alloc::string::String = core::iter::repeat('x')
            .take(cols as usize + 3)
            .collect::<alloc::string::String>();
        let (place, cells, _) = draw(&long).unwrap();
        assert_eq!(row_text(&cells, place.row0).len(), cols as usize);
        assert_eq!(row_text(&cells, place.row0 + 1), "xxx");
        assert!(
            cells.iter().all(|(c, _, _)| *c < cols),
            "a character landed outside the margins"
        );
    }

    /// When the screen fills up, the reports that did not fit are COUNTED. A
    /// stop screen that silently ends mid-report reads like a complete one.
    #[test]
    fn reports_that_do_not_fit_are_counted_not_dropped_silently() {
        let _g = LOCK.lock();
        reset();
        let (_, rows) = geometry(SW, SH);
        let filler: alloc::string::String = core::iter::repeat('a')
            .take(rows as usize + 10)
            .map(|c| {
                let mut s = alloc::string::String::new();
                s.push(c);
                s.push('\n');
                s
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(cut_reports(), 0);
        draw(&filler).unwrap();
        assert_eq!(cut_reports(), 1, "the overlong report itself was cut");
        assert!(
            draw("this one has nowhere to go").is_none(),
            "a full screen must refuse, not overdraw the footer"
        );
        assert_eq!(cut_reports(), 2);
    }

    /// Text never reaches the footer's rows or the title bar's.
    #[test]
    fn the_body_stays_clear_of_the_title_and_the_footer() {
        let _g = LOCK.lock();
        reset();
        let (_, rows) = geometry(SW, SH);
        let tall: alloc::string::String = core::iter::repeat("line\n")
            .take(rows as usize * 2)
            .collect();
        let (_, cells, _) = draw(&tall).unwrap();
        let top = cells.iter().map(|(_, r, _)| *r).min().unwrap();
        let bottom = cells.iter().map(|(_, r, _)| *r).max().unwrap();
        assert_eq!(top, TITLE_ROWS, "the body starts under the title bar");
        assert!(
            bottom < TITLE_ROWS + rows,
            "row {} is inside the footer's rows",
            bottom
        );
        assert!(bottom * CHAR_H + CHAR_H <= SH, "a row ran off the screen");
    }

    /// The body never reaches the footer's row, including on a height that is
    /// not a whole number of character rows. 1080 is 67.5 of them, and the
    /// footer used to be placed with `sh - 2 * CHAR_H` while the body was
    /// placed by row -- half a row apart, which is an overprinted last line.
    #[test]
    fn the_footer_row_is_below_every_body_row() {
        let _g = LOCK.lock();
        for sh in [1080u32, 1050, 768, 1200, 1051] {
            reset();
            let (_, rows) = geometry(SW, sh);
            assert!(rows > 0, "sh={} should be usable", sh);
            let tall: alloc::string::String =
                core::iter::repeat("x\n").take(rows as usize * 2).collect();
            let place = claim(SW, sh).unwrap();
            let mut bottom = 0;
            let end = lay_out(&tall, &place, |_, r, _| bottom = bottom.max(r));
            release(end);
            assert!(
                bottom < footer_row(sh),
                "sh={}: last body row {} is at or past the footer row {}",
                sh,
                bottom,
                footer_row(sh)
            );
        }
    }

    /// A screen too small for a title, a line and a footer gets nothing drawn
    /// on it rather than a scribble over its edges.
    #[test]
    fn a_screen_too_small_to_report_on_draws_nothing() {
        let _g = LOCK.lock();
        reset();
        assert_eq!(
            geometry(CHAR_W * 4, SH),
            (0, 0),
            "narrower than the margins"
        );
        assert_eq!(geometry(SW, CHAR_H * 3), (0, 0), "no room under the title");
        assert!(claim(SW, CHAR_H * 3).is_none());
        assert_eq!(cut_reports(), 1);
    }
}
