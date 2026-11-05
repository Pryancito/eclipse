//! The session menu: what the bar's power button opens.
//!
//! It was four rows with nothing to show which one the pointer was on, no
//! keyboard, and a single click on "apagar" switched the machine off at once.
//! Now it is a complete session menu: a header that says whose session it is
//! and how long the machine has been up, the row under the pointer lit, ↑/↓
//! and Enter to drive it, the two that end the machine set apart under a rule,
//! and every action that ends something (the session, or the machine) asks
//! for a second press on the same row before it happens. Locking does not end
//! anything, so it goes at once.
//!
//! A child of `main.rs` because that file is at its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;

const ROW_H: i32 = 32;
const HEAD_H: i32 = 32;
const PAD: i32 = 8;
/// The rule between the session's actions and the machine's.
const GAP: i32 = 8;
const PW: i32 = 276;
const ICON: i32 = 16;

/// The rows, in order.
pub const ROWS: [Action; 4] = [
    Action::PowerLock,
    Action::PowerLogout,
    Action::PowerReboot,
    Action::PowerShutdown,
];

/// Whether `a` ends something, and so waits for a second press.
pub fn needs_confirm(a: Action) -> bool {
    !matches!(a, Action::PowerLock)
}

/// Mockup A's "ok" green.
const OK: Rgb = (0x6e, 0xe7, 0xa8);

/// Each action's own colour, from mockup A's palette, graded by how much it
/// ends: locking keeps everything (green), leaving the session is the
/// accent, restarting is the gold of a caution, switching off the warning.
pub fn colour(a: Action) -> Rgb {
    match a {
        Action::PowerLock => OK,
        Action::PowerLogout => pal().accent,
        Action::PowerReboot => TECNO_GOLD,
        _ => pal().warn,
    }
}

/// The panel size.
pub fn size() -> (i32, i32) {
    (PW, HEAD_H + ROWS.len() as i32 * ROW_H + GAP + PAD)
}

/// `user@host`, as a terminal prompt says it.
fn who() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".into());
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "eclipse".into());
    format!("{user}@{host}")
}

/// The label of row `a`, or its confirmation question when it is armed.
pub fn label(a: Action, armed: bool) -> String {
    let lang = i18n::Lang::current();
    let base = match a {
        Action::PowerLock => lang.power_lock(),
        Action::PowerLogout => lang.power_logout(),
        Action::PowerReboot => lang.power_reboot(),
        _ => lang.power_shutdown(),
    };
    let s = if armed {
        match lang {
            i18n::Lang::Es => format!("¿{base}? otra vez"),
            i18n::Lang::En => format!("{base}? again"),
        }
    } else {
        base.to_string()
    };
    if tecno() {
        s.to_uppercase()
    } else {
        s
    }
}

/// Paint the menu and return its row hitboxes. `sel` is the lit row and
/// `armed` the row waiting for its second press.
pub fn draw(
    cv: &mut Canvas,
    px: i32,
    py: i32,
    pw: i32,
    sel: usize,
    armed: Option<Action>,
    uptime: Option<&str>,
) -> Vec<(i32, i32, i32, i32, Action)> {
    let ph = size().1;
    draw_panel(cv, px, py, pw, ph, 10);

    // Header: whose session this is, and how long the machine has been up.
    let hy = py + (HEAD_H - GLYPH_H) / 2;
    cv.text_bold(&who(), px + 14, hy, pal().text);
    if let Some(up) = uptime {
        let up = match i18n::Lang::current() {
            i18n::Lang::Es => format!("activo {up}"),
            i18n::Lang::En => format!("up {up}"),
        };
        let ux = px + pw - 14 - Canvas::text_width(&up);
        cv.text(&up, ux, hy, pal().dim);
    }
    cv.hline(px + 10, py + HEAD_H - 1, pw - 20, pal().rule, 0.7);

    let mut hits = Vec::new();
    let mut ry = py + HEAD_H;
    for (i, &a) in ROWS.iter().enumerate() {
        if a == Action::PowerReboot {
            cv.hline(px + 10, ry + GAP / 2, pw - 20, pal().rule, 0.5);
            ry += GAP;
        }
        let lit = i == sel;
        let is_armed = armed == Some(a);
        let c = colour(a);
        if lit || is_armed {
            plate_a(cv, px + 5, ry + 1, pw - 10, ROW_H - 2, 6, pal().menu_hover, 1.0);
            if tecno() {
                cv.fill_rect_a(px + 5, ry + 1, 2, ROW_H - 2, c, 1.0);
            }
        }
        let (ix, iy) = (px + 16, ry + (ROW_H - ICON) / 2);
        match a {
            Action::PowerLock => cv.lock_icon(ix, iy, ICON, c),
            Action::PowerLogout => cv.exit_icon(ix, iy, ICON, c),
            Action::PowerReboot => cv.reboot_icon(ix, iy, ICON, c),
            _ => cv.power_icon(ix, iy, ICON, c),
        }
        let col = if is_armed {
            c
        } else if lit {
            pal().text
        } else {
            pal().muted
        };
        let ty = ry + (ROW_H - GLYPH_H) / 2;
        cv.text(&label(a, is_armed), px + 16 + ICON + 14, ty, col);
        hits.push((px + 5, ry, px + pw - 5, ry + ROW_H, a));
        ry += ROW_H;
    }
    hits
}

impl State {
    /// A press on session-menu row `a`: arm it when it ends something and is
    /// not armed yet, otherwise carry it out.
    pub(crate) fn power_act(&mut self, qh: &QueueHandle<State>, a: Action) {
        if let Some(Popup { kind: PopupKind::PowerMenu { sel, armed }, .. }) = self.popup.as_mut() {
            if needs_confirm(a) && *armed != Some(a) {
                *armed = Some(a);
                if let Some(i) = ROWS.iter().position(|&r| r == a) {
                    *sel = i;
                }
                self.render_popup(qh);
                return;
            }
        }
        self.close_popup();
        match a {
            Action::PowerLock => self.spawn("eclipse-lock || swaylock || lock"),
            Action::PowerLogout => self.spawn("labwc --exit || killall labwc || pkill labwc"),
            Action::PowerReboot => self.request_power(true),
            Action::PowerShutdown => self.request_power(false),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_locking_goes_at_once() {
        assert!(!needs_confirm(Action::PowerLock));
        for a in [Action::PowerLogout, Action::PowerReboot, Action::PowerShutdown] {
            assert!(needs_confirm(a));
        }
    }

    #[test]
    fn every_action_has_its_own_colour() {
        for (i, &a) in ROWS.iter().enumerate() {
            for &b in &ROWS[..i] {
                assert_ne!(colour(a), colour(b), "{i}");
            }
        }
    }

    #[test]
    fn the_panel_fits_its_rows_and_every_question() {
        let (pw, ph) = size();
        assert_eq!(ph, HEAD_H + 4 * ROW_H + GAP + PAD);
        for &a in &ROWS {
            for armed in [false, true] {
                let end = 16 + ICON + 14 + Canvas::text_width(&label(a, armed));
                assert!(end <= pw - 12, "{}", label(a, armed));
            }
        }
    }
}
