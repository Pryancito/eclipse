//! lunarrun — Eclipse OS's KRunner stand-in, and KDE's Super+D.
//!
//! Plasma's krunner cannot run here at all: it is a D-Bus service (its whole
//! API is `org.kde.krunner`), it needs Qt 6 + KF6, and Eclipse OS has no
//! session bus on purpose — `DBUS_SESSION_BUS_ADDRESS` points at a path with
//! no daemon so libdbus fails fast instead of `autolaunch:`ing a
//! dbus-launch/dbus-daemon/babysitter chain (the one that hung gzdoom). So
//! this is the same thing rebuilt on what the session does have: one static
//! musl binary over wlr-layer-shell + wl_shm, sharing lunarbar's drawing
//! stack, .desktop scanner and icon cache. No GTK, no Qt, no D-Bus.
//!
//! Modes:
//! - default: a centred search overlay. Type to filter installed
//!   applications (accent-insensitive, the same matcher the panel's menu
//!   uses), ↑/↓ or Tab to pick, Enter to launch, Esc to close; clicking
//!   outside closes it. Anything that is not an application but IS a program
//!   on `$PATH` runs as a command, so `Alt+Space top` works like KRunner's.
//! - `--toggle-desktop`: KDE's Super+D. Minimises every window through
//!   wlr-foreign-toplevel-management, or restores them all when they are
//!   already minimised. Done this way rather than with a labwc action because
//!   labwc's action list varies by release, while this protocol does not.
//! - `--dump PATH:WxH`: render the overlay to a raw ARGB8888 file and exit,
//!   for offline verification with no compositor.
//!
//! Env: `LUNARRUN_TERMINAL` (default `/usr/local/bin/eclipse-terminal`),
//! `ECLIPSE_LOOK=kde|eclipse` to override `/etc/eclipse/look`.

use std::os::fd::AsFd;

use lunarbar::apps::{norm_key, scan_apps, AppEntry};
use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::i18n::Lang;
use lunarbar::icons::IconCache;
use lunarbar::keys::{
    key_char, BTN_LEFT, KEY_BACKSPACE_WL, KEY_DOWN_WL, KEY_ENTER_WL, KEY_ESC_WL, KEY_KPENTER_WL,
    KEY_PGDN_WL, KEY_PGUP_WL, KEY_TAB_WL, KEY_UP_WL, WHEEL_NOTCH,
};
use lunarbar::look::Look;
use lunarbar::proc::{map_shm_pool, spawn_detached};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

// ── palette ──────────────────────────────────────────────────────────────────

/// The colours of one look. Two instances, picked from `/etc/eclipse/look`.
struct Palette {
    /// Full-screen dim behind the panel (KRunner dims the desktop too).
    scrim: Rgb,
    scrim_a: f32,
    panel: Rgb,
    /// Search field / row well, a shade darker than the panel.
    field: Rgb,
    text: Rgb,
    dim: Rgb,
    accent: Rgb,
    /// Selected row background.
    sel: Rgb,
    border: Rgb,
}

/// KDE Breeze Dark: window `#2a2e32`, view `#1b1e20`, text `#fcfcfc`,
/// inactive text `#7f8c8d`, selection `#3daee9`.
const PAL_KDE: Palette = Palette {
    scrim: (0x0d, 0x0f, 0x11),
    scrim_a: 0.45,
    panel: (0x2a, 0x2e, 0x32),
    field: (0x1b, 0x1e, 0x20),
    text: (0xfc, 0xfc, 0xfc),
    dim: (0x7f, 0x8c, 0x8d),
    accent: (0x3d, 0xae, 0xe9),
    sel: (0x3d, 0xae, 0xe9),
    border: (0x4d, 0x4d, 0x4d),
};

/// Eclipse's own violet, matching lunarbar's menu.
const PAL_ECLIPSE: Palette = Palette {
    scrim: (0x04, 0x07, 0x0e),
    scrim_a: 0.45,
    panel: (0x0b, 0x12, 0x20),
    field: (0x12, 0x21, 0x38),
    text: (0xff, 0xff, 0xff),
    dim: (0x6d, 0x7f, 0xa3),
    accent: (0x6e, 0xa8, 0xff),
    sel: (0x1f, 0x3a, 0x63),
    border: (0x27, 0x5a, 0x9e),
};

