//! hunter on the desktop: the shield on the bottom bar and the menu it opens.
//!
//! The data and the scripts are `lunarbar::hunter`'s; this is only the panel
//! side — where the shield sits, what the menu draws, and what each click
//! sends. Its own file because `main.rs` is at its size ceiling (see
//! `xtask/src/tamano.rs`), and a child module of the binary because the
//! palette, `Action` and `State` are the binary's.

use super::*;
use lunarbar::hunter::{self, Mode, Shield, Status};
use lunarbar::hunter_media as media;

/// Re-read `/proc/hunter` every this many 1 Hz ticks. The report carries the
/// whole event ring, which the kernel formats on every read, so not every
/// tick; a click re-reads at once anyway, so the menu never shows a stale mode
/// after a change.
pub(super) const REFRESH_TICKS: u64 = 3;

/// What the hunter menu shows: a snapshot of the status (re-taken after every
/// change it sends), whether the lockdown button is waiting for its second
/// click, and the outcome of the last change when it did not go through.
pub(super) struct Menu {
    pub st: Status,
    pub confirm: bool,
    pub msg: Option<&'static str>,
}

const PW: i32 = 380;
const PAD: i32 = 14;
const HEAD_H: i32 = 34;
const COUNT_H: i32 = 24;
const ROW_H: i32 = 26;
/// Six domains plus learning.
const ROWS: i32 = hunter::DOMAINS.len() as i32 + 1;
/// Wide enough for the longest mode name ("enforce", "bloqueo": 7 glyphs).
const SEG_W: i32 = 74;
const EVENT_H: i32 = 18;
/// The removable-media line, under the switches.
const MEDIA_H: i32 = 22;
const MSG_H: i32 = 20;
const BTN_H: i32 = 28;

/// The panel size. Fixed, so a message appearing never has to reposition the
/// popup: its line is always reserved.
pub(super) fn size() -> (i32, i32) {
    (
        PW,
        HEAD_H
            + COUNT_H
            + ROWS * ROW_H
            + MEDIA_H
            + 8
            + hunter::RECENT as i32 * EVENT_H
            + MSG_H
            + BTN_H
            + PAD,
    )
}

/// Draw the shield pill at the right edge `rx` of what is left of the bar,
/// moving `rx` past it. Returns its hitbox, or (0,0) when it does not fit.
pub(super) fn draw_badge(
    cv: &mut Canvas,
    rx: &mut i32,
    left_min: i32,
    btn_y: i32,
    btn_h: i32,
    hovered: bool,
    shield: Shield,
) -> (i32, i32) {
    let size = 14;
    let w = size + 16;
    if *rx - w < left_min {
        return (0, 0);
    }
    *rx -= w;
    let pill = if hovered {
        pal().pill_hover
    } else {
        pal().pill
    };
    cv.round_rect(*rx, btn_y, w, btn_h, 6, pill);
    let (col, solid) = match shield {
        Shield::Alert => (pal().warn, true),
        Shield::Open => (pal().dim, false),
        Shield::Guarded => (pal().accent, true),
    };
    cv.shield_icon(
        *rx + (w - size) / 2,
        btn_y + (btn_h - size) / 2,
        size,
        col,
        solid,
    );
    let hit = (*rx, *rx + w);
    *rx -= 10;
    hit
}

/// The x of segment `i` (0..3) of a row in a panel at `px`.
fn seg_x(px: i32, i: usize) -> i32 {
    px + PW - PAD - (3 - i as i32) * SEG_W
}

/// Cut `s` to what fits in `w` pixels, marking the cut.
fn fit(s: &str, w: i32) -> String {
    let max = (w / GLYPH_W).max(1) as usize;
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max - 1).chain(['.']).collect()
}

