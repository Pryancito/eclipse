//! lunardesk — the desktops at a glance (Super+Tab).
//!
//! One plate per labwc desktop, side by side, each listing the windows on
//! it with their icons. The current desktop wears the gold brackets. A
//! click on a window raises it (labwc goes to its desktop), a click
//! elsewhere on a plate goes to that desktop; `1`-`9` and ←/→ pick a plate,
//! Enter goes there, Esc or Super+Tab again closes.
//!
//! Neither protocol labwc speaks says which window is on which desktop
//! (wlr-foreign-toplevel lists every window, ext-workspace lists the
//! desktops), so the bar, which watches both all session long, writes that
//! down in `$XDG_RUNTIME_DIR/lunar/desktops` (see `workspaces.rs`). A window
//! the file does not know is shown on the current desktop.
//!
//! `--dump PATH:WxH` renders a made-up session to a raw ARGB8888 file.

use std::os::fd::{AsFd, AsRawFd};

use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::i18n::{Key, Lang};
use lunarbar::icons::IconCache;
use lunarbar::keys::{KEY_ENTER_WL, KEY_ESC_WL, KEY_KPENTER_WL, KEY_TAB_WL};
use lunarbar::proc::{self, map_shm_pool};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

// ── look ─────────────────────────────────────────────────────────────────────

const GROUND: Rgb = (0x04, 0x07, 0x0d);
const SURFACE: Rgb = (0x0a, 0x14, 0x23);
const HAIRLINE: Rgb = (0x1d, 0x35, 0x50);
const TEXT: Rgb = (0xe6, 0xf1, 0xff);
const MUTED: Rgb = (0x8a, 0xa4, 0xc2);
const CYAN: Rgb = (0x38, 0xe1, 0xff);
const GOLD: Rgb = (0xf2, 0xc1, 0x4e);
const SEL: Rgb = (0x0c, 0x2a, 0x3a);

const MARGIN: i32 = 48;
const PLATE_GAP: i32 = 24;
const PLATE_MAX_W: i32 = 360;
const PLATE_MIN_W: i32 = 160;
const HEAD_H: i32 = 48;
const ROW_H: i32 = 28;
const PAD: i32 = 12;
const ICON: i32 = 16;
/// Rows a plate lists before it says "+N".
const MAX_ROWS: usize = 12;

// ── what is shown ────────────────────────────────────────────────────────────

/// One window, as the overview lists it.
#[derive(Clone, Debug, PartialEq)]
struct Win {
    app_id: String,
    title: String,
    activated: bool,
    minimized: bool,
}

/// `(desktop, app_id, title)` rows of the bar's file.
fn parse_desktops(text: &str) -> Vec<(usize, String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let d = f.next()?.parse().ok()?;
            Some((d, f.next()?.to_string(), f.next()?.to_string()))
        })
        .collect()
}

/// The desktop of each window: the first unused row of the file with the
/// same application and title (two identical terminals take one row each),
/// or `current` for a window the bar has not written down.
fn assign(wins: &[Win], rows: &[(usize, String, String)], current: usize, n: usize) -> Vec<usize> {
    let mut used = vec![false; rows.len()];
    wins.iter()
        .map(|w| {
            let hit = rows
                .iter()
                .enumerate()
                .find(|(i, (_, a, t))| !used[*i] && *a == w.app_id && *t == w.title);
            match hit {
                Some((i, (d, _, _))) if *d < n => {
                    used[i] = true;
                    *d
                }
                _ => current,
            }
        })
        .collect()
}

/// The plates' rectangles for `n` desktops on a `w` x `h` screen: as wide as
/// fits up to [`PLATE_MAX_W`], centred, all as tall as the tallest needs.
fn plates(w: i32, h: i32, n: usize, rows: usize) -> Vec<(i32, i32, i32, i32)> {
    let n = n.max(1) as i32;
    let pw = ((w - 2 * MARGIN - (n - 1) * PLATE_GAP) / n).clamp(PLATE_MIN_W, PLATE_MAX_W);
    let total = n * pw + (n - 1) * PLATE_GAP;
    let x0 = (w - total) / 2;
    let ph = (HEAD_H + rows.clamp(2, MAX_ROWS + 1) as i32 * ROW_H + PAD).min(h - 2 * MARGIN - 64);
    let y = ((h - ph) / 2).max(MARGIN + 48);
    (0..n).map(|i| (x0 + i * (pw + PLATE_GAP), y, pw, ph)).collect()
}

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Desk(usize),
    /// Desktop, and the window's index in the session's list.
    Win(usize, usize),
}

