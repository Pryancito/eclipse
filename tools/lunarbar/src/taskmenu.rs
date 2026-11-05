//! The window menu: what a right click on a taskbar button opens.
//!
//! It was three lines of text (focus, minimise, close) with nothing to show
//! which one the pointer was on and no way to reach them from the keyboard.
//! Now it is a complete window menu: every action wlr-foreign-toplevel offers
//! — restore or minimise, maximise or restore the size, enter or leave
//! fullscreen — labelled for the state the window is in, each with its own
//! pictogram, the row under the pointer lit, ↑/↓ and Enter to drive it, and
//! closing set apart under a rule in the warning colour.
//!
//! A child of `main.rs` because that file is at its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;
use draw::Ink;

/// The window's state when the menu opened: which way each toggle goes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Win {
    /// The window already has the focus: focusing it would do nothing.
    pub activated: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
}

/// The wlr-foreign-toplevel `state` values this menu reads.
pub const STATE_MAXIMIZED: u32 = 0;
pub const STATE_FULLSCREEN: u32 = 3;

const ROW_H: i32 = 32;
const HEAD_H: i32 = 32;
const PAD: i32 = 8;
/// The rule between the window's actions and closing it.
const GAP: i32 = 8;
const PW: i32 = 264;
const ICON: i32 = 16;

/// One row: label, action, pictogram, and whether it is the destructive one.
pub struct Item {
    pub label: &'static str,
    pub act: Action,
    pub ink: &'static [Ink],
    pub danger: bool,
}

const I_FOCUS: &[Ink] = &[
    Ink::Oval(2.0, 2.0, 8.0, 8.0),
    Ink::Dot(6.0, 6.0),
    Ink::Line(&[(6.0, 0.0), (6.0, 2.0)]),
    Ink::Line(&[(6.0, 10.0), (6.0, 12.0)]),
    Ink::Line(&[(0.0, 6.0), (2.0, 6.0)]),
    Ink::Line(&[(10.0, 6.0), (12.0, 6.0)]),
];
const I_MIN: &[Ink] = &[
    Ink::Line(&[(2.0, 10.5), (10.0, 10.5)]),
    Ink::Line(&[(6.0, 1.5), (6.0, 7.5)]),
    Ink::Line(&[(3.5, 5.0), (6.0, 7.5), (8.5, 5.0)]),
];
const I_RESTORE_UP: &[Ink] = &[
    Ink::Line(&[(2.0, 10.5), (10.0, 10.5)]),
    Ink::Line(&[(6.0, 7.5), (6.0, 1.5)]),
    Ink::Line(&[(3.5, 4.0), (6.0, 1.5), (8.5, 4.0)]),
];
const I_MAX: &[Ink] = &[Ink::Line(&[(1.5, 1.5), (10.5, 1.5), (10.5, 10.5), (1.5, 10.5), (1.5, 1.5)])];
const I_UNMAX: &[Ink] = &[
    Ink::Line(&[(1.5, 4.0), (8.0, 4.0), (8.0, 10.5), (1.5, 10.5), (1.5, 4.0)]),
    Ink::Line(&[(4.0, 4.0), (4.0, 1.5), (10.5, 1.5), (10.5, 8.0), (8.0, 8.0)]),
];
const I_FULL: &[Ink] = &[
    Ink::Line(&[(1.0, 4.0), (1.0, 1.0), (4.0, 1.0)]),
    Ink::Line(&[(8.0, 1.0), (11.0, 1.0), (11.0, 4.0)]),
    Ink::Line(&[(11.0, 8.0), (11.0, 11.0), (8.0, 11.0)]),
    Ink::Line(&[(4.0, 11.0), (1.0, 11.0), (1.0, 8.0)]),
];
const I_UNFULL: &[Ink] = &[
    Ink::Line(&[(1.0, 4.0), (4.0, 4.0), (4.0, 1.0)]),
    Ink::Line(&[(8.0, 1.0), (8.0, 4.0), (11.0, 4.0)]),
    Ink::Line(&[(11.0, 8.0), (8.0, 8.0), (8.0, 11.0)]),
    Ink::Line(&[(4.0, 11.0), (4.0, 8.0), (1.0, 8.0)]),
];
const I_CLOSE: &[Ink] = &[
    Ink::Line(&[(2.0, 2.0), (10.0, 10.0)]),
    Ink::Line(&[(10.0, 2.0), (2.0, 10.0)]),
];