/// One segmented control: `labels` drawn over the right-hand segments
/// (`first` is the slot the first label takes), `on` highlighted, and the
/// ones `enabled` refuses dimmed and left without a hitbox.
#[allow(clippy::too_many_arguments)]
fn segments(
    cv: &mut Canvas,
    px: i32,
    ry: i32,
    labels: &[&str],
    first: usize,
    on: usize,
    enabled: impl Fn(usize) -> bool,
    action: impl Fn(usize) -> Action,
    hits: &mut Vec<(i32, i32, i32, i32, Action)>,
) {
    let (sy, sh) = (ry + 3, ROW_H - 6);
    for (i, label) in labels.iter().enumerate() {
        let x = seg_x(px, first + i) + 2;
        let w = SEG_W - 4;
        let ok = enabled(i);
        let (bg, a, fg) = if i == on {
            (pal().accent, 1.0, pal().bar_bg)
        } else if ok {
            (pal().pill, 1.0, pal().muted)
        } else {
            (pal().pill, 0.4, pal().dim)
        };
        cv.round_rect_a(x, sy, w, sh, 5, bg, a);
        let label = fit(label, w - 4);
        let tx = x + (w - Canvas::text_width(&label)) / 2;
        cv.text(&label, tx, sy + (sh - GLYPH_H) / 2, fg);
        if ok && i != on {
            hits.push((x, sy, x + w, sy + sh, action(i)));
        }
    }
}

/// Paint the menu into the panel rect and return its hitboxes.
pub(super) fn draw(
    cv: &mut Canvas,
    px: i32,
    py: i32,
    pw: i32,
    ph: i32,
    m: &Menu,
) -> Vec<(i32, i32, i32, i32, Action)> {
    let t = hunter::text(i18n::Lang::current());
    let st = &m.st;
    draw_panel(cv, px, py, pw, ph, 10);
    cv.round_rect_a(px, py, pw, ph, 10, pal().rule, 0.4);
    let mut hits = Vec::new();

    // Header: the shield as the bar shows it (minus the alert, which opening
    // this menu just acknowledged), the name, and the seal.
    let shield = st.shield(st.criticals);
    let col = if shield == Shield::Open {
        pal().dim
    } else {
        pal().accent
    };
    cv.shield_icon(px + PAD, py + 9, 16, col, shield != Shield::Open);
    cv.text_bold(t.title, px + PAD + 24, py + 10, pal().text);
    // "open": the rest of hunter, in its own window.
    let (ox, ow) = (px + PAD + 24 + Canvas::text_width(t.title) + 12, Canvas::text_width(t.open) + 16);
    cv.round_rect(ox, py + 7, ow, GLYPH_H + 6, 5, pal().pill);
    cv.text(t.open, ox + 8, py + 10, pal().accent);
    hits.push((ox, py + 7, ox + ow, py + 7 + GLYPH_H + 6, Action::HunterOpen));
    if st.sealed {
        let w = Canvas::text_width(t.sealed);
        cv.lock_icon(px + pw - PAD - w - 18, py + 10, 14, pal().warn);
        cv.text(t.sealed, px + pw - PAD - w, py + 10, pal().warn);
    }
    cv.hline(px + 10, py + HEAD_H - 4, pw - 20, pal().rule, 0.5);

    let mut y = py + HEAD_H;
    let counts = format!(
        "{} {} · {} {} · {} {}",
        t.blocked, st.blocked, t.alerts, st.criticals, t.signatures, st.signatures
    );
    cv.text(&fit(&counts, pw - 2 * PAD), px + PAD, y + 2, pal().muted);
    y += COUNT_H;

    // One row per domain: name, then off / report / enforce.
    for (d, name) in hunter::DOMAINS.iter().enumerate() {
        cv.text(name, px + PAD, y + (ROW_H - GLYPH_H) / 2, pal().text);
        let on = Mode::ALL
            .iter()
            .position(|&x| x == st.modes[d])
            .unwrap_or(0);
        segments(
            cv,
            px,
            y,
            &t.modes,
            0,
            on,
            |i| !(st.sealed && st.relaxes(d, Mode::ALL[i])),
            |i| Action::HunterMode(d as u8, Mode::ALL[i]),
            &mut hits,
        );
        y += ROW_H;
    }
    // Learning: off / on in the two right-hand slots. On is the relaxation.
    cv.text(t.learning, px + PAD, y + (ROW_H - GLYPH_H) / 2, pal().text);
    segments(
        cv,
        px,
        y,
        &["off", "on"],
        1,
        st.learning as usize,
        |i| !(st.sealed && i == 1),
        |i| Action::HunterLearning(i == 1),
        &mut hits,
    );
    y += ROW_H;

    // USB sticks and disks: the one being scanned, an infected one, or the
    // newest, with the verdict. A kernel without the media table has no line.
    if st.media_known {
        let lang = i18n::Lang::current();
        let col = match media::headline(st) {
            Some(d) if media::is_bad(d) => pal().warn,
            Some(d) if d.state == hunter::MediaState::Clean => pal().muted,
            Some(_) => pal().accent,
            None => pal().dim,
        };
        let line = media::summary(lang, st);
        cv.text(&fit(&line, pw - 2 * PAD), px + PAD, y + (MEDIA_H - GLYPH_H) / 2, col);
    }
    y += MEDIA_H + 8;

    // The newest warnings and criticals.
    cv.hline(px + 10, y - 5, pw - 20, pal().rule, 0.5);
    if st.recent.is_empty() {
        cv.text(t.no_events, px + PAD, y + 1, pal().dim);
    }
    for e in &st.recent {
        let col = if e.critical { pal().warn } else { pal().muted };
        cv.text(&fit(&e.text, pw - 2 * PAD), px + PAD, y + 1, col);
        y += EVENT_H;
    }
    y = py + ph - PAD - BTN_H - MSG_H;
    if let Some(msg) = m.msg {
        cv.text(&fit(msg, pw - 2 * PAD), px + PAD, y + 2, pal().warn);
    }
    y += MSG_H;

    // Buttons: scan, update signatures, the log, and lockdown (two clicks).
    let buttons = [
        (t.scan, Action::HunterScan),
        (t.update, Action::HunterUpdate),
        (t.log, Action::HunterLog),
        (
            if m.confirm { t.confirm } else { t.lockdown },
            Action::HunterLockdown,
        ),
    ];
    let gap = 6;
    let bw = (pw - 2 * PAD - 3 * gap) / 4;
    for (i, (label, act)) in buttons.iter().enumerate() {
        let x = px + PAD + i as i32 * (bw + gap);
        let armed = act == &Action::HunterLockdown && m.confirm;
        let bg = if armed { pal().warn } else { pal().pill };
        cv.round_rect(x, y, bw, BTN_H, 6, bg);
        let label = fit(label, bw - 6);
        let fg = if armed { pal().white } else { pal().text };
        cv.text(
            &label,
            x + (bw - Canvas::text_width(&label)) / 2,
            y + (BTN_H - GLYPH_H) / 2,
            fg,
        );
        hits.push((x, y, x + bw, y + BTN_H, *act));
    }
    hits
}

