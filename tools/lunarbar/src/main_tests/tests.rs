//! Tests for the panel binary (`main.rs`), moved out of it so the file
//! stays under its size ceiling (see `xtask/src/tamano.rs`).

use super::*;

fn apps(n: usize) -> PopupKind {
    PopupKind::Apps {
        all: (0..n)
            .map(|i| apps::AppEntry {
                name: format!("app {i}"),
                ..apps::AppEntry::default()
            })
            .collect(),
        filter: String::new(),
        visible: (0..n).collect(),
        sel: 0,
        scroll: 0,
    }
}

const KINDS: [fn() -> PopupKind; 4] = [
    || PopupKind::Calendar { year: 2026, month: 8 },
    || PopupKind::PowerMenu,
    || PopupKind::TaskMenu { tid: 1, title: String::new() },
    || PopupKind::Volume { level: 50 },
];

/// The panel is sized from the result list, and the scroll clamp, the
/// keyboard navigation and the drawing all re-derive the row count from
/// the panel HEIGHT. If those two disagree the menu scrolls past its own
/// last row, or hides one.
#[test]
fn panel_height_and_row_count_agree() {
    for oh in [600, 768, 1080, 1440, 2160] {
        let fits = apps_rows_fit(oh, 34);
        for n in [0, 1, 2, 7, 40, 500] {
            let (_, ph) = popup_size(&apps(n), oh, 34);
            let shown = n.clamp(1, fits);
            assert_eq!(
                panel_rows_fit(ph),
                shown,
                "oh={oh} n={n}: panel {ph}px was sized for {shown} rows"
            );
        }
    }
}

/// An empty result list still needs a row to say so in.
#[test]
fn an_empty_result_list_still_gets_a_row() {
    let (_, ph) = popup_size(&apps(0), 1080, 34);
    assert_eq!(panel_rows_fit(ph), 1);
    assert!(ph > APPS_HEADER_H + APPS_SEARCH_H);
}

/// The app menu must fit between the two bars: it is an xdg_popup now, so
/// a panel taller than the output is one the compositor has to slide or
/// flip, and either way the user loses rows off an edge.
#[test]
fn the_app_menu_fits_between_the_bars() {
    for oh in [480, 600, 768, 1080, 1440] {
        let bar_h = 34;
        let (_, ph) = popup_size(&apps(10_000), oh, bar_h);
        let span = (oh - bar_h - 6) - (bar_h + 8);
        assert!(ph <= span.max(1), "oh={oh}: panel {ph}px over a {span}px gap");
    }
}

/// Every kind must ask for a positive size: `xdg_positioner.set_size`
/// rejects zero, and a rejected positioner is a protocol error, which this
/// event loop exits on.
#[test]
fn every_popup_asks_for_a_positive_size() {
    for oh in [0, 1, 100, 1080] {
        for bar_h in [0, 34, 4096] {
            for mk in KINDS {
                let (w, h) = popup_size(&mk(), oh, bar_h);
                assert!(w > 0 && h > 0, "{w}x{h} at oh={oh} bar_h={bar_h}");
            }
            for n in [0, 3, 900] {
                let (w, h) = popup_size(&apps(n), oh, bar_h);
                assert!(w > 0 && h > 0, "apps({n}) {w}x{h} at oh={oh} bar_h={bar_h}");
            }
        }
    }
}

/// A shorter output means fewer rows, never more — the scroll window is
/// sized from this and a non-monotonic count would make it jump.
#[test]
fn a_shorter_output_never_fits_more_rows() {
    let mut last = usize::MAX;
    for oh in (200..=2160).rev().step_by(37) {
        let fits = apps_rows_fit(oh, 34);
        assert!(fits <= last, "oh={oh} fits {fits} > {last}");
        assert!(fits >= 1);
        last = fits;
    }
}

/// The search field's character cap is what the field can actually show.
/// If the cap were the larger of the two, typing would silently vanish.
#[test]
fn the_filter_cap_fits_the_search_field() {
    let field_px = APPS_PW - 20 - 24;
    assert!(APPS_FILTER_MAX as i32 * GLYPH_W <= field_px);
}

