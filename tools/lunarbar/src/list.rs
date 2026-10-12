//! Keyboard, wheel and pointer navigation of a scrolling row list.
//!
//! Three clients draw the same thing — a panel with a fixed number of visible
//! rows over a longer list — and all three have to agree on what Down does at
//! the bottom, what a Page key does near the end, where a click lands and how
//! much a wheel notch is worth. This was written for `lunarrun` and lived
//! inside it; `lunarfile` needs the identical answers, so it moved here rather
//! than being copied, because every function below is the kind whose edge cases
//! are easy to get subtly wrong once and impossible to keep in sync twice.
//!
//! The page size and the row height are **parameters**, not constants: the
//! launcher shows 8 rows of 40 px and the file browser shows 14 of 30, and a
//! module-level constant would have silently given one of them the other's
//! geometry. Everything here is pure arithmetic — no Wayland, no I/O — so the
//! invariants are tested directly.

/// How far the ends of the result list are from each other.
///
/// Up/Down wrap, because one step off the end of a short list is a reach for
/// the other end and every launcher does it. Page Up/Down **stop**: with
/// `rem_euclid` they wrapped too, and a wrap of eight rows does not land on an
/// end, it lands eight from it. Page Up on the first row of a twenty-row list
/// selected **row twelve**, in the middle of the list, and Page Down on the
/// last one selected row seven -- a jump nobody asked for to a place nobody
/// could predict. Home/End are the keys for the ends; a page key that cannot
/// advance a page stays where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Arrow keys and Tab: falling off one end arrives at the other.
    Wrap,
    /// Page keys, Home/End: the ends are walls.
    Clamp,
}

/// The `(sel, top)` after moving the selection by `delta` over `n` rows, with
/// `page` rows visible at a time.
///
/// `top` is the first visible row, and it only ever moves as much as it must to
/// keep `sel` on screen: the list does not recentre under the cursor, which is
/// what makes holding Down read as a cursor walking down a still list until it
/// reaches the bottom edge.
pub fn move_selection(
    sel: usize,
    top: usize,
    n: usize,
    delta: i32,
    step: Step,
    page: usize,
) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    // A zero page would divide by zero below and means "nothing is visible",
    // which no caller can draw; one row is the smallest thing that can be.
    let page = page.max(1);
    let last = n as i32 - 1;
    // Neither arm needs `sel` to be in range first: `rem_euclid` and `clamp`
    // both bring any value back inside, which is what makes this total when the
    // list shrank under a selection.
    let want = (sel as i32).saturating_add(delta);
    let sel = match step {
        Step::Wrap => want.rem_euclid(n as i32) as usize,
        Step::Clamp => want.clamp(0, last) as usize,
    };
    // A window that starts past the end is brought back first, so the caller
    // cannot be handed a `top` that shows rows which do not exist.
    let top = top.min(n.saturating_sub(page));
    // Then scroll by the least that shows the selection. `sel + 1 - page`
    // cannot underflow: this arm needs `sel >= top + page`, so `sel >= page`.
    let top = if sel < top {
        sel
    } else if sel >= top + page {
        sel + 1 - page
    } else {
        top
    };
    (sel, top)
}

