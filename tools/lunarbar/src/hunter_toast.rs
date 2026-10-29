//! hunter's notifications on the desktop: cards that pop up in the corner
//! above the bar, one per alert, with its buttons.
//!
//! What an alert is, where they come from and what their buttons run is
//! `lunarbar::hunter_notify`'s; this is the panel side — a layer surface of
//! its own on the overlay layer, the cards drawn on it, the clicks, and the
//! clock that takes them down. Its own file because `main.rs` is at its size
//! ceiling (see `xtask/src/tamano.rs`), and a child module of the binary
//! because the palette, the canvas and `State` are the binary's.
//!
//! One surface for the whole stack, not one per card: the cards come and go
//! together often (a burst of events, a click), and one surface is one
//! configure to wait for instead of one per card.

use super::*;
use lunarbar::hunter_notify::{self as notify, Alert, Severity};
use std::collections::HashSet;
use std::path::Path;

/// Udata marker for the notification surface and its buffers.
#[derive(Clone, Copy)]
pub(super) struct ToastId;

/// Cards on screen at once; the rest wait behind a "+N more" line.
const VISIBLE: usize = 3;
/// Body lines a card shows before cutting.
const BODY_LINES: usize = 3;
/// Kernel events ignored for the session, at most (a flood of distinct
/// events must not grow the set without end).
const MAX_MUTED: usize = 64;

const W: i32 = 360;
const M: i32 = POPUP_SHADOW;
const GAP: i32 = 8;
const PAD: i32 = 12;
const LINE: i32 = GLYPH_H + 4;
const BTN_H: i32 = 24;
const MORE_H: i32 = 24;
const CLOSE: i32 = 18;

/// Where a card came from, which is what its buttons have to answer.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    /// The kernel's event ring. `key` is the event's text: ignoring mutes it.
    Kernel { key: String },
    /// A file in the spool, answered there when the card goes.
    Spool { id: String },
}

#[derive(Clone, Debug)]
pub(super) struct Toast {
    alert: Alert,
    source: Source,
    /// The same kernel event again while its card was up: one card, counted.
    count: u32,
    /// Set when the card first shows, so a card waiting behind others does
    /// not time out unseen.
    deadline: Option<Instant>,
}

/// What a click on the surface lands on. Cards are indexed into `list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hit {
    Action(usize, usize),
    Close(usize),
    More,
}

/// The ARGB surface the stack draws into; same double buffering as the
/// tooltip's.
struct Surf {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// The size asked for; the configure answers it.
    want: (u32, u32),
    width: u32,
    height: u32,
    scale: u32,
    map: *mut u8,
    map_len: usize,
    buffers: [Option<wl_buffer::WlBuffer>; BUFFERS],
    busy: [bool; BUFFERS],
    next: usize,
    generation: u64,
}