/// The drop shadow replaced the full-output scrim, so it has to actually
/// be there: alpha outside the panel that falls off with distance, and an
/// opaque panel inside. Alpha 0 all round would mean the menu floats with
/// nothing separating it from what is behind.
#[test]
fn the_panel_draws_a_shadow_that_fades_outward() {
    let (pw, ph) = (60, 40);
    let (px, py) = (POPUP_SHADOW, POPUP_SHADOW);
    let (w, h) = ((pw + 2 * POPUP_SHADOW) as usize, (ph + 2 * POPUP_SHADOW) as usize);
    let mut cv = Canvas::try_new(w, h).unwrap();
    draw_panel(&mut cv, px, py, pw, ph, 10);
    let mut buf = vec![0u8; w * h * 4];
    assert!(cv.blit_argb(&mut buf));
    let alpha = |x: i32, y: i32| buf[((y as usize * w) + x as usize) * 4 + 3];

    // Inside the panel: effectively opaque.
    assert!(alpha(px + pw / 2, py + ph / 2) > 240);
    // Just outside its left edge: the shadow's dark core.
    let near = alpha(px - 2, py + ph / 2);
    // Out at the margin: still shadow, but faint.
    let far = alpha(px - POPUP_SHADOW, py + ph / 2);
    assert!(near > 0, "no shadow beside the panel");
    assert!(far > 0, "no shadow at the margin");
    assert!(near > far, "shadow does not fade outward: {near} then {far}");
    // The very corner of the surface is past the shadow: clear.
    assert_eq!(alpha(0, 0), 0);
}

/// Every hit rect a popup returns has to land inside the panel it was
/// drawn in. A hit outside is one the pointer can never reach, because
/// the popup's input region stops at the panel.
#[test]
fn popup_hits_stay_inside_the_panel() {
    let (px, py) = (POPUP_SHADOW, POPUP_SHADOW);
    let mut icons = IconCache::default();
    let mut kinds: Vec<PopupKind> = KINDS.iter().map(|mk| mk()).collect();
    kinds.push(apps(6));
    for kind in kinds.iter_mut() {
        let (pw, ph) = popup_size(kind, 1080, 34);
        let (w, h) = ((pw + 2 * POPUP_SHADOW) as usize, (ph + 2 * POPUP_SHADOW) as usize);
        let mut cv = Canvas::try_new(w, h).unwrap();
        let hits = draw_popup(&mut cv, px, py, pw, ph, kind, &mut icons);
        assert!(!hits.is_empty(), "a popup with nothing to click");
        for (x0, y0, x1, y1, _) in hits {
            assert!(x1 > x0 && y1 > y0, "empty hit {x0},{y0}..{x1},{y1}");
            assert!(
                x0 >= px && y0 >= py && x1 <= px + pw && y1 <= py + ph,
                "hit {x0},{y0}..{x1},{y1} outside the {pw}x{ph} panel at {px},{py}"
            );
        }
    }
}

