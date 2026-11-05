//! The calendar: what the bar's clock and date pills open.
//!
//! It was a month grid with arrows. Now it is a complete calendar: the ISO
//! week number beside every row, the day under the pointer lit, a footer
//! that names that day with its day of the year (today
//! when the pointer is elsewhere), a "today" button that is lit whenever the
//! grid shows another month and brings it back, Home or T to do the same
//! from the keyboard, and the weekend in mockup A's gold rather than only
//! dimmer.
//!
//! A child of `main.rs` because that file is at its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;

/// The week-number column, left of the grid.
pub const WEEK_W: i32 = 28;
/// The footer under the grid.
pub const FOOT_H: i32 = 32;

pub fn size() -> (i32, i32) {
    (
        WEEK_W + 7 * CAL_CELL_W + 2 * CAL_PAD,
        CAL_HEADER_H + CAL_WKD_H + 6 * CAL_CELL_H + CAL_PAD + FOOT_H,
    )
}

/// The grid's left edge in a panel at `px`.
pub fn grid_x(px: i32) -> i32 {
    px + CAL_PAD + WEEK_W
}

/// Day of the year, 1-based.
pub fn day_of_year(y: i32, m0: u32, d: u32) -> u32 {
    (0..m0).map(|m| sysinfo::days_in_month(y, m)).sum::<u32>() + d
}

/// Weekday, 0 = Monday.
pub fn weekday(y: i32, m0: u32, d: u32) -> u32 {
    (sysinfo::first_weekday_mon0(y, m0) + d - 1) % 7
}

/// ISO-8601 weeks in year `y`: 53 when it starts on a Thursday, or on a
/// Wednesday in a leap year.
fn iso_weeks(y: i32) -> u32 {
    let jan1 = sysinfo::first_weekday_mon0(y, 0);
    if jan1 == 3 || (jan1 == 2 && sysinfo::is_leap(y)) {
        53
    } else {
        52
    }
}

/// ISO-8601 week number of a date.
pub fn iso_week(y: i32, m0: u32, d: u32) -> u32 {
    let doy = day_of_year(y, m0, d) as i32;
    let wd = weekday(y, m0, d) as i32 + 1; // 1 = Monday .. 7 = Sunday
    let w = (doy - wd + 10) / 7;
    if w < 1 {
        iso_weeks(y - 1)
    } else if w as u32 > iso_weeks(y) {
        1
    } else {
        w as u32
    }
}

/// The footer's text for a date: `jue 5 nov · día 309`. Its week is the
/// number beside its row (rows run Monday to Sunday, as ISO weeks do), so
/// the footer does not repeat it.
pub fn describe(y: i32, m0: u32, d: u32) -> String {
    let lang = i18n::Lang::current();
    let wd = (weekday(y, m0, d) + 1) % 7; // Sunday-first index
    let doy = day_of_year(y, m0, d);
    let s = match lang {
        i18n::Lang::Es => format!(
            "{} {d} {} · día {doy}",
            lang.weekday_sun_first()[wd as usize],
            lang.month_short()[m0 as usize % 12]
        ),
        i18n::Lang::En => format!(
            "{} {d} {} · day {doy}",
            lang.weekday_sun_first()[wd as usize],
            lang.month_short()[m0 as usize % 12]
        ),
    };
    if tecno() {
        s.to_uppercase()
    } else {
        s
    }
}