impl Drop for Surf {
    fn drop(&mut self) {
        for b in self.buffers.iter().flatten() {
            b.destroy();
        }
        if !self.map.is_null() {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        self.layer.destroy();
        self.surface.destroy();
    }
}

#[derive(Default)]
pub(super) struct Toasts {
    /// Oldest first; the newest [`VISIBLE`] are on screen, newest at the
    /// bottom, nearest the bar.
    list: Vec<Toast>,
    /// The newest ring event already turned into a card (or seen at startup,
    /// which shows nothing: the backlog is what the menu and the window are
    /// for), `None` before the first read.
    last_seq: Option<u64>,
    primed: bool,
    muted: HashSet<String>,
    surf: Option<Surf>,
    hits: Vec<(i32, i32, i32, i32, Hit)>,
    hover: Option<Hit>,
    pub(super) pointer_in: bool,
    /// The list changed since the surface last drew it.
    dirty: bool,
}

/// The color of a severity: the stripe, the shield and the first button.
fn sev_color(s: Severity) -> Rgb {
    match s {
        Severity::Crit => pal().warn,
        // KDE's and Windows' warning amber; the palettes have no amber of
        // their own, and red would make every warning read as critical.
        Severity::Warn => (0xf6, 0x74, 0x00),
        Severity::Info => pal().accent,
    }
}

/// Cut `s` into at most `lines` lines of `w` pixels, at spaces when it can,
/// marking a cut at the end.
fn wrap(s: &str, w: i32, lines: usize) -> Vec<String> {
    let max = (w / GLYPH_W).max(2) as usize;
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let mut word = word.to_string();
        loop {
            let need = cur.chars().count() + usize::from(!cur.is_empty()) + word.chars().count();
            if need <= max {
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur += &word;
                break;
            }
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            // A word longer than a line (a path): break it.
            let head: String = word.chars().take(max).collect();
            word = word.chars().skip(max).collect();
            out.push(head);
            if word.is_empty() {
                break;
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.len() > lines {
        out.truncate(lines);
        let last = out.last_mut().unwrap();
        let keep: String = last.chars().take(max - 1).collect();
        *last = keep + ".";
    }
    out
}

fn card_h(t: &Toast) -> i32 {
    let body = wrap(&t.alert.body, W - 2 * PAD, BODY_LINES).len() as i32;
    let btns = if t.alert.actions.is_empty() {
        0
    } else {
        8 + BTN_H
    };
    PAD + LINE + body * LINE + btns + PAD - 4
}

/// The surface size for these cards and this many waiting behind them.
fn size(shown: &[&Toast], hidden: usize) -> (i32, i32) {
    let cards: i32 = shown.iter().map(|t| card_h(t)).sum();
    let gaps = (shown.len() as i32 - 1).max(0) * GAP;
    let more = if hidden > 0 { MORE_H + GAP } else { 0 };
    (W + 2 * M, cards + gaps + more + 2 * M)
}

/// Paint the stack and return its hitboxes. `idx` is each shown card's index
/// into the list (what a [`Hit`] names).
fn draw(
    cv: &mut Canvas,
    origin: (i32, i32),
    shown: &[(usize, &Toast)],
    hidden: usize,
    hover: Option<Hit>,
) -> Vec<(i32, i32, i32, i32, Hit)> {
    let t = notify::text(i18n::Lang::current());
    let mut hits = Vec::new();
    let x0 = origin.0 + M;
    let mut y = origin.1 + M;
    if hidden > 0 {
        let label = format!("+{hidden} {}", t.more);
        let w = Canvas::text_width(&label) + 24;
        let x = x0 + W - w;
        let bg = if hover == Some(Hit::More) {
            pal().pill_hover
        } else {
            pal().menu_panel
        };
        cv.round_rect_a(x, y, w, MORE_H, 8, bg, 0.95);
        cv.text(&label, x + 12, y + (MORE_H - GLYPH_H) / 2, pal().muted);
        hits.push((x, y, x + w, y + MORE_H, Hit::More));
        y += MORE_H + GAP;
    }
    for &(i, toast) in shown {
        let a = &toast.alert;
        let h = card_h(toast);
        let col = sev_color(a.severity);
        draw_panel(cv, x0, y, W, h, 10);
        cv.round_rect_a(x0, y, W, h, 10, pal().rule, 0.35);
        cv.round_rect_a(x0 + 1, y + 1, W - 2, h - 2, 9, pal().menu_panel, 1.0);
        // The severity as a stripe down the left edge.
        cv.round_rect(x0 + 1, y + 8, 3, h - 16, 1, col);

        let ty = y + PAD;
        cv.shield_icon(x0 + PAD, ty, 14, col, true);
        let mut title = a.title.clone();
        if toast.count > 1 {
            title += &format!("  x{}", toast.count);
        }
        let room = W - 2 * PAD - 22 - CLOSE - 6;
        let title = wrap(&title, room, 1).pop().unwrap_or_default();
        cv.text_bold(&title, x0 + PAD + 22, ty, pal().text);
        // The close cross.
        let (cx, cy) = (x0 + W - PAD - CLOSE + 4, ty - 2);
        if hover == Some(Hit::Close(i)) {
            cv.round_rect(cx, cy, CLOSE, CLOSE, 4, pal().pill_hover);
        }
        cv.text(
            "x",
            cx + (CLOSE - GLYPH_W) / 2,
            cy + (CLOSE - GLYPH_H) / 2,
            pal().dim,
        );
        hits.push((cx, cy, cx + CLOSE, cy + CLOSE, Hit::Close(i)));

        let mut ly = ty + LINE;
        for line in wrap(&a.body, W - 2 * PAD, BODY_LINES) {
            cv.text(&line, x0 + PAD, ly, pal().muted);
            ly += LINE;
        }

        // The buttons, right-aligned, the first one (the producer's main
        // answer) filled in the severity's color.
        if !a.actions.is_empty() {
            let by = ly + 8 - 4;
            let mut bx = x0 + W - PAD;
            for (j, act) in a.actions.iter().enumerate().rev() {
                let label = wrap(&act.label, 120, 1).pop().unwrap_or_default();
                let bw = Canvas::text_width(&label) + 20;
                bx -= bw;
                let hot = hover == Some(Hit::Action(i, j));
                let (bg, fg) = match (j, hot) {
                    (0, _) => (col, pal().white),
                    (_, true) => (pal().pill_hover, pal().text),
                    _ => (pal().pill, pal().text),
                };
                cv.round_rect(bx, by, bw, BTN_H, 6, bg);
                if j == 0 && hot {
                    cv.round_rect_a(bx, by, bw, BTN_H, 6, pal().white, 0.15);
                }
                cv.text(&label, bx + 10, by + (BTN_H - GLYPH_H) / 2, fg);
                hits.push((bx, by, bx + bw, by + BTN_H, Hit::Action(i, j)));
                bx -= 6;
            }
        }
        y += h + GAP;
    }
    hits
}

impl Toasts {
    /// The cards on screen, with their index into the list, top to bottom
    /// (oldest of the shown first, newest last), and how many wait.
    fn shown(&self) -> (Vec<(usize, &Toast)>, usize) {
        let start = self.list.len().saturating_sub(VISIBLE);
        (self.list.iter().enumerate().skip(start).collect(), start)
    }

    /// Add a card, or count it on the one already showing the same event.
    fn push(&mut self, t: Toast) {
        if let Some(pos) = self.list.iter().position(|x| x.source == t.source) {
            let mut old = self.list.remove(pos);
            old.count += 1;
            old.deadline = None;
            self.list.push(old);
        } else {
            self.list.push(t);
        }
        self.dirty = true;
    }

    /// Take card `i` down, answering its producer.
    fn close(&mut self, i: usize, answer: &str) -> Option<Toast> {
        if i >= self.list.len() {
            return None;
        }
        let t = self.list.remove(i);
        if let Source::Spool { id } = &t.source {
            notify::answer(Path::new(notify::SPOOL), id, t.alert.reply, answer);
        }
        self.dirty = true;
        Some(t)
    }

    /// The ring's new warnings and criticals since the last read become
    /// cards. The status keeps only the newest few, so a burst bigger than
    /// that shows its tail — the shield and the window have the rest.
    fn ingest(&mut self, st: &hunter::Status) {
        let newest = st.recent.first().map(|e| e.seq);
        if !self.primed {
            self.primed = true;
            self.last_seq = newest;
            return;
        }
        let lang = i18n::Lang::current();
        for e in st.recent.iter().rev() {
            if self.last_seq.is_some_and(|last| e.seq <= last) {
                continue;
            }
            self.last_seq = Some(e.seq);
            if self.muted.contains(&e.text) {
                continue;
            }
            self.push(Toast {
                alert: notify::from_event(e, st.sealed, lang),
                source: Source::Kernel {
                    key: e.text.clone(),
                },
                count: 1,
                deadline: None,
            });
        }
    }

    /// The spool's alerts: new ones become cards, and the cards of the ones
    /// whose file is gone (the producer stopped waiting) go.
    fn ingest_spool(&mut self, pending: Vec<(String, Alert)>) {
        let ids: HashSet<&str> = pending.iter().map(|(id, _)| id.as_str()).collect();
        let before = self.list.len();
        self.list
            .retain(|t| !matches!(&t.source, Source::Spool { id } if !ids.contains(id.as_str())));
        if self.list.len() != before {
            self.dirty = true;
        }
        for (id, alert) in pending {
            let source = Source::Spool { id };
            if self.list.iter().any(|t| t.source == source) {
                continue;
            }
            self.list.push(Toast {
                alert,
                source,
                count: 1,
                deadline: None,
            });
            self.dirty = true;
        }
    }

    /// Start the clocks of the cards that just came into view, and take down
    /// the ones whose time is up — not while the pointer is on them.
    fn expire(&mut self, now: Instant) {
        if !self.pointer_in {
            let mut i = self.list.len().saturating_sub(VISIBLE);
            while i < self.list.len() {
                if self.list[i].deadline.is_some_and(|d| d <= now) {
                    self.close(i, notify::TIMEOUT);
                } else {
                    i += 1;
                }
            }
        }
        let start = self.list.len().saturating_sub(VISIBLE);
        for t in &mut self.list[start..] {
            if t.deadline.is_none() {
                if let Some(s) = t.alert.lifetime() {
                    t.deadline = Some(now + Duration::from_secs(s as u64));
                }
            }
        }
    }
}

impl State {
    /// After a read of `/proc/hunter` (`refresh_hunter`): its new events.
    pub(super) fn toast_kernel(&mut self) {
        if let Some(st) = self.hunter.as_ref() {
            self.toasts.ingest(st);
        }
    }

    /// Once a tick: the spool, the clocks, and a repaint when anything moved.
    pub(super) fn toast_tick(&mut self, qh: &QueueHandle<State>) {
        self.toasts
            .ingest_spool(notify::pending(Path::new(notify::SPOOL)));
        self.toasts.expire(Instant::now());
        if self.toasts.dirty {
            self.toasts.dirty = false;
            self.toast_sync(qh);
        }
    }

    /// Make the surface match the list: gone when it is empty, created or
    /// resized when its size changed (the configure that answers paints it),
    /// repainted in place otherwise.
    fn toast_sync(&mut self, qh: &QueueHandle<State>) {
        let (w, h) = {
            let (shown, hidden) = self.toasts.shown();
            if shown.is_empty() {
                self.toasts.surf = None;
                self.toasts.hits.clear();
                self.toasts.hover = None;
                self.toasts.pointer_in = false;
                return;
            }
            let cards: Vec<&Toast> = shown.iter().map(|(_, t)| *t).collect();
            size(&cards, hidden)
        };
        let want = (w as u32, h as u32);
        if let Some(s) = self.toasts.surf.as_mut() {
            if s.want != want {
                s.want = want;
                // Until the configure repaints, the old boxes name cards
                // that may have moved.
                self.toasts.hits.clear();
                s.layer.set_size(want.0, want.1);
                s.surface.commit();
            } else {
                self.toast_render();
            }
            return;
        }
        let (Some(comp), Some(ls)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        // The bottom bar's output, where the shield is.
        let Some(bar) = self
            .bars
            .iter()
            .find(|b| b.role == Role::Task)
            .or(self.bars.first())
        else {
            return;
        };
        let surface = comp.create_surface(qh, ());
        let layer = ls.get_layer_surface(
            &surface,
            Some(&bar.output),
            zwlr_layer_shell_v1::Layer::Overlay,
            "hunter-notify".into(),
            qh,
            ToastId,
        );
        layer.set_anchor(Anchor::Bottom | Anchor::Right);
        layer.set_size(want.0, want.1);
        // Relative to the work area, which already leaves the bar out.
        layer.set_margin(0, 4, 4, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        self.toasts.surf = Some(Surf {
            surface,
            layer,
            want,
            width: 0,
            height: 0,
            scale: bar.scale.clamp(1, 8),
            map: std::ptr::null_mut(),
            map_len: 0,
            buffers: [None, None],
            busy: [false, false],
            next: 0,
            generation: 0,
        });
    }

    /// (Re)allocate the pool after a configure and paint. The same sizing
    /// rules as `configure_tip`.
    fn toast_configure(&mut self, qh: &QueueHandle<State>, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let (w, h) = (w.max(1), h.max(1));
        if matches!(self.toasts.surf.as_ref(), Some(s) if s.width == w && s.height == h && !s.map.is_null())
        {
            self.toast_render();
            return;
        }
        let scale = self.toasts.surf.as_ref().map(|s| s.scale).unwrap_or(1);
        let (bw, bh) = (w.saturating_mul(scale), h.saturating_mul(scale));
        if bw > fill_guard::MAX_BUFFER_DIM
            || bh > fill_guard::MAX_BUFFER_DIM
            || (bw as usize).saturating_mul(bh as usize) > fill_guard::MAX_BUFFER_PIXELS
        {
            eprintln!("lunarbar: notifications {bw}x{bh} too large; skipping");
            return;
        }
        let Some(geom) = lunarbar::proc::pool_geometry(bw, bh) else {
            return;
        };
        let Some((map, fd)) = Self::map_shm_pool(geom.total) else {
            return;
        };
        let generation = self.next_generation();
        let pool = shm.create_pool(fd.as_fd(), geom.total as i32, qh, ());
        let mk = |i: usize| {
            pool.create_buffer(
                (i * geom.frame_size) as i32,
                bw as i32,
                bh as i32,
                geom.stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (ToastId, i, generation),
            )
        };
        let buffers = [Some(mk(0)), Some(mk(1))];
        pool.destroy();
        let Some(s) = self.toasts.surf.as_mut() else {
            unsafe { libc::munmap(map as *mut libc::c_void, geom.total) };
            return;
        };
        for b in s.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !s.map.is_null() {
            unsafe { libc::munmap(s.map as *mut libc::c_void, s.map_len) };
        }
        s.width = w;
        s.height = h;
        s.map = map;
        s.map_len = geom.total;
        s.buffers = buffers;
        s.busy = [false, false];
        s.next = 0;
        s.generation = generation;
        if s.surface.version() >= 3 {
            s.surface.set_buffer_scale(scale as i32);
        }
        self.toast_render();
    }

    fn toast_render(&mut self) {
        let tt = &mut self.toasts;
        let Some(s) = tt.surf.as_mut() else {
            return;
        };
        if s.map.is_null() {
            return;
        }
        let scale = s.scale.max(1) as usize;
        let (w, h) = (s.width as usize, s.height as usize);
        let frame_size = w * scale * h * scale * 4;
        let i = if !s.busy[s.next] {
            s.next
        } else if !s.busy[1 - s.next] {
            1 - s.next
        } else {
            return;
        };
        let Some(mut cv) = Canvas::try_new(w, h) else {
            return;
        };
        let start = tt.list.len().saturating_sub(VISIBLE);
        let shown: Vec<(usize, &Toast)> = tt.list.iter().enumerate().skip(start).collect();
        tt.hits = draw(&mut cv, (0, 0), &shown, start, tt.hover);
        let data: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(s.map.add(i * frame_size), frame_size) };
        if !cv.blit_argb_scaled(data, scale as u32) {
            return;
        }
        s.next = 1 - i;
        s.busy[i] = true;
        if let Some(buf) = s.buffers[i].as_ref() {
            s.surface.attach(Some(buf), 0, 0);
            damage(
                &s.surface,
                scale as u32,
                0,
                0,
                (w * scale) as i32,
                (h * scale) as i32,
            );
            s.surface.commit();
        }
    }

    /// Pointer entered `surface`: is it the notifications'?
    pub(super) fn toast_enter(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) -> bool {
        self.toasts.pointer_in = self
            .toasts
            .surf
            .as_ref()
            .is_some_and(|s| s.surface.id() == surface.id());
        if self.toasts.pointer_in {
            self.toast_motion(x as i32, y as i32);
        }
        self.toasts.pointer_in
    }

    pub(super) fn toast_leave(&mut self) {
        self.toasts.pointer_in = false;
        if self.toasts.hover.take().is_some() {
            self.toast_render();
        }
    }

    fn toast_hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.toasts
            .hits
            .iter()
            .find(|h| x >= h.0 && x < h.2 && y >= h.1 && y < h.3)
            .map(|h| h.4)
    }

    pub(super) fn toast_motion(&mut self, x: i32, y: i32) {
        let hit = self.toast_hit(x, y);
        self.set_cursor(if hit.is_some() {
            Shape::Pointer
        } else {
            Shape::Default
        });
        if hit != self.toasts.hover {
            self.toasts.hover = hit;
            self.toast_render();
        }
    }

    /// The event log: `lunarhunter` on its events tab when it is installed,
    /// a terminal running `hunterctl events` when it is not.
    fn toast_open_log(&self) {
        let cmd = if apps::executable_exists(hunter::APP) {
            hunter::app_events_command()
        } else {
            format!(
                "{} sh -c \"{}\"",
                self.terminal,
                hunter::log_command().replace('"', "\\\"")
            )
        };
        self.spawn(&cmd);
    }

    pub(super) fn toast_click(&mut self, qh: &QueueHandle<State>, x: i32, y: i32) {
        match self.toast_hit(x, y) {
            Some(Hit::Close(i)) => {
                self.toasts.close(i, notify::DISMISS);
            }
            Some(Hit::Action(i, j)) => {
                let Some(act) = self.toasts.list.get(i).and_then(|t| t.alert.actions.get(j)) else {
                    return;
                };
                let act = act.clone();
                let Some(t) = self.toasts.close(i, &act.id) else {
                    return;
                };
                if let Some(run) = &act.run {
                    self.spawn(run);
                }
                if let Source::Kernel { key } = t.source {
                    match act.id.as_str() {
                        notify::VIEW => self.toast_open_log(),
                        notify::IGNORE if self.toasts.muted.len() < MAX_MUTED => {
                            self.toasts.muted.insert(key);
                        }
                        _ => {}
                    }
                }
            }
            Some(Hit::More) => {
                // Everything the kernel said is in the log about to open; the
                // spool's cards stay, since their producers wait for answers.
                let before = self.toasts.list.len();
                self.toasts
                    .list
                    .retain(|t| matches!(t.source, Source::Spool { .. }));
                self.toasts.dirty |= self.toasts.list.len() != before;
                self.toast_open_log();
            }
            None => return,
        }
        self.toasts.hover = None;
        self.toasts.dirty = false;
        self.toast_sync(qh);
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ToastId> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &ToastId,
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if !matches!(state.toasts.surf.as_ref(), Some(s) if s.layer.id() == layer.id()) {
                    return;
                }
                state.toast_configure(qh, width, height);
            }
            // The output went away. The cards stay in the list and the next
            // change maps them again, on whichever bar is left.
            zwlr_layer_surface_v1::Event::Closed => {
                state.toasts.surf = None;
                state.toasts.pointer_in = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (ToastId, usize, u64)> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        (_, i, generation): &(ToastId, usize, u64),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(s) = state.toasts.surf.as_mut() {
                if s.generation == *generation {
                    s.busy[*i] = false;
                }
            }
        }
    }
}

/// The stack `LUNARBAR_DUMP_TOAST` previews in the bottom-right corner of a
/// `w` x `h` canvas: one of each kind, and some waiting.
pub(super) fn draw_preview(cv: &mut Canvas, w: i32, h: i32) {
    let lang = i18n::Lang::current();
    let ev = |critical, text: &str| hunter::Event {
        seq: 0,
        critical,
        text: text.into(),
    };
    let toast = |alert, n| Toast {
        alert,
        source: Source::Kernel { key: String::new() },
        count: n,
        deadline: None,
    };
    let list = [
        toast(
            notify::from_event(
                &ev(false, "EXEC WARNING: exec policy violation (not on trusted-program allowlist): /opt/tool/bin/run"),
                false,
                lang,
            ),
            1,
        ),
        toast(
            notify::from_event(&ev(false, "ANOMALY WARNING: fork flood from pid 412"), false, lang),
            3,
        ),
        toast(
            notify::from_event(
                &ev(true, "AV BLOCKED: malware Eicar-Test-Signature in /root/Descargas/eicar.com"),
                false,
                lang,
            ),
            1,
        ),
    ];
    let shown: Vec<(usize, &Toast)> = list.iter().enumerate().collect();
    let cards: Vec<&Toast> = list.iter().collect();
    let (sw, sh) = size(&cards, 2);
    draw(
        cv,
        (w - sw - 4, h - sh - 4),
        &shown,
        2,
        Some(Hit::Action(2, 0)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(sev: Severity, body: &str, actions: usize) -> Alert {
        Alert {
            severity: sev,
            title: "t".into(),
            body: body.into(),
            actions: (0..actions)
                .map(|i| notify::Action {
                    id: format!("a{i}"),
                    label: "cuarentena".into(),
                    run: None,
                })
                .collect(),
            ..Alert::default()
        }
    }

    fn kernel(text: &str) -> Toast {
        Toast {
            alert: alert(Severity::Warn, text, 2),
            source: Source::Kernel { key: text.into() },
            count: 1,
            deadline: None,
        }
    }

    fn spool(id: &str, a: Alert) -> Toast {
        Toast {
            alert: a,
            source: Source::Spool { id: id.into() },
            count: 1,
            deadline: None,
        }
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_cuts_long_words() {
        let w = 10 * GLYPH_W;
        assert_eq!(wrap("malware X in /tmp", w, 3), ["malware X", "in /tmp"]);
        assert_eq!(
            wrap("/a/very/long/path/name", w, 3),
            ["/a/very/lo", "ng/path/na", "me"]
        );
        let cut = wrap("one two three four five six seven", w, 2);
        assert_eq!(cut.len(), 2);
        assert!(cut[1].ends_with('.'));
        assert!(cut.iter().all(|l| l.chars().count() <= 10));
    }

    #[test]
    fn every_control_lands_inside_its_card_and_the_surface() {
        let list = [
            spool("1", alert(Severity::Crit, &"long body ".repeat(40), 3)),
            spool("2", alert(Severity::Info, "", 0)),
            kernel("EXEC x"),
        ];
        let cards: Vec<&Toast> = list.iter().collect();
        let (w, h) = size(&cards, 4);
        let mut cv = Canvas::new(w as usize, h as usize);
        let shown: Vec<(usize, &Toast)> = list.iter().enumerate().collect();
        let hits = draw(&mut cv, (0, 0), &shown, 4, None);
        assert!(hits.iter().any(|h| h.4 == Hit::More));
        assert_eq!(
            hits.iter().filter(|h| matches!(h.4, Hit::Close(_))).count(),
            3
        );
        for (x0, y0, x1, y1, hit) in &hits {
            assert!(
                *x0 >= M && *y0 >= M && *x1 <= w - M && *y1 <= h - M,
                "{hit:?}"
            );
        }
        // No two controls overlap.
        for (a, ha) in hits.iter().enumerate() {
            for hb in &hits[a + 1..] {
                let apart = ha.2 <= hb.0 || hb.2 <= ha.0 || ha.3 <= hb.1 || hb.3 <= ha.1;
                assert!(apart, "{:?} {:?}", ha.4, hb.4);
            }
        }
    }

    #[test]
    fn three_buttons_fit_beside_each_other_in_either_language() {
        for l in [i18n::Lang::Es, i18n::Lang::En] {
            let t = notify::text(l);
            // A card has quarantine or allow, never both, then view and ignore.
            let first = Canvas::text_width(t.quarantine).max(Canvas::text_width(t.allow));
            let w: i32 = [
                first,
                Canvas::text_width(t.view),
                Canvas::text_width(t.ignore),
            ]
            .iter()
            .map(|w| w + 20 + 6)
            .sum();
            assert!(w <= W - 2 * PAD, "{l:?} {w}");
        }
    }

    #[test]
    fn the_same_event_again_is_counted_not_stacked() {
        let mut t = Toasts::default();
        t.push(kernel("ANOMALY fork flood"));
        t.push(kernel("WX x"));
        t.push(kernel("ANOMALY fork flood"));
        assert_eq!(t.list.len(), 2);
        // Moved to the newest place, with its count.
        assert_eq!(t.list[1].count, 2);
    }

    #[test]
    fn only_events_after_the_first_read_pop_up() {
        let ev = |seq, text: &str| hunter::Event {
            seq,
            critical: true,
            text: text.into(),
        };
        let st = |recent: Vec<hunter::Event>| hunter::Status {
            recent,
            ..hunter::Status::default()
        };
        let mut t = Toasts::default();
        t.ingest(&st(vec![ev(5, "AV old")]));
        assert!(t.list.is_empty(), "the backlog is not news");
        t.ingest(&st(vec![ev(7, "WX b"), ev(6, "EXEC a"), ev(5, "AV old")]));
        let keys: Vec<_> = t.list.iter().map(|x| x.source.clone()).collect();
        assert_eq!(
            keys,
            [
                Source::Kernel {
                    key: "EXEC a".into()
                },
                Source::Kernel { key: "WX b".into() }
            ]
        );
        // Read again: nothing new.
        t.ingest(&st(vec![ev(7, "WX b"), ev(6, "EXEC a")]));
        assert_eq!(t.list.len(), 2);
        // A muted event stays down.
        t.muted.insert("WX b".into());
        t.ingest(&st(vec![ev(8, "WX b")]));
        assert_eq!(t.list.len(), 2);
        assert_eq!(t.last_seq, Some(8));
    }

    #[test]
    fn an_empty_ring_at_startup_still_shows_its_first_event() {
        let mut t = Toasts::default();
        t.ingest(&hunter::Status::default());
        t.ingest(&hunter::Status {
            recent: vec![hunter::Event {
                seq: 0,
                critical: false,
                text: "EXEC x".into(),
            }],
            ..hunter::Status::default()
        });
        assert_eq!(t.list.len(), 1);
    }

    #[test]
    fn a_spool_card_goes_when_its_file_does() {
        let mut t = Toasts::default();
        let a = alert(Severity::Info, "b", 1);
        t.ingest_spool(vec![("1".into(), a.clone()), ("2".into(), a.clone())]);
        assert_eq!(t.list.len(), 2);
        t.ingest_spool(vec![("2".into(), a.clone())]);
        assert_eq!(t.list.len(), 1);
        assert_eq!(t.list[0].source, Source::Spool { id: "2".into() });
    }

    #[test]
    fn cards_time_out_only_once_shown_and_not_under_the_pointer() {
        let now = Instant::now();
        let mut t = Toasts::default();
        for i in 0..5 {
            t.list.push(kernel(&format!("EXEC {i}")));
        }
        t.list.push(spool("crit", alert(Severity::Crit, "", 1)));
        t.expire(now);
        // The three shown have clocks (the critical has none); the two
        // waiting do not.
        assert!(t.list[..3].iter().all(|x| x.deadline.is_none()));
        assert!(t.list[3].deadline.is_some() && t.list[5].deadline.is_none());
        let later = now + Duration::from_secs(60);
        t.pointer_in = true;
        t.expire(later);
        assert_eq!(t.list.len(), 6);
        t.pointer_in = false;
        t.expire(later);
        // The two shown warnings went; the critical stays, and the ones
        // behind it come into view with fresh clocks.
        assert_eq!(t.list.len(), 4);
        assert!(matches!(t.list[3].source, Source::Spool { .. }));
        assert!(t.list[1].deadline.unwrap() > later);
    }
}
