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
            if tecno() {
                let ch: String = label.chars().take(1).collect();
                cv.glyph_box(px + 12, iy, is, &ch, pal().pill, c, c);
            } else {
                match icons.get(icon_name(s), is as u32) {
                    Some(pm) => cv.pixmap(px + 12, iy, &pm),
                    None => cv.badge(px + 12, iy, is, label.chars().next().unwrap_or('?'), pal().pill, c),
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
    fn one_category_alone_is_a_flat_list() {
        let all = vec![entry("A", "System"), entry("B", "System")];
        assert_eq!(rows(&all, "", None), [Row::App(0), Row::App(1)]);
    }
}