/// A string cut to `cols` characters, with an ellipsis when it was longer.
fn fit(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        return s.to_string();
    }
    let mut t: String = s.chars().take(cols.saturating_sub(1)).collect();
    t.push('…');
    t
}

struct Scene<'a> {
    wins: &'a [Win],
    desk_of: &'a [usize],
    n: usize,
    current: usize,
    sel: usize,
    hover: Option<Hit>,
    lang: Lang,
}

/// Each plate's window rows, in the order they are drawn: `(window index, y)`.
fn layout_rows(s: &Scene, plates: &[(i32, i32, i32, i32)]) -> Vec<Vec<(usize, i32)>> {
    plates
        .iter()
        .enumerate()
        .map(|(d, &(_, y, _, _))| {
            s.desk_of
                .iter()
                .enumerate()
                .filter(|(_, &dd)| dd == d)
                .take(MAX_ROWS)
                .enumerate()
                .map(|(r, (i, _))| (i, y + HEAD_H + r as i32 * ROW_H))
                .collect()
        })
        .collect()
}

fn max_rows(s: &Scene) -> usize {
    (0..s.n).map(|d| s.desk_of.iter().filter(|&&x| x == d).count()).max().unwrap_or(0)
}

fn hit_test(s: &Scene, w: i32, h: i32, x: i32, y: i32) -> Option<Hit> {
    let ps = plates(w, h, s.n, max_rows(s));
    let rows = layout_rows(s, &ps);
    for (d, &(px, py, pw, ph)) in ps.iter().enumerate() {
        if x >= px && x < px + pw && y >= py && y < py + ph {
            for &(i, ry) in &rows[d] {
                if y >= ry && y < ry + ROW_H {
                    return Some(Hit::Win(d, i));
                }
            }
            return Some(Hit::Desk(d));
        }
    }
    None
}

fn draw(cv: &mut Canvas, w: i32, h: i32, s: &Scene, icons: &mut IconCache) {
    // Translucent: the desktop shows through, dimmed, behind the plates.
    cv.fill_rect_a(0, 0, w, h, GROUND, 0.9);
    let (title, keys, empty, more) = (
        s.lang.s(Key::desk_desktops),
        s.lang.s(Key::desk_keys),
        s.lang.s(Key::desk_empty),
        s.lang.s(Key::file_more),
    );
    let adv = cv.text_bold(title, MARGIN, MARGIN, CYAN);
    cv.text(keys, MARGIN + adv + 16, MARGIN, MUTED);
    cv.hline(MARGIN, MARGIN + GLYPH_H + 12, w - 2 * MARGIN, HAIRLINE, 1.0);

    let ps = plates(w, h, s.n, max_rows(s));
    let rows = layout_rows(s, &ps);
    let cols = |pw: i32| ((pw - 2 * PAD - ICON - 8) / GLYPH_W).max(4) as usize;
    for (d, &(px, py, pw, ph)) in ps.iter().enumerate() {
        cv.fill_rect_a(px, py, pw, ph, SURFACE, 1.0);
        let edge = if d == s.sel || s.hover.is_some_and(|hv| matches!(hv, Hit::Desk(x) | Hit::Win(x, _) if x == d)) {
            CYAN
        } else {
            HAIRLINE
        };
        for (x, y, rw, rh) in [(px, py, pw, 1), (px, py + ph - 1, pw, 1), (px, py, 1, ph), (px + pw - 1, py, 1, ph)] {
            cv.fill_rect_a(x, y, rw, rh, edge, 1.0);
        }
        if d == s.current {
            cv.brackets(px, py, pw, ph, 16, 2, GOLD);
        }
        // Head: the number, how many windows, and "actual".
        let num = format!("{:02}", d + 1);
        cv.text_big(&num, px + PAD, py + 12, if d == s.current { TEXT } else { MUTED });
        let count = s.desk_of.iter().filter(|&&x| x == d).count();
        let tag = match (d == s.current, s.lang) {
            (true, Lang::Es) => format!("actual · {count}"),
            (true, Lang::En) => format!("current · {count}"),
            (false, _) => format!("{count}"),
        };
        cv.text(&tag, px + pw - PAD - Canvas::text_width(&tag), py + 16, if d == s.current { GOLD } else { MUTED });
        cv.hline(px + PAD, py + HEAD_H - 8, pw - 2 * PAD, HAIRLINE, 1.0);

        if rows[d].is_empty() {
            cv.text(empty, px + PAD, py + HEAD_H + (ROW_H - GLYPH_H) / 2, MUTED);
        }
        for &(i, ry) in &rows[d] {
            let win = &s.wins[i];
            if s.hover == Some(Hit::Win(d, i)) {
                cv.fill_rect_a(px + 4, ry, pw - 8, ROW_H, SEL, 1.0);
            }
            let iy = ry + (ROW_H - ICON) / 2;
            match icons.get(&win.app_id, ICON as u32) {
                Some(pm) => cv.pixmap(px + PAD, iy, &pm),
                None => cv.fill_rect_a(px + PAD + 4, iy + 4, ICON - 8, ICON - 8, MUTED, 1.0),
            }
            let label = if win.title.trim().is_empty() { &win.app_id } else { &win.title };
            let c = if win.minimized { MUTED } else { TEXT };
            cv.text(&fit(label, cols(pw)), px + PAD + ICON + 8, ry + (ROW_H - GLYPH_H) / 2, c);
            if win.activated {
                cv.fill_rect_a(px + 1, ry + 6, 2, ROW_H - 12, CYAN, 1.0);
            }
        }
        if count > MAX_ROWS {
            let t = format!("+{} {more}", count - MAX_ROWS);
            cv.text(&t, px + PAD, py + HEAD_H + MAX_ROWS as i32 * ROW_H + 4, MUTED);
        }
    }
}

