//! The application menu's submenus and their colours.
//!
//! The launcher used to be one flat list: every program, sorted by name, with
//! a search field. Each `.desktop` entry already says what it is
//! (`Categories=`), and labwc's right-click menu files entries by it
//! ([`obmenu`]); the panel ignored it. Now, with nothing typed, the menu's top
//! level is one row per category and choosing one opens that category's
//! programs in the same panel, under a row that goes back. Typing still
//! searches every program at once, whatever level the menu is on.
//!
//! A child of `main.rs` because that file is at its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;
use draw::Ink;

/// One row of the application menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    /// A program: index into the menu's `all`.
    App(usize),
    /// A category: its [`obmenu`] schema slot.
    Cat(usize),
    /// Back to the categories.
    Back,
}

/// The rows the menu shows for `filter` at level `cat` (a schema slot, or
/// the top level).
///
/// - Something typed: every matching program, flat, wherever the menu is.
/// - A menu whose programs all fall in one category: that one list, flat,
///   because a submenu with nothing beside it is only an extra click.
/// - Top level: one row per non-empty category, in schema order.
/// - Inside a category: the way back, then its programs.
pub fn rows(all: &[apps::AppEntry], filter: &str, cat: Option<usize>) -> Vec<Row> {
    if !filter.is_empty() {
        return filter_apps(all, filter).into_iter().map(Row::App).collect();
    }
    let groups = obmenu::grouped_indices(all);
    if groups.len() <= 1 {
        return (0..all.len()).map(Row::App).collect();
    }
    match cat.and_then(|c| groups.iter().find(|(s, _)| *s == c)) {
        Some((_, ix)) => std::iter::once(Row::Back)
            .chain(ix.iter().map(|&i| Row::App(i)))
            .collect(),
        None => groups.iter().map(|(s, _)| Row::Cat(*s)).collect(),
    }
}

/// How many programs category `slot` holds.
pub fn count(all: &[apps::AppEntry], slot: usize) -> usize {
    all.iter().filter(|e| obmenu::slot(e) == slot).count()
}

/// How many programs the menu reaches from here, for its header: the ones
/// listed, or every program when the rows are categories.
pub fn programs(all: &[apps::AppEntry], visible: &[Row]) -> usize {
    match visible.iter().filter(|r| matches!(r, Row::App(_))).count() {
        0 if visible.iter().any(|r| matches!(r, Row::Cat(_))) => all.len(),
        n => n,
    }
}

/// Rebuild the rows after the filter or the level changed, selecting `sel`.
pub fn refresh(
    all: &[apps::AppEntry],
    filter: &str,
    cat: Option<usize>,
    visible: &mut Vec<Row>,
    sel: &mut usize,
    scroll: &mut usize,
) {
    *visible = rows(all, filter, cat);
    *sel = 0;
    *scroll = 0;
}

/// Choose `row`: a program is returned to be launched; a category opens and
/// the way back closes it (the menu then has to be re-laid-out).
pub fn activate(
    all: &[apps::AppEntry],
    cat: &mut Option<usize>,
    visible: &mut Vec<Row>,
    sel: &mut usize,
    scroll: &mut usize,
    row: Row,
) -> Option<String> {
    match row {
        Row::App(i) => return all.get(i).map(|e| e.exec.clone()),
        Row::Cat(s) => {
            *cat = Some(s);
            refresh(all, "", *cat, visible, sel, scroll);
            // The first program, not the way back: Enter, Enter launches.
            if visible.len() > 1 {
                *sel = 1;
            }
        }
        Row::Back => {
            back(all, cat, visible, sel, scroll);
        }
    }
    None
}

/// Leave the open category, selecting it on the top level. False when the
/// menu was already there.
pub fn back(
    all: &[apps::AppEntry],
    cat: &mut Option<usize>,
    visible: &mut Vec<Row>,
    sel: &mut usize,
    scroll: &mut usize,
) -> bool {
    let Some(was) = cat.take() else {
        return false;
    };
    refresh(all, "", None, visible, sel, scroll);
    *sel = visible.iter().position(|r| *r == Row::Cat(was)).unwrap_or(0);
    true
}