/// Windows 11 dark: flyout `#2b2b2b` over a `#202020` ground, accent
/// `#0078d4`, secondary text `#c5c5c5`. Drawn, not copied: no Microsoft font,
/// icon or image is involved.
const PAL_WIN11: Palette = Palette {
    scrim: (0x10, 0x10, 0x10),
    scrim_a: 0.40,
    panel: (0x2b, 0x2b, 0x2b),
    field: (0x1c, 0x1c, 0x1c),
    text: (0xff, 0xff, 0xff),
    dim: (0xc5, 0xc5, 0xc5),
    accent: (0x00, 0x78, 0xd4),
    sel: (0x00, 0x78, 0xd4),
    border: (0x3d, 0x3d, 0x3d),
};

fn palette(look: Look) -> &'static Palette {
    match look {
        Look::Win11 => &PAL_WIN11,
        Look::Kde => &PAL_KDE,
        Look::Eclipse => &PAL_ECLIPSE,
    }
}

// ── geometry ─────────────────────────────────────────────────────────────────

const PANEL_W: i32 = 720;
const FIELD_H: i32 = 52;
const ROW_H: i32 = 40;
const PAD: i32 = 10;
const FOOT_H: i32 = 24;
const ICON: i32 = 24;
/// Rows drawn at most; longer result sets scroll.
const MAX_ROWS: usize = 8;
/// Filter length cap: far past what fits, and bounds the per-key rescan.
const MAX_FILTER: usize = 64;

/// State value from wlr-foreign-toplevel-management-unstable-v1 (the `state`
/// array carries u32 enum values). Same table lunarbar reads.
const TOPLEVEL_STATE_MINIMIZED: u32 = 1;

// ── items ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A `.desktop` entry, or one of the builtin rows.
    App,
    /// "run what was typed", synthesised from the filter.
    Command,
}

struct Item {
    name: String,
    exec: String,
    icon: Option<String>,
    kind: Kind,
    /// `name` and `exec` normalised once, so filtering never re-normalises.
    key: String,
}

impl Item {
    fn new(name: String, exec: String, icon: Option<String>, kind: Kind) -> Self {
        let key = format!("{} {}", norm_key(&name), norm_key(&exec));
        Self {
            name,
            exec,
            icon,
            kind,
            key,
        }
    }
}

/// Applications, plus the handful of rows KRunner has that are not programs.
fn build_items(terminal: &str) -> Vec<Item> {
    let lang = Lang::current();
    let mut items = vec![
        Item::new(
            "Terminal".into(),
            terminal.to_string(),
            Some("utilities-terminal".into()),
            Kind::App,
        ),
        Item::new(
            lang.run_reload().into(),
            "labwc --reconfigure".into(),
            Some("view-refresh".into()),
            Kind::App,
        ),
        Item::new(
            lang.power_logout().into(),
            "labwc --exit || killall labwc || pkill labwc".into(),
            Some("system-log-out".into()),
            Kind::App,
        ),
        Item::new(
            lang.power_reboot().into(),
            "reboot -f || reboot".into(),
            Some("system-reboot".into()),
            Kind::App,
        ),
        Item::new(
            lang.power_shutdown().into(),
            "poweroff -f || poweroff".into(),
            Some("system-shutdown".into()),
            Kind::App,
        ),
    ];
    for AppEntry { name, exec, icon } in scan_apps(terminal) {
        items.push(Item::new(name, exec, icon, Kind::App));
    }
    items
}

/// Indices of the items matching `filter`, prefix matches first, then
/// substring, each group in discovery order (builtins before applications).
/// An empty filter matches everything.
fn matches(items: &[Item], filter: &str) -> Vec<usize> {
    let f = norm_key(filter);
    if f.is_empty() {
        return (0..items.len()).collect();
    }
    let mut head = Vec::new();
    let mut tail = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if it.kind == Kind::Command {
            continue;
        }
        let name = norm_key(&it.name);
        if name.starts_with(&f) {
            head.push(i);
        } else if it.key.contains(&f) {
            tail.push(i);
        }
    }
    head.extend(tail);
    head
}

