//! lunarlock — Eclipse OS's lock screen, without a password.
//!
//! The power menu's "bloquear" used to run `eclipse-lock || swaylock`, and
//! neither exists on the image, so the button did nothing. This is the screen
//! it now opens: every output covered, a large clock with a meter of the
//! minute's seconds, the date, and since when the screen has been locked.
//!
//! It asks for no password, on purpose (Moebius, 2026-11-05: "me gusta la
//! pantalla de bloqueo pero sin contraseña"). So it is a curtain, not a
//! guard: it hides the desktop and takes the keyboard and the pointer, and a
//! key or a click brings the desktop back. That is also why it is a
//! wlr-layer-shell overlay and not an `ext-session-lock-v1` client: a
//! session lock whose client dies leaves the session locked, and with no
//! password there is nothing that lock would protect in exchange for that
//! risk.
//!
//! The first [`GRACE_MS`] after mapping ignore input, so the Super+L or the
//! click that opened it does not close it at once.
//!
//! Modes:
//! - default: lock every output until a key or a click.
//! - `--dump PATH:WxH`: render one screen to a raw ARGB8888 file and exit.
//!   `LUNARLOCK_TIME` (seconds since the epoch) fixes the clock for it.
//!
//! Env: `ECLIPSE_LOOK` to override `/etc/eclipse/look`.

use std::os::fd::{AsFd, AsRawFd};
use std::time::Instant;

use lunarbar::draw::{Canvas, Rgb, GLYPH_H};
use lunarbar::files::now_secs;
use lunarbar::fill_guard;
use lunarbar::i18n::Lang;
use lunarbar::look::Look;
use lunarbar::proc::{self, free_buffer, map_shm_pool, BUFFERS};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// Input in the first moments after mapping is the one that opened it.
const GRACE_MS: u128 = 600;

// ── palette ──────────────────────────────────────────────────────────────────

struct Palette {
    ground: Rgb,
    text: Rgb,
    dim: Rgb,
    accent: Rgb,
    hairline: Rgb,
    /// The tecno look's gold corner brackets; `None` elsewhere.
    gold: Option<Rgb>,
}

/// Mockup A: ground `#04070D`, text `#E6F1FF`, muted `#8AA4C2`, cyan
/// `#38E1FF`, hairline `#1D3550`, gold `#F2C14E`.
const PAL_TECNO: Palette = Palette {
    ground: (0x04, 0x07, 0x0d),
    text: (0xe6, 0xf1, 0xff),
    dim: (0x8a, 0xa4, 0xc2),
    accent: (0x38, 0xe1, 0xff),
    hairline: (0x1d, 0x35, 0x50),
    gold: Some((0xf2, 0xc1, 0x4e)),
};

const PAL_ECLIPSE: Palette = Palette {
    ground: (0x04, 0x07, 0x0e),
    text: (0xff, 0xff, 0xff),
    dim: (0x6d, 0x7f, 0xa3),
    accent: (0x6e, 0xa8, 0xff),
    hairline: (0x27, 0x5a, 0x9e),
    gold: None,
};

const PAL_KDE: Palette = Palette {
    ground: (0x1b, 0x1e, 0x20),
    text: (0xfc, 0xfc, 0xfc),
    dim: (0x7f, 0x8c, 0x8d),
    accent: (0x3d, 0xae, 0xe9),
    hairline: (0x4d, 0x4d, 0x4d),
    gold: None,
};

const PAL_WIN11: Palette = Palette {
    ground: (0x20, 0x20, 0x20),
    text: (0xff, 0xff, 0xff),
    dim: (0xc5, 0xc5, 0xc5),
    accent: (0x00, 0x78, 0xd4),
    hairline: (0x3d, 0x3d, 0x3d),
    gold: None,
};