/// The menu header's title: the menu's, then the open category's.
pub fn title(cat: Option<usize>) -> String {
    let lang = i18n::Lang::current();
    let t = match cat {
        Some(s) => format!("{} / {}", lang.apps_title(), obmenu::slot_label(s, lang)),
        None => lang.apps_title().to_string(),
    };
    if tecno() {
        t.to_uppercase()
    } else {
        t
    }
}

/// The freedesktop icon of category `slot`, for the looks that draw icons.
fn icon_name(slot: usize) -> &'static str {
    const NAMES: [&str; 12] = [
        "applications-games",
        "applications-multimedia",
        "applications-graphics",
        "applications-office",
        "applications-development",
        "applications-education",
        "applications-science",
        "applications-internet",
        "preferences-desktop",
        "applications-system",
        "applications-accessories",
        "applications-other",
    ];
    NAMES.get(slot).copied().unwrap_or("applications-other")
}

/// The hairline pictogram of category `slot` (schema order, catch-all last),
/// on [`Canvas::line_icon`]'s 12-unit grid: drawn, not loaded, so tecno's
/// menu has the same thin cyan instruments whatever icon theme is installed.
pub fn pictogram(slot: usize) -> &'static [Ink] {
    use Ink::*;
    const P: [&[Ink]; 12] = [
        // Games: a pad, its cross and two buttons.
        &[
            Line(&[(1.0, 4.0), (11.0, 4.0), (11.0, 9.0), (1.0, 9.0), (1.0, 4.0)]),
            Line(&[(2.6, 6.5), (5.4, 6.5)]),
            Line(&[(4.0, 5.1), (4.0, 7.9)]),
            Dot(7.8, 5.9),
            Dot(9.4, 7.3),
        ],
        // Multimedia: play.
        &[Line(&[(3.5, 2.0), (10.0, 6.0), (3.5, 10.0), (3.5, 2.0)])],
        // Graphics: a pen nib on its stroke.
        &[
            Line(&[(2.0, 10.0), (2.8, 7.2), (8.6, 1.4), (10.6, 3.4), (4.8, 9.2), (2.0, 10.0)]),
            Line(&[(7.4, 2.6), (9.4, 4.6)]),
        ],
        // Office: a page of text.
        &[
            Line(&[(2.5, 1.0), (7.5, 1.0), (9.5, 3.0), (9.5, 11.0), (2.5, 11.0), (2.5, 1.0)]),
            Line(&[(4.2, 5.0), (7.8, 5.0)]),
            Line(&[(4.2, 7.0), (7.8, 7.0)]),
            Line(&[(4.2, 9.0), (6.4, 9.0)]),
        ],
        // Development: </>.
        &[
            Line(&[(3.8, 3.0), (0.8, 6.0), (3.8, 9.0)]),
            Line(&[(8.2, 3.0), (11.2, 6.0), (8.2, 9.0)]),
            Line(&[(7.0, 2.0), (5.0, 10.0)]),
        ],
        // Education: a mortarboard.
        &[
            Line(&[(0.8, 4.6), (6.0, 2.2), (11.2, 4.6), (6.0, 7.0), (0.8, 4.6)]),
            Line(&[(3.2, 5.8), (3.2, 8.6), (6.0, 9.8), (8.8, 8.6), (8.8, 5.8)]),
        ],
        // Science: a flask.
        &[
            Line(&[(4.6, 1.0), (4.6, 4.8), (1.6, 11.0), (10.4, 11.0), (7.4, 4.8), (7.4, 1.0)]),
            Line(&[(3.6, 1.0), (8.4, 1.0)]),
            Line(&[(3.0, 8.0), (9.0, 8.0)]),
        ],
        // Internet: a globe.
        &[
            Oval(1.0, 1.0, 10.0, 10.0),
            Oval(3.8, 1.0, 4.4, 10.0),
            Line(&[(1.0, 6.0), (11.0, 6.0)]),
        ],
        // Settings: two sliders.
        &[
            Line(&[(1.0, 3.5), (11.0, 3.5)]),
            Line(&[(1.0, 8.5), (11.0, 8.5)]),
            Oval(2.6, 2.0, 3.0, 3.0),
            Oval(6.4, 7.0, 3.0, 3.0),
        ],
        // System: a monitor on its foot.
        &[
            Line(&[(1.0, 1.5), (11.0, 1.5), (11.0, 8.0), (1.0, 8.0), (1.0, 1.5)]),
            Line(&[(6.0, 8.0), (6.0, 10.5)]),
            Line(&[(3.5, 10.5), (8.5, 10.5)]),
        ],
        // Accessories: a toolbox.
        &[
            Line(&[(1.0, 4.0), (11.0, 4.0), (11.0, 10.5), (1.0, 10.5), (1.0, 4.0)]),
            Line(&[(4.2, 4.0), (4.2, 1.8), (7.8, 1.8), (7.8, 4.0)]),
            Line(&[(1.0, 7.0), (11.0, 7.0)]),
        ],
        // Other: four tiles.
        &[
            Line(&[(1.5, 1.5), (5.0, 1.5), (5.0, 5.0), (1.5, 5.0), (1.5, 1.5)]),
            Line(&[(7.0, 1.5), (10.5, 1.5), (10.5, 5.0), (7.0, 5.0), (7.0, 1.5)]),
            Line(&[(1.5, 7.0), (5.0, 7.0), (5.0, 10.5), (1.5, 10.5), (1.5, 7.0)]),
            Line(&[(7.0, 7.0), (10.5, 7.0), (10.5, 10.5), (7.0, 10.5), (7.0, 7.0)]),
        ],
    ];
    P.get(slot).copied().unwrap_or(P[11])
}