/// The menu `LUNARBAR_DUMP_HUNTER` previews: this kernel's hunter when there
/// is one, otherwise a sample with a little of everything to show.
pub(super) fn preview() -> Menu {
    let st = hunter::read().unwrap_or_else(|| Status {
        modes: [
            Mode::Enforce,
            Mode::Report,
            Mode::Enforce,
            Mode::Off,
            Mode::Report,
            Mode::Enforce,
            Mode::Report,
        ],
        sealed: true,
        blocked: 3,
        warnings: 5,
        criticals: 1,
        signatures: 5002,
        detections: 1,
        pinned: 7,
        learning: false,
        media_known: true,
        media_scan: true,
        media_lock: false,
        media: vec![hunter::Media {
            id: 3,
            state: hunter::MediaState::Scanning,
            source: "/dev/sdb1".into(),
            target: "/media/usb".into(),
            ..Default::default()
        }],
        recent: vec![
            hunter::Event {
                seq: 9,
                critical: true,
                text: "AV AV BLOCKED: /tmp/eicar.com (Eicar-Test-Signature)".into(),
            },
            hunter::Event {
                seq: 7,
                critical: false,
                text: "EXEC REPORTED: untrusted exec /tmp/a.out".into(),
            },
        ],
    });
    Menu {
        st,
        confirm: false,
        msg: None,
    }
}

impl State {
    /// Re-read hunter's status. A kernel without hunter leaves it `None`,
    /// which hides the shield.
    pub(super) fn refresh_hunter(&mut self) {
        self.hunter = hunter::read();
        self.hunter_media.tick(self.hunter.as_ref());
        self.metrics.hunter = self.hunter.as_ref().map(|s| s.shield(self.hunter_seen));
        self.toast_kernel();
    }

