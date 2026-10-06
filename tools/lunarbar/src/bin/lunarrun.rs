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
use lunarbar::proc::{self, map_shm_pool, spawn_detached};
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
/// Narrowest the panel is drawn, before it is clamped to the output.
const MIN_PANEL_W: i32 = 320;
/// Gap kept between a row's name and the command on its right.
const ROW_TEXT_GAP: i32 = 24;

/// State value from wlr-foreign-toplevel-management-unstable-v1 (the `state`
/// array carries u32 enum values). Same table lunarbar reads.
const TOPLEVEL_STATE_MINIMIZED: u32 = 1;

// ── items ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// The name normalised, for the prefix test. Its own field and not a prefix
    /// of `key`, because `key` runs the name into the command: with a name of
    /// "ab" and an exec of "cd", `key.starts_with("ab c")` is true and the name
    /// does not start with "ab c".
    name_key: String,
    /// Name AND command normalised, for the substring test.
    key: String,
}

impl Item {
    fn new(name: String, exec: String, icon: Option<String>, kind: Kind) -> Self {
        // Both normalised ONCE, here. `matches` used to call `norm_key(&it.name)`
        // for every item on every keystroke -- an allocation per app per key, and
        // exactly what this field's comment already claimed did not happen.
        let name_key = norm_key(&name);
        let key = format!("{name_key} {}", norm_key(&exec));
        Self {
            name,
            exec,
            icon,
            kind,
            name_key,
            key,
        }
    }
}

/// Applications, plus the handful of rows KRunner has that are not programs.
fn build_items(terminal: &str) -> Vec<Item> {
    let mut items = builtin_items(terminal, Lang::current());
    for AppEntry { name, exec, icon } in scan_apps(terminal) {
        items.push(Item::new(name, exec, icon, Kind::App));
    }
    items
}

/// The rows that are not `.desktop` entries: the terminal, the compositor
/// reload and the three power actions, in the order they appear above the
/// applications.
///
/// Its own function because [`build_items`] goes on to call `scan_apps`, which
/// walks `/usr/share/applications` and `$XDG_DATA_*`: from a test there is no
/// way to see this table without whatever the machine happens to have
/// installed. Each of these rows but the first runs a command that ends the
/// session or powers the box off, so one wired to the wrong action is not a
/// cosmetic mistake.
fn builtin_items(terminal: &str, lang: Lang) -> Vec<Item> {
    vec![
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
    ]
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
        if it.name_key.starts_with(&f) {
            head.push(i);
        } else if it.key.contains(&f) {
            tail.push(i);
        }
    }
    head.extend(tail);
    head
}

/// What the row shows on its right: the command's basename, with its arguments
/// and its directory gone. A `Terminal=true` entry's full
/// `/usr/local/bin/eclipse-terminal -e ...` says nothing and eats the row.
fn cmd_basename(exec: &str) -> &str {
    exec.split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
}

/// Is `cmd`'s first word an executable on `$PATH` (or an absolute path)?
/// Gates the "run what was typed" row, so Enter on a typo cannot spawn a shell
/// that exits 127 with nothing on screen to say why.
fn runnable(cmd: &str) -> bool {
    let path = search_path(std::env::var("PATH").ok());
    runnable_in(cmd, &path, |p| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// `$PATH`, or the crate's default when it is unset or empty.
///
/// **The same list `apps.rs` uses for `TryExec`.** There used to be one here and
/// a different one there, and the one here was missing `/sbin` and `/usr/sbin`:
/// a command the menu accepts and the runner rejects (or the other way round) is
/// a bug nobody can explain from either side.
fn search_path(var: Option<String>) -> String {
    match var {
        Some(p) if !p.is_empty() => p,
        _ => lunarbar::apps::DEFAULT_PATH.to_string(),
    }
}

/// The decision behind [`runnable`], with `$PATH` and the disk handed in.
///
/// A leading `/` or any `/` at all means a path, looked up as written; a bare
/// word is looked for in each non-empty component of `path`. **Empty components
/// are skipped**: POSIX reads one as the current directory, so `PATH=:/usr/bin`
/// made the launcher's answer depend on where the panel happened to be started
/// from, and a `./foo` in the user's home could shadow a real command.
fn runnable_in(cmd: &str, path: &str, is_exec: impl Fn(&std::path::Path) -> bool) -> bool {
    let Some(word) = cmd.split_whitespace().next() else {
        return false;
    };
    if word.contains('/') {
        return is_exec(std::path::Path::new(word));
    }
    path.split(':')
        .filter(|d| !d.is_empty())
        .any(|d| is_exec(&std::path::Path::new(d).join(word)))
}

/// What a key press does. A separate enum from the handler so the table can be
/// checked without a compositor: the keyboard handler needs a live `State` full
/// of Wayland objects, and the one thing worth pinning about it -- **which keys
/// wrap and which stop** -- lived only inside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    /// Esc: close without launching anything.
    Close,
    /// Enter: run the selected row, or close if there is nothing to run.
    Launch,
    /// Backspace: drop the last character of the filter.
    Backspace,
    /// Move the selection, by this many rows, wrapping or stopping.
    Move(i32, Step),
    /// A printable character: append it to the filter.
    Type(char),
    /// A key with nothing bound to it.
    Ignore,
}

/// The key table. Arrows and Tab wrap; the page keys stop at the ends.
fn key_action(key: u32) -> Action {
    match key {
        KEY_ESC_WL => Action::Close,
        KEY_ENTER_WL | KEY_KPENTER_WL => Action::Launch,
        KEY_BACKSPACE_WL => Action::Backspace,
        KEY_UP_WL => Action::Move(-1, Step::Wrap),
        KEY_DOWN_WL | KEY_TAB_WL => Action::Move(1, Step::Wrap),
        KEY_PGUP_WL => Action::Move(-(MAX_ROWS as i32), Step::Clamp),
        KEY_PGDN_WL => Action::Move(MAX_ROWS as i32, Step::Clamp),
        k => match key_char(k) {
            Some(c) => Action::Type(c),
            None => Action::Ignore,
        },
    }
}

/// How far the ends of the result list are from each other.
///
/// Up/Down wrap, because one step off the end of a short list is a reach for
/// the other end and every launcher does it. Page Up/Down **stop**: with
/// `rem_euclid` they wrapped too, and a wrap of eight rows does not land on an
/// end, it lands eight from it. Page Up on the first row of a twenty-row list
/// selected **row twelve**, in the middle of the list, and Page Down on the
/// last one selected row seven -- a jump nobody asked for to a place nobody
/// could predict. Home/End are the keys for the ends; a page key that cannot
/// advance a page stays where it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// Arrow keys and Tab: falling off one end arrives at the other.
    Wrap,
    /// Page keys: the ends are walls.
    Clamp,
}

/// The `(sel, top)` after moving the selection by `delta` over `n` rows.
///
/// `top` is the first visible row, and it only ever moves as much as it must to
/// keep `sel` on screen: the list does not recentre under the cursor, which is
/// what makes holding Down read as a cursor walking down a still list until it
/// reaches the bottom edge.
fn move_selection(sel: usize, top: usize, n: usize, delta: i32, step: Step) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let last = n as i32 - 1;
    // Neither arm needs `sel` to be in range first: `rem_euclid` and `clamp`
    // both bring any value back inside, which is what makes this total when the
    // list shrank under a selection.
    let want = (sel as i32).saturating_add(delta);
    let sel = match step {
        Step::Wrap => want.rem_euclid(n as i32) as usize,
        Step::Clamp => want.clamp(0, last) as usize,
    };
    // A window that starts past the end is brought back first, so the caller
    // cannot be handed a `top` that shows rows which do not exist.
    let top = top.min(n.saturating_sub(MAX_ROWS));
    // Then scroll by the least that shows the selection. `sel + 1 - MAX_ROWS`
    // cannot underflow: this arm needs `sel >= top + MAX_ROWS`, so
    // `sel >= MAX_ROWS`.
    let top = if sel < top {
        sel
    } else if sel >= top + MAX_ROWS {
        sel + 1 - MAX_ROWS
    } else {
        top
    };
    (sel, top)
}

// ── state ────────────────────────────────────────────────────────────────────