/// A title is mapped onto the bitmap font's repertoire: titles arrive as
/// arbitrary UTF-8 and every glyph the font does not have would otherwise
/// render as a full-width '?', one per codepoint.
#[test]
fn every_glyph_a_title_can_carry_is_mapped_or_dropped() {
    // The dashes Firefox and every other browser put in a title.
    assert_eq!(sanitize_title("page \u{2014} browser"), "page - browser");
    assert_eq!(
        sanitize_title("\u{2010}\u{2011}\u{2012}\u{2013}\u{2014}\u{2015}"),
        "------"
    );
    // Curly quotes, both pairs, and the ellipsis that is one codepoint.
    assert_eq!(sanitize_title("\u{2018}a\u{2019}"), "'a'");
    assert_eq!(sanitize_title("\u{201c}a\u{201d}"), "\"a\"");
    assert_eq!(sanitize_title("Saving\u{2026}"), "Saving...");
    // Invisible: zero-width marks, variation selectors, combining marks.
    assert_eq!(sanitize_title("a\u{200b}b\u{200f}c"), "abc");
    assert_eq!(sanitize_title("a\u{fe00}b\u{fe0f}c"), "abc");
    assert_eq!(sanitize_title("a\u{0300}b\u{036f}c"), "abc");
    // A control character becomes a space: a stray newline would draw a
    // second line of text bleeding out of the button.
    assert_eq!(sanitize_title("a\nb\tc"), "a b c");
    // Latin-1 has a glyph; anything above it collapses into ONE '?' per
    // run, so a CJK title is one mark and not one per character.
    assert_eq!(sanitize_title("café ñ"), "café ñ");
    assert_eq!(sanitize_title("\u{4e2d}\u{6587}\u{6587}"), "?");
    assert_eq!(sanitize_title("a\u{4e2d}b\u{6587}"), "a?b?");
    assert_eq!(sanitize_title("\u{1f600}\u{1f600}x"), "?x");
}

/// The button label: the window's own title where it has one, the app_id
/// otherwise, capped to what keeps a button a sane width, and the flag
/// that tells the hover tooltip there is more to show.
#[test]
fn a_button_is_labelled_by_its_title_and_capped_where_it_says_it_is() {
    assert_eq!(short_label("Title", "app.id"), ("Title".into(), false));
    // A title of spaces is no title: the app_id is the fallback.
    assert_eq!(short_label("   ", "app.id"), ("app.id".into(), false));
    assert_eq!(short_label("", "app.id"), ("app.id".into(), false));
    // Neither: a window still needs a word on it.
    assert_eq!(short_label("", ""), ("window".into(), false));
    assert_eq!(short_label("\u{200b}", ""), ("window".into(), false));
    // Exactly eighteen characters is not capped; nineteen is, and the cap
    // keeps eighteen of them plus the mark that says so.
    let (s, long) = short_label(&"x".repeat(18), "");
    assert_eq!((s.as_str(), long), (&*"x".repeat(18), false));
    let (s, long) = short_label(&"x".repeat(19), "");
    assert_eq!((s.as_str(), long), (&*format!("{}.", "x".repeat(18)), true));
    // The title is sanitized BEFORE it is capped, so the count is of
    // glyphs the font can draw and not of codepoints that vanish.
    assert_eq!(short_label("a\u{2014}b", "").0, "a-b");
    // A title made of nothing but invisible characters is a title as far
    // as the fallback is concerned -- `trim` does not strip a zero-width
    // space -- so what is left after sanitizing is the generic word, not
    // the app_id.
    assert_eq!(short_label(&"\u{200b}".repeat(40), "app").0, "window");
    // The source is the title whole, not trimmed into the label.
    assert_eq!(short_label("  spaced  ", "").0, "spaced");
}

/// A metric with no reading shows that it has none, rather than an empty
/// gap the eye reads as a zero.
#[test]
fn a_metric_with_no_reading_says_so() {
    assert_eq!(opt(Some(0)), "0");
    assert_eq!(opt(Some(97)), "97");
    assert_eq!(opt(None), "--");
}

/// The menu filter matches anywhere in the name and ignores case and the
/// accents a Spanish menu has, because that is how someone types.
#[test]
fn the_menu_filter_matches_the_way_someone_types_it() {
    let all: Vec<apps::AppEntry> = ["Calculadora", "Música", "Firefox"]
        .iter()
        .map(|n| apps::AppEntry {
            name: (*n).into(),
            ..apps::AppEntry::default()
        })
        .collect();
    // An empty filter is every app, in order.
    assert_eq!(filter_apps(&all, ""), [0, 1, 2]);
    // A filter of spaces is not an empty filter: the field's text is
    // matched as typed, so it matches no name at all.
    assert!(filter_apps(&all, "   ").is_empty());
    // Case and accents are ignored, on both sides.
    assert_eq!(filter_apps(&all, "MUSICA"), [1]);
    assert_eq!(filter_apps(&all, "músi"), [1]);
    // And it matches inside the name, not only at its start.
    assert_eq!(filter_apps(&all, "fox"), [2]);
    assert_eq!(filter_apps(&all, "cula"), [0]);
    assert!(filter_apps(&all, "zz").is_empty());
}