    /// Shield click: toggle the menu. Opening it counts as having seen the
    /// criticals so far, which takes the alert off the shield.
    pub(super) fn toggle_hunter(&mut self, qh: &QueueHandle<State>, at_top: bool) {
        if matches!(&self.popup, Some(p) if matches!(p.kind, PopupKind::Hunter(_))) {
            self.close_popup();
            return;
        }
        self.refresh_hunter();
        let Some(st) = self.hunter.clone() else {
            return;
        };
        self.hunter_seen = st.criticals;
        self.metrics.hunter = Some(st.shield(st.criticals));
        self.render_task_bars();
        let menu = Menu {
            st,
            confirm: false,
            msg: None,
        };
        self.open_popup(qh, PopupKind::Hunter(menu), at_top);
    }

    /// A click on one of the menu's controls.
    pub(super) fn hunter_click(&mut self, qh: &QueueHandle<State>, act: Action) {
        let lang = i18n::Lang::current();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let term =
            |cmd: String| format!("{} sh -c \"{}\"", self.terminal, cmd.replace('"', "\\\""));
        let (script, keep_confirm) = match act {
            Action::HunterMode(d, mode) => (hunter::mode_script(d as usize, mode), false),
            Action::HunterLearning(on) => (
                Some(format!("learning {}\n", if on { "on" } else { "off" })),
                false,
            ),
            Action::HunterLockdown => {
                let armed = matches!(&self.popup, Some(p) if matches!(&p.kind, PopupKind::Hunter(m) if m.confirm));
                if armed {
                    (Some(hunter::LOCKDOWN.to_string()), false)
                } else {
                    (None, true)
                }
            }
            Action::HunterScan | Action::HunterUpdate | Action::HunterLog | Action::HunterOpen => {
                // lunarhunter when it is installed, on the tab the button
                // names; a terminal running hunterctl when it is not.
                let cmd = if apps::executable_exists(hunter::APP) {
                    match act {
                        Action::HunterScan => hunter::app_scan_command(&home),
                        Action::HunterUpdate => hunter::app_update_command(),
                        Action::HunterLog => hunter::app_events_command(),
                        _ => hunter::app_command(),
                    }
                } else {
                    term(match act {
                        Action::HunterScan => hunter::scan_command(&home),
                        Action::HunterUpdate => hunter::update_command(),
                        // The log, and "open" with no window to open.
                        _ => hunter::log_command(),
                    })
                };
                self.close_popup();
                self.spawn(&cmd);
                return;
            }
            _ => return,
        };
        let msg = match &script {
            Some(s) => {
                let knobs = |st: &Option<Status>| st.as_ref().map(|s| (s.modes, s.learning));
                let before = knobs(&self.hunter);
                match hunter::send(s) {
                    Ok(()) => {
                        self.refresh_hunter();
                        // hunter takes a refused relaxation without an error
                        // (it says so only in its ring, which moves the
                        // counters), so check the knobs themselves.
                        if before.is_some()
                            && before == knobs(&self.hunter)
                            && act != Action::HunterLockdown
                        {
                            Some(hunter::text(lang).refused)
                        } else {
                            None
                        }
                    }
                    Err(e) => Some(hunter::error_text(lang, e)),
                }
            }
            None => None,
        };
        let st = self.hunter.clone();
        if let Some(PopupKind::Hunter(m)) = self.popup.as_mut().map(|p| &mut p.kind) {
            if let Some(st) = st {
                m.st = st;
            }
            m.confirm = keep_confirm;
            m.msg = msg;
        }
        self.render_popup(qh);
        self.render_task_bars();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status {
            modes: [
                Mode::Enforce,
                Mode::Report,
                Mode::Report,
                Mode::Off,
                Mode::Report,
                Mode::Report,
                Mode::Report,
            ],
            recent: (0..hunter::RECENT as u64)
                .map(|seq| hunter::Event {
                    seq,
                    critical: seq == 0,
                    text: "EXEC x".repeat(30),
                })
                .collect(),
            ..Status::default()
        }
    }

    type Hits = Vec<(i32, i32, i32, i32, Action)>;

    fn hits_of(m: &Menu) -> (Hits, (i32, i32)) {
        let (pw, ph) = size();
        let mut cv = Canvas::new(pw as usize, ph as usize);
        (draw(&mut cv, 0, 0, pw, ph, m), (pw, ph))
    }