// ── wayland ──────────────────────────────────────────────────────────────────

struct Top {
    handle: ZwlrForeignToplevelHandleV1,
    win: Win,
}

struct Desk {
    handle: ExtWorkspaceHandleV1,
    coords: Vec<u32>,
    active: bool,
    hidden: bool,
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    ws_mgr: Option<ExtWorkspaceManagerV1>,
    tops: Vec<Top>,
    desks: Vec<Desk>,

    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    w: u32,
    h: u32,
    map: *mut u8,
    len: usize,
    buffer: Option<wl_buffer::WlBuffer>,

    rows: Vec<(usize, String, String)>,
    sel: usize,
    hover: Option<Hit>,
    icons: IconCache,
    lang: Option<Lang>,
    done: bool,
}

impl State {
    /// Visible desktops in their order (by coordinates, then announcement).
    fn order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.desks.len()).filter(|&i| !self.desks[i].hidden).collect();
        idx.sort_by(|&a, &b| (&self.desks[a].coords, a).cmp(&(&self.desks[b].coords, b)));
        idx
    }

    fn current(&self) -> usize {
        self.order().iter().position(|&i| self.desks[i].active).unwrap_or(0)
    }

    fn wins(&self) -> Vec<Win> {
        self.tops.iter().map(|t| t.win.clone()).collect()
    }

    fn render(&mut self) {
        let (Some(surface), Some(buf)) = (self.surface.clone(), self.buffer.clone()) else {
            return;
        };
        if self.map.is_null() {
            return;
        }
        let wins = self.wins();
        let n = self.order().len().max(1);
        let current = self.current();
        let desk_of = assign(&wins, &self.rows, current, n);
        let scene = Scene { wins: &wins, desk_of: &desk_of, n, current, sel: self.sel, hover: self.hover, lang: self.lang.unwrap_or(Lang::Es) };
        let Some(mut cv) = Canvas::try_new(self.w as usize, self.h as usize) else {
            return;
        };
        let mut icons = std::mem::take(&mut self.icons);
        draw(&mut cv, self.w as i32, self.h as i32, &scene, &mut icons);
        self.icons = icons;
        // SAFETY: `map` is the `len` bytes mapped for this surface's buffer.
        let data = unsafe { std::slice::from_raw_parts_mut(self.map, self.len) };
        if cv.blit_argb(data) {
            surface.attach(Some(&buf), 0, 0);
            surface.damage_buffer(0, 0, self.w as i32, self.h as i32);
            surface.commit();
        }
    }

    fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        let wins = self.wins();
        let n = self.order().len().max(1);
        let current = self.current();
        let desk_of = assign(&wins, &self.rows, current, n);
        let scene = Scene { wins: &wins, desk_of: &desk_of, n, current, sel: self.sel, hover: None, lang: Lang::Es };
        hit_test(&scene, self.w as i32, self.h as i32, x as i32, y as i32)
    }

    fn go_desk(&mut self, d: usize) {
        if let (Some(mgr), Some(&i)) = (&self.ws_mgr, self.order().get(d)) {
            self.desks[i].handle.activate();
            mgr.commit();
        }
        self.done = true;
    }

    fn go_win(&mut self, i: usize) {
        if let (Some(t), Some(seat)) = (self.tops.get(i), &self.seat) {
            if t.win.minimized {
                t.handle.unset_minimized();
            }
            t.handle.activate(seat);
        }
        self.done = true;
    }

    fn configure(&mut self, qh: &QueueHandle<State>, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let (w, h) = (if w == 0 { 1280 } else { w }, if h == 0 { 720 } else { h });
        if fill_guard::check_buffer(w as usize, h as usize).is_err() {
            return;
        }
        if self.w != w || self.h != h {
            let Some(geom) = proc::pool_geometry(w, h) else {
                return;
            };
            let Some((map, fd)) = map_shm_pool(geom.frame_size, "lunardesk") else {
                return;
            };
            if let Some(b) = self.buffer.take() {
                b.destroy();
            }
            if !self.map.is_null() {
                unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
            }
            let pool = shm.create_pool(fd.as_fd(), geom.frame_size as i32, qh, ());
            self.buffer = Some(pool.create_buffer(0, w as i32, h as i32, geom.stride as i32, wl_shm::Format::Argb8888, qh, ()));
            pool.destroy();
            self.map = map;
            self.len = geom.frame_size;
            self.w = w;
            self.h = h;
        }
        self.render();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer_shell = Some(reg.bind(name, version.min(4), qh, ())),
                "zwlr_foreign_toplevel_manager_v1" => {
                    let _: ZwlrForeignToplevelManagerV1 = reg.bind(name, version.min(3), qh, ());
                }
                "ext_workspace_manager_v1" => st.ws_mgr = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(5), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(st: &mut Self, _: &ZwlrForeignToplevelManagerV1, ev: zwlr_foreign_toplevel_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = ev {
            if st.tops.len() < 256 {
                st.tops.push(Top { handle: toplevel, win: Win { app_id: String::new(), title: String::new(), activated: false, minimized: false } });
            }
        }
    }

    wayland_client::event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(st: &mut Self, h: &ZwlrForeignToplevelHandleV1, ev: zwlr_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        use zwlr_foreign_toplevel_handle_v1::Event as E;
        let Some(pos) = st.tops.iter().position(|t| &t.handle == h) else {
            return;
        };
        match ev {
            E::Title { title } => st.tops[pos].win.title = fill_guard::truncate_chars(&title, fill_guard::MAX_TITLE_CHARS),
            E::AppId { app_id } => st.tops[pos].win.app_id = fill_guard::truncate_chars(&app_id, fill_guard::MAX_APP_ID_CHARS),
            E::State { state } => {
                let vals: Vec<u32> = state.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect();
                st.tops[pos].win.activated = vals.contains(&2);
                st.tops[pos].win.minimized = vals.contains(&1);
            }
            E::Done => st.render(),
            E::Closed => {
                st.tops.remove(pos).handle.destroy();
                st.render();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event(st: &mut Self, _: &ExtWorkspaceManagerV1, ev: ext_workspace_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match ev {
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                if st.desks.len() < 16 {
                    st.desks.push(Desk { handle: workspace, coords: Vec::new(), active: false, hidden: false });
                }
            }
            ext_workspace_manager_v1::Event::Done => st.render(),
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(_: &mut Self, _: &ExtWorkspaceGroupHandleV1, _: ext_workspace_group_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn event(st: &mut Self, h: &ExtWorkspaceHandleV1, ev: ext_workspace_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        let Some(d) = st.desks.iter_mut().find(|d| &d.handle == h) else {
            return;
        };
        match ev {
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                d.coords = coordinates.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect();
            }
            ext_workspace_handle_v1::Event::State { state: WEnum::Value(s) } => {
                d.active = s.contains(ext_workspace_handle_v1::State::Active);
                d.hidden = s.contains(ext_workspace_handle_v1::State::Hidden);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(st: &mut Self, l: &ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                l.ack_configure(serial);
                st.configure(qh, width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(st: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev {
            if caps.contains(wl_seat::Capability::Keyboard) && st.keyboard.is_none() {
                st.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && st.pointer.is_none() {
                st.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

/// Evdev codes of the digit row `1`..`9` and the arrows.
const KEY_1: u32 = 2;
const KEY_9: u32 = 10;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(st: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        let wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } = ev else {
            return;
        };
        let n = st.order().len().max(1);
        match key {
            KEY_ESC_WL | KEY_TAB_WL => st.done = true,
            KEY_ENTER_WL | KEY_KPENTER_WL => {
                let d = st.sel;
                st.go_desk(d);
            }
            KEY_LEFT => {
                st.sel = (st.sel + n - 1) % n;
                st.render();
            }
            KEY_RIGHT => {
                st.sel = (st.sel + 1) % n;
                st.render();
            }
            k if (KEY_1..=KEY_9).contains(&k) && ((k - KEY_1) as usize) < n => st.go_desk((k - KEY_1) as usize),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(st: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match ev {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } | wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                let h = st.hit(surface_x, surface_y);
                if h != st.hover {
                    st.hover = h;
                    if let Some(Hit::Desk(d) | Hit::Win(d, _)) = h {
                        st.sel = d;
                    }
                    st.render();
                }
            }
            wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => match st.hover {
                Some(Hit::Win(_, i)) => st.go_win(i),
                Some(Hit::Desk(d)) => st.go_desk(d),
                None => st.done = true,
            },
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

// ── main ─────────────────────────────────────────────────────────────────────

fn sample() -> (Vec<Win>, Vec<usize>) {
    let w = |a: &str, t: &str, act: bool, min: bool| Win { app_id: a.into(), title: t.into(), activated: act, minimized: min };
    (
        vec![
            w("foot", "root@eclipse: ~/eclipse/tools/lunarbar", false, false),
            w("firefox", "labwc - window switcher - Mozilla Firefox", true, false),
            w("org.gnome.TextEditor", "notas.txt - ~/Documentos", false, false),
            w("foot", "htop", false, true),
            w("silversurfer", "Eclipse OS - inicio", false, false),
            w("gimp", "captura-2026-11-05-053112.png - GIMP", false, false),
            w("lunarhunter", "hunter - Resumen", false, false),
        ],
        vec![0, 0, 0, 0, 1, 2, 2],
    )
}

fn dump(spec: &str) -> ! {
    let (path, dims) = spec.rsplit_once(':').unwrap_or((spec, "1600x900"));
    let (w, h) = dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .unwrap_or((1600, 900));
    if fill_guard::check_buffer(w, h).is_err() {
        std::process::exit(1);
    }
    let (wins, desk_of) = sample();
    let hover = std::env::var("LUNARDESK_HOVER").ok().and_then(|v| v.parse().ok()).map(|i: usize| Hit::Win(desk_of[i], i));
    let scene = Scene { wins: &wins, desk_of: &desk_of, n: 4, current: 0, sel: 0, hover, lang: Lang::current() };
    let mut cv = Canvas::try_new(w, h).unwrap_or_else(|| std::process::exit(1));
    cv.set_square(true);
    let mut icons = IconCache::default();
    draw(&mut cv, w as i32, h as i32, &scene, &mut icons);
    let mut buf = vec![0u8; w * h * 4];
    if !cv.blit_argb(&mut buf) || std::fs::write(path, &buf).is_err() {
        eprintln!("lunardesk: could not write {path}");
        std::process::exit(1);
    }
    println!("lunardesk: wrote {path} ({w}x{h} ARGB8888)");
    std::process::exit(0);
}

/// Super+Tab twice closes it: a second start signals the first and leaves.
fn toggle_or_lock() -> Option<std::fs::File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&dir).join("lunardesk.lock");
    let f = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path).ok()?;
    // SAFETY: flock on a descriptor this process owns.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if let Some(pid) = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
            if pid > 1 {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        std::process::exit(0);
    }
    let _ = std::fs::write(&path, std::process::id().to_string());
    Some(f)
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => {}
        Some("--dump") => dump(&args.next().unwrap_or_else(|| "lunardesk.raw".into())),
        Some(_) => {
            eprintln!("usage: lunardesk [--dump PATH:WxH]");
            std::process::exit(2);
        }
    }
    let _lock = toggle_or_lock();
    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunardesk: no Wayland compositor found");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = State { lang: Some(Lang::current()), ..State::default() };
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/0".into());
    st.rows = std::fs::read_to_string(std::path::Path::new(&dir).join("lunar/desktops"))
        .map(|t| parse_desktops(&t))
        .unwrap_or_default();
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunardesk: wayland roundtrip failed");
            std::process::exit(1);
        }
    }
    let (Some(comp), Some(ls)) = (st.compositor.clone(), st.layer_shell.clone()) else {
        eprintln!("lunardesk: compositor lacks wlr-layer-shell");
        std::process::exit(1);
    };
    st.sel = st.current();
    st.icons.prewarm();
    let surface = comp.create_surface(&qh, ());
    let layer = ls.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Overlay, "desktops".into(), &qh, ());
    layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer.set_size(0, 0);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    surface.commit();
    st.surface = Some(surface);
    st.layer = Some(layer);
    while !st.done {
        if queue.blocking_dispatch(&mut st).is_err() {
            break;
        }
    }
    if let Some(l) = st.layer.take() {
        l.destroy();
    }
    if let Some(s) = st.surface.take() {
        s.destroy();
    }
    let _ = conn.flush();
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn win(a: &str, t: &str) -> Win {
        Win { app_id: a.into(), title: t.into(), activated: false, minimized: false }
    }

    #[test]
    fn the_bars_file_reads_back_and_skips_what_is_not_a_row() {
        let rows = parse_desktops("0\tfoot\tbash\n2\tfirefox\ta\tb\nx\ty\tz\n1\tgimp\n");
        assert_eq!(rows, [(0, "foot".into(), "bash".into()), (2, "firefox".into(), "a\tb".into())]);
    }

    #[test]
    fn identical_windows_take_one_row_each_and_unknown_ones_are_here() {
        let rows = parse_desktops("1\tfoot\tbash\n3\tfoot\tbash\n9\tgimp\tx\n");
        let wins = [win("foot", "bash"), win("foot", "bash"), win("foot", "bash"), win("gimp", "x"), win("new", "n")];
        assert_eq!(assign(&wins, &rows, 0, 4), [1, 3, 0, 0, 0], "a third twin, a desktop out of range and a new window are on the current one");
    }

    #[test]
    fn the_plates_fit_side_by_side_inside_the_margins() {
        for (w, h, n) in [(1920, 1080, 4), (1280, 720, 4), (1600, 900, 2), (1280, 720, 9)] {
            let ps = plates(w, h, n, 5);
            assert_eq!(ps.len(), n);
            assert!(ps.windows(2).all(|p| p[0].0 + p[0].2 + PLATE_GAP == p[1].0), "evenly spaced");
            let (x0, x1) = (ps[0].0, ps[n - 1].0 + ps[n - 1].2);
            assert!((x0 - (w - x1)).abs() <= 1, "centred: {x0} {x1}");
            if n <= 4 {
                assert!(x0 >= MARGIN && x1 <= w - MARGIN, "{w}x{h}: {ps:?}");
            }
        }
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
    }

    #[test]
    fn a_click_finds_the_window_row_or_the_plate() {
        let (wins, desk_of) = sample();
        let s = Scene { wins: &wins, desk_of: &desk_of, n: 4, current: 0, sel: 0, hover: None, lang: Lang::Es };
        let ps = plates(1600, 900, 4, max_rows(&s));
        let (px, py, _, ph) = ps[0];
        assert_eq!(hit_test(&s, 1600, 900, px + 20, py + HEAD_H + 2), Some(Hit::Win(0, 0)));
        assert_eq!(hit_test(&s, 1600, 900, px + 20, py + HEAD_H + ROW_H + 2), Some(Hit::Win(0, 1)));
        assert_eq!(hit_test(&s, 1600, 900, px + 20, py + 4), Some(Hit::Desk(0)));
        assert_eq!(hit_test(&s, 1600, 900, ps[1].0 + 20, py + HEAD_H + 2), Some(Hit::Win(1, 4)));
        assert_eq!(hit_test(&s, 1600, 900, ps[3].0 + 20, py + ph - 4), Some(Hit::Desk(3)), "an empty desktop");
        assert_eq!(hit_test(&s, 1600, 900, 4, 4), None);
        let mut cv = Canvas::new(1600, 900);
        draw(&mut cv, 1600, 900, &s, &mut IconCache::default());
    }
}