/// The app menu's panel is sized from its result list, and an empty list
/// still gets a row to say so in -- the row the drawing code then draws
/// the "no results" line into.
#[test]
fn an_empty_app_menu_is_still_a_row_tall() {
    let (_, ph) = popup_size(&apps(0), 1080, 34);
    assert!(
        ph >= APPS_HEADER_H + APPS_SEARCH_H + APPS_ROW_H + APPS_PAD,
        "a {ph}px panel has no room for a row"
    );
    // Every panel carries the padding below its last row.
    for n in [0, 1, 5] {
        let (_, ph) = popup_size(&apps(n), 1080, 34);
        let rows = panel_rows_fit(ph) as i32;
        assert_eq!(
            ph,
            APPS_HEADER_H + APPS_SEARCH_H + rows * APPS_ROW_H + APPS_PAD,
            "n={n}"
        );
    }
}

/// The row count a panel height yields has to be rows that FIT. The
/// client asks for a size, but the compositor configures the one it gives,
/// and the drawing, the scroll clamp and the keyboard navigation all
/// re-derive the count from whatever height arrived.
#[test]
fn a_panel_only_counts_the_rows_that_fit_inside_it() {
    for ph in 0..400 {
        let rows = panel_rows_fit(ph) as i32;
        // One row always, even where there is no room: there has to be
        // somewhere to say that nothing matched.
        assert!(rows >= 1, "ph={ph} counted no rows at all");
        if rows > 1 {
            let needed = APPS_HEADER_H + APPS_SEARCH_H + rows * APPS_ROW_H + APPS_PAD;
            assert!(needed <= ph, "ph={ph}: {rows} rows need {needed}px");
        }
    }
}

/// The calendar panel holds the grid it draws: seven columns and the six
/// rows it always draws, plus the padding around them. A panel short of a
/// row drops the last days of a month that needs it off the bottom.
#[test]
fn the_calendar_panel_holds_seven_columns_and_six_rows() {
    let (pw, ph) = popup_size(
        &PopupKind::Calendar {
            year: 2021,
            month: 0,
        },
        1080,
        34,
    );
    assert_eq!(pw, 7 * CAL_CELL_W + 2 * CAL_PAD);
    assert_eq!(ph, CAL_HEADER_H + CAL_WKD_H + 6 * CAL_CELL_H + CAL_PAD);
    // Six rows is the most any month needs, and every month there is
    // needs no more: the grid is drawn at a fixed six, so the panel
    // height does not change as the user walks through the months.
    for year in 2024..2032 {
        for month in 0..12 {
            let first = sysinfo::first_weekday_mon0(year, month) as i32;
            let last = first + sysinfo::days_in_month(year, month) as i32 - 1;
            assert!(last / 7 < 6, "{year}-{month} needs {} rows", last / 7 + 1);
        }
    }
}

/// The pixels of a popup panel, as the compositor would get them.
fn painted(w: usize, h: usize, f: impl FnOnce(&mut Canvas)) -> Vec<u8> {
    let mut cv = Canvas::try_new(w, h).expect("canvas");
    f(&mut cv);
    let mut buf = vec![0u8; w * h * 4];
    assert!(cv.blit_argb(&mut buf));
    buf
}