/// Paint the calendar and return its hitboxes: the arrows, every day, and
/// the today button. `hover` is the day under the pointer, in this month.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    cv: &mut Canvas,
    px: i32,
    py: i32,
    pw: i32,
    ph: i32,
    year: i32,
    month: u32,
    hover: Option<u32>,
) -> Vec<(i32, i32, i32, i32, Action)> {
    let (cell_w, cell_h) = (CAL_CELL_W, CAL_CELL_H);
    let header_h = CAL_HEADER_H;
    let wkd_h = CAL_WKD_H;
    draw_panel(cv, px, py, pw, ph, 12);
    cv.hline(px + 10, py + header_h - 1, pw - 20, pal().rule, 0.7);
    let weekend = if tecno() { TECNO_GOLD } else { pal().dim };

    // Header: ◂ month year ▸.
    let ts = 10;
    let ay = py + (header_h - ts) / 2;
    cv.triangle_h(px + 16, ay, ts, true, pal().accent);
    cv.triangle_h(px + pw - 16 - ts, ay, ts, false, pal().accent);
    let title = format!("{} {}", i18n::Lang::current().month_full()[month as usize % 12], year);
    let title = if tecno() { title.to_uppercase() } else { title };
    let tw = Canvas::text_width(&title);
    cv.text_bold(&title, px + (pw - tw) / 2, py + (header_h - GLYPH_H) / 2, pal().text);
    let mut hits = vec![
        (px + 4, py, px + 44, py + header_h, Action::PrevMonth),
        (px + pw - 44, py, px + pw - 4, py + header_h, Action::NextMonth),
    ];

    // Weekday header, Monday-first, and the week column's own.
    let gx = grid_x(px);
    let hy = py + header_h + (wkd_h - GLYPH_H) / 2;
    let wk = match i18n::Lang::current() {
        i18n::Lang::Es => "s",
        i18n::Lang::En => "w",
    };
    let wk = if tecno() { wk.to_uppercase() } else { wk.to_string() };
    cv.text(&wk, px + CAL_PAD + (WEEK_W - Canvas::text_width(&wk)) / 2, hy, pal().rule);
    for (i, wd) in i18n::Lang::current().weekday_mon_first().iter().enumerate() {
        let wd = &if tecno() { wd.to_uppercase() } else { wd.to_string() };
        let x = gx + i as i32 * cell_w + (cell_w - Canvas::text_width(wd)) / 2;
        cv.text(wd, x, hy, if i >= 5 { weekend } else { pal().dim });
    }

    // Day grid (6 rows always, so the panel height is stable across months).
    let today = sysinfo::today();
    let first = sysinfo::first_weekday_mon0(year, month) as i32;
    let ndays = sysinfo::days_in_month(year, month) as i32;
    let gy = py + header_h + wkd_h;
    let rows = (first + ndays - 1) / 7 + 1;
    for row in 0..rows {
        // The week of the row's first day of this month.
        let d = (row * 7 - first + 1).max(1) as u32;
        let s = iso_week(year, month, d).to_string();
        let y = gy + row * cell_h + (cell_h - GLYPH_H) / 2;
        cv.text(&s, px + CAL_PAD + (WEEK_W - Canvas::text_width(&s)) / 2, y, pal().rule);
    }
    cv.fill_rect_a(gx - 3, gy + 2, 1, rows * cell_h - 4, pal().rule, 0.5);
    for d in 1..=ndays {
        let idx = first + d - 1;
        let (row, col) = (idx / 7, idx % 7);
        let x = gx + col * cell_w;
        let y = gy + row * cell_h;
        let s = d.to_string();
        let tx = x + (cell_w - Canvas::text_width(&s)) / 2;
        let dy = y + (cell_h - GLYPH_H) / 2;
        let is_today = today == Some((year, month, d as u32));
        if hover == Some(d as u32) && !is_today {
            plate_a(cv, x + 2, y + 1, cell_w - 4, cell_h - 2, 6, pal().accent, 0.16);
            cv.fill_rect_a(x + 2, y + cell_h - 3, cell_w - 4, 1, pal().accent, 0.9);
        }
        if is_today && tecno() {
            // A's focus: the gold brackets, small, around today.
            plate(cv, x + 2, y + 1, cell_w - 4, cell_h - 2, 0, pal().btn_active);
            cv.brackets(x + 2, y + 1, cell_w - 4, cell_h - 2, 4, 1, TECNO_GOLD);
            cv.text_bold(&s, tx, dy, pal().white);
        } else if is_today {
            plate(cv, x + 2, y + 1, cell_w - 4, cell_h - 2, 6, pal().rule);
            cv.text_bold(&s, tx, dy, pal().white);
        } else {
            let c = if hover == Some(d as u32) {
                pal().text
            } else if col >= 5 {
                weekend
            } else {
                pal().muted
            };
            cv.text(&s, tx, dy, c);
        }
        hits.push((x, y, x + cell_w, y + cell_h, Action::CalDay(d as u8)));
    }

    // Footer: the day in full, and the way back to today.
    let fy = py + ph - FOOT_H;
    cv.hline(px + 10, fy, pw - 20, pal().rule, 0.5);
    let label = today_label();
    let bw = button_w(&label);
    let (bx, by, bh) = (px + pw - 12 - bw, fy + 6, FOOT_H - 12);
    let away = today.is_some_and(|(ty, tm, _)| (ty, tm) != (year, month));
    let edge = if away { pal().accent } else { pal().rule };
    cv.glyph_box(bx, by, bh, "", pal().pill, edge, pal().accent);
    cv.fill_rect_a(bx + bh, by, bw - bh, 1, edge, 1.0);
    cv.fill_rect_a(bx + bh, by + bh - 1, bw - bh, 1, edge, 1.0);
    cv.fill_rect_a(bx + bw - 1, by, 1, bh, edge, 1.0);
    cv.fill_rect_a(bx + 1, by + 1, bw - 2, bh - 2, pal().pill, 1.0);
    cv.text(&label, bx + 8, by + (bh - GLYPH_H) / 2, if away { pal().accent } else { pal().dim });
    hits.push((bx, by, bx + bw, by + bh, Action::CalToday));
    let shown = match (hover, today) {
        (Some(d), _) => Some((year, month, d)),
        (None, Some(t)) => Some(t),
        _ => None,
    };
    if let Some((y, m, d)) = shown {
        let s = describe(y, m, d);
        let max = ((bx - 8 - (px + 14)) / GLYPH_W) as usize;
        let s: String = if s.chars().count() > max {
            s.chars().take(max.saturating_sub(1)).chain(['.']).collect()
        } else {
            s
        };
        cv.text(&s, px + 14, fy + (FOOT_H - GLYPH_H) / 2, if hover.is_some() { pal().text } else { pal().muted });
    }
    hits
}