/// The crate's one double-buffer count; see `proc::BUFFERS`.
use lunarbar::proc::BUFFERS;

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

    fn move_sel(&mut self, delta: i32, step: Step) {
        let (sel, top) = move_selection(self.sel, self.top, self.rows(), delta, step);
        self.sel = sel;
        self.top = top;
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
    fn configure(&mut self, qh: &QueueHandle<State>, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        // Defaults for a zero, and the same ceilings `--dump` applies.
        let Some((w, h)) = surface_size(w, h) else {
            eprintln!("lunarrun: {w}x{h} is not renderable; skipping");
            return;
        };
        if self.configured && self.width == w && self.height == h {
            self.render();
            return;
        }
        // One place for the i32 the protocol actually uses, shared with the
        // panel's three bars: `create_pool` and `create_buffer` both take i32,
        // and a pool past it arrives negative and kills the client.
        let Some(geom) = proc::pool_geometry(w, h) else {
            eprintln!("lunarrun: {w}x{h} does not fit a wl_shm pool; skipping");
            return;
        };
        let (total, stride, frame_size) = (geom.total, geom.stride, geom.frame_size);
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
        row_at_geom(self.panel, self.row0_y, self.top, self.rows(), x, y)
    }

    fn inside_panel(&self, x: f64, y: f64) -> bool {
        inside_rect(self.panel, x, y)
    }

    /// `--toggle-desktop`: minimise every window, or restore them all when
    /// every one is minimised already (KDE's Super+D behaviour).
    fn apply_toggle_desktop(&mut self) {
        let states: Vec<bool> = self.toplevels.iter().map(|(_, m)| *m).collect();
        if let Some(all_min) = toggle_desktop_action(&states) {
            for (h, min) in &self.toplevels {
                if all_min {
                    h.unset_minimized();
                } else if !*min {
                    h.set_minimized();
                }
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

/// The panel rect `(x, y, w, h)` for an output of `w` x `h` showing `rows`
/// results.
///
/// Where the panel sits is the look's, not the layout's: Windows 11 opens Start
/// just above the taskbar, KRunner sits in the upper third. Both are clamped so
/// a short screen cannot push the panel off the top.
fn panel_rect(w: i32, h: i32, rows: usize, look: Look) -> (i32, i32, i32, i32) {
    // `.max(MIN_PANEL_W)` used to come last, so on an output narrower than
    // MIN_PANEL_W the panel came out WIDER than the screen and `x` went
    // negative. Clamp to the output afterwards: a cramped panel is legible, one
    // hanging off the left edge is not.
    let roomy = PANEL_W.min(w - 96).max(MIN_PANEL_W);
    // Not `clamp`: its floor and ceiling cross over exactly in the case being
    // guarded against (an output narrower than MIN_PANEL_W), and it panics then.
    let pw = roomy.min(w.max(1));
    // At least one row of height, so the "no results" line has somewhere to go.
    let shown = rows.clamp(1, MAX_ROWS) as i32;
    let ph = PAD + FIELD_H + PAD + shown * ROW_H + FOOT_H + PAD;
    let px = (w - pw) / 2;
    let py = match look {
        Look::Win11 => (h - ph - 64).max(24),
        _ => (h / 5).min(h - ph - 24).max(24),
    };
    (px, py, pw, ph)
}

/// How many characters of the name fit on a row `avail` px wide, and whether
/// the command fits beside it.
///
/// When it does not, the name gets the WHOLE row. It used to be charged for the
/// command's width either way while the command itself was drawn only if the
/// leftover was positive, so a long command on a narrow panel left the name one
/// character -- a row reading "." with no command next to it. One decision, so
/// the space the name is charged for is the space the command takes.
fn row_text_layout(avail: i32, cmd_w: i32) -> (usize, bool) {
    let room = avail - cmd_w - ROW_TEXT_GAP;
    if room > 0 {
        ((room / GLYPH_W).max(1) as usize, true)
    } else {
        (((avail - 10) / GLYPH_W).max(1) as usize, false)
    }
}

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

    let (px, py, pw, ph) = panel_rect(w as i32, h as i32, st.rows(), st.look);

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
            st.filter
                .chars()
                .skip(st.filter.chars().count() - max_chars)
                .collect()
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
        cv.text(
            st.lang.run_empty(),
            fx + 8,
            row0 + (ROW_H - GLYPH_H) / 2,
            p.dim,
        );
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
        match it
            .icon
            .as_deref()
            .and_then(|name| icons.get(name, ICON as u32))
        {
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
        let cmd = cmd_basename(&it.exec);
        let cmd_w = Canvas::text_width(cmd);
        let (max_chars, show_cmd) = row_text_layout(fw - (text_x - fx), cmd_w);
        let name: String = if it.name.chars().count() > max_chars {
            it.name
                .chars()
                .take(max_chars.saturating_sub(1))
                .chain(['.'])
                .collect()
        } else {
            it.name.clone()
        };
        cv.text(&name, text_x, ty, p.text);
        if show_cmd {
            let cx = fx + fw - cmd_w - 10;
            cv.text(cmd, cx, ty, if selected { p.text } else { p.dim });
        }
    }

    // Scrollbar, only when the list overflows.
    if n > MAX_ROWS {
        let track_h = MAX_ROWS as i32 * ROW_H - 4;
        let thumb_h = ((MAX_ROWS as f32 / n as f32) * track_h as f32).max(12.0) as i32;
        let max_top = (n - MAX_ROWS) as f32;
        let frac = if max_top > 0.0 {
            st.top as f32 / max_top
        } else {
            0.0
        };
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
    cv.text(
        &count,
        fx + fw - 8 - Canvas::text_width(&count),
        foot_y,
        p.dim,
    );

    ((px, py, pw, ph), row0)
}

// ── hit testing and the wheel ──────────────────────────────────────

/// Is `(x, y)` inside the rect `(x, y, w, h)`?
///
/// Half-open on the far edges, because `w` is a WIDTH: a rect at x=10 of width
/// 100 owns columns 10..110, and 110 belongs to whatever is next. With `<=` the
/// column one past the panel counted as inside, so a click there was swallowed
/// instead of dismissing the overlay.
fn inside_rect(rect: (i32, i32, i32, i32), x: f64, y: f64) -> bool {
    let (rx, ry, rw, rh) = rect;
    let (x, y) = (x.floor() as i32, y.floor() as i32);
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

/// Which result row `(x, y)` is over, given the panel rect, the y of the first
/// row, the scroll position and how many rows exist.
///
/// A click below the last drawn row -- the footer, or the empty space of a short
/// list -- is no row at all, which is what keeps it from launching whatever
/// happens to be selected.
fn row_at_geom(
    panel: (i32, i32, i32, i32),
    row0_y: i32,
    top: usize,
    rows: usize,
    x: f64,
    y: f64,
) -> Option<usize> {
    if !inside_rect(panel, x, y) {
        return None;
    }
    let y = y.floor() as i32;
    if y < row0_y {
        return None;
    }
    let row = ((y - row0_y) / ROW_H) as usize + top;
    let shown = rows.min(top.saturating_add(MAX_ROWS));
    (row < shown).then_some(row)
}

/// The `(top, leftover)` after a wheel event of `delta`, with `acc` already
/// carrying what earlier events did not amount to a notch.
///
/// The leftover is what is left BELOW a whole notch, and nothing else: a
/// high-resolution wheel sends fractions of a notch, and dropping them makes a
/// slow scroll never move, but a whole notch is spent whether or not the list
/// could move. Bank the refused ones instead and holding the wheel against the
/// top of the list builds a charge that swallows the first flick back down.
///
/// Arithmetic rather than a loop, so a compositor sending one enormous value
/// costs one division and not a billion iterations.
fn scroll_top(top: usize, acc: f64, delta: f64, rows: usize) -> (usize, f64) {
    let max_top = rows.saturating_sub(MAX_ROWS);
    let top = top.min(max_top);
    let total = acc + delta;
    // A non-finite value would poison the accumulator for the rest of the
    // session, so it is dropped rather than carried.
    if !total.is_finite() {
        return (top, 0.0);
    }
    let notches = (total / WHEEL_NOTCH).trunc();
    let leftover = total - notches * WHEEL_NOTCH;
    // Float-to-int casts saturate, so an absurd count cannot wrap.
    let top = if notches >= 0.0 {
        top.saturating_add(notches as usize).min(max_top)
    } else {
        top.saturating_sub(-notches as usize)
    };
    (top, leftover)
}

/// What `--toggle-desktop` should do to the windows in `minimized`: restore
/// them all, minimise the ones that are not, or nothing because there are none.
///
/// KDE's Super+D: it shows the desktop, and shows it again the second time by
/// putting everything back. "Everything is already minimised" is the only thing
/// that flips it, so a session with one window left up minimises that one
/// rather than restoring the rest.
fn toggle_desktop_action(minimized: &[bool]) -> Option<bool> {
    if minimized.is_empty() {
        return None;
    }
    Some(minimized.iter().all(|m| *m))
}

/// Is `TOPLEVEL_STATE_MINIMIZED` in a wlr-foreign-toplevel `state` array?
///
/// The array is a packed list of u32 enum values in the host's byte order (the
/// Wayland wire format is native-endian, same machine both sides). A trailing
/// partial value is ignored rather than read past.
fn state_array_has(bytes: &[u8], want: u32) -> bool {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .any(|c| u32::from_ne_bytes(*c) == want)
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
                "wl_compositor" => {
                    st.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
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
        if matches!(event, wl_buffer::Event::Release) && generation == st.generation && i < BUFFERS
        {
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
        match key_action(key) {
            Action::Close => st.done = true,
            Action::Launch => {
                let sel = st.sel;
                if st.rows() > 0 {
                    st.launch(sel);
                } else {
                    st.done = true;
                }
            }
            Action::Backspace => {
                st.filter.pop();
                st.refilter();
                st.render();
            }
            Action::Move(delta, step) => {
                st.move_sel(delta, step);
                st.render();
            }
            Action::Type(c) => {
                if st.filter.chars().count() < MAX_FILTER {
                    st.filter.push(c);
                    st.refilter();
                    st.render();
                }
            }
            Action::Ignore => {}
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
                let (top, acc) = scroll_top(st.top, st.scroll_acc, value, st.rows());
                st.scroll_acc = acc;
                if top != st.top {
                    st.top = top;
                    // The hover follows the list under a still pointer, or the
                    // highlight stays on the row that scrolled away from it.
                    let (x, y) = st.ptr;
                    st.hover = row_at_geom(st.panel, st.row0_y, top, st.rows(), x, y);
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
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            // Through the shared guard, so a compositor that never sends Closed
            // cannot grow this Vec without bound.
            fill_guard::try_push_bounded(
                &mut st.toplevels,
                (toplevel, false),
                fill_guard::MAX_TRACKED_TOPLEVELS,
            );
        }
    }

    // The toplevel event carries a new object, so wayland-client needs to be
    // told what to build for it. Without this it panics on the first window.
    wayland_client::event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
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
                let minimized = state_array_has(&state, TOPLEVEL_STATE_MINIMIZED);
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

/// Floors for `--dump`, so a tiny request still renders something a person can
/// look at rather than a sliver. The ceilings are `fill_guard::check_buffer`'s.
const MIN_DUMP_W: usize = 320;
const MIN_DUMP_H: usize = 240;

/// Split `--dump PATH:WxH` into its three parts, defaulting the size.
///
/// The size is the last `:` segment ONLY when it is digits, `x`, digits. It used
/// to be accepted as soon as the segment merely contained an `x`, and then each
/// side was `parse().unwrap_or(default)`: `--dump /tmp/frame:extra.raw` passed
/// (the `x` of "extra"), both sides failed to parse, and the dump was written to
/// `/tmp/frame` at the default size -- losing part of the path AND the size the
/// caller asked for, without a word about either.
///
/// `parse` alone is not the check: `usize::from_str` accepts a leading `+`, so
/// `out:+800x600` would have gone through as a size too.
fn parse_dump_spec(spec: &str) -> (String, usize, usize) {
    let dims = spec.rsplit_once(':').and_then(|(path, dims)| {
        // `split_once` or `rsplit_once` make no difference given the digits
        // check below: with two or more `x` in the segment, either split leaves
        // one of them on a side, and that side is then not all digits.
        let (ws, hs) = dims.split_once('x')?;
        // `all` on an empty side is vacuously true, and `parse` below is what
        // rejects it -- but `parse` is NOT the whole check: `usize::from_str`
        // accepts a leading `+`, so `+800` has to be refused here.
        let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
        if !digits(ws) || !digits(hs) {
            return None;
        }
        Some((path.to_string(), ws.parse().ok()?, hs.parse().ok()?))
    });
    dims.unwrap_or((spec.to_string(), 1280, 720))
}

/// Everything `--dump PATH:WxH` decides: the path, the size, the floors and the
/// ceilings. One function, so the offline render cannot end up applying a
/// different limit from the surface it is standing in for -- which it did.
fn dump_target(spec: &str) -> Result<(String, usize, usize), fill_guard::TooBig> {
    let (path, w, h) = parse_dump_spec(spec);
    let (w, h) = (w.max(MIN_DUMP_W), h.max(MIN_DUMP_H));
    fill_guard::check_buffer(w, h)?;
    Ok((path, w, h))
}

/// The buffer size to allocate for a `configure` of `w` x `h`, or `None` when
/// the compositor asked for something this client will not render.
///
/// A zero from the compositor means "you choose" (a layer surface anchored to
/// all four edges of an output whose mode has not settled yet), so it gets a
/// size rather than a 0-byte pool.
fn surface_size(w: u32, h: u32) -> Option<(u32, u32)> {
    let w = if w == 0 { 1280 } else { w };
    let h = if h == 0 { 720 } else { h };
    fill_guard::check_buffer(w as usize, h as usize).ok()?;
    Some((w, h))
}

// ── main ─────────────────────────────────────────────────────────────────────

fn new_state(toggle_desktop: bool) -> State {
    let terminal = std::env::var("LUNARRUN_TERMINAL")
        .unwrap_or_else(|_| "/usr/local/bin/eclipse-terminal".into());
    let items = if toggle_desktop {
        Vec::new() // no menu to build for a one-shot minimise
    } else {
        build_items(&terminal)
    };
    // ONE read of the look, not two. `Look::current()` reads /etc/eclipse/look,
    // and `eclipse-look` rewrites that file when the user switches: two calls
    // could return two different looks and leave the palette from one beside the
    // geometry of the other.
    let look = Look::current();
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
        pal: palette(look),
        look,
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

/// What the command line asked for.
#[derive(Default, PartialEq, Eq, Debug)]
struct Cli {
    toggle_desktop: bool,
    dump: Option<String>,
    /// A bare word prefills the search field, so a keybind can open the runner
    /// already filtered. Several words are joined, not overwritten.
    filter: String,
}

/// Parse the command line, or `None` to print the usage and exit 2.
fn parse_args(args: impl Iterator<Item = String>) -> Option<Cli> {
    let mut cli = Cli::default();
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--toggle-desktop" => cli.toggle_desktop = true,
            // A missing argument used to leave this None and fall through to the
            // compositor path, so `lunarrun --dump` with the spec forgotten
            // opened the overlay on the user's screen instead of saying so. And
            // the next argument is only the spec if it is not itself a flag.
            "--dump" => cli.dump = Some(args.next_if(|n| !n.starts_with('-'))?),
            "-h" | "--help" => return None,
            other if !other.starts_with('-') => {
                // Joined, not replaced: `lunarrun text editor` used to search
                // for "editor" alone, dropping the word before it.
                if !cli.filter.is_empty() {
                    cli.filter.push(' ');
                }
                cli.filter.push_str(other);
            }
            _ => return None,
        }
    }
    Some(cli)
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
    let Cli {
        toggle_desktop,
        dump,
        filter,
    } = match parse_args(std::env::args().skip(1)) {
        Some(c) => c,
        None => usage(),
    };

    // Offline render: no compositor, no seat, just a file to eyeball.
    if let Some(spec) = dump {
        // The SAME ceilings the compositor path applies in `configure`. This
        // used to clamp to a hardcoded 16384 -- twice this crate's
        // MAX_BUFFER_DIM -- and never looked at the area, so `--dump x:16384x16384`
        // asked for a gigabyte and aborted inside `Canvas::new`'s expect.
        let (path, w, h) = match dump_target(&spec) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("lunarrun: {spec}: past {e:?}; nothing written");
                std::process::exit(1);
            }
        };
        let mut st = new_state(false);
        st.filter = filter;
        st.refilter();
        // try_new, like `render`: refusing beats aborting (`panic = "abort"`).
        let Some(mut cv) = Canvas::try_new(w, h) else {
            eprintln!("lunarrun: cannot allocate a {w}x{h} canvas");
            std::process::exit(1);
        };
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
        println!(
            "lunarrun: wrote {path} ({w}x{h} ARGB8888, {} rows)",
            st.rows()
        );
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

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, exec: &str) -> Item {
        Item::new(name.into(), exec.into(), None, Kind::App)
    }

    // ── the selection ────────────────────────────────────────────────────────

    #[test]
    fn an_arrow_key_wraps_around_both_ends() {
        // One step off an end reaches the other: with four results, Up from the
        // first lands on the last, which is how every launcher behaves.
        assert_eq!(move_selection(0, 0, 4, -1, Step::Wrap).0, 3);
        assert_eq!(move_selection(3, 0, 4, 1, Step::Wrap).0, 0);
        assert_eq!(move_selection(1, 0, 4, 1, Step::Wrap).0, 2);
        assert_eq!(move_selection(1, 0, 4, -1, Step::Wrap).0, 0);
        // A single result: every step stays on it.
        assert_eq!(move_selection(0, 0, 1, 1, Step::Wrap).0, 0);
        assert_eq!(move_selection(0, 0, 1, -1, Step::Wrap).0, 0);
    }

    #[test]
    fn a_page_key_stops_at_the_end_instead_of_landing_in_the_middle() {
        // THE REGRESSION. Page Up and Page Down went through `rem_euclid` like
        // the arrows, and a wrap of eight rows does not land on an end -- it
        // lands eight rows from it. On a twenty-row list, Page Up on the first
        // row selected row TWELVE and Page Down on the last selected row SEVEN:
        // a jump into the middle of the list that nobody could predict.
        let n = 20;
        assert_eq!(
            move_selection(0, 0, n, -(MAX_ROWS as i32), Step::Clamp).0,
            0
        );
        assert_eq!(
            move_selection(n - 1, 0, n, MAX_ROWS as i32, Step::Clamp).0,
            n - 1
        );
        // What it used to do, kept here so the fix cannot be quietly undone.
        assert_eq!((0i32 - MAX_ROWS as i32).rem_euclid(n as i32), 12);
        assert_eq!((n as i32 - 1 + MAX_ROWS as i32).rem_euclid(n as i32), 7);

        // A page from the middle still moves a full page.
        assert_eq!(move_selection(10, 5, n, MAX_ROWS as i32, Step::Clamp).0, 18);
        assert_eq!(
            move_selection(10, 5, n, -(MAX_ROWS as i32), Step::Clamp).0,
            2
        );
        // And a page that would overshoot goes exactly to the end.
        assert_eq!(
            move_selection(15, 8, n, MAX_ROWS as i32, Step::Clamp).0,
            n - 1
        );
        assert_eq!(
            move_selection(3, 0, n, -(MAX_ROWS as i32), Step::Clamp).0,
            0
        );
    }

    #[test]
    fn the_window_scrolls_by_the_least_that_keeps_the_selection_visible() {
        // Walking down a long list: `top` stays put until the selection reaches
        // the bottom edge, then follows it one row at a time. A window that
        // recentred would make the whole list jump under every keypress.
        let n = 40;
        let (mut sel, mut top) = (0usize, 0usize);
        for _ in 0..(MAX_ROWS - 1) {
            (sel, top) = move_selection(sel, top, n, 1, Step::Wrap);
            assert_eq!(top, 0, "the list must not move while sel is on screen");
        }
        assert_eq!(sel, MAX_ROWS - 1);
        (sel, top) = move_selection(sel, top, n, 1, Step::Wrap);
        assert_eq!((sel, top), (MAX_ROWS, 1));

        // The selection is always inside the window, whatever the step.
        for &step in &[Step::Wrap, Step::Clamp] {
            for start in 0..n {
                for delta in [-(MAX_ROWS as i32), -3, -1, 1, 3, MAX_ROWS as i32] {
                    let (s, t) = move_selection(start, start.saturating_sub(2), n, delta, step);
                    assert!(s >= t && s < t + MAX_ROWS, "{s} outside {t}..+{MAX_ROWS}");
                    assert!(
                        t + MAX_ROWS <= n.max(MAX_ROWS),
                        "top {t} scrolled past the end"
                    );
                }
            }
        }
    }

    #[test]
    fn a_list_with_nothing_in_it_moves_nowhere() {
        // Enter on an empty list closes the overlay rather than launching row 0,
        // so this must not hand back a row that `row_item` would resolve.
        assert_eq!(move_selection(0, 0, 0, 1, Step::Wrap), (0, 0));
        assert_eq!(move_selection(5, 3, 0, -1, Step::Clamp), (0, 0));
    }

    #[test]
    fn a_selection_left_past_the_end_is_brought_back_in() {
        // Refilter resets sel to 0, but a shorter list arriving any other way
        // (a window closing, a filter applied from argv) must not leave sel out
        // of range and index nothing.
        let (sel, top) = move_selection(30, 25, 4, 1, Step::Wrap);
        assert!(sel < 4, "sel {sel} is past the end of a 4-row list");
        assert!(top < 4);
        let (sel, _) = move_selection(30, 25, 4, -1, Step::Clamp);
        assert!(sel < 4);
        // Total, not merely usually right: an absurd selection must come back in
        // range rather than overflow the i32 the step is computed in.
        for &step in &[Step::Wrap, Step::Clamp] {
            for sel in [i32::MAX as usize, usize::MAX, usize::MAX / 2] {
                let (s, t) = move_selection(sel, 0, 4, 1, step);
                assert!(s < 4, "sel {sel} came back as {s}");
                assert!(t < 4);
            }
        }
    }

    // ── the --dump spec ──────────────────────────────────────────────────────

    #[test]
    fn a_path_that_merely_contains_an_x_is_not_a_size() {
        // THE REGRESSION: the last `:` segment was taken as a size as soon as it
        // held an `x`, then each side fell back to a default when it failed to
        // parse. `--dump /tmp/frame:extra.raw` wrote `/tmp/frame` at 1280x720,
        // losing the rest of the path without a word.
        assert_eq!(
            parse_dump_spec("/tmp/frame:extra.raw"),
            ("/tmp/frame:extra.raw".to_string(), 1280, 720)
        );
        assert_eq!(
            parse_dump_spec("/tmp/box.raw"),
            ("/tmp/box.raw".to_string(), 1280, 720)
        );
    }

    #[test]
    fn a_size_is_digits_x_digits_and_nothing_else() {
        // `parse` alone is not the check: `usize::from_str` accepts a leading
        // `+`, so `+800x600` would have gone through as a size.
        for bad in [
            "out:+800x600",
            "out:800x",
            "out:x600",
            "out:800x600x480",
            "out:-800x600",
            "out: 800x600",
            "out:800 x600",
            "out:0x800x600",
        ] {
            let (path, w, h) = parse_dump_spec(bad);
            assert_eq!((w, h), (1280, 720), "{bad} was read as a size");
            assert_eq!(path, bad, "{bad} lost part of its path");
        }
    }

    #[test]
    fn a_real_size_is_taken_and_the_path_keeps_its_own_colons() {
        assert_eq!(
            parse_dump_spec("/tmp/f.raw:800x600"),
            ("/tmp/f.raw".to_string(), 800, 600)
        );
        // Only the LAST colon splits, so a path with colons in it survives.
        assert_eq!(
            parse_dump_spec("/tmp/a:b:c.raw:640x480"),
            ("/tmp/a:b:c.raw".to_string(), 640, 480)
        );
        // A zero is a number; the floor is what deals with it, not the parse.
        assert_eq!(parse_dump_spec("o:0x0"), ("o".to_string(), 0, 0));
    }

    #[test]
    fn the_dump_size_lands_inside_the_same_ceiling_the_surface_uses() {
        // The two paths render the same overlay and used to disagree about what
        // was renderable: this one clamped to a hardcoded 16384, which is TWICE
        // this crate's MAX_BUFFER_DIM, and never looked at the area at all.
        assert!(16384 > fill_guard::MAX_BUFFER_DIM as usize);
        assert!(fill_guard::check_buffer(16384, 16384).is_err());
        // The floors are renderable, and so is every ordinary output.
        assert!(fill_guard::check_buffer(MIN_DUMP_W, MIN_DUMP_H).is_ok());
        for (w, h) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
            assert!(fill_guard::check_buffer(w, h).is_ok(), "{w}x{h}");
        }
        // And a spec that asks for too much is refused, not clamped down to
        // something that renders a different picture than was asked for.
        let (_, w, h) = parse_dump_spec("o:20000x20000");
        assert_eq!(
            fill_guard::check_buffer(w.max(MIN_DUMP_W), h.max(MIN_DUMP_H)),
            Err(fill_guard::TooBig::Dim)
        );
    }

    // ── the panel's geometry ─────────────────────────────────────────────────

    #[test]
    fn the_panel_never_hangs_off_the_side_of_the_output() {
        // `.max(MIN_PANEL_W)` used to come last, so an output narrower than 320
        // got a panel WIDER than itself and an x that went negative.
        for w in [1, 2, 64, 200, 320, 321, 640, 800, 1280, 1920, 3840, 7680] {
            let (px, _, pw, _) = panel_rect(w, 1080, 5, Look::Kde);
            assert!(px >= 0, "{w}px output put the panel at x={px}");
            assert!(pw >= 1, "{w}px output gave a {pw}px panel");
            assert!(px + pw <= w, "{w}px output: panel {px}..{}", px + pw);
            // Centred, within the rounding of an odd leftover: the margins
            // either side differ by at most a pixel.
            let right = w - (px + pw);
            assert!(
                (px - right).abs() <= 1,
                "{w}px output: {px}px left, {right}px right"
            );
        }
        // On a wide screen the panel is its nominal width, not the floor.
        assert_eq!(panel_rect(1920, 1080, 5, Look::Kde).2, PANEL_W);
        assert_eq!(panel_rect(1920, 1080, 5, Look::Kde).0, (1920 - PANEL_W) / 2);
        // And there is scrim either side wherever there is room for it: an
        // overlay flush with both edges of the screen reads as a window, and
        // clicking outside to dismiss stops being possible left or right.
        for w in [416, 500, 760, 800, 816, 1000, 1280, 1920] {
            let (_, _, pw, _) = panel_rect(w, 1080, 5, Look::Kde);
            assert!(w - pw >= 96, "{w}px output: {pw}px panel leaves no scrim");
        }
    }

    #[test]
    fn the_panel_is_never_pushed_off_the_top_of_a_short_screen() {
        // A short screen cannot fit the panel; the clamp keeps its TOP on
        // screen, which is the half that has the search field in it.
        for look in [Look::Kde, Look::Win11, Look::Eclipse] {
            for h in [1, 100, 240, 480, 600, 768, 1080, 2160] {
                let (_, py, _, ph) = panel_rect(1920, h, 8, look);
                assert!(py >= 24, "{h}px tall put the panel at y={py}");
                assert!(ph > 0);
            }
            // On a normal screen it sits where the look says, not at the clamp.
            let (_, py, _, ph) = panel_rect(1920, 1080, 8, look);
            assert!(py > 24 && py + ph <= 1080, "y={py} h={ph}");
        }
    }

    #[test]
    fn the_panel_grows_one_row_at_a_time_up_to_the_row_cap() {
        let h0 = panel_rect(1920, 1080, 0, Look::Kde).3;
        let h1 = panel_rect(1920, 1080, 1, Look::Kde).3;
        // Zero results still get a row's height: that is where the "no results"
        // line is drawn, and without it the text lands outside the panel.
        assert_eq!(h0, h1);
        for rows in 1..MAX_ROWS {
            let a = panel_rect(1920, 1080, rows, Look::Kde).3;
            let b = panel_rect(1920, 1080, rows + 1, Look::Kde).3;
            assert_eq!(b - a, ROW_H, "{rows} -> {} rows", rows + 1);
        }
        // Past the cap the panel stops growing; the list scrolls instead.
        let capped = panel_rect(1920, 1080, MAX_ROWS, Look::Kde).3;
        for rows in [MAX_ROWS + 1, MAX_ROWS * 4, 900] {
            assert_eq!(panel_rect(1920, 1080, rows, Look::Kde).3, capped);
        }
    }

    #[test]
    fn a_row_too_narrow_for_its_command_gives_the_name_the_whole_row() {
        // The name was charged for the command's width even when the command
        // was then not drawn, so a long command on a narrow panel left the name
        // ONE character: a row reading "." and nothing else.
        let avail = 200;
        let (chars, show) = row_text_layout(avail, 400);
        assert!(!show, "there is no room for the command");
        assert!(
            chars > 1,
            "the name got {chars} characters of a {avail}px row"
        );
        assert!(chars as i32 * GLYPH_W <= avail);

        // With room for both, the name is charged for the command and the gap.
        let (chars, show) = row_text_layout(avail, 40);
        assert!(show);
        assert_eq!(chars, ((avail - 40 - ROW_TEXT_GAP) / GLYPH_W) as usize);
        // More command, less name, monotonically, and never zero characters.
        let mut prev = usize::MAX;
        for cmd_w in 0..avail + 100 {
            let (chars, _) = row_text_layout(avail, cmd_w);
            assert!(chars >= 1, "{cmd_w}px command left no name at all");
            if cmd_w < avail - ROW_TEXT_GAP {
                assert!(chars <= prev, "{cmd_w}px command widened the name");
                prev = chars;
            }
        }
    }

    // ── hit testing ──────────────────────────────────────────────────────────

    const PANEL: (i32, i32, i32, i32) = (100, 50, 400, 300);

    #[test]
    fn the_column_one_past_the_panel_is_outside_it() {
        // `w` is a WIDTH: a panel at x=100 of width 400 owns 100..500, and 500
        // is the scrim. With `<=` that column swallowed the click that should
        // have dismissed the overlay.
        assert!(inside_rect(PANEL, 100.0, 50.0));
        assert!(inside_rect(PANEL, 499.0, 349.0));
        assert!(!inside_rect(PANEL, 500.0, 200.0));
        assert!(!inside_rect(PANEL, 300.0, 350.0));
        assert!(!inside_rect(PANEL, 99.0, 200.0));
        assert!(!inside_rect(PANEL, 300.0, 49.0));
        // The pointer never entered: (-1,-1) must read as the scrim, or the
        // first click before any Motion would land on a row.
        assert!(!inside_rect(PANEL, -1.0, -1.0));
        // Sub-pixel coordinates floor, so 499.9 is still column 499.
        assert!(inside_rect(PANEL, 499.9, 349.9));
    }

    #[test]
    fn each_row_band_maps_to_its_own_row() {
        let row0 = 120;
        for row in 0..5usize {
            let top_y = row0 + row as i32 * ROW_H;
            for dy in [0, 1, ROW_H / 2, ROW_H - 1] {
                assert_eq!(
                    row_at_geom(PANEL, row0, 0, 5, 200.0, (top_y + dy) as f64),
                    Some(row),
                    "row {row} at +{dy}"
                );
            }
        }
        // Above the first row is the search field: inert, not row 0.
        assert_eq!(row_at_geom(PANEL, row0, 0, 5, 200.0, 119.0), None);
    }

    #[test]
    fn a_click_below_the_last_row_is_no_row_at_all() {
        let row0 = 120;
        // Three results: the space under them is the footer, and a click there
        // must not launch whatever happens to be selected.
        assert_eq!(row_at_geom(PANEL, row0, 0, 3, 200.0, 239.0), Some(2));
        assert_eq!(row_at_geom(PANEL, row0, 0, 3, 200.0, 240.0), None);
        // Scrolled: only the MAX_ROWS actually drawn are hittable, even when
        // the panel is tall enough for the click to be inside it.
        let rows = 20;
        let tall = (100, 50, 400, ROW_H * (MAX_ROWS as i32 + 4));
        assert_eq!(
            row_at_geom(tall, row0, 5, rows, 200.0, row0 as f64),
            Some(5)
        );
        let last = row0 + (MAX_ROWS as i32 - 1) * ROW_H;
        assert_eq!(
            row_at_geom(tall, row0, 5, rows, 200.0, last as f64),
            Some(5 + MAX_ROWS - 1)
        );
        let past = row0 + MAX_ROWS as i32 * ROW_H;
        assert!(
            inside_rect(tall, 200.0, past as f64),
            "the click is in the panel"
        );
        assert_eq!(
            row_at_geom(tall, row0, 5, rows, 200.0, past as f64),
            None,
            "only MAX_ROWS rows are drawn, so only MAX_ROWS are hittable"
        );
        // And outside the panel is never a row, however the rows line up.
        assert_eq!(row_at_geom(PANEL, row0, 0, 20, 500.0, 130.0), None);
        assert_eq!(row_at_geom(PANEL, row0, 0, 20, -1.0, -1.0), None);
        assert_eq!(row_at_geom(PANEL, row0, 0, 0, 200.0, 130.0), None);
    }

    // ── the wheel ────────────────────────────────────────────────────────────

    #[test]
    fn the_wheel_stops_at_both_ends_of_the_list() {
        let rows = 20;
        let max_top = rows - MAX_ROWS;
        let (top, _) = scroll_top(0, 0.0, WHEEL_NOTCH, rows);
        assert_eq!(top, 1);
        let (top, _) = scroll_top(0, 0.0, WHEEL_NOTCH * 5.0, rows);
        assert_eq!(top, 5);
        // Past the end it stops, and does not wrap or overflow.
        let (top, _) = scroll_top(0, 0.0, WHEEL_NOTCH * 100.0, rows);
        assert_eq!(top, max_top);
        let (top, _) = scroll_top(max_top, 0.0, -WHEEL_NOTCH * 100.0, rows);
        assert_eq!(top, 0);
        // A list that fits needs no scrolling at all.
        for r in 0..=MAX_ROWS {
            assert_eq!(scroll_top(0, 0.0, WHEEL_NOTCH * 3.0, r).0, 0, "{r} rows");
        }
    }

    #[test]
    fn fractions_of_a_notch_add_up_instead_of_being_dropped() {
        // A high-resolution wheel sends a fraction per event; throwing the
        // remainder away would make a slow scroll never move at all.
        let rows = 40;
        let (mut top, mut acc) = (0usize, 0.0);
        for _ in 0..4 {
            (top, acc) = scroll_top(top, acc, WHEEL_NOTCH / 4.0, rows);
        }
        assert_eq!(top, 1, "four quarter-notches are one notch");
        // Half a notch each way nets out to no movement.
        let (t2, _) = scroll_top(top, acc, WHEEL_NOTCH / 2.0, rows);
        assert_eq!(t2, top);
    }

    #[test]
    fn holding_the_wheel_against_an_end_builds_up_no_charge() {
        // Otherwise the leftover grows for as long as the wheel turns, and the
        // first flick the other way unwinds all of it at once.
        let rows = 20;
        let (mut top, mut acc) = (0usize, 0.0);
        for _ in 0..50 {
            (top, acc) = scroll_top(top, acc, -WHEEL_NOTCH, rows);
        }
        assert_eq!(top, 0);
        assert!(acc.abs() <= WHEEL_NOTCH, "leftover grew to {acc}");
        let (top, _) = scroll_top(top, acc, WHEEL_NOTCH, rows);
        assert_eq!(top, 1, "the first notch back must move exactly one row");

        // One big flick against the end is the case that actually builds a
        // charge: ten notches' worth at the top, then one notch back. Without
        // the bound the leftover eats that notch and the list does not move.
        let (top, acc) = scroll_top(0, 0.0, -WHEEL_NOTCH * 10.0, rows);
        assert_eq!(top, 0);
        let (top, _) = scroll_top(top, acc, WHEEL_NOTCH, rows);
        assert_eq!(top, 1, "a notch after a big flick must still move a row");
        // Same at the far end.
        let (top, acc) = scroll_top(0, 0.0, WHEEL_NOTCH * 50.0, rows);
        assert_eq!(top, rows - MAX_ROWS);
        let (top, _) = scroll_top(top, acc, -WHEEL_NOTCH, rows);
        assert_eq!(top, rows - MAX_ROWS - 1);
    }

    #[test]
    fn a_nonsense_wheel_value_does_not_poison_the_accumulator() {
        // One NaN carried in `acc` would make every later comparison false and
        // the wheel dead for the rest of the session.
        let rows = 20;
        let (top, acc) = scroll_top(3, 0.0, f64::NAN, rows);
        assert_eq!(top, 3);
        assert_eq!(acc, 0.0);
        let (top, acc) = scroll_top(3, 0.0, f64::INFINITY, rows);
        assert_eq!((top, acc), (3, 0.0));
        let (top, acc) = scroll_top(3, 0.0, f64::NEG_INFINITY, rows);
        assert_eq!((top, acc), (3, 0.0));
        // And the wheel still works afterwards.
        assert_eq!(scroll_top(top, acc, WHEEL_NOTCH, rows).0, 4);
        // An absurd but finite value saturates at the ends instead of wrapping.
        assert_eq!(scroll_top(0, 0.0, 1e300, rows).0, rows - MAX_ROWS);
        assert_eq!(scroll_top(rows - MAX_ROWS, 0.0, -1e300, rows).0, 0);
    }

    #[test]
    fn a_scroll_position_past_the_end_is_brought_back_in() {
        // The list shrinks under the wheel when a filter is typed, so `top` can
        // arrive pointing at rows that no longer exist.
        let (top, _) = scroll_top(30, 0.0, 0.0, 12);
        assert_eq!(top, 12 - MAX_ROWS);
        let (top, _) = scroll_top(30, 0.0, WHEEL_NOTCH, 12);
        assert_eq!(top, 12 - MAX_ROWS);
        let (top, _) = scroll_top(30, 0.0, -WHEEL_NOTCH, 12);
        assert_eq!(top, 12 - MAX_ROWS - 1);
        // A list that no longer overflows scrolls back to the top.
        assert_eq!(scroll_top(9, 0.0, 0.0, 3).0, 0);
        assert_eq!(scroll_top(9, 0.0, 0.0, 0).0, 0);
    }

    // ── Super+D ──────────────────────────────────────────────────────────────

    #[test]
    fn super_d_shows_the_desktop_and_then_puts_it_back() {
        // Nothing minimised, or only some of it: minimise. Everything already
        // minimised is the only state that restores, so a session with one
        // window still up minimises that one instead of un-minimising the rest.
        assert_eq!(toggle_desktop_action(&[false, false]), Some(false));
        assert_eq!(toggle_desktop_action(&[true, false, true]), Some(false));
        assert_eq!(toggle_desktop_action(&[true, true]), Some(true));
        assert_eq!(toggle_desktop_action(&[false]), Some(false));
        assert_eq!(toggle_desktop_action(&[true]), Some(true));
        // No windows: nothing to do, and no requests sent.
        assert_eq!(toggle_desktop_action(&[]), None);
    }

    #[test]
    fn the_minimised_flag_is_read_out_of_the_packed_state_array() {
        let min = TOPLEVEL_STATE_MINIMIZED.to_ne_bytes();
        let other = 4u32.to_ne_bytes();
        assert!(state_array_has(&min, TOPLEVEL_STATE_MINIMIZED));
        assert!(!state_array_has(&other, TOPLEVEL_STATE_MINIMIZED));
        assert!(!state_array_has(&[], TOPLEVEL_STATE_MINIMIZED));
        // Anywhere in the array, not just first: labwc sends activated,
        // maximized and minimized in whatever order it has them.
        let mut many = Vec::new();
        many.extend_from_slice(&other);
        many.extend_from_slice(&2u32.to_ne_bytes());
        many.extend_from_slice(&min);
        assert!(state_array_has(&many, TOPLEVEL_STATE_MINIMIZED));
        // A trailing partial value is ignored, not read past the end.
        let mut ragged = many.clone();
        ragged.extend_from_slice(&[0, 0, 0]);
        assert!(state_array_has(&ragged, TOPLEVEL_STATE_MINIMIZED));
        assert!(!state_array_has(&[1, 0, 0], TOPLEVEL_STATE_MINIMIZED));
    }

    // ── the matcher ──────────────────────────────────────────────────────────

    #[test]
    fn prefix_matches_come_before_substring_matches() {
        let items = vec![
            item("Text Editor", "gedit"),
            item("Editor", "kate"),
            item("Firefox", "firefox"),
        ];
        // "edit" starts "Editor" and only appears inside "Text Editor", so the
        // exact word the user is typing comes first.
        assert_eq!(matches(&items, "edit"), vec![1, 0]);
        assert_eq!(matches(&items, "fire"), vec![2]);
        // Each group keeps discovery order, so the builtins stay on top.
        let items = vec![item("Aa", "x"), item("Ab", "y"), item("zAa", "z")];
        assert_eq!(matches(&items, "a"), vec![0, 1, 2]);
    }

    #[test]
    fn the_name_key_does_not_run_into_the_command() {
        // `key` is "<name> <exec>", so a prefix test against it would match a
        // filter that spans the two. The name has its own normalised field.
        let items = vec![item("ab", "cd")];
        assert_eq!(items[0].name_key, "ab");
        assert_eq!(items[0].key, "ab cd");
        // Prefix of the name: a head match.
        assert_eq!(matches(&items, "ab"), vec![0]);
        // Spans into the command: the substring group takes it, and it must NOT
        // be reported as a name prefix -- which only shows up in the order. Item
        // 1's name really does start with "ab c", so it has to come first; a
        // prefix test against `key` would put item 0 ahead of it because
        // "ab cd" starts with "ab c" too.
        assert!(!items[0].name_key.starts_with("ab c"));
        let two = vec![item("ab", "cd"), item("ab cx", "z")];
        assert_eq!(
            matches(&two, "ab c"),
            vec![1, 0],
            "a real name prefix must outrank a match that spans into the command"
        );
        assert_eq!(matches(&items, "ab c"), vec![0]);
        // And the command alone still finds it, which is what lets someone type
        // the binary name of an app whose title they do not remember.
        assert_eq!(matches(&items, "cd"), vec![0]);
    }

    #[test]
    fn the_filter_ignores_case_and_accents_like_the_panels_menu() {
        // Same `norm_key` the panel's application menu uses; typing "camara"
        // has to find "Cámara" or the runner is useless in Spanish.
        let items = vec![item("Cámara", "cheese"), item("Terminal", "sh")];
        assert_eq!(matches(&items, "camara"), vec![0]);
        assert_eq!(matches(&items, "CÁMARA"), vec![0]);
        assert_eq!(matches(&items, "cám"), vec![0]);
        assert_eq!(matches(&items, "TERM"), vec![1]);
        // An empty or blank filter matches everything, in order.
        assert_eq!(matches(&items, ""), vec![0, 1]);
        assert_eq!(matches(&items, "zzz"), Vec::<usize>::new());
    }

    #[test]
    fn a_row_shows_the_commands_basename_and_not_its_path_or_arguments() {
        assert_eq!(
            cmd_basename("/usr/local/bin/eclipse-terminal -e sh"),
            "eclipse-terminal"
        );
        assert_eq!(cmd_basename("firefox %U"), "firefox");
        assert_eq!(cmd_basename("  sh -c 'x'  "), "sh");
        assert_eq!(cmd_basename(""), "");
        assert_eq!(cmd_basename("/"), "");
        assert_eq!(cmd_basename("labwc --reconfigure"), "labwc");
    }

    // ── the typed command row ────────────────────────────────────────────────

    #[test]
    fn a_typed_command_is_looked_for_in_each_non_empty_path_component() {
        let seen = std::cell::RefCell::new(Vec::new());
        let probe = |p: &std::path::Path| {
            seen.borrow_mut().push(p.to_string_lossy().into_owned());
            p.ends_with("top")
        };
        assert!(runnable_in("top", "/bin:/usr/bin", probe));
        assert_eq!(seen.borrow().as_slice(), ["/bin/top"]);
        // Only the FIRST word is the program; the rest are its arguments.
        seen.borrow_mut().clear();
        assert!(runnable_in("top -b -n1", "/bin", probe));
        assert_eq!(seen.borrow().as_slice(), ["/bin/top"]);
        // Nothing typed is not a command.
        assert!(!runnable_in("", "/bin", probe));
        assert!(!runnable_in("   ", "/bin", probe));
    }

    #[test]
    fn an_empty_path_component_is_not_the_current_directory() {
        // POSIX reads an empty component as ".", so without the filter the
        // answer depended on where the panel was started from -- and a `./top`
        // in someone's home would shadow the real one.
        let probe = |p: &std::path::Path| p == std::path::Path::new("top");
        assert!(!runnable_in("top", ":/usr/bin", probe));
        assert!(!runnable_in("top", "/usr/bin::", probe));
        assert!(!runnable_in("top", "", probe));
        // A path with a slash IS looked up as written, which is how an absolute
        // command someone types by hand still runs.
        assert!(runnable_in("./top", "", |p: &std::path::Path| p
            == std::path::Path::new("./top")));
        assert!(runnable_in("/sbin/ip", "", |p: &std::path::Path| p
            == std::path::Path::new("/sbin/ip")));
    }

    #[test]
    fn the_runner_and_the_menu_look_on_the_same_path() {
        // A command the menu's TryExec accepts and the runner rejects (or the
        // other way round) is a bug nobody can explain from either side. There
        // used to be two lists; `/sbin` was in one of them.
        let dirs: Vec<&str> = lunarbar::apps::DEFAULT_PATH.split(':').collect();
        for d in ["/usr/local/bin", "/bin", "/usr/bin", "/sbin", "/usr/sbin"] {
            assert!(dirs.contains(&d), "{d} is missing from DEFAULT_PATH");
        }
        let probe = |p: &std::path::Path| p.starts_with("/sbin");
        assert!(
            runnable_in("ip", lunarbar::apps::DEFAULT_PATH, probe),
            "a /sbin binary must be runnable"
        );
    }

    // ── the key table ────────────────────────────────────────────────────────

    #[test]
    fn the_page_keys_are_bound_to_the_step_that_stops() {
        // The regression's other half: `move_selection` can be right and the
        // handler still hand it Wrap. This is the table that decides it.
        assert_eq!(
            key_action(KEY_PGUP_WL),
            Action::Move(-(MAX_ROWS as i32), Step::Clamp)
        );
        assert_eq!(
            key_action(KEY_PGDN_WL),
            Action::Move(MAX_ROWS as i32, Step::Clamp)
        );
        // And the arrows are the ones that wrap.
        assert_eq!(key_action(KEY_UP_WL), Action::Move(-1, Step::Wrap));
        assert_eq!(key_action(KEY_DOWN_WL), Action::Move(1, Step::Wrap));
        // Tab moves down like KRunner's, rather than typing a tab into the
        // filter, which is what an unbound key would do.
        assert_eq!(key_action(KEY_TAB_WL), Action::Move(1, Step::Wrap));
    }

    #[test]
    fn the_rest_of_the_key_table() {
        assert_eq!(key_action(KEY_ESC_WL), Action::Close);
        assert_eq!(key_action(KEY_ENTER_WL), Action::Launch);
        assert_eq!(key_action(KEY_KPENTER_WL), Action::Launch);
        assert_eq!(key_action(KEY_BACKSPACE_WL), Action::Backspace);
        // A printable key types; the navigation keys must not fall through to
        // `key_char` and end up in the filter.
        for k in [
            KEY_ESC_WL,
            KEY_ENTER_WL,
            KEY_KPENTER_WL,
            KEY_BACKSPACE_WL,
            KEY_UP_WL,
            KEY_DOWN_WL,
            KEY_TAB_WL,
            KEY_PGUP_WL,
            KEY_PGDN_WL,
        ] {
            assert!(
                !matches!(key_action(k), Action::Type(_)),
                "key {k} typed into the filter"
            );
        }
        // Something is bound to typing, or the search field can never be used.
        assert!((1..250).any(|k| matches!(key_action(k), Action::Type(_))));
    }

    // ── the command line ─────────────────────────────────────────────────────

    fn cli(args: &[&str]) -> Option<Cli> {
        parse_args(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn dump_without_a_spec_is_a_usage_error_and_not_an_overlay() {
        // It used to leave `dump` as None and fall through to the compositor
        // path, so a forgotten spec opened the launcher on the user's screen.
        assert_eq!(cli(&["--dump"]), None);
        // Nor does it swallow the next flag as the spec.
        assert_eq!(cli(&["--dump", "--toggle-desktop"]), None);
        assert_eq!(
            cli(&["--dump", "/tmp/f.raw:800x600"]),
            Some(Cli {
                toggle_desktop: false,
                dump: Some("/tmp/f.raw:800x600".into()),
                filter: String::new(),
            })
        );
    }

    #[test]
    fn the_rest_of_the_command_line() {
        assert_eq!(cli(&[]), Some(Cli::default()));
        assert!(cli(&["--toggle-desktop"]).unwrap().toggle_desktop);
        assert_eq!(cli(&["-h"]), None);
        assert_eq!(cli(&["--help"]), None);
        assert_eq!(cli(&["--nope"]), None);
        // A bare word prefills the field; SEVERAL words are joined, not
        // replaced -- `lunarrun text editor` used to search for "editor" alone.
        assert_eq!(cli(&["firefox"]).unwrap().filter, "firefox");
        assert_eq!(cli(&["text", "editor"]).unwrap().filter, "text editor");
        // And a filter beside a flag still reaches the field.
        let c = cli(&["--toggle-desktop", "top"]).unwrap();
        assert!(c.toggle_desktop && c.filter == "top");
    }

    // ── the two render paths agree ────────────────────────────────────────────

    #[test]
    fn the_dump_and_the_surface_apply_the_same_ceiling() {
        // This is the whole point of `check_buffer` living in the lib: the
        // offline render used to accept 16384 -- twice the surface's limit --
        // and then abort inside an `expect` instead of declining.
        assert_eq!(
            dump_target("/tmp/f.raw:16384x16384"),
            Err(fill_guard::TooBig::Dim)
        );
        assert_eq!(surface_size(16384, 16384), None);
        // What both accept.
        assert_eq!(
            dump_target("/tmp/f.raw:1920x1080"),
            Ok(("/tmp/f.raw".to_string(), 1920, 1080))
        );
        assert_eq!(surface_size(1920, 1080), Some((1920, 1080)));
        // The floor: a tiny request still renders something to look at, and a
        // compositor's 0 means "you choose" rather than a zero-byte pool.
        assert_eq!(
            dump_target("f:1x1"),
            Ok(("f".to_string(), MIN_DUMP_W, MIN_DUMP_H))
        );
        assert_eq!(
            dump_target("f:0x0"),
            Ok(("f".to_string(), MIN_DUMP_W, MIN_DUMP_H))
        );
        let (w, h) = surface_size(0, 0).expect("a zero configure gets a default");
        assert!(w >= MIN_DUMP_W as u32 && h >= MIN_DUMP_H as u32);
        assert_eq!(surface_size(1920, 0), Some((1920, h)));
        assert_eq!(surface_size(0, 1080), Some((w, 1080)));
        // A spec with no size at all still renders at the default.
        assert_eq!(
            dump_target("/tmp/f.raw"),
            Ok(("/tmp/f.raw".to_string(), 1280, 720))
        );
    }

    #[test]
    fn the_runner_falls_back_to_the_same_path_the_menu_does() {
        // With `$PATH` unset or empty, both sides have to look in the same
        // places or an entry shows in one and not the other.
        assert_eq!(search_path(None), lunarbar::apps::DEFAULT_PATH);
        assert_eq!(
            search_path(Some(String::new())),
            lunarbar::apps::DEFAULT_PATH
        );
        assert_eq!(search_path(Some("/opt/bin".into())), "/opt/bin");
        // The fallback really does cover /sbin, which is where a busybox image
        // keeps a great many of its commands.
        assert!(search_path(None).split(':').any(|d| d == "/sbin"));
    }

    // ── source-order invariants ──────────────────────────────────────────────

    #[test]
    fn the_dump_path_returns_before_any_attempt_to_reach_a_compositor() {
        // `--dump` is the offline check: it must render with no WAYLAND_DISPLAY,
        // because that is the only way this overlay gets looked at in CI. If the
        // connect moves above it, `--dump` starts exiting 1 on a headless box
        // and the check silently stops checking. `main` cannot be called from a
        // test, so the order is read out of the source.
        let src = include_str!("lunarrun.rs");
        let main_at = src.find("\nfn main() {").expect("fn main");
        let body = &src[main_at..];
        let dump = body
            .find("if let Some(spec) = dump")
            .expect("the dump block");
        let connect = body
            .find("Connection::connect_to_env")
            .expect("the connect");
        assert!(
            dump < connect,
            "the --dump block must come before the compositor connect"
        );
        // And it must still end in a `return`, not fall through into it.
        assert!(body[dump..connect].contains("        return;"));
    }

    #[test]
    fn the_look_is_read_once_and_only_once() {
        // `Look::current()` reads /etc/eclipse/look, and `eclipse-look` rewrites
        // that file when the user switches looks. Two calls could return two
        // different looks and leave the palette from one beside the geometry of
        // the other -- KDE's colours with Windows 11's placement. `new_state`
        // builds a `State` full of Wayland handles, so the source is what can be
        // checked.
        let src = include_str!("lunarrun.rs");
        let code = src.split("\n// \u{2500}\u{2500} tests").next().unwrap();
        // Comment lines dropped, so explaining the rule does not break it.
        let calls = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("Look::current()"))
            .count();
        assert_eq!(calls, 1, "the look must be read exactly once per process");
    }

    #[test]
    fn the_pool_is_sized_by_the_one_function_that_knows_the_protocols_limits() {
        // `create_pool` and `create_buffer` take their sizes as i32, so an
        // open-coded `w * 4 * h * BUFFERS` that forgets the ceiling hands the
        // compositor a negative number and the client is killed. The panel's
        // three bars and this overlay all went through their own copy of that
        // arithmetic; now there is one. `configure` needs a live wl_shm, so the
        // source is what can be checked.
        let src = include_str!("lunarrun.rs");
        let code = src.split("\n// \u{2500}\u{2500} tests").next().unwrap();
        assert_eq!(
            code.matches("pool_geometry(").count(),
            1,
            "the pool must be sized in exactly one place"
        );
        let hand_rolled: Vec<&str> = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("* 4 *") || l.contains("as usize * 4"))
            .collect();
        assert!(
            hand_rolled.is_empty(),
            "the pool size is being computed by hand again: {hand_rolled:?}"
        );
    }

    #[test]
    fn every_toplevel_is_tracked_through_the_bounded_push() {
        // A compositor that never sends `Closed` would otherwise grow this Vec
        // for as long as the session lives, which is the class of leak
        // `fill_guard` exists for. The push happens inside a Dispatch impl that
        // needs live Wayland objects, so the source is what can be checked.
        let src = include_str!("lunarrun.rs");
        let code = src.split("\n// \u{2500}\u{2500} tests").next().unwrap();
        assert!(
            code.contains("fill_guard::try_push_bounded(\n                &mut st.toplevels,"),
            "the toplevel push must go through try_push_bounded"
        );
        assert!(
            !code.contains("toplevels.push("),
            "an unbounded toplevels.push() is back"
        );
    }

    // ── the palettes ─────────────────────────────────────────────────────────

    /// Every look there is. The match below is exhaustive on purpose: a
    /// fourth variant stops this compiling, which is the point, because the
    /// palette tests walk this list and a look missing from it would go
    /// unchecked in exactly the silent way a palette bug already is.
    fn every_look() -> [Look; 3] {
        let all = [Look::Win11, Look::Kde, Look::Eclipse];
        for look in all {
            match look {
                Look::Win11 | Look::Kde | Look::Eclipse => {}
            }
        }
        all
    }

    /// How light a colour is, nought to seven hundred and sixty-five. Rough
    /// on purpose: these palettes are shades of one hue each, so the sum of
    /// the channels orders them the way a weighted luminance would.
    fn light((r, g, b): Rgb) -> u32 {
        r as u32 + g as u32 + b as u32
    }

    /// How much colour there is in it: nought for any grey, and the spread
    /// of the channels for anything else. This is what tells an accent from
    /// a hairline, since the two can be the same lightness.
    fn colourful((r, g, b): Rgb) -> u8 {
        r.max(g).max(b) - r.min(g).min(b)
    }

    /// Each look gets its OWN palette. A mis-wired row here dresses the
    /// launcher as another desktop, which reads as a setting that did not
    /// stick rather than as a bug, so nobody would ever report it.
    #[test]
    fn every_look_gets_its_own_palette() {
        for (i, a) in every_look().into_iter().enumerate() {
            for b in every_look().into_iter().skip(i + 1) {
                assert!(
                    !std::ptr::eq(palette(a), palette(b)),
                    "{a:?} and {b:?} share a palette"
                );
                assert_ne!(
                    palette(a).panel,
                    palette(b).panel,
                    "{a:?} and {b:?} paint the same panel"
                );
            }
        }
    }

    /// Each palette has to match the desktop it imitates, because that is
    /// its whole job, and the colours below are the ones its own doc comment
    /// cites from Breeze Dark and from Windows 11 dark. The roles cross over
    /// easily -- panel and field are both greys, text and dim are both near
    /// whites -- and crossed over they still LOOK like a palette, so nothing
    /// short of naming them catches it.
    #[test]
    fn each_palette_matches_the_desktop_it_imitates() {
        // KDE Breeze Dark: window, view, text, inactive text, selection.
        let kde = palette(Look::Kde);
        assert_eq!(kde.panel, (0x2a, 0x2e, 0x32), "Breeze Dark's window grey");
        assert_eq!(kde.field, (0x1b, 0x1e, 0x20), "Breeze Dark's view grey");
        assert_eq!(kde.text, (0xfc, 0xfc, 0xfc), "Breeze Dark's text");
        assert_eq!(kde.dim, (0x7f, 0x8c, 0x8d), "Breeze Dark's inactive text");
        assert_eq!(kde.sel, (0x3d, 0xae, 0xe9), "Breeze Dark's selection blue");
        assert_eq!(kde.accent, kde.sel, "and KDE accents with its selection");

        // Windows 11 dark: flyout grey, secondary text, accent.
        let win = palette(Look::Win11);
        assert_eq!(win.panel, (0x2b, 0x2b, 0x2b), "the Windows 11 flyout grey");
        assert_eq!(win.dim, (0xc5, 0xc5, 0xc5), "its secondary text");
        assert_eq!(win.accent, (0x00, 0x78, 0xd4), "its accent");
        assert_eq!(win.sel, win.accent, "and it selects with the accent");
        assert!(
            light(win.field) < light(win.panel),
            "its well sinks into the flyout"
        );

        // Eclipse's own. Note that it is the one look whose field is LIGHTER
        // than its panel, against what the field's doc comment says: the
        // panel is blue-black and the well is the lighter blue above it.
        let ecl = palette(Look::Eclipse);
        assert_eq!(ecl.panel, (0x0b, 0x12, 0x20), "Eclipse's blue-black panel");
        assert_eq!(ecl.accent, (0x6e, 0xa8, 0xff), "Eclipse's own blue");
        assert!(
            light(ecl.field) > light(ecl.panel),
            "and its well is the lighter blue, not a darker one"
        );
        assert_ne!(ecl.sel, ecl.accent, "and it selects darker than it accents");
    }

    /// What every look has to agree on, whatever its colours are. Each of
    /// these is a pair of roles that reads as the other role when the two
    /// are crossed over, and all three palettes keep all of them.
    #[test]
    fn every_palette_keeps_the_roles_its_colours_are_for() {
        for look in every_look() {
            let p = palette(look);
            // Live text is brighter than inactive text. Crossed over, the
            // greyed-out half of a row is the half that stands out.
            assert!(
                light(p.text) > light(p.dim),
                "{look:?}: text {:?} is not brighter than dim {:?}",
                p.text,
                p.dim
            );
            // The accent is a colour and the border is a hairline. In two of
            // the three looks they are the same lightness, so lightness is
            // no help: what separates them is that one has a hue.
            assert!(
                colourful(p.accent) > colourful(p.border),
                "{look:?}: accent {:?} is no more coloured than border {:?}",
                p.accent,
                p.border
            );
            // The scrim DIMS the desktop, it does not hide it. Half opaque
            // and up, the wallpaper is gone and this is a different window.
            assert!(
                p.scrim_a > 0.0 && p.scrim_a < 0.5,
                "{look:?}: a scrim at {} is not a dim",
                p.scrim_a
            );
            // And the scrim is the darkest thing in the palette, being the
            // ground that the panel and its wells float on.
            assert!(
                light(p.scrim) < light(p.panel) && light(p.scrim) < light(p.field),
                "{look:?}: the scrim is not the darkest of the three"
            );
            assert_ne!(
                p.panel, p.field,
                "{look:?}: the well does not read as a well"
            );
        }
    }
}

#[cfg(test)]
mod builtin_row_tests {
    use super::*;

    const LANGS: [Lang; 2] = [Lang::Es, Lang::En];

    /// The row named by its icon, which is the one field that does not change
    /// with the language.
    fn by_icon<'a>(items: &'a [Item], icon: &str) -> &'a Item {
        items
            .iter()
            .find(|it| it.icon.as_deref() == Some(icon))
            .unwrap_or_else(|| panic!("no row with the icon {icon}"))
    }

    #[test]
    fn the_builtin_rows_are_the_five_the_launcher_promises() {
        // The table nobody walked: five rows, each with a name, a command and
        // an icon, and the only thing above them in the list is nothing. A row
        // dropped here is an action the launcher silently stops offering.
        for lang in LANGS {
            let items = builtin_items("xterm", lang);
            assert_eq!(items.len(), 5, "{lang:?}");
            // In this order, because the list is shown in it and the first row
            // is the one Enter runs on an empty filter: the terminal, never a
            // power action.
            let icons: Vec<&str> = items
                .iter()
                .map(|it| it.icon.as_deref().expect("every builtin row has an icon"))
                .collect();
            assert_eq!(
                icons,
                [
                    "utilities-terminal",
                    "view-refresh",
                    "system-log-out",
                    "system-reboot",
                    "system-shutdown",
                ],
                "{lang:?}"
            );
            // All of them are applications and none is the synthesised
            // "run what was typed" row, which `matches` skips: a builtin
            // marked `Command` would never show up in any result list.
            for it in &items {
                assert_eq!(it.kind, Kind::App, "{:?} is not an App row", it.name);
                assert!(!it.name.is_empty(), "a builtin row has no name");
                assert!(!it.exec.is_empty(), "{:?} runs nothing", it.name);
            }
        }
    }

    #[test]
    fn no_two_builtin_rows_share_a_name_an_icon_or_a_command() {
        // What a copy-pasted row leaves behind, and the only thing that catches
        // it: two rows reading the same are two rows where one of the actions
        // has silently become the other. Checked per field, because a duplicate
        // command with its own name is the dangerous one -- "log out" that
        // reboots.
        for lang in LANGS {
            let items = builtin_items("xterm", lang);
            for (i, a) in items.iter().enumerate() {
                for b in &items[i + 1..] {
                    assert_ne!(a.name, b.name, "{lang:?}: two rows named {:?}", a.name);
                    assert_ne!(a.icon, b.icon, "{lang:?}: two rows with one icon");
                    assert_ne!(
                        a.exec, b.exec,
                        "{lang:?}: {:?} and {:?} run the same command",
                        a.name, b.name
                    );
                }
            }
        }
    }

    #[test]
    fn each_power_row_runs_the_action_it_is_named_after() {
        // The one mistake here that costs the user their session: a row wired
        // to another row's command. Anchored on the icon, which is the row's
        // identity in both languages, and asserted against the word the
        // command has to contain -- a `reboot` under "log out" would end the
        // session AND take the machine down with it.
        let items = builtin_items("xterm", Lang::En);
        let logout = &by_icon(&items, "system-log-out").exec;
        assert!(logout.contains("labwc --exit"), "log out: {logout}");
        assert!(
            !logout.contains("reboot") && !logout.contains("poweroff"),
            "log out takes the machine down: {logout}"
        );
        let reboot = &by_icon(&items, "system-reboot").exec;
        assert!(reboot.contains("reboot"), "reboot: {reboot}");
        assert!(
            !reboot.contains("poweroff"),
            "reboot powers off instead: {reboot}"
        );
        // The reload row only talks to the compositor: it must not be able to
        // end the session, which is the row right under it.
        let reload = &by_icon(&items, "view-refresh").exec;
        assert!(reload.contains("--reconfigure"), "reload: {reload}");
        for bad in ["--exit", "killall", "pkill", "reboot", "poweroff"] {
            assert!(!reload.contains(bad), "the reload row can {bad}: {reload}");
        }
    }

    #[test]
    fn a_power_row_falls_back_rather_than_doing_nothing() {
        // Each command is a `||` chain because the image does not guarantee
        // which of the tools is there: a single `poweroff` that is missing
        // leaves the row doing nothing at all, with the launcher already
        // closed and no word on screen. What has to hold is that every chain
        // has somewhere to fall to.
        let items = builtin_items("xterm", Lang::En);
        for icon in ["system-log-out", "system-reboot", "system-shutdown"] {
            let exec = &by_icon(&items, icon).exec;
            assert!(
                exec.contains("||"),
                "{icon} has a single command and no fallback: {exec}"
            );
            // Every branch of the chain is a real command and not an empty
            // one, which a stray `||` would leave: `a || || b` runs a shell
            // syntax error instead of the action.
            for branch in exec.split("||") {
                assert!(
                    !branch.trim().is_empty(),
                    "{icon} has an empty branch: {exec}"
                );
            }
        }
    }

    #[test]
    fn the_terminal_row_runs_the_terminal_it_was_handed() {
        // Whatever `$TERMINAL` or the probe settled on, verbatim: the row
        // hard-coding one would launch a terminal that is not the image's, or
        // none at all. And its name is the only builtin that is NOT
        // translated, because "Terminal" is the same word in both.
        for cmd in ["xterm", "eclipse-terminal", "/usr/bin/foot -a x"] {
            for lang in LANGS {
                let items = builtin_items(cmd, lang);
                let term = by_icon(&items, "utilities-terminal");
                assert_eq!(term.exec, cmd, "{lang:?}");
                assert_eq!(term.name, "Terminal", "{lang:?}");
            }
        }
    }

    #[test]
    fn a_builtin_row_is_named_in_the_language_the_session_is_in() {
        // The names come from `i18n`, so they differ per language, and the
        // only thing that can go wrong invisibly is a row that forgot to ask:
        // a hard-coded English name in a Spanish session. The three that are
        // translated must actually differ between the two.
        let es = builtin_items("xterm", Lang::Es);
        let en = builtin_items("xterm", Lang::En);
        for icon in [
            "view-refresh",
            "system-log-out",
            "system-reboot",
            "system-shutdown",
        ] {
            assert_ne!(
                by_icon(&es, icon).name,
                by_icon(&en, icon).name,
                "{icon} reads the same in both languages"
            );
        }
        // And each name is the one `i18n` holds for that action, so the rows
        // cannot be shuffled against their labels.
        for lang in LANGS {
            let items = builtin_items("xterm", lang);
            assert_eq!(by_icon(&items, "view-refresh").name, lang.run_reload());
            assert_eq!(by_icon(&items, "system-log-out").name, lang.power_logout());
            assert_eq!(by_icon(&items, "system-reboot").name, lang.power_reboot());
            assert_eq!(
                by_icon(&items, "system-shutdown").name,
                lang.power_shutdown()
            );
        }
    }

    #[test]
    fn a_builtin_row_is_findable_by_typing_its_name() {
        // The rows are only reachable through `matches`, which tests the
        // normalised name and the normalised name-plus-command. A row whose
        // keys were not built from its own name and exec is a row the filter
        // can never bring up -- it is in the list and unreachable.
        for lang in LANGS {
            let items = builtin_items("xterm", lang);
            for (i, it) in items.iter().enumerate() {
                // Its whole name brings up that row, and as a prefix match: it
                // has to be in the first group, not behind every application
                // whose command happens to contain the same letters.
                let hits = matches(&items, &it.name);
                assert_eq!(
                    hits.first(),
                    Some(&i),
                    "{lang:?}: typing {:?} does not bring up its own row first",
                    it.name
                );
                // And its command does too, through the substring test.
                let word = it.exec.split_whitespace().next().unwrap();
                assert!(
                    matches(&items, word).contains(&i),
                    "{lang:?}: {:?} is not findable by its command {word:?}",
                    it.name
                );
            }
            // An empty filter shows every one of them, in order: that is the
            // list the launcher opens with.
            assert_eq!(matches(&items, ""), (0..items.len()).collect::<Vec<_>>());
        }
        // Accents are folded, which is what makes the Spanish rows typeable on
        // a layout the search field cannot produce them on: "cerrar sesion"
        // with no accent has to find "cerrar sesión".
        let es = builtin_items("xterm", Lang::Es);
        let sesion = es
            .iter()
            .position(|it| it.icon.as_deref() == Some("system-log-out"))
            .expect("the log-out row");
        assert!(matches(&es, "cerrar sesion").contains(&sesion));
    }

    #[test]
    fn the_builtin_rows_come_before_the_applications() {
        // `build_items` appends `scan_apps` after them, and `matches` keeps
        // discovery order within each group: the builtins being first is what
        // puts "shut down" above whatever `.desktop` file also matches "shut".
        // Checked through the real `build_items` so the order of the two halves
        // is what is tested, not the table again.
        let all = build_items("xterm");
        let builtins = builtin_items("xterm", Lang::current());
        assert!(all.len() >= builtins.len());
        for (i, it) in builtins.iter().enumerate() {
            assert_eq!(all[i].name, it.name, "row {i} is not the builtin one");
            assert_eq!(all[i].exec, it.exec, "row {i} runs something else");
            assert_eq!(all[i].icon, it.icon, "row {i} wears another icon");
        }
    }
}