/// The drop shadow is offset downward, so the panel reads as lit from
/// above, and it surrounds the panel on every side. The existing shadow
/// test covers the left edge and the fade; these are the other three
/// things the concentric rects are shaped by.
#[test]
fn the_panel_shadow_falls_downward_and_on_every_side() {
    let (pw, ph) = (60, 40);
    let (px, py) = (POPUP_SHADOW, POPUP_SHADOW);
    let (w, h) = (
        (pw + 2 * POPUP_SHADOW) as usize,
        (ph + 2 * POPUP_SHADOW) as usize,
    );
    let buf = painted(w, h, |cv| draw_panel(cv, px, py, pw, ph, 10));
    let alpha = |x: i32, y: i32| buf[((y as usize * w) + x as usize) * 4 + 3];

    // Every side has shadow beside it, the right one included: the rects
    // grow by the same amount on both axes.
    assert!(
        alpha(px + pw + 3, py + ph / 2) > 0,
        "no shadow to the right"
    );
    assert!(alpha(px + pw / 2, py + ph + 3) > 0, "no shadow below");
    assert!(alpha(px + pw / 2, py - 3) > 0, "no shadow above");
    // And more of it below than above: that offset is the light.
    assert!(
        alpha(px + pw / 2, py + ph + 3) > alpha(px + pw / 2, py - 3),
        "the shadow does not fall downward"
    );
    // The rects are rounded more the further out they are, so the shadow
    // is never stronger at a corner than along the edge beside it, and it
    // runs out at the corner first. A shadow drawn at one radius would
    // reach the same distance all the way round.
    for d in 1..POPUP_SHADOW {
        let (corner, edge) = (alpha(px - d, py - d), alpha(px + pw / 2, py - d));
        assert!(
            corner <= edge,
            "{d}px out: corner {corner} over edge {edge}"
        );
    }
    let d = POPUP_SHADOW / 2;
    assert_eq!(alpha(px - d, py - d), 0, "the shadow's corner is square");
    assert!(alpha(px + pw / 2, py - d) > 0, "the shadow's edge is short");
}

/// The colours drawn inside one grid cell, one entry per distinct colour
/// that is not the panel's own ground.
type Ink = Vec<[u8; 3]>;

/// The hit rects a popup returns, as `draw_popup` returns them.
type Hits = Vec<(i32, i32, i32, i32, Action)>;

/// The calendar grid, as cells that have something drawn in them. January
/// 2021 began on a Friday, so the first four cells of the first row are
/// empty and the 31st lands in the last column of the fifth row -- a month
/// that pins the row, the column and both ends of the range at once.
fn cal_cells(year: i32, month: u32) -> (Vec<Vec<Ink>>, Hits) {
    let kind = PopupKind::Calendar { year, month };
    let (pw, ph) = popup_size(&kind, 1080, 34);
    let (px, py) = (POPUP_SHADOW, POPUP_SHADOW);
    let (w, h) = (
        (pw + 2 * POPUP_SHADOW) as usize,
        (ph + 2 * POPUP_SHADOW) as usize,
    );
    let mut cv = Canvas::try_new(w, h).expect("canvas");
    let hits = draw_calendar(&mut cv, px, py, pw, ph, year, month);
    let mut buf = vec![0u8; w * h * 4];
    assert!(cv.blit_argb(&mut buf));
    let at = |x: i32, y: i32| {
        let i = ((y as usize * w) + x as usize) * 4;
        [buf[i + 2], buf[i + 1], buf[i]]
    };
    // The ground is the panel left of the grid's own padding.
    let gy = py + CAL_HEADER_H + CAL_WKD_H;
    let ground = at(px + 2, gy + CAL_CELL_H);
    let mut rows = Vec::new();
    for row in 0..6 {
        let mut cols = Vec::new();
        for col in 0..7 {
            let x0 = px + CAL_PAD + col * CAL_CELL_W;
            let y0 = gy + row * CAL_CELL_H;
            let mut ink: Ink = Vec::new();
            for y in (y0 + 3)..(y0 + CAL_CELL_H - 3) {
                for x in (x0 + 4)..(x0 + CAL_CELL_W - 4) {
                    let c = at(x, y);
                    if c != ground && !ink.contains(&c) {
                        ink.push(c);
                    }
                }
            }
            cols.push(ink);
        }
        rows.push(cols);
    }
    (rows, hits)
}