/// The today button's word.
fn today_label() -> String {
    let label = match i18n::Lang::current() {
        i18n::Lang::Es => "hoy",
        i18n::Lang::En => "today",
    };
    if tecno() {
        label.to_uppercase()
    } else {
        label.to_string()
    }
}

/// The today button's width: its word, with room either side.
fn button_w(label: &str) -> i32 {
    Canvas::text_width(label) + 16
}

impl State {
    /// Bring the calendar back to this month.
    pub(crate) fn cal_today(&mut self, qh: &QueueHandle<State>) {
        let Some((ty, tm, _)) = sysinfo::today() else {
            return;
        };
        if let Some(Popup { kind: PopupKind::Calendar { year, month, hover }, .. }) = self.popup.as_mut() {
            (*year, *month, *hover) = (ty, tm, None);
        }
        self.render_popup(qh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_weeks_match_the_standard_at_both_ends_of_the_year() {
        // 2021-01-01 (Friday) is still week 53 of 2020; 2021-01-04 starts week 1.
        assert_eq!(iso_week(2021, 0, 1), 53);
        assert_eq!(iso_week(2021, 0, 4), 1);
        // 2024-12-30 (Monday) is week 1 of 2025.
        assert_eq!(iso_week(2024, 11, 30), 1);
        // 2026-11-05 is a Thursday in week 45, day 309.
        assert_eq!((weekday(2026, 10, 5), iso_week(2026, 10, 5), day_of_year(2026, 10, 5)), (3, 45, 309));
        assert_eq!(day_of_year(2024, 11, 31), 366);
    }

    #[test]
    fn every_footer_fits_beside_the_today_button() {
        let (pw, _) = size();
        for m in 0..12 {
            for d in [1, 28, 30] {
                let s = describe(2026, m, d.min(sysinfo::days_in_month(2026, m)));
                let room = pw - 12 - button_w(&today_label()) - 8 - 14;
                assert!(Canvas::text_width(&s) <= room, "{s}");
            }
        }
    }
}