/// Is `cmd`'s first word an executable on `$PATH` (or an absolute path)?
/// Gates the "run what was typed" row, so Enter on a typo cannot spawn a shell
/// that exits 127 with nothing on screen to say why.
fn runnable(cmd: &str) -> bool {
    let Some(word) = cmd.split_whitespace().next() else {
        return false;
    };
    let is_exec = |p: &std::path::Path| {
        p.is_file()
            && {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
            }
    };
    if word.contains('/') {
        return is_exec(std::path::Path::new(word));
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/bin:/usr/bin:/sbin:/usr/sbin".into());
    path.split(':')
        .filter(|d| !d.is_empty())
        .any(|d| is_exec(&std::path::Path::new(d).join(word)))
}

// ── state ────────────────────────────────────────────────────────────────────

const BUFFERS: usize = 2;

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    foreign_mgr: Option<ZwlrForeignToplevelManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: Vec<wl_output::WlOutput>,

    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    width: u32,
    height: u32,
    map: *mut u8,
    map_len: usize,
    buffers: [Option<wl_buffer::WlBuffer>; BUFFERS],
    busy: [bool; BUFFERS],
    next: usize,
    generation: u64,
    configured: bool,

    pal: &'static Palette,
    look: Look,
    lang: Lang,
    items: Vec<Item>,
    hits: Vec<usize>,
    /// Synthetic "run this" item, rebuilt whenever the filter changes.
    typed: Option<Item>,
    filter: String,
    sel: usize,
    top: usize,
    hover: Option<usize>,
    /// Last pointer position, so a click can be placed even without a fresh
    /// Motion (and a click on the search field does not read as "outside").
    ptr: (f64, f64),
    icons: IconCache,
    /// Panel rect (x, y, w, h) of the last frame, for hit testing.
    panel: (i32, i32, i32, i32),
    row0_y: i32,
    scroll_acc: f64,

    /// `--toggle-desktop`: toplevels seen, and whether each is minimised.
    toplevels: Vec<(ZwlrForeignToplevelHandleV1, bool)>,
    done: bool,
}

impl State {
    /// Rows currently drawable: the matches, plus the typed command row.
    fn rows(&self) -> usize {
        self.hits.len() + usize::from(self.typed.is_some())
    }

    fn row_item(&self, row: usize) -> Option<&Item> {
        if row < self.hits.len() {
            self.items.get(self.hits[row])
        } else if row == self.hits.len() {
            self.typed.as_ref()
        } else {
            None
        }
    }

    /// Recompute matches and the typed-command row after the filter changed.
    fn refilter(&mut self) {
        self.hits = matches(&self.items, &self.filter);
        let f = self.filter.trim();
        self.typed = if !f.is_empty() && runnable(f) {
            Some(Item::new(
                format!("{}: {f}", self.lang.run_command()),
                f.to_string(),
                Some("system-run".into()),
                Kind::Command,
            ))
        } else {
            None
        };
        self.sel = 0;
        self.top = 0;
        self.hover = None;
    }

    fn move_sel(&mut self, delta: i32) {
        let n = self.rows();
        if n == 0 {
            return;
        }
        let cur = self.sel as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        self.sel = next;
        // Keep the selection inside the visible window.
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + MAX_ROWS {
            self.top = self.sel + 1 - MAX_ROWS;
        }
    }

    fn launch(&mut self, row: usize) {
        if let Some(it) = self.row_item(row) {
            let cmd = it.exec.clone();
            spawn_detached(&cmd);
        }
        self.done = true;
    }