/// Every day of a month lands in the cell its weekday says, and nothing
/// lands anywhere else. The grid is the one place in the panel where an
/// off-by-one is a wrong answer and not a wrong pixel.
#[test]
fn every_day_of_a_month_lands_under_its_own_weekday() {
    let (year, month) = (2021, 0);
    let first = sysinfo::first_weekday_mon0(year, month) as i32;
    let ndays = sysinfo::days_in_month(year, month) as i32;
    assert_eq!((first, ndays), (4, 31), "January 2021 began on a Friday");
    let (cells, _) = cal_cells(year, month);
    for (row, cols) in cells.iter().enumerate() {
        for (col, ink) in cols.iter().enumerate() {
            let day = (row as i32 * 7 + col as i32) - first + 1;
            let wanted = (1..=ndays).contains(&day);
            assert_eq!(
                !ink.is_empty(),
                wanted,
                "row {row} col {col} should {} hold a day",
                if wanted { "" } else { "not" }
            );
        }
    }
}

/// The two weekend columns are drawn dimmer than the five weekdays, the
/// way every other desktop calendar draws them. Saturday is one of the
/// two, which is what the comparison is for.
#[test]
fn the_weekend_columns_are_drawn_dimmer_than_the_weekdays() {
    let (cells, _) = cal_cells(2021, 0);
    // The first row holds Friday the 1st through Sunday the 3rd.
    let friday = &cells[0][4];
    let saturday = &cells[0][5];
    let sunday = &cells[0][6];
    assert!(!friday.is_empty() && !saturday.is_empty() && !sunday.is_empty());
    assert_ne!(friday, saturday, "Saturday is drawn as a weekday");
    assert_eq!(saturday, sunday, "the two weekend days differ");
}

/// The month is named in full over the grid. The short form is the date
/// pill's, where there is no room, and it is three letters where this has
/// room for the word.
#[test]
fn the_calendar_names_its_month_in_full() {
    let (year, month) = (2021, 0);
    let kind = PopupKind::Calendar { year, month };
    let (pw, ph) = popup_size(&kind, 1080, 34);
    let (px, py) = (POPUP_SHADOW, POPUP_SHADOW);
    let (w, h) = (
        (pw + 2 * POPUP_SHADOW) as usize,
        (ph + 2 * POPUP_SHADOW) as usize,
    );
    let buf = painted(w, h, |cv| {
        let _ = draw_calendar(cv, px, py, pw, ph, year, month);
    });
    let at = |x: i32, y: i32| {
        let i = ((y as usize * w) + x as usize) * 4;
        [buf[i + 2], buf[i + 1], buf[i]]
    };
    // Between the two arrows, the width of what is written there.
    let ground = at(px + pw / 2, py + 2);
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    for x in (px + 44)..(px + pw - 44) {
        for y in py..(py + CAL_HEADER_H - 1) {
            if at(x, y) != ground {
                lo = lo.min(x);
                hi = hi.max(x);
            }
        }
    }
    let title = format!(
        "{} {}",
        i18n::Lang::current().month_full()[month as usize],
        year
    );
    let want = Canvas::text_width(&title);
    assert!(hi > lo, "nothing written over the grid");
    assert!(
        (hi - lo + 1 - want).abs() <= 2,
        "{}px of title where {want}px was expected",
        hi - lo + 1
    );
}

/// The arrow on the left goes back a month and the one on the right goes
/// forward. Swapped, the calendar walks the wrong way under the pointer.
#[test]
fn the_calendars_arrows_go_the_way_they_point() {
    let (_, hits) = cal_cells(2021, 0);
    let prev = hits
        .iter()
        .find(|h| h.4 == Action::PrevMonth)
        .expect("no way back");
    let next = hits
        .iter()
        .find(|h| h.4 == Action::NextMonth)
        .expect("no way forward");
    assert!(prev.0 < next.0, "the arrows are the wrong way round");
}