    #[test]
    fn every_control_lands_inside_the_panel() {
        let (hits, (pw, ph)) = hits_of(&Menu {
            st: status(),
            confirm: false,
            msg: Some("x"),
        });
        assert!(!hits.is_empty());
        for (x0, y0, x1, y1, _) in hits {
            assert!(
                x0 >= 0 && y0 >= 0 && x1 <= pw && y1 <= ph,
                "{x0},{y0}..{x1},{y1}"
            );
        }
    }

    #[test]
    fn the_rows_and_the_event_list_never_reach_the_buttons() {
        let (hits, _) = hits_of(&Menu {
            st: status(),
            confirm: false,
            msg: None,
        });
        let lowest_row = hits
            .iter()
            .filter(|h| matches!(h.4, Action::HunterMode(..) | Action::HunterLearning(_)))
            .map(|h| h.3)
            .max()
            .unwrap();
        let top_button = hits
            .iter()
            .filter(|h| matches!(h.4, Action::HunterScan))
            .map(|h| h.1)
            .min()
            .unwrap();
        let events = hunter::RECENT as i32 * EVENT_H + MSG_H;
        assert!(
            lowest_row + events <= top_button,
            "{lowest_row} + {events} > {top_button}"
        );
    }

    #[test]
    fn the_current_mode_is_not_offered_again() {
        let (hits, _) = hits_of(&Menu {
            st: status(),
            confirm: false,
            msg: None,
        });
        // syscall is at enforce: off and report are offered, enforce is not.
        assert!(hits.iter().any(|h| h.4 == Action::HunterMode(0, Mode::Off)));
        assert!(!hits
            .iter()
            .any(|h| h.4 == Action::HunterMode(0, Mode::Enforce)));
    }

    #[test]
    fn a_sealed_system_is_offered_no_relaxation() {
        let mut st = status();
        st.sealed = true;
        let (hits, _) = hits_of(&Menu {
            st: st.clone(),
            confirm: false,
            msg: None,
        });
        for h in &hits {
            match h.4 {
                Action::HunterMode(d, m) => assert!(!st.relaxes(d as usize, m), "{d} {m:?}"),
                Action::HunterLearning(on) => assert!(!on),
                _ => {}
            }
        }
        // Tightening is still there.
        assert!(hits
            .iter()
            .any(|h| h.4 == Action::HunterMode(1, Mode::Enforce)));
    }

    #[test]
    fn the_segments_of_a_row_do_not_overlap_its_label() {
        let longest = hunter::DOMAINS
            .iter()
            .chain(std::iter::once(&hunter::text(i18n::Lang::Es).learning))
            .map(|d| Canvas::text_width(d))
            .max()
            .unwrap();
        assert!(PAD + longest + 4 <= seg_x(0, 0), "{longest}");
    }

    #[test]
    fn no_label_is_cut_in_either_language() {
        let bw = (PW - 2 * PAD - 3 * 6) / 4;
        for l in [i18n::Lang::Es, i18n::Lang::En] {
            let t = hunter::text(l);
            for m in t.modes.iter().chain(&["off", "on"]) {
                assert_eq!(fit(m, SEG_W - 8), *m, "{l:?}");
            }
            for b in [t.scan, t.update, t.log, t.lockdown, t.confirm] {
                assert_eq!(fit(b, bw - 6), b, "{l:?}");
            }
        }
    }

    #[test]
    fn the_open_button_sits_between_the_title_and_the_seal() {
        let mut st = status();
        st.sealed = true;
        let (hits, (pw, _)) = hits_of(&Menu {
            st,
            confirm: false,
            msg: None,
        });
        let open = hits.iter().find(|h| h.4 == Action::HunterOpen).unwrap();
        assert!(open.3 <= HEAD_H, "inside the header");
        let sealed_w = Canvas::text_width(hunter::text(i18n::Lang::Es).sealed) + 18;
        assert!(open.2 + 8 <= pw - PAD - sealed_w, "clear of the seal");
    }

    #[test]
    fn a_badge_that_does_not_fit_takes_no_room() {
        let mut cv = Canvas::new(100, 30);
        let mut rx = 40;
        assert_eq!(
            draw_badge(&mut cv, &mut rx, 30, 3, 24, false, Shield::Guarded),
            (0, 0)
        );
        assert_eq!(rx, 40);
        let mut rx = 90;
        assert_eq!(
            draw_badge(&mut cv, &mut rx, 30, 3, 24, false, Shield::Alert),
            (60, 90)
        );
        assert_eq!(rx, 50);
    }
}