/// Is `(x, y)` inside `rect` = `(x, y, w, h)`?
pub fn inside_rect(rect: (i32, i32, i32, i32), x: f64, y: f64) -> bool {
    let (rx, ry, rw, rh) = rect;
    let (x, y) = (x.floor() as i32, y.floor() as i32);
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

/// Which row `(x, y)` is over, given the panel rect, the y of the first row,
/// the row height, the scroll position and how many rows exist.
///
/// A click below the last drawn row -- the footer, or the empty space of a short
/// list -- is no row at all, which is what keeps it from launching whatever
/// happens to be selected.
#[allow(clippy::too_many_arguments)]
pub fn row_at_geom(
    panel: (i32, i32, i32, i32),
    row0_y: i32,
    row_h: i32,
    page: usize,
    top: usize,
    rows: usize,
    x: f64,
    y: f64,
) -> Option<usize> {
    if !inside_rect(panel, x, y) {
        return None;
    }
    // A zero or negative row height would divide by zero or map every y to the
    // same row; a caller that computed one has no rows to hit.
    if row_h <= 0 {
        return None;
    }
    let y = y.floor() as i32;
    if y < row0_y {
        return None;
    }
    let row = ((y - row0_y) / row_h) as usize + top;
    let shown = rows.min(top.saturating_add(page.max(1)));
    (row < shown).then_some(row)
}

/// The `(top, leftover)` after a wheel event of `delta`, with `acc` already
/// carrying what earlier events did not amount to a notch. `notch` is one
/// detent in `wl_pointer.axis` surface-local units (see `keys::WHEEL_NOTCH`).
///
/// The leftover is what is left BELOW a whole notch, and nothing else: a
/// high-resolution wheel sends fractions of a notch, and dropping them makes a
/// slow scroll never move, but a whole notch is spent whether or not the list
/// could move. Bank the refused ones instead and holding the wheel against the
/// top of the list builds a charge that swallows the first flick back down.
///
/// Arithmetic rather than a loop, so a compositor sending one enormous value
/// costs one division and not a billion iterations.
pub fn scroll_top(
    top: usize,
    acc: f64,
    delta: f64,
    rows: usize,
    page: usize,
    notch: f64,
) -> (usize, f64) {
    let max_top = rows.saturating_sub(page.max(1));
    let top = top.min(max_top);
    let total = acc + delta;
    // A non-finite value would poison the accumulator for the rest of the
    // session, so it is dropped rather than carried. A non-positive notch would
    // divide by zero or scroll backwards, and a NaN one compares false against
    // everything, so it is ruled out by `is_finite` before the comparison
    // rather than by it.
    if !total.is_finite() || !notch.is_finite() || notch <= 0.0 {
        return (top, 0.0);
    }
    let notches = (total / notch).trunc();
    let leftover = total - notches * notch;
    // Float-to-int casts saturate, so an absurd count cannot wrap.
    let top = if notches >= 0.0 {
        top.saturating_add(notches as usize).min(max_top)
    } else {
        top.saturating_sub(-notches as usize)
    };
    (top, leftover)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// What `lunarrun` shows, which is what these cases were written against.
    const MAX_ROWS: usize = 8;
    /// `lunarrun`'s row height.
    const ROW_H: i32 = 40;
    /// `keys::WHEEL_NOTCH`, repeated rather than imported so a change there
    /// cannot silently rescale these cases.
    const WHEEL_NOTCH: f64 = 15.0;

    fn mv(sel: usize, top: usize, n: usize, delta: i32, step: Step) -> (usize, usize) {
        move_selection(sel, top, n, delta, step, MAX_ROWS)
    }

    fn hit(
        panel: (i32, i32, i32, i32),
        row0: i32,
        top: usize,
        rows: usize,
        x: f64,
        y: f64,
    ) -> Option<usize> {
        row_at_geom(panel, row0, ROW_H, MAX_ROWS, top, rows, x, y)
    }

    fn wheel(top: usize, acc: f64, delta: f64, rows: usize) -> (usize, f64) {
        scroll_top(top, acc, delta, rows, MAX_ROWS, WHEEL_NOTCH)
    }

    // ── the selection ────────────────────────────────────────────────────────

    #[test]
    fn an_arrow_key_wraps_around_both_ends() {
        // One step off an end reaches the other: with four results, Up from the
        // first lands on the last, which is how every launcher behaves.
        assert_eq!(mv(0, 0, 4, -1, Step::Wrap).0, 3);
        assert_eq!(mv(3, 0, 4, 1, Step::Wrap).0, 0);
        assert_eq!(mv(1, 0, 4, 1, Step::Wrap).0, 2);
        assert_eq!(mv(1, 0, 4, -1, Step::Wrap).0, 0);
        // A single result: every step stays on it.
        assert_eq!(mv(0, 0, 1, 1, Step::Wrap).0, 0);
        assert_eq!(mv(0, 0, 1, -1, Step::Wrap).0, 0);
    }

    #[test]
    fn a_page_key_stops_at_the_end_instead_of_landing_in_the_middle() {
        // THE REGRESSION. Page Up and Page Down went through `rem_euclid` like
        // the arrows, and a wrap of eight rows does not land on an end -- it
        // lands eight rows from it. On a twenty-row list, Page Up on the first
        // row selected row TWELVE and Page Down on the last selected row SEVEN:
        // a jump into the middle of the list that nobody could predict.
        let n = 20;
        assert_eq!(mv(0, 0, n, -(MAX_ROWS as i32), Step::Clamp).0, 0);
        assert_eq!(mv(n - 1, 0, n, MAX_ROWS as i32, Step::Clamp).0, n - 1);
        // What it used to do, kept here so the fix cannot be quietly undone.
        assert_eq!((0i32 - MAX_ROWS as i32).rem_euclid(n as i32), 12);
        assert_eq!((n as i32 - 1 + MAX_ROWS as i32).rem_euclid(n as i32), 7);

        // A page from the middle still moves a full page.
        assert_eq!(mv(10, 5, n, MAX_ROWS as i32, Step::Clamp).0, 18);
        assert_eq!(mv(10, 5, n, -(MAX_ROWS as i32), Step::Clamp).0, 2);
        // And a page that would overshoot goes exactly to the end.
        assert_eq!(mv(15, 8, n, MAX_ROWS as i32, Step::Clamp).0, n - 1);
        assert_eq!(mv(3, 0, n, -(MAX_ROWS as i32), Step::Clamp).0, 0);
    }

    #[test]
    fn the_window_scrolls_by_the_least_that_keeps_the_selection_visible() {
        // Walking down a long list: `top` stays put until the selection reaches
        // the bottom edge, then follows it one row at a time. A window that
        // recentred would make the whole list jump under every keypress.
        let n = 40;
        let (mut sel, mut top) = (0usize, 0usize);
        for _ in 0..(MAX_ROWS - 1) {
            (sel, top) = mv(sel, top, n, 1, Step::Wrap);
            assert_eq!(top, 0, "the list must not move while sel is on screen");
        }
        assert_eq!(sel, MAX_ROWS - 1);
        (sel, top) = mv(sel, top, n, 1, Step::Wrap);
        assert_eq!((sel, top), (MAX_ROWS, 1));

        // The selection is always inside the window, whatever the step.
        for &step in &[Step::Wrap, Step::Clamp] {
            for start in 0..n {
                for delta in [-(MAX_ROWS as i32), -3, -1, 1, 3, MAX_ROWS as i32] {
                    let (s, t) = mv(start, start.saturating_sub(2), n, delta, step);
                    assert!(s >= t && s < t + MAX_ROWS, "{s} outside {t}..+{MAX_ROWS}");
                    assert!(
                        t + MAX_ROWS <= n.max(MAX_ROWS),
                        "top {t} scrolled past the end"
                    );
                }
            }
        }
    }

    #[test]
    fn a_list_with_nothing_in_it_moves_nowhere() {
        // Enter on an empty list closes the overlay rather than launching row 0,
        // so this must not hand back a row that `row_item` would resolve.
        assert_eq!(mv(0, 0, 0, 1, Step::Wrap), (0, 0));
        assert_eq!(mv(5, 3, 0, -1, Step::Clamp), (0, 0));
    }

    #[test]
    fn a_selection_left_past_the_end_is_brought_back_in() {
        // Refilter resets sel to 0, but a shorter list arriving any other way
        // (a window closing, a filter applied from argv) must not leave sel out
        // of range and index nothing.
        let (sel, top) = mv(30, 25, 4, 1, Step::Wrap);
        assert!(sel < 4, "sel {sel} is past the end of a 4-row list");
        assert!(top < 4);
        let (sel, _) = mv(30, 25, 4, -1, Step::Clamp);
        assert!(sel < 4);
        // Total, not merely usually right: an absurd selection must come back in
        // range rather than overflow the i32 the step is computed in.
        for &step in &[Step::Wrap, Step::Clamp] {
            for sel in [i32::MAX as usize, usize::MAX, usize::MAX / 2] {
                let (s, t) = mv(sel, 0, 4, 1, step);
                assert!(s < 4, "sel {sel} came back as {s}");
                assert!(t < 4);
            }
        }
    }

    // ── the page size is the caller's, not a constant ────────────────────────

    #[test]
    fn the_window_is_as_tall_as_the_caller_says_and_not_one_fixed_size() {
        // Why this module takes `page` at all: the launcher shows 8 rows and
        // the file browser 14, and the one constant this code used to carry
        // would have given one of them the other's scrolling. With 14 visible,
        // walking to row 13 must not scroll; row 14 scrolls by exactly one.
        let n = 40;
        let (mut sel, mut top) = (0usize, 0usize);
        for _ in 0..13 {
            (sel, top) = move_selection(sel, top, n, 1, Step::Wrap, 14);
            assert_eq!(top, 0, "a 14-row window must not move until row 14");
        }
        assert_eq!((sel, top), (13, 0));
        (sel, top) = move_selection(sel, top, n, 1, Step::Wrap, 14);
        assert_eq!((sel, top), (14, 1));
        // And the same list with an 8-row window has already scrolled by then.
        let (_, top8) = move_selection(13, 6, n, 1, Step::Wrap, 8);
        assert_eq!(top8, 7);

        // A page of zero is not drawable; it must not divide by zero or leave
        // the selection outside a window of no rows.
        let (s, t) = move_selection(5, 0, 40, 1, Step::Wrap, 0);
        assert_eq!((s, t), (6, 6));
        assert_eq!(scroll_top(0, 0.0, WHEEL_NOTCH, 20, 0, WHEEL_NOTCH).0, 1);
        assert_eq!(
            row_at_geom((0, 0, 100, 100), 0, ROW_H, 0, 0, 20, 10.0, 10.0),
            Some(0)
        );
    }

    // ── hit testing ──────────────────────────────────────────────────────────

    const PANEL: (i32, i32, i32, i32) = (100, 50, 400, 300);

    #[test]
    fn the_column_one_past_the_panel_is_outside_it() {
        // `w` is a WIDTH: a panel at x=100 of width 400 owns 100..500, and 500
        // is the scrim. With `<=` that column swallowed the click that should
        // have dismissed the overlay.
        assert!(inside_rect(PANEL, 100.0, 50.0));
        assert!(inside_rect(PANEL, 499.0, 349.0));
        assert!(!inside_rect(PANEL, 500.0, 200.0));
        assert!(!inside_rect(PANEL, 300.0, 350.0));
        assert!(!inside_rect(PANEL, 99.0, 200.0));
        assert!(!inside_rect(PANEL, 300.0, 49.0));
        // The pointer never entered: (-1,-1) must read as the scrim, or the
        // first click before any Motion would land on a row.
        assert!(!inside_rect(PANEL, -1.0, -1.0));
        // Sub-pixel coordinates floor, so 499.9 is still column 499.
        assert!(inside_rect(PANEL, 499.9, 349.9));
    }

    #[test]
    fn each_row_band_maps_to_its_own_row() {
        let row0 = 120;
        for row in 0..5usize {
            let top_y = row0 + row as i32 * ROW_H;
            for dy in [0, 1, ROW_H / 2, ROW_H - 1] {
                assert_eq!(
                    hit(PANEL, row0, 0, 5, 200.0, (top_y + dy) as f64),
                    Some(row),
                    "row {row} at +{dy}"
                );
            }
        }
        // Above the first row is the search field: inert, not row 0.
        assert_eq!(hit(PANEL, row0, 0, 5, 200.0, 119.0), None);
        // A row height of zero would divide by zero; it is no row instead.
        assert_eq!(
            row_at_geom(PANEL, row0, 0, MAX_ROWS, 0, 5, 200.0, 130.0),
            None
        );
        assert_eq!(
            row_at_geom(PANEL, row0, -40, MAX_ROWS, 0, 5, 200.0, 130.0),
            None
        );
    }

    #[test]
    fn a_click_below_the_last_row_is_no_row_at_all() {
        let row0 = 120;
        // Three results: the space under them is the footer, and a click there
        // must not launch whatever happens to be selected.
        assert_eq!(hit(PANEL, row0, 0, 3, 200.0, 239.0), Some(2));
        assert_eq!(hit(PANEL, row0, 0, 3, 200.0, 240.0), None);
        // Scrolled: only the MAX_ROWS actually drawn are hittable, even when
        // the panel is tall enough for the click to be inside it.
        let rows = 20;
        let tall = (100, 50, 400, ROW_H * (MAX_ROWS as i32 + 4));
        assert_eq!(hit(tall, row0, 5, rows, 200.0, row0 as f64), Some(5));
        let last = row0 + (MAX_ROWS as i32 - 1) * ROW_H;
        assert_eq!(
            hit(tall, row0, 5, rows, 200.0, last as f64),
            Some(5 + MAX_ROWS - 1)
        );
        let past = row0 + MAX_ROWS as i32 * ROW_H;
        assert!(
            inside_rect(tall, 200.0, past as f64),
            "the click is in the panel"
        );
        assert_eq!(
            hit(tall, row0, 5, rows, 200.0, past as f64),
            None,
            "only MAX_ROWS rows are drawn, so only MAX_ROWS are hittable"
        );
        // And outside the panel is never a row, however the rows line up.
        assert_eq!(hit(PANEL, row0, 0, 20, 500.0, 130.0), None);
        assert_eq!(hit(PANEL, row0, 0, 20, -1.0, -1.0), None);
        assert_eq!(hit(PANEL, row0, 0, 0, 200.0, 130.0), None);
    }

    // ── the wheel ────────────────────────────────────────────────────────────

    #[test]
    fn the_wheel_stops_at_both_ends_of_the_list() {
        let rows = 20;
        let max_top = rows - MAX_ROWS;
        let (top, _) = wheel(0, 0.0, WHEEL_NOTCH, rows);
        assert_eq!(top, 1);
        let (top, _) = wheel(0, 0.0, WHEEL_NOTCH * 5.0, rows);
        assert_eq!(top, 5);
        // Past the end it stops, and does not wrap or overflow.
        let (top, _) = wheel(0, 0.0, WHEEL_NOTCH * 100.0, rows);
        assert_eq!(top, max_top);
        let (top, _) = wheel(max_top, 0.0, -WHEEL_NOTCH * 100.0, rows);
        assert_eq!(top, 0);
        // A list that fits needs no scrolling at all.
        for r in 0..=MAX_ROWS {
            assert_eq!(wheel(0, 0.0, WHEEL_NOTCH * 3.0, r).0, 0, "{r} rows");
        }
    }

    #[test]
    fn fractions_of_a_notch_add_up_instead_of_being_dropped() {
        // A high-resolution wheel sends a fraction per event; throwing the
        // remainder away would make a slow scroll never move at all.
        let rows = 40;
        let (mut top, mut acc) = (0usize, 0.0);
        for _ in 0..4 {
            (top, acc) = wheel(top, acc, WHEEL_NOTCH / 4.0, rows);
        }
        assert_eq!(top, 1, "four quarter-notches are one notch");
        // Half a notch each way nets out to no movement.
        let (t2, _) = wheel(top, acc, WHEEL_NOTCH / 2.0, rows);
        assert_eq!(t2, top);
    }

    #[test]
    fn holding_the_wheel_against_an_end_builds_up_no_charge() {
        // Otherwise the leftover grows for as long as the wheel turns, and the
        // first flick the other way unwinds all of it at once.
        let rows = 20;
        let (mut top, mut acc) = (0usize, 0.0);
        for _ in 0..50 {
            (top, acc) = wheel(top, acc, -WHEEL_NOTCH, rows);
        }
        assert_eq!(top, 0);
        assert!(acc.abs() <= WHEEL_NOTCH, "leftover grew to {acc}");
        let (top, _) = wheel(top, acc, WHEEL_NOTCH, rows);
        assert_eq!(top, 1, "the first notch back must move exactly one row");

        // One big flick against the end is the case that actually builds a
        // charge: ten notches' worth at the top, then one notch back. Without
        // the bound the leftover eats that notch and the list does not move.
        let (top, acc) = wheel(0, 0.0, -WHEEL_NOTCH * 10.0, rows);
        assert_eq!(top, 0);
        let (top, _) = wheel(top, acc, WHEEL_NOTCH, rows);
        assert_eq!(top, 1, "a notch after a big flick must still move a row");
        // Same at the far end.
        let (top, acc) = wheel(0, 0.0, WHEEL_NOTCH * 50.0, rows);
        assert_eq!(top, rows - MAX_ROWS);
        let (top, _) = wheel(top, acc, -WHEEL_NOTCH, rows);
        assert_eq!(top, rows - MAX_ROWS - 1);
    }

    #[test]
    fn a_nonsense_wheel_value_does_not_poison_the_accumulator() {
        // One NaN carried in `acc` would make every later comparison false and
        // the wheel dead for the rest of the session.
        let rows = 20;
        let (top, acc) = wheel(3, 0.0, f64::NAN, rows);
        assert_eq!(top, 3);
        assert_eq!(acc, 0.0);
        let (top, acc) = wheel(3, 0.0, f64::INFINITY, rows);
        assert_eq!((top, acc), (3, 0.0));
        let (top, acc) = wheel(3, 0.0, f64::NEG_INFINITY, rows);
        assert_eq!((top, acc), (3, 0.0));
        // And the wheel still works afterwards.
        assert_eq!(wheel(top, acc, WHEEL_NOTCH, rows).0, 4);
        // An absurd but finite value saturates at the ends instead of wrapping.
        assert_eq!(wheel(0, 0.0, 1e300, rows).0, rows - MAX_ROWS);
        assert_eq!(wheel(rows - MAX_ROWS, 0.0, -1e300, rows).0, 0);
    }

    #[test]
    fn a_scroll_position_past_the_end_is_brought_back_in() {
        // The list shrinks under the wheel when a filter is typed, so `top` can
        // arrive pointing at rows that no longer exist.
        let (top, _) = wheel(30, 0.0, 0.0, 12);
        assert_eq!(top, 12 - MAX_ROWS);
        let (top, _) = wheel(30, 0.0, WHEEL_NOTCH, 12);
        assert_eq!(top, 12 - MAX_ROWS);
        let (top, _) = wheel(30, 0.0, -WHEEL_NOTCH, 12);
        assert_eq!(top, 12 - MAX_ROWS - 1);
        // A list that no longer overflows scrolls back to the top.
        assert_eq!(wheel(9, 0.0, 0.0, 3).0, 0);
        assert_eq!(wheel(9, 0.0, 0.0, 0).0, 0);
    }

    #[test]
    fn a_wheel_with_no_notch_moves_nothing_instead_of_dividing_by_zero() {
        // The notch is the caller's too (`keys::WHEEL_NOTCH`), so a zero or a
        // negative one has to be refused here rather than divide by zero or
        // scroll the list backwards.
        assert_eq!(scroll_top(3, 7.0, 15.0, 20, MAX_ROWS, 0.0), (3, 0.0));
        assert_eq!(scroll_top(3, 7.0, 15.0, 20, MAX_ROWS, -15.0), (3, 0.0));
        assert_eq!(scroll_top(3, 7.0, 15.0, 20, MAX_ROWS, f64::NAN), (3, 0.0));
        // Half the notch is half as many rows per event, which is the whole
        // point of the parameter.
        assert_eq!(scroll_top(0, 0.0, 15.0, 40, MAX_ROWS, 7.5), (2, 0.0));
    }
}