fn palette(look: Look) -> &'static Palette {
    match look {
        Look::Tecno => &PAL_TECNO,
        Look::Eclipse => &PAL_ECLIPSE,
        Look::Kde => &PAL_KDE,
        Look::Win11 => &PAL_WIN11,
    }
}

// ── time ─────────────────────────────────────────────────────────────────────

/// A local moment, as the screen says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Clock {
    year: i32,
    /// 0 = January.
    month: usize,
    day: u32,
    /// 0 = Sunday.
    wday: usize,
    hour: u32,
    min: u32,
    sec: u32,
}

/// `t` (seconds since the epoch) in the local zone.
fn local(t: i64) -> Clock {
    let tt = t as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` it is handed, and an
    // all-zero `tm` is a valid value of that plain C struct.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    Clock {
        year: tm.tm_year + 1900,
        month: tm.tm_mon.clamp(0, 11) as usize,
        day: tm.tm_mday.max(1) as u32,
        wday: tm.tm_wday.clamp(0, 6) as usize,
        hour: tm.tm_hour.max(0) as u32,
        min: tm.tm_min.max(0) as u32,
        sec: tm.tm_sec.clamp(0, 59) as u32,
    }
}

/// Now, or `LUNARLOCK_TIME` when it is set (dumps and tests).
fn now() -> i64 {
    std::env::var("LUNARLOCK_TIME").ok().and_then(|v| v.trim().parse().ok()).unwrap_or_else(now_secs)
}

/// "jueves 5 de noviembre de 2026" / "Thursday 5 November 2026".
fn date_line(c: &Clock, lang: Lang) -> String {
    const ES_DAYS: [&str; 7] = ["domingo", "lunes", "martes", "miércoles", "jueves", "viernes", "sábado"];
    const EN_DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let month = lang.month_full()[c.month];
    match lang {
        Lang::Es => format!("{} {} de {month} de {}", ES_DAYS[c.wday], c.day, c.year),
        Lang::En => {
            let mut m = month.to_string();
            if let Some(f) = m.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            format!("{} {} {m} {}", EN_DAYS[c.wday], c.day, c.year)
        }
    }
}

fn hm(c: &Clock) -> String {
    format!("{:02}:{:02}", c.hour, c.min)
}

/// How long it has been locked: "hace 1 h 05 min" / "for 1 h 05 min".
fn locked_for(secs: i64, lang: Lang) -> String {
    let m = secs.max(0) / 60;
    let span = if m < 1 {
        return match lang {
            Lang::Es => "ahora mismo".into(),
            Lang::En => "just now".into(),
        };
    } else if m < 60 {
        format!("{m} min")
    } else {
        format!("{} h {:02} min", m / 60, m % 60)
    };
    match lang {
        Lang::Es => format!("hace {span}"),
        Lang::En => format!("for {span}"),
    }
}

fn user_at_host() -> String {
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "root".into());
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "eclipse".into());
    format!("{user}@{host}")
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// The clock's em for an output `h` pixels tall: a fifth of it, on the 4 px
/// grid, between 64 and 240.
fn clock_px(h: i32) -> i32 {
    ((h / 5) / 4 * 4).clamp(64, 240)
}

const MARGIN: i32 = 32;
const METER_H: i32 = 4;
const METER_GAP: i32 = 4;
const BRACKET_PAD: i32 = 32;
const BRACKET_LEN: i32 = 24;

/// What one frame says.
struct Face<'a> {
    pal: &'a Palette,
    lang: Lang,
    now: Clock,
    since: Clock,
    locked_secs: i64,
    who: &'a str,
}

/// The `[x0, x1)` of each of the 60 seconds of a meter `w` wide at `x`.
fn meter_cells(x: i32, w: i32) -> Vec<(i32, i32)> {
    // A narrow meter (the smallest clock) closes its gaps to a pixel.
    let gap = if w >= 60 * (2 + METER_GAP) { METER_GAP } else { 1 };
    let cell = ((w - 59 * gap) / 60).max(1);
    let used = 60 * cell + 59 * gap;
    let x = x + (w - used) / 2;
    (0..60).map(|i| (x + i * (cell + gap), x + i * (cell + gap) + cell)).collect()
}

fn draw(cv: &mut Canvas, w: i32, h: i32, f: &Face) {
    let p = f.pal;
    cv.clear(p.ground);

    // The clock, centred a little above the middle.
    let px = clock_px(h);
    let s = hm(&f.now);
    let cw = Canvas::px_advance(px) * s.chars().count() as i32;
    let ch = px * 6 / 5;
    let date = date_line(&f.now, f.lang);
    let date_w = Canvas::text_width(&date);
    let block_h = ch + 16 + METER_H + 20 + GLYPH_H;
    let x = (w - cw) / 2;
    let y = (h - block_h) / 2 - h / 16;
    cv.text_px(&s, x, y, px, p.text);

    // A meter of the minute's seconds: those gone lit, this one gold (or the
    // accent where there is no gold), those to come a hairline.
    let my = y + ch + 16;
    let now_c = p.gold.unwrap_or(p.text);
    for (i, (a, b)) in meter_cells(x, cw).into_iter().enumerate() {
        let sec = f.now.sec as usize;
        let (c, alpha) = match i.cmp(&sec) {
            std::cmp::Ordering::Less => (p.accent, 1.0),
            std::cmp::Ordering::Equal => (now_c, 1.0),
            std::cmp::Ordering::Greater => (p.hairline, 1.0),
        };
        cv.fill_rect_a(a, my, b - a, METER_H, c, alpha);
    }

    // The date, under the meter.
    let dy = my + METER_H + 20;
    cv.text(&date, (w - date_w) / 2, dy, p.dim);

    if let Some(gold) = p.gold {
        let bw = cw.max(date_w) + 2 * BRACKET_PAD;
        // The em's top is air above the digits: the box starts at the ink.
        let ink = y + px / 5;
        let bh = dy + GLYPH_H - ink + 2 * BRACKET_PAD;
        cv.brackets((w - bw) / 2, ink - BRACKET_PAD, bw, bh, BRACKET_LEN, 2, gold);
    }

    // Top left: the state, and since when.
    let lock_s = 16;
    cv.lock_icon(MARGIN, MARGIN, lock_s, p.accent);
    let state = match f.lang {
        Lang::Es => "BLOQUEADO",
        Lang::En => "LOCKED",
    };
    let tx = MARGIN + lock_s + 8;
    let ty = MARGIN + (lock_s - GLYPH_H) / 2;
    let adv = cv.text_bold(state, tx, ty, p.text);
    let since = match f.lang {
        Lang::Es => format!("desde las {} · {}", hm(&f.since), locked_for(f.locked_secs, f.lang)),
        Lang::En => format!("since {} · {}", hm(&f.since), locked_for(f.locked_secs, f.lang)),
    };
    cv.text(&since, tx + adv + 12, ty, p.dim);

    // Top right: whose session this is.
    let who_w = Canvas::text_width(f.who);
    cv.text(f.who, w - MARGIN - who_w, ty, p.dim);

    // Bottom: how to come back, over a hairline.
    let hint = match f.lang {
        Lang::Es => "pulsa una tecla o haz clic para volver",
        Lang::En => "press a key or click to return",
    };
    let hy = h - MARGIN - GLYPH_H;
    cv.hline(MARGIN, hy - 16, w - 2 * MARGIN, p.hairline, 1.0);
    cv.text(hint, (w - Canvas::text_width(hint)) / 2, hy, p.dim);
}

// ── wayland ──────────────────────────────────────────────────────────────────

/// One output's cover.
struct Screen {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    width: u32,
    height: u32,
    map: *mut u8,
    map_len: usize,
    buffers: [Option<wl_buffer::WlBuffer>; BUFFERS],
    busy: [bool; BUFFERS],
    next: usize,
    generation: u64,
    configured: bool,
}

impl Screen {
    fn release_pool(&mut self) {
        for b in self.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !self.map.is_null() && self.map_len > 0 {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        self.map = std::ptr::null_mut();
        self.map_len = 0;
    }
}

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: Vec<wl_output::WlOutput>,
    screens: Vec<Screen>,

    pal: &'static Palette,
    lang: Lang,
    who: String,
    locked_at: i64,
    mapped_at: Instant,
    /// The second last drawn, so the loop repaints once a second.
    shown: i64,
    done: bool,
}

impl State {
    fn new() -> Self {
        Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            seat: None,
            keyboard: None,
            pointer: None,
            outputs: Vec::new(),
            screens: Vec::new(),
            pal: palette(Look::current()),
            lang: Lang::current(),
            who: user_at_host(),
            locked_at: now(),
            mapped_at: Instant::now(),
            shown: 0,
            done: false,
        }
    }

    fn face(&self, t: i64) -> Face<'_> {
        Face {
            pal: self.pal,
            lang: self.lang,
            now: local(t),
            since: local(self.locked_at),
            locked_secs: t - self.locked_at,
            who: &self.who,
        }
    }

    /// Cover every output (or the one the compositor picks, when it named
    /// none) with an overlay that takes the keyboard.
    fn map_all(&mut self, qh: &QueueHandle<State>) {
        let (Some(comp), Some(ls)) = (self.compositor.clone(), self.layer_shell.clone()) else {
            return;
        };
        let outs: Vec<Option<wl_output::WlOutput>> =
            if self.outputs.is_empty() { vec![None] } else { self.outputs.iter().cloned().map(Some).collect() };
        for (i, out) in outs.into_iter().enumerate() {
            let surface = comp.create_surface(qh, ());
            let layer = ls.get_layer_surface(&surface, out.as_ref(), zwlr_layer_shell_v1::Layer::Overlay, "lock".into(), qh, i);
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_size(0, 0);
            layer.set_exclusive_zone(-1);
            // Only one surface can hold the keyboard; the first does.
            layer.set_keyboard_interactivity(if i == 0 {
                KeyboardInteractivity::Exclusive
            } else {
                KeyboardInteractivity::None
            });
            surface.commit();
            self.screens.push(Screen {
                surface,
                layer,
                width: 0,
                height: 0,
                map: std::ptr::null_mut(),
                map_len: 0,
                buffers: [None, None],
                busy: [false; BUFFERS],
                next: 0,
                generation: 0,
                configured: false,
            });
        }
        self.mapped_at = Instant::now();
    }

    fn configure(&mut self, qh: &QueueHandle<State>, i: usize, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let w = if w == 0 { 1280 } else { w };
        let h = if h == 0 { 720 } else { h };
        if fill_guard::check_buffer(w as usize, h as usize).is_err() {
            return;
        }
        let Some(sc) = self.screens.get_mut(i) else {
            return;
        };
        if !(sc.configured && sc.width == w && sc.height == h) {
            let Some(geom) = proc::pool_geometry(w, h) else {
                return;
            };
            let Some((map, fd)) = map_shm_pool(geom.total, "lunarlock") else {
                return;
            };
            sc.release_pool();
            sc.generation += 1;
            let pool = shm.create_pool(fd.as_fd(), geom.total as i32, qh, ());
            let gen = sc.generation;
            let mk = |b: usize| {
                pool.create_buffer(
                    (b * geom.frame_size) as i32,
                    w as i32,
                    h as i32,
                    geom.stride as i32,
                    wl_shm::Format::Argb8888,
                    qh,
                    (i, b, gen),
                )
            };
            sc.buffers = [Some(mk(0)), Some(mk(1))];
            pool.destroy();
            sc.map = map;
            sc.map_len = geom.total;
            sc.width = w;
            sc.height = h;
            sc.busy = [false; BUFFERS];
            sc.next = 0;
            sc.configured = true;
        }
        let t = now();
        self.paint(i, t);
    }

    fn paint(&mut self, i: usize, t: i64) {
        let face = self.face(t);
        let Some(sc) = self.screens.get(i) else {
            return;
        };
        if !sc.configured || sc.map.is_null() {
            return;
        }
        // A screen whose two buffers are both still held skips this second;
        // the next one repaints it.
        let Some(b) = free_buffer(sc.busy, sc.next) else {
            return;
        };
        let (w, h) = (sc.width as usize, sc.height as usize);
        let Some(mut cv) = Canvas::try_new(w, h) else {
            return;
        };
        cv.set_square(true);
        draw(&mut cv, w as i32, h as i32, &face);
        let frame = w * h * 4;
        let data: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(sc.map.add(b * frame), frame) };
        if !cv.blit_argb(data) {
            return;
        }
        let sc = &mut self.screens[i];
        if let Some(buf) = &sc.buffers[b] {
            sc.surface.attach(Some(buf), 0, 0);
            sc.surface.damage_buffer(0, 0, w as i32, h as i32);
            sc.surface.commit();
            sc.busy[b] = true;
            sc.next = 1 - b;
        }
    }

    /// Repaint every screen when the second has changed.
    fn tick(&mut self) {
        let t = now();
        if t == self.shown {
            return;
        }
        self.shown = t;
        for i in 0..self.screens.len() {
            self.paint(i, t);
        }
    }

    /// A key or a click: back to the desktop, unless it is the one that
    /// opened the screen.
    fn input(&mut self) {
        if self.mapped_at.elapsed().as_millis() >= GRACE_MS {
            self.done = true;
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        for mut sc in self.screens.drain(..) {
            sc.release_pool();
            sc.layer.destroy();
            sc.surface.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer_shell = Some(registry.bind(name, version.min(4), qh, ())),
                "wl_output" => {
                    if st.outputs.len() < fill_guard::MAX_OUTPUTS {
                        st.outputs.push(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "wl_seat" if st.seat.is_none() => st.seat = Some(registry.bind(name, version.min(5), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        st: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        &i: &usize,
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                st.configure(qh, i, width, height);
            }
            // An output went away: its cover goes with it, the rest stay.
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(sc) = st.screens.get_mut(i) {
                    sc.configured = false;
                }
                if st.screens.iter().all(|s| !s.configured) {
                    st.done = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (usize, usize, u64)> for State {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        &(i, b, gen): &(usize, usize, u64),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            if let Some(sc) = st.screens.get_mut(i) {
                if sc.generation == gen && b < BUFFERS {
                    sc.busy[b] = false;
                }
            }
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
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
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
        if let wl_keyboard::Event::Key { state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } = event {
            st.input();
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
        if let wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } = event {
            st.input();
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

fn dump(spec: &str) -> ! {
    let (path, dims) = spec.rsplit_once(':').unwrap_or((spec, "1920x1080"));
    let (w, h) = dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .unwrap_or((1920, 1080));
    let (w, h) = (w.max(320), h.max(240));
    if fill_guard::check_buffer(w, h).is_err() {
        eprintln!("lunarlock: {w}x{h} is too large");
        std::process::exit(1);
    }
    let mut st = State::new();
    // A dump shows a screen locked a while ago, so the "since" reads.
    st.locked_at = now() - 754;
    let Some(mut cv) = Canvas::try_new(w, h) else {
        std::process::exit(1);
    };
    cv.set_square(true);
    draw(&mut cv, w as i32, h as i32, &st.face(now()));
    let mut buf = vec![0u8; w * h * 4];
    if !cv.blit_argb(&mut buf) || std::fs::write(path, &buf).is_err() {
        eprintln!("lunarlock: could not write {path}");
        std::process::exit(1);
    }
    println!("lunarlock: wrote {path} ({w}x{h} ARGB8888)");
    std::process::exit(0);
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => {}
        Some("--dump") => dump(&args.next().unwrap_or_else(|| "lunarlock.raw".into())),
        Some(_) => {
            eprintln!("usage: lunarlock [--dump PATH:WxH]");
            std::process::exit(2);
        }
    }

    // One lock at a time: a second Super+L while locked is a no-op.
    let lock = std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()))
        .join("lunarlock.lock");
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock) else {
        eprintln!("lunarlock: cannot open {}", lock.display());
        std::process::exit(1);
    };
    // SAFETY: flock on a descriptor this process owns.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return;
    }

    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunarlock: no Wayland compositor found (is the session up?)");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = State::new();
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunarlock: wayland roundtrip failed");
            std::process::exit(1);
        }
    }
    if st.layer_shell.is_none() || st.shm.is_none() || st.compositor.is_none() {
        eprintln!("lunarlock: compositor lacks wlr-layer-shell/wl_shm");
        std::process::exit(1);
    }
    st.map_all(&qh);
    while !st.done {
        if queue.flush().is_err() {
            break;
        }
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // Wake a little after each second turns, to move the meter.
            let ms = 1000 - (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_millis())
                .unwrap_or(0) as i32)
                + 5;
            // SAFETY: one valid pollfd for the length of the call.
            if unsafe { libc::poll(&mut pfd, 1, ms) } > 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        if queue.dispatch_pending(&mut st).is_err() {
            break;
        }
        st.tick();
    }
    drop(st);
    let _ = conn.flush();
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(h: u32, m: u32, s: u32) -> Clock {
        Clock { year: 2026, month: 10, day: 5, wday: 4, hour: h, min: m, sec: s }
    }

    #[test]
    fn the_date_reads_the_way_each_language_writes_it() {
        assert_eq!(date_line(&clock(4, 58, 0), Lang::Es), "jueves 5 de noviembre de 2026");
        assert_eq!(date_line(&clock(4, 58, 0), Lang::En), "Thursday 5 November 2026");
        assert_eq!(hm(&clock(4, 8, 0)), "04:08");
    }

    #[test]
    fn how_long_it_has_been_locked() {
        assert_eq!(locked_for(20, Lang::Es), "ahora mismo");
        assert_eq!(locked_for(12 * 60 + 34, Lang::Es), "hace 12 min");
        assert_eq!(locked_for(65 * 60, Lang::En), "for 1 h 05 min");
    }

    #[test]
    fn the_meter_has_sixty_cells_inside_its_width_in_order() {
        for w in [200, 517, 1200] {
            let cells = meter_cells(100, w);
            assert_eq!(cells.len(), 60);
            assert!(cells[0].0 >= 100 && cells[59].1 <= 100 + w, "{w}: {cells:?}");
            assert!(cells.windows(2).all(|p| p[0].1 < p[1].0));
        }
    }

    #[test]
    fn the_clock_grows_with_the_screen_on_the_grid() {
        assert_eq!(clock_px(1080), 216);
        assert_eq!(clock_px(200), 64);
        assert_eq!(clock_px(4000), 240);
        assert!([480, 720, 1080, 1440].iter().all(|&h| clock_px(h) % 4 == 0));
    }

    #[test]
    fn every_look_draws_a_whole_screen_without_leaving_it() {
        for look in [Look::Tecno, Look::Eclipse, Look::Kde, Look::Win11] {
            for (w, h) in [(1920, 1080), (1280, 720), (640, 480)] {
                let mut cv = Canvas::new(w, h);
                let f = Face {
                    pal: palette(look),
                    lang: Lang::Es,
                    now: clock(23, 59, 59),
                    since: clock(23, 0, 0),
                    locked_secs: 3540,
                    who: "root@eclipse",
                };
                draw(&mut cv, w as i32, h as i32, &f);
                assert!(cv.width() as usize == w && cv.height() as usize == h);
            }
        }
    }
}