/// An empty framed well (the glyph box without its glyph).
fn frame(cv: &mut Canvas, x: i32, y: i32, s: i32) {
    cv.glyph_box(x, y, s, "", pal().pill, pal().rule, pal().accent);
}

/// Paint a program's marker in tecno: its theme icon set in the framed well,
/// or, without one, the well with its initial (a terminal gets `>`).
pub fn app_marker(cv: &mut Canvas, x: i32, y: i32, s: i32, e: &apps::AppEntry, icons: &mut IconCache) {
    let inner = s - 6;
    let pm = e
        .icon
        .as_deref()
        .and_then(|n| icons.get(n, inner as u32))
        .or_else(|| icons.get(&e.name, inner as u32));
    if let Some(pm) = pm {
        frame(cv, x, y, s);
        cv.pixmap(x + 3, y + 3, &pm);
        return;
    }
    let t = e.exec.contains("term") || e.exec.split_whitespace().next() == Some("foot");
    let ch: String = if t {
        ">".into()
    } else {
        e.name.chars().take(1).flat_map(char::to_uppercase).collect()
    };
    cv.glyph_box(x, y, s, &ch, pal().pill, pal().rule, pal().accent);
}

/// Paint a category or back row's content (the selection plate is already
/// down) in the row rect starting at `y`. `is` is the icon slot.
#[allow(clippy::too_many_arguments)]
pub fn draw_row(
    cv: &mut Canvas,
    px: i32,
    y: i32,
    pw: i32,
    is: i32,
    row: Row,
    selected: bool,
    all: &[apps::AppEntry],
    icons: &mut IconCache,
) {
    let lang = i18n::Lang::current();
    let ty = y + (APPS_ROW_H - GLYPH_H) / 2;
    let iy = y + (APPS_ROW_H - is) / 2;
    let col = if selected { pal().text } else { pal().muted };
    let ts = 8;
    let ay = y + (APPS_ROW_H - ts) / 2;
    match row {
        Row::Back => {
            cv.triangle_h(px + 12 + (is - ts) / 2, ay, ts, true, pal().accent);
            let label = match lang {
                i18n::Lang::Es => "Volver",
                i18n::Lang::En => "Back",
            };
            let label = if tecno() { label.to_uppercase() } else { label.to_string() };
            cv.text(&label, px + 12 + is + 10, ty, pal().dim);
        }
        Row::Cat(s) => {
            let c = pal().accent;
            let label = obmenu::slot_label(s, lang);
            let themed = if tecno() { None } else { icons.get(icon_name(s), is as u32) };
            match themed {
                Some(pm) => cv.pixmap(px + 12, iy, &pm),
                None => {
                    frame(cv, px + 12, iy, is);
                    cv.line_icon(px + 12, iy, is, pictogram(s), c);
                }
            }
            let label = if tecno() { label.to_uppercase() } else { label.to_string() };
            cv.text_bold(&label, px + 12 + is + 10, ty, col);
            let n = count(all, s).to_string();
            let nx = px + pw - 14 - ts - 8 - Canvas::text_width(&n);
            cv.text(&n, nx, ty, pal().dim);
            cv.triangle_h(px + pw - 14 - ts, ay, ts, false, c);
        }
        Row::App(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, cat: &str) -> apps::AppEntry {
        apps::AppEntry {
            name: name.into(),
            exec: name.to_lowercase(),
            categories: vec![cat.into()],
            ..apps::AppEntry::default()
        }
    }

    fn sample() -> Vec<apps::AppEntry> {
        vec![
            entry("Terminal", "System"),
            entry("Firefox", "Network"),
            entry("Editor", "Utility"),
            entry("Monitor", "System"),
        ]
    }

    #[test]
    fn the_top_level_is_one_row_per_category_in_schema_order() {
        let all = sample();
        // Network, System, Utility: schema order, not entry order.
        assert_eq!(rows(&all, "", None), [Row::Cat(7), Row::Cat(9), Row::Cat(10)]);
        assert_eq!(count(&all, 9), 2);
        assert_eq!(programs(&all, &rows(&all, "", None)), 4);
        assert_eq!(programs(&all, &rows(&all, "", Some(9))), 2);
    }

    #[test]
    fn a_category_opens_its_programs_under_the_way_back() {
        let all = sample();
        let (mut cat, mut vis, mut sel, mut scroll) = (None, rows(&all, "", None), 0, 0);
        assert!(activate(&all, &mut cat, &mut vis, &mut sel, &mut scroll, Row::Cat(9)).is_none());
        assert_eq!(vis, [Row::Back, Row::App(0), Row::App(3)]);
        assert_eq!(sel, 1, "Enter after opening launches the first program");
        assert_eq!(
            activate(&all, &mut cat, &mut vis, &mut sel, &mut scroll, Row::App(3)).as_deref(),
            Some("monitor")
        );
        // Back lands on the category it came from.
        assert!(back(&all, &mut cat, &mut vis, &mut sel, &mut scroll));
        assert_eq!((cat, vis[sel]), (None, Row::Cat(9)));
        assert!(!back(&all, &mut cat, &mut vis, &mut sel, &mut scroll));
    }

    #[test]
    fn typing_searches_every_category_at_once() {
        let all = sample();
        assert_eq!(rows(&all, "or", Some(7)), [Row::App(2), Row::App(3)]);
    }

    #[test]
    fn every_category_has_its_own_pictogram() {
        let all: Vec<usize> = (0..=obmenu::SCHEMA.len()).collect();
        for &a in &all {
            for &b in &all[..a] {
                assert!(!std::ptr::eq(pictogram(a), pictogram(b)), "{a} and {b} share one");
            }
            for op in pictogram(a) {
                if let Ink::Line(pts) = op {
                    for &(x, y) in *pts {
                        assert!((0.0..=12.0).contains(&x) && (0.0..=12.0).contains(&y), "{a}: {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn one_category_alone_is_a_flat_list() {
        let all = vec![entry("A", "System"), entry("B", "System")];
        assert_eq!(rows(&all, "", None), [Row::App(0), Row::App(1)]);
    }
}