/// The rows for window `tid` in state `w`, closing last.
pub fn items(tid: u32, w: Win) -> Vec<Item> {
    let es = i18n::Lang::current() == i18n::Lang::Es;
    let t = |a: &'static str, b: &'static str| if es { a } else { b };
    let mut v = Vec::new();
    if w.minimized {
        v.push(Item { label: t("restaurar", "restore"), act: Action::TaskFocus(tid), ink: I_RESTORE_UP, danger: false });
    } else if !w.activated {
        // Offered only to a window that does not have the focus already.
        v.push(Item { label: t("enfocar", "focus"), act: Action::TaskFocus(tid), ink: I_FOCUS, danger: false });
    }
    if !w.minimized {
        v.push(Item { label: t("minimizar", "minimize"), act: Action::TaskMinimize(tid), ink: I_MIN, danger: false });
    }
    v.push(if w.maximized {
        Item { label: t("restaurar tamaño", "unmaximize"), act: Action::TaskMaximize(tid), ink: I_UNMAX, danger: false }
    } else {
        Item { label: t("maximizar", "maximize"), act: Action::TaskMaximize(tid), ink: I_MAX, danger: false }
    });
    v.push(if w.fullscreen {
        Item { label: t("pantalla normal", "leave fullscreen"), act: Action::TaskFullscreen(tid), ink: I_UNFULL, danger: false }
    } else {
        Item { label: t("pantalla completa", "fullscreen"), act: Action::TaskFullscreen(tid), ink: I_FULL, danger: false }
    });
    v.push(Item { label: t("cerrar ventana", "close window"), act: Action::TaskClose(tid), ink: I_CLOSE, danger: true });
    v
}

/// The panel size for state `w`.
pub fn size(w: Win) -> (i32, i32) {
    (PW, HEAD_H + items(0, w).len() as i32 * ROW_H + GAP + PAD)
}

/// Paint the menu and return its row hitboxes. `sel` is the lit row.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    cv: &mut Canvas,
    px: i32,
    py: i32,
    pw: i32,
    tid: u32,
    title: &str,
    w: Win,
    sel: usize,
) -> Vec<(i32, i32, i32, i32, Action)> {
    let ph = size(w).1;
    draw_panel(cv, px, py, pw, ph, 10);

    // The window's marker as the taskbar draws it, then its title.
    let ms = 12;
    let my = py + (HEAD_H - ms) / 2;
    if tecno() {
        cv.task_square(px + 16, my, ms, pal().rule, if w.activated { pal().accent } else { pal().dim });
    } else {
        cv.fill_rect_a(px + 16, my, ms, ms, if w.activated { pal().accent } else { pal().dim }, 0.8);
    }
    let tx = px + 14 + ICON + 12;
    let max_chars = ((px + pw - 12 - tx) / GLYPH_W) as usize;
    let head: String = if title.chars().count() > max_chars {
        title.chars().take(max_chars.saturating_sub(1)).chain(['.']).collect()
    } else {
        title.to_string()
    };
    cv.text_bold(&head, tx, py + (HEAD_H - GLYPH_H) / 2, if w.activated { pal().text } else { pal().muted });
    cv.hline(px + 10, py + HEAD_H - 1, pw - 20, pal().rule, 0.7);

    let mut hits = Vec::new();
    let mut ry = py + HEAD_H;
    for (i, it) in items(tid, w).iter().enumerate() {
        if it.danger {
            cv.hline(px + 10, ry + GAP / 2, pw - 20, pal().rule, 0.5);
            ry += GAP;
        }
        let lit = i == sel;
        if lit {
            plate_a(cv, px + 5, ry + 1, pw - 10, ROW_H - 2, 6, pal().menu_hover, 1.0);
            if tecno() {
                cv.fill_rect_a(px + 5, ry + 1, 2, ROW_H - 2, if it.danger { pal().warn } else { pal().accent }, 1.0);
            }
        }
        let ic = if it.danger {
            pal().warn
        } else if lit {
            pal().accent
        } else {
            pal().dim
        };
        cv.line_icon(px + 14, ry + (ROW_H - ICON) / 2, ICON, it.ink, ic);
        let label = if tecno() { it.label.to_uppercase() } else { it.label.to_string() };
        let col = if lit { pal().text } else { pal().muted };
        cv.text(&label, px + 14 + ICON + 12, ry + (ROW_H - GLYPH_H) / 2, col);
        hits.push((px + 5, ry, px + pw - 5, ry + ROW_H, it.act));
        ry += ROW_H;
    }
    hits
}