    /// Map the overlay: full output, Overlay layer, exclusive keyboard.
    fn map_overlay(&mut self, qh: &QueueHandle<State>) {
        let (Some(comp), Some(ls)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        if self.surface.is_some() {
            return;
        }
        let surface = comp.create_surface(qh, ());
        let layer = ls.get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Overlay,
            "launcher".into(),
            qh,
            (),
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_size(0, 0);
        // -1: cover the FULL output, panels included. With the default 0 the
        // compositor hands back the usable area (already minus the panel's
        // exclusive zone) and the scrim would stop at the panel, leaving
        // taskbar clicks live while this surface holds the keyboard.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        surface.commit();
        self.surface = Some(surface);
        self.layer = Some(layer);
    }

    /// (Re)allocate the ARGB pool after a configure, then paint.
    fn configure(&mut self, qh: &QueueHandle<State>, mut w: u32, mut h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        if w == 0 {
            w = 1280;
        }
        if h == 0 {
            h = 720;
        }
        if w > fill_guard::MAX_BUFFER_DIM || h > fill_guard::MAX_BUFFER_DIM {
            eprintln!("lunarrun: {w}x{h} past MAX_BUFFER_DIM; skipping");
            return;
        }
        if (w as usize).saturating_mul(h as usize) > fill_guard::MAX_BUFFER_PIXELS {
            eprintln!("lunarrun: {w}x{h} past MAX_BUFFER_PIXELS; skipping");
            return;
        }
        if self.configured && self.width == w && self.height == h {
            self.render();
            return;
        }
        let Some(total) = (w as usize)
            .checked_mul(4)
            .and_then(|s| s.checked_mul(h as usize))
            .and_then(|f| f.checked_mul(BUFFERS))
            .filter(|t| *t <= i32::MAX as usize)
        else {
            return;
        };
        let stride = w as usize * 4;
        let frame_size = stride * h as usize;
        let Some((map, fd)) = map_shm_pool(total, "lunarrun") else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
        let mk = |i: usize| {
            pool.create_buffer(
                (i * frame_size) as i32,
                w as i32,
                h as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (i, generation),
            )
        };
        let buffers = [Some(mk(0)), Some(mk(1))];
        pool.destroy();

        for b in self.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !self.map.is_null() && self.map_len > 0 {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        self.width = w;
        self.height = h;
        self.map = map;
        self.map_len = total;
        self.buffers = buffers;
        self.busy = [false, false];
        self.next = 0;
        self.configured = true;
        self.render();
    }

    fn render(&mut self) {
        if !self.configured || self.map.is_null() {
            return;
        }
        let (Some(surface), Some(_)) = (self.surface.clone(), self.layer.as_ref()) else {
            return;
        };
        let i = if !self.busy[self.next] {
            self.next
        } else if !self.busy[1 - self.next] {
            1 - self.next
        } else {
            return; // both held by the compositor; the next Release repaints
        };
        let (w, h) = (self.width as usize, self.height as usize);
        let frame_size = w * h * 4;
        let Some(mut cv) = Canvas::try_new(w, h) else {
            return;
        };
        // The draw borrows the icon cache mutably (it loads and memoises PNGs
        // on demand) while `self` is borrowed immutably, so move it out for the
        // duration and put it back.
        let mut icons = std::mem::take(&mut self.icons);
        let (panel, row0) = draw_overlay(&mut cv, w, h, self, &mut icons);
        self.icons = icons;
        self.panel = panel;
        self.row0_y = row0;
        let data: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(self.map.add(i * frame_size), frame_size) };
        if !cv.blit_argb(data) {
            return;
        }
        if let Some(b) = &self.buffers[i] {
            surface.attach(Some(b), 0, 0);
            surface.damage_buffer(0, 0, w as i32, h as i32);
            surface.commit();
            self.busy[i] = true;
            self.next = 1 - i;
        }
    }

    /// Which result row the pointer is over, if any.
    fn row_at(&self, x: f64, y: f64) -> Option<usize> {
        let (px, _, pw, _) = self.panel;
        let x = x as i32;
        let y = y as i32;
        if x < px || x > px + pw {
            return None;
        }
        if y < self.row0_y {
            return None;
        }
        let row = ((y - self.row0_y) / ROW_H) as usize + self.top;
        let shown = self.rows().min(self.top + MAX_ROWS);
        if row < shown {
            Some(row)
        } else {
            None
        }
    }

    fn inside_panel(&self, x: f64, y: f64) -> bool {
        let (px, py, pw, ph) = self.panel;
        let (x, y) = (x as i32, y as i32);
        x >= px && x <= px + pw && y >= py && y <= py + ph
    }

    /// `--toggle-desktop`: minimise every window, or restore them all when
    /// every one is minimised already (KDE's Super+D behaviour).
    fn apply_toggle_desktop(&mut self) {
        if self.toplevels.is_empty() {
            self.done = true;
            return;
        }
        let all_min = self.toplevels.iter().all(|(_, min)| *min);
        for (h, min) in &self.toplevels {
            if all_min {
                h.unset_minimized();
            } else if !*min {
                h.set_minimized();
            }
        }
        self.done = true;
    }
}

impl Drop for State {
    fn drop(&mut self) {
        for b in self.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !self.map.is_null() && self.map_len > 0 {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        if let Some(l) = self.layer.take() {
            l.destroy();
        }
        if let Some(s) = self.surface.take() {
            s.destroy();
        }
    }
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// Paint the scrim and the centred panel. Returns the panel rect and the y of
/// the first result row, which is what hit testing needs.
fn draw_overlay(
    cv: &mut Canvas,
    w: usize,
    h: usize,
    st: &State,
    icons: &mut IconCache,
) -> ((i32, i32, i32, i32), i32) {
    let p = st.pal;
    // The canvas starts fully transparent, so the scrim IS the background.
    cv.fill_rect_a(0, 0, w as i32, h as i32, p.scrim, p.scrim_a);

    let pw = PANEL_W.min(w as i32 - 96).max(320);
    let shown = st.rows().min(MAX_ROWS).max(1);
    let ph = PAD + FIELD_H + PAD + shown as i32 * ROW_H + FOOT_H + PAD;
    let px = (w as i32 - pw) / 2;
    // Where the panel sits is the look's, not the layout's: Windows 11 opens
    // Start just above the taskbar, KRunner sits in the upper third. Both are
    // clamped so a short screen cannot push the panel off.
    let py = match st.look {
        Look::Win11 => (h as i32 - ph - 64).max(24),
        _ => ((h as i32) / 5).min(h as i32 - ph - 24).max(24),
    };

    // 1px accent-tinted border under the panel, drawn as a slightly larger
    // rounded rect: with no compositor shadows a borderless panel floats
    // shapelessly over the scrim.
    cv.round_rect_a(px - 1, py - 1, pw + 2, ph + 2, 9, p.border, 0.9);
    // Windows 11's flyouts are acrylic; with no blur available in labwc, flat
    // translucency is the honest approximation (see lunarbar's bar_alpha).
    let panel_a = if st.look == Look::Win11 { 0.92 } else { 0.97 };
    cv.round_rect_a(px, py, pw, ph, 8, p.panel, panel_a);

    // ── search field ──
    let fx = px + PAD;
    let fw = pw - 2 * PAD;
    let fy = py + PAD;
    cv.round_rect(fx, fy, fw, FIELD_H, 6, p.field);
    let ty = fy + (FIELD_H - GLYPH_H) / 2;
    // A ">" prompt instead of a magnifier: the font is a bitmap ISO-8859-1
    // face with no icon glyphs, and a hand-drawn lens at 15px reads as dirt.
    cv.text_bold(">", fx + 10, ty, p.accent);
    let tx = fx + 10 + 2 * GLYPH_W;
    if st.filter.is_empty() {
        cv.text(st.lang.run_hint(), tx, ty, p.dim);
    } else {
        let max_chars = ((fw - 20 - 2 * GLYPH_W) / GLYPH_W).max(1) as usize;
        let shown_text: String = if st.filter.chars().count() > max_chars {
            st.filter.chars().skip(st.filter.chars().count() - max_chars).collect()
        } else {
            st.filter.clone()
        };
        let end = cv.text_bold(&shown_text, tx, ty, p.text);
        // Caret: a 2px bar after the text, so an empty-looking field after a
        // backspace still shows where typing lands.
        cv.round_rect(tx + end + 2, fy + 12, 2, FIELD_H - 24, 1, p.accent);
    }

    // ── result rows ──
    let row0 = fy + FIELD_H + PAD;
    let n = st.rows();
    if n == 0 {
        cv.text(st.lang.run_empty(), fx + 8, row0 + (ROW_H - GLYPH_H) / 2, p.dim);
    }
    let last = (st.top + MAX_ROWS).min(n);
    // A themed PNG at ICON px, or the entry's initial as a badge.
    for (k, row) in (st.top..last).enumerate() {
        let Some(it) = st.row_item(row) else { continue };
        let y = row0 + k as i32 * ROW_H;
        let selected = row == st.sel;
        let hovered = st.hover == Some(row);
        if selected {
            cv.round_rect(fx, y, fw, ROW_H - 2, 6, p.sel);
        } else if hovered {
            cv.round_rect_a(fx, y, fw, ROW_H - 2, 6, p.sel, 0.35);
        }
        let iy = y + (ROW_H - 2 - ICON) / 2;
        let ix = fx + 8;
        match it.icon.as_deref().and_then(|name| icons.get(name, ICON as u32)) {
            Some(pm) => cv.pixmap(ix, iy, &pm),
            None => {
                let ch = it.name.chars().next().unwrap_or('?');
                cv.badge(ix, iy, ICON, ch, p.field, p.accent);
            }
        }
        let text_x = ix + ICON + 10;
        let ty = y + (ROW_H - 2 - GLYPH_H) / 2;
        // Name on the left, command on the right in dim: KRunner's two-line
        // row does not fit a 15px bitmap font, one line does. Basename only —
        // a wrapped Terminal=true entry's full /usr/local/bin/... path says
        // nothing and eats the row.
        let cmd = it
            .exec
            .split_whitespace()
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
        let cmd_w = Canvas::text_width(cmd);
        let room = fw - (text_x - fx) - cmd_w - 24;
        let max_chars = (room / GLYPH_W).max(1) as usize;
        let name: String = if it.name.chars().count() > max_chars {
            it.name.chars().take(max_chars.saturating_sub(1)).chain(['.']).collect()
        } else {
            it.name.clone()
        };
        cv.text(&name, text_x, ty, p.text);
        if room > 0 {
            let cx = fx + fw - cmd_w - 10;
            cv.text(cmd, cx, ty, if selected { p.text } else { p.dim });
        }
    }

    // Scrollbar, only when the list overflows.
    if n > MAX_ROWS {
        let track_h = MAX_ROWS as i32 * ROW_H - 4;
        let thumb_h = ((MAX_ROWS as f32 / n as f32) * track_h as f32).max(12.0) as i32;
        let max_top = (n - MAX_ROWS) as f32;
        let frac = if max_top > 0.0 { st.top as f32 / max_top } else { 0.0 };
        let tx = fx + fw - 4;
        cv.round_rect_a(tx, row0, 3, track_h, 1, p.dim, 0.25);
        let ty = row0 + (frac * (track_h - thumb_h) as f32) as i32;
        cv.round_rect_a(tx, ty, 3, thumb_h, 1, p.accent, 0.8);
    }

    // ── footer ──
    let foot_y = py + ph - FOOT_H + (FOOT_H - GLYPH_H) / 2 - 2;
    cv.hline(fx, foot_y - 6, fw, p.border, 0.35);
    cv.text(st.lang.run_keys(), fx + 8, foot_y, p.dim);
    let count = format!("{n}");
    cv.text(&count, fx + fw - 8 - Canvas::text_width(&count), foot_y, p.dim);

    ((px, py, pw, ph), row0)
}

// ── wayland plumbing ─────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    st.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "zwlr_foreign_toplevel_manager_v1" => {
                    st.foreign_mgr = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "wl_output" => {
                    if st.outputs.len() < fill_guard::MAX_OUTPUTS {
                        st.outputs.push(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "wl_seat" => {
                    if st.seat.is_none() {
                        st.seat = Some(registry.bind(name, version.min(5), qh, ()));
                    }
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        st: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
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
                st.configure(qh, width, height);
            }
            // The compositor took the surface away (output gone, session
            // ending): there is nothing left to run in, so exit.
            zwlr_layer_surface_v1::Event::Closed => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (usize, u64)> for State {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        &(i, generation): &(usize, u64),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        // Ignore releases for a retired pool: `i` indexes the CURRENT buffers.
        if matches!(event, wl_buffer::Event::Release) && generation == st.generation && i < BUFFERS {
            st.busy[i] = false;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        st: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && st.keyboard.is_none() {
                st.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && st.pointer.is_none() {
                st.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        else {
            return;
        };
        match key {
            KEY_ESC_WL => st.done = true,
            KEY_ENTER_WL | KEY_KPENTER_WL => {
                let sel = st.sel;
                if st.rows() > 0 {
                    st.launch(sel);
                } else {
                    st.done = true;
                }
            }
            KEY_BACKSPACE_WL => {
                st.filter.pop();
                st.refilter();
                st.render();
            }
            KEY_UP_WL => {
                st.move_sel(-1);
                st.render();
            }
            KEY_DOWN_WL | KEY_TAB_WL => {
                st.move_sel(1);
                st.render();
            }
            KEY_PGUP_WL => {
                st.move_sel(-(MAX_ROWS as i32));
                st.render();
            }
            KEY_PGDN_WL => {
                st.move_sel(MAX_ROWS as i32);
                st.render();
            }
            k => {
                if let Some(c) = key_char(k) {
                    if st.filter.chars().count() < MAX_FILTER {
                        st.filter.push(c);
                        st.refilter();
                        st.render();
                    }
                }
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            // Enter carries the position too: without it a click that lands
            // before any Motion would be placed at (-1,-1) and dismiss.
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                st.ptr = (surface_x, surface_y);
                let row = st.row_at(surface_x, surface_y);
                if row != st.hover {
                    st.hover = row;
                    st.render();
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                st.ptr = (surface_x, surface_y);
                let row = st.row_at(surface_x, surface_y);
                if row != st.hover {
                    st.hover = row;
                    st.render();
                }
            }
            wl_pointer::Event::Leave { .. } => {
                st.ptr = (-1.0, -1.0);
                if st.hover.is_some() {
                    st.hover = None;
                    st.render();
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                if button != BTN_LEFT {
                    return;
                }
                // A click on a row launches it; a click anywhere else on the
                // panel (the search field, the footer) is inert; a click on
                // the scrim dismisses, which is how every launcher behaves.
                let (x, y) = st.ptr;
                if let Some(row) = st.row_at(x, y) {
                    st.launch(row);
                } else if !st.inside_panel(x, y) {
                    st.done = true;
                }
            }
            wl_pointer::Event::Axis { value, .. } => {
                st.scroll_acc += value;
                let n = st.rows();
                let max_top = n.saturating_sub(MAX_ROWS);
                let mut moved = false;
                while st.scroll_acc >= WHEEL_NOTCH {
                    st.scroll_acc -= WHEEL_NOTCH;
                    if st.top < max_top {
                        st.top += 1;
                        moved = true;
                    }
                }
                while st.scroll_acc <= -WHEEL_NOTCH {
                    st.scroll_acc += WHEEL_NOTCH;
                    if st.top > 0 {
                        st.top -= 1;
                        moved = true;
                    }
                }
                if moved {
                    st.render();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        st: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                if st.toplevels.len() < fill_guard::MAX_TRACKED_TOPLEVELS {
                    st.toplevels.push((toplevel, false));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        st: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::State { state } => {
                // The array is a packed list of u32 state enums.
                let minimized = state.chunks_exact(4).any(|c| {
                    u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == TOPLEVEL_STATE_MINIMIZED
                });
                let id = handle.id().protocol_id();
                if let Some(slot) = st
                    .toplevels
                    .iter_mut()
                    .find(|(h, _)| h.id().protocol_id() == id)
                {
                    slot.1 = minimized;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                let id = handle.id().protocol_id();
                st.toplevels.retain(|(h, _)| h.id().protocol_id() != id);
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

// ── main ─────────────────────────────────────────────────────────────────────

fn new_state(toggle_desktop: bool) -> State {
    let terminal =
        std::env::var("LUNARRUN_TERMINAL").unwrap_or_else(|_| "/usr/local/bin/eclipse-terminal".into());
    let items = if toggle_desktop {
        Vec::new() // no menu to build for a one-shot minimise
    } else {
        build_items(&terminal)
    };
    let mut st = State {
        compositor: None,
        shm: None,
        layer_shell: None,
        foreign_mgr: None,
        seat: None,
        keyboard: None,
        pointer: None,
        outputs: Vec::new(),
        surface: None,
        layer: None,
        width: 0,
        height: 0,
        map: std::ptr::null_mut(),
        map_len: 0,
        buffers: [None, None],
        busy: [false, false],
        next: 0,
        generation: 0,
        configured: false,
        pal: palette(Look::current()),
        look: Look::current(),
        lang: Lang::current(),
        items,
        hits: Vec::new(),
        typed: None,
        filter: String::new(),
        sel: 0,
        top: 0,
        hover: None,
        ptr: (-1.0, -1.0),
        icons: IconCache::default(),
        panel: (0, 0, 0, 0),
        row0_y: 0,
        scroll_acc: 0.0,
        toplevels: Vec::new(),
        done: false,
    };
    st.hits = matches(&st.items, "");
    st
}

fn usage() -> ! {
    eprintln!(
        "usage: lunarrun [--toggle-desktop] [--dump PATH:WxH]\n\
         \n\
         \x20 (no args)         centred application/command launcher overlay\n\
         \x20 --toggle-desktop  minimise every window, or restore them all\n\
         \x20 --dump P:WxH      render the overlay to a raw ARGB8888 file\n\
         \n\
         env: LUNARRUN_TERMINAL, ECLIPSE_LOOK=kde|eclipse"
    );
    std::process::exit(2)
}

fn main() {
    let mut toggle_desktop = false;
    let mut dump: Option<String> = None;
    let mut filter = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--toggle-desktop" => toggle_desktop = true,
            "--dump" => dump = args.next(),
            "-h" | "--help" => usage(),
            // A bare word prefills the search field, so a keybind can open the
            // runner already filtered.
            other if !other.starts_with('-') => filter = other.to_string(),
            _ => usage(),
        }
    }

    // Offline render: no compositor, no seat, just a file to eyeball.
    if let Some(spec) = dump {
        let (path, w, h) = match spec.rsplit_once(':') {
            Some((p, dims)) if dims.contains('x') => {
                let (ws, hs) = dims.split_once('x').unwrap();
                (
                    p.to_string(),
                    ws.parse().unwrap_or(1280usize),
                    hs.parse().unwrap_or(720usize),
                )
            }
            _ => (spec, 1280usize, 720usize),
        };
        let (w, h) = (w.clamp(320, 16384), h.clamp(240, 16384));
        let mut st = new_state(false);
        st.filter = filter;
        st.refilter();
        let mut cv = Canvas::new(w, h);
        let mut icons = std::mem::take(&mut st.icons);
        let _ = draw_overlay(&mut cv, w, h, &st, &mut icons);
        let mut buf = vec![0u8; w * h * 4];
        if !cv.blit_argb(&mut buf) {
            eprintln!("lunarrun: blit failed");
            std::process::exit(1);
        }
        if let Err(e) = std::fs::write(&path, &buf) {
            eprintln!("lunarrun: {path}: {e}");
            std::process::exit(1);
        }
        println!("lunarrun: wrote {path} ({w}x{h} ARGB8888, {} rows)", st.rows());
        return;
    }

    let Ok(conn) = Connection::connect_to_env() else {
        eprintln!("lunarrun: no WAYLAND_DISPLAY (is the session up?)");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut st = new_state(toggle_desktop);
    st.filter = filter;
    st.refilter();
    // Two roundtrips: the first delivers the globals, the second the events
    // they emit on bind (seat capabilities, the toplevel list and its states).
    if queue.roundtrip(&mut st).is_err() {
        eprintln!("lunarrun: wayland roundtrip failed");
        std::process::exit(1);
    }
    if queue.roundtrip(&mut st).is_err() {
        eprintln!("lunarrun: wayland roundtrip failed");
        std::process::exit(1);
    }

    if toggle_desktop {
        if st.foreign_mgr.is_none() {
            eprintln!("lunarrun: compositor has no wlr-foreign-toplevel-management");
            std::process::exit(1);
        }
        st.apply_toggle_desktop();
        let _ = queue.roundtrip(&mut st);
        let _ = conn.flush();
        return;
    }

    if st.layer_shell.is_none() || st.shm.is_none() || st.compositor.is_none() {
        eprintln!("lunarrun: compositor lacks wlr-layer-shell/wl_shm");
        std::process::exit(1);
    }
    st.icons.prewarm();
    st.map_overlay(&qh);
    while !st.done {
        if queue.blocking_dispatch(&mut st).is_err() {
            break;
        }
    }
    // Flush the launch/teardown before the process exits: Drop destroys the
    // surfaces, but an unflushed destroy leaves the overlay on screen for as
    // long as the compositor takes to notice the socket closed.
    drop(st);
    let _ = conn.flush();
}