impl State {
    /// Carry out a window-menu action on window `tid` and close the menu.
    pub(crate) fn task_act(&mut self, act: Action) {
        self.close_popup();
        let tid = match act {
            Action::TaskFocus(t) | Action::TaskMinimize(t) | Action::TaskMaximize(t) | Action::TaskFullscreen(t) | Action::TaskClose(t) => t,
            _ => return,
        };
        let Some(t) = self.toplevels.iter().find(|t| t.handle.id().protocol_id() == tid) else {
            return;
        };
        match act {
            Action::TaskFocus(_) => {
                if t.minimized {
                    t.handle.unset_minimized();
                }
                if let Some(seat) = &self.seat {
                    t.handle.activate(seat);
                }
            }
            Action::TaskMinimize(_) => t.handle.set_minimized(),
            Action::TaskMaximize(_) if t.maximized => t.handle.unset_maximized(),
            Action::TaskMaximize(_) => t.handle.set_maximized(),
            Action::TaskFullscreen(_) if t.fullscreen => t.handle.unset_fullscreen(),
            // On whichever output the compositor picks: the one it is on.
            Action::TaskFullscreen(_) => t.handle.set_fullscreen(None),
            Action::TaskClose(_) => t.handle.close(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_follow_the_windows_state() {
        // An active window is not offered focus it already has.
        let active = items(7, Win { activated: true, ..Win::default() });
        assert!(!active.iter().any(|i| i.act == Action::TaskFocus(7)));
        let plain = items(7, Win::default());
        let acts: Vec<Action> = plain.iter().map(|i| i.act).collect();
        assert!(
            acts == [
                Action::TaskFocus(7),
                Action::TaskMinimize(7),
                Action::TaskMaximize(7),
                Action::TaskFullscreen(7),
                Action::TaskClose(7)
            ]
        );
        // Only closing is destructive, and it is last.
        assert!(plain.iter().rev().skip(1).all(|i| !i.danger) && plain.last().unwrap().danger);
        // A minimised window cannot be minimised again: it is restored.
        let min = items(7, Win { minimized: true, ..Win::default() });
        assert_eq!(min.len(), 4);
        assert!(!min.iter().any(|i| i.act == Action::TaskMinimize(7)));
        // The toggles name the way they go.
        let max = items(7, Win { maximized: true, fullscreen: true, ..Win::default() });
        assert!(!std::ptr::eq(max[2].ink, plain[2].ink));
        assert!(!std::ptr::eq(max[3].ink, plain[3].ink));
    }

    #[test]
    fn the_panel_fits_its_rows_and_the_longest_label() {
        for w in [Win::default(), Win { activated: true, minimized: true, maximized: true, fullscreen: true }] {
            let (pw, ph) = size(w);
            assert_eq!(ph, HEAD_H + items(0, w).len() as i32 * ROW_H + GAP + PAD);
            for it in items(0, w) {
                assert!(14 + ICON + 12 + Canvas::text_width(it.label) <= pw - 8, "{}", it.label);
            }
        }
    }
}
