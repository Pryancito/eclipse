//! lunarnotes — the notifications that came in, newest first (Super+N, or
//! the AVISOS chip on the bar).
//!
//! Every card the bar shows is written down (`lunarbar::notes`); this is where
//! one that came and went can still be read. The ones that arrived since you
//! last looked are marked, and opening the list is what counts as looking: the
//! bar's chip goes back to no number.
//!
//! ↑/↓ or the wheel scroll, Supr forgets everything, Esc or a click outside
//! the list closes, and Super+N again closes too.
//!
//! `--dump PATH:WxH` renders a made-up history to a raw ARGB8888 file.

use std::os::fd::{AsFd, AsRawFd};

use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::i18n::{Key, Lang};
use lunarbar::keys::{KEY_DOWN_WL, KEY_ESC_WL, KEY_PGDN_WL, KEY_PGUP_WL, KEY_UP_WL};
use lunarbar::notes::{self, Entry};
use lunarbar::proc::{self, map_shm_pool};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
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
const WARN: Rgb = (0xff, 0x8a, 0x5c);
const AMBER: Rgb = (0xf6, 0x74, 0x00);

const MARGIN: i32 = 48;
const PANEL_MAX_W: i32 = 720;
const HEAD_H: i32 = 48;
const ENTRY_H: i32 = 72;
const FOOT_H: i32 = 32;
const PAD: i32 = 12;
/// Between two lines of one entry.
const LINE: i32 = GLYPH_H + 4;

/// The evdev code of Delete (Supr on a Spanish keyboard).
const KEY_DELETE: u32 = 111;

// ── what is drawn ────────────────────────────────────────────────────────────

struct Scene<'a> {
    entries: &'a [Entry],
    /// Entries newer than this are new since the last look.
    seen: u64,
    now: u64,
    /// The first entry shown.
    top: usize,
    lang: Lang,
}

/// The panel: `(x, y, w, h)`, centred, as tall as the entries need.
fn panel(w: i32, h: i32, n: usize) -> (i32, i32, i32, i32) {
    let pw = PANEL_MAX_W.min(w - 2 * MARGIN).max(240.min(w));
    let room = (h - 2 * MARGIN - HEAD_H - FOOT_H).max(ENTRY_H);
    let fits = (room / ENTRY_H).max(1);
    let rows = (n as i32).clamp(1, fits);
    let ph = HEAD_H + rows * ENTRY_H + FOOT_H;
    ((w - pw) / 2, ((h - ph) / 2).max(0), pw, ph)
}

/// How many entries fit in the panel at once.
fn visible(w: i32, h: i32, n: usize) -> usize {
    let (_, _, _, ph) = panel(w, h, n);
    ((ph - HEAD_H - FOOT_H) / ENTRY_H).max(1) as usize
}

fn fit(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        return s.to_string();
    }
    let mut t: String = s.chars().take(cols.saturating_sub(1)).collect();
    t.push('…');
    t
}

fn sev_color(sev: &str) -> Rgb {
    match sev {
        "crit" => WARN,
        "warn" => AMBER,
        _ => CYAN,
    }
}

fn draw(cv: &mut Canvas, w: i32, h: i32, s: &Scene) {
    cv.fill_rect_a(0, 0, w, h, GROUND, 0.9);
    let (px, py, pw, ph) = panel(w, h, s.entries.len());
    cv.fill_rect_a(px, py, pw, ph, SURFACE, 1.0);
    for (x, y, rw, rh) in [
        (px, py, pw, 1),
        (px, py + ph - 1, pw, 1),
        (px, py, 1, ph),
        (px + pw - 1, py, 1, ph),
    ] {
        cv.fill_rect_a(x, y, rw, rh, HAIRLINE, 1.0);
    }
    cv.brackets(px, py, pw, ph, 16, 2, GOLD);

    let new = notes::unseen(s.entries, s.seen);
    let (title, empty, keys, fresh) = (
        s.lang.s(Key::notes_notifications),
        s.lang.s(Key::notes_no_notifications_yet),
        s.lang.s(Key::notes_keys),
        s.lang.s(Key::notes_new),
    );
    let hy = py + (HEAD_H - GLYPH_H) / 2;
    cv.text_bold(title, px + PAD + 4, hy, CYAN);
    let count = if new > 0 {
        format!("{} · {new} {fresh}", s.entries.len())
    } else {
        s.entries.len().to_string()
    };
    let cw = Canvas::text_width(&count);
    cv.text(
        &count,
        px + pw - PAD - 4 - cw,
        hy,
        if new > 0 { GOLD } else { MUTED },
    );
    cv.hline(px + PAD, py + HEAD_H - 1, pw - 2 * PAD, HAIRLINE, 1.0);

    let cols = ((pw - 2 * PAD - 16) / GLYPH_W).max(8) as usize;
    if s.entries.is_empty() {
        cv.text(
            empty,
            px + PAD + 4,
            py + HEAD_H + (ENTRY_H - GLYPH_H) / 2,
            MUTED,
        );
    }
    let shown = visible(w, h, s.entries.len());
    for (n, e) in s.entries.iter().skip(s.top).take(shown).enumerate() {
        let ey = py + HEAD_H + n as i32 * ENTRY_H;
        if n > 0 {
            cv.hline(px + PAD, ey, pw - 2 * PAD, HAIRLINE, 0.6);
        }
        let x = px + PAD + 4;
        let l1 = ey + 10;
        // New since the last look: a bar down the left edge, as the active
        // window wears in the overview.
        if e.when > s.seen {
            cv.fill_rect_a(px + 1, ey + 8, 2, ENTRY_H - 16, CYAN, 1.0);
        }
        let col = sev_color(&e.severity);
        cv.fill_rect_a(x, l1 + (GLYPH_H - 8) / 2, 8, 8, col, 1.0);
        let when = notes::ago(e.when, s.now);
        let ww = Canvas::text_width(&when);
        cv.text(
            &fit(
                &e.app.to_uppercase(),
                cols.saturating_sub(when.chars().count() + 4),
            ),
            x + 16,
            l1,
            MUTED,
        );
        cv.text(&when, px + pw - PAD - 4 - ww, l1, MUTED);
        cv.text_bold(&fit(&e.title, cols), x, l1 + LINE, TEXT);
        if !e.body.is_empty() {
            cv.text(&fit(&e.body, cols), x, l1 + 2 * LINE, MUTED);
        }
    }
    let foot_y = py + ph - FOOT_H + (FOOT_H - GLYPH_H) / 2;
    cv.hline(px + PAD, py + ph - FOOT_H, pw - 2 * PAD, HAIRLINE, 1.0);
    cv.text(keys, px + PAD + 4, foot_y, MUTED);
    let hidden = s.entries.len().saturating_sub(s.top + shown);
    if hidden > 0 || s.top > 0 {
        let pos = format!(
            "{}–{} de {}",
            s.top + 1,
            (s.top + shown).min(s.entries.len()),
            s.entries.len()
        );
        let w = Canvas::text_width(&pos);
        cv.text(&pos, px + pw - PAD - 4 - w, foot_y, MUTED);
    }
}

// ── wayland ──────────────────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    w: u32,
    h: u32,
    map: *mut u8,
    len: usize,
    buffer: Option<wl_buffer::WlBuffer>,

    entries: Vec<Entry>,
    seen: u64,
    top: usize,
    ptr: (f64, f64),
    lang: Option<Lang>,
    done: bool,
}

impl State {
    fn render(&mut self) {
        let (Some(surface), Some(buf)) = (self.surface.clone(), self.buffer.clone()) else {
            return;
        };
        if self.map.is_null() {
            return;
        }
        let Some(mut cv) = Canvas::try_new(self.w as usize, self.h as usize) else {
            return;
        };
        cv.set_square(true);
        let scene = Scene {
            entries: &self.entries,
            seen: self.seen,
            now: notes::now(),
            top: self.top,
            lang: self.lang.unwrap_or(Lang::Es),
        };
        draw(&mut cv, self.w as i32, self.h as i32, &scene);
        // SAFETY: `map` is the `len` bytes mapped for this surface's buffer.
        let data = unsafe { std::slice::from_raw_parts_mut(self.map, self.len) };
        if cv.blit_argb(data) {
            surface.attach(Some(&buf), 0, 0);
            surface.damage_buffer(0, 0, self.w as i32, self.h as i32);
            surface.commit();
        }
    }

    fn scroll(&mut self, delta: i32) {
        let shown = visible(self.w as i32, self.h as i32, self.entries.len());
        let max = self.entries.len().saturating_sub(shown);
        let top = (self.top as i32 + delta).clamp(0, max as i32) as usize;
        if top != self.top {
            self.top = top;
            self.render();
        }
    }

    fn clear(&mut self) {
        let (path, _) = notes::session_paths();
        let _ = notes::clear(&path);
        self.entries.clear();
        self.top = 0;
        self.render();
    }

    fn inside(&self, x: f64, y: f64) -> bool {
        let (px, py, pw, ph) = panel(self.w as i32, self.h as i32, self.entries.len());
        let (x, y) = (x as i32, y as i32);
        x >= px && x < px + pw && y >= py && y < py + ph
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
            let Some((map, fd)) = map_shm_pool(geom.frame_size, "lunarnotes") else {
                return;
            };
            if let Some(b) = self.buffer.take() {
                b.destroy();
            }
            if !self.map.is_null() {
                unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
            }
            let pool = shm.create_pool(fd.as_fd(), geom.frame_size as i32, qh, ());
            self.buffer = Some(pool.create_buffer(
                0,
                w as i32,
                h as i32,
                geom.stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            ));
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
    fn event(
        st: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    st.layer_shell = Some(reg.bind(name, version.min(4), qh, ()))
                }
                "wl_seat" if st.seat.is_none() => {
                    st.seat = Some(reg.bind(name, version.min(5), qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        st: &mut Self,
        l: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                l.ack_configure(serial);
                st.configure(qh, width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        st: &mut Self,
        seat: &wl_seat::WlSeat,
        ev: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = ev
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
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = ev
        else {
            return;
        };
        let page = visible(st.w as i32, st.h as i32, st.entries.len()) as i32;
        match key {
            KEY_ESC_WL => st.done = true,
            KEY_UP_WL => st.scroll(-1),
            KEY_DOWN_WL => st.scroll(1),
            KEY_PGUP_WL => st.scroll(-page),
            KEY_PGDN_WL => st.scroll(page),
            KEY_DELETE => st.clear(),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_pointer::WlPointer,
        ev: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match ev {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                st.ptr = (surface_x, surface_y);
            }
            wl_pointer::Event::Button {
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                if !st.inside(st.ptr.0, st.ptr.1) {
                    st.done = true;
                }
            }
            wl_pointer::Event::Axis { value, .. } => st.scroll(if value > 0.0 { 1 } else { -1 }),
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

fn sample(now: u64) -> Vec<Entry> {
    let e = |ago: u64, sev: &str, app: &str, title: &str, body: &str| Entry {
        when: now - ago,
        severity: sev.into(),
        app: app.into(),
        title: title.into(),
        body: body.into(),
    };
    vec![
        e(
            40,
            "info",
            "Firefox",
            "Descarga completada",
            "informe-trimestral.pdf · 2,1 MiB",
        ),
        e(
            4 * 60,
            "info",
            "foot",
            "Compilación terminada",
            "cargo build --release · 0 errores, 1 aviso",
        ),
        e(
            9 * 60,
            "crit",
            "hunter",
            "Malware detectado",
            "Eicar-Test-Signature en /root/Descargas/eicar.com",
        ),
        e(
            26 * 60,
            "info",
            "Thunderbird",
            "3 mensajes nuevos",
            "Moebius: revisión del arranque · Alpine: actualizaciones",
        ),
        e(
            2 * 3600,
            "warn",
            "hunter",
            "Conexión bloqueada",
            "/usr/bin/curl → 93.184.216.34:443",
        ),
        e(
            5 * 3600,
            "info",
            "lunarshot",
            "Captura guardada",
            "/root/Imágenes/captura-2026-11-05-053112.png",
        ),
    ]
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
    let now = notes::now();
    let entries = sample(now);
    // The first three arrived since the last look.
    let scene = Scene {
        entries: &entries,
        seen: now - 10 * 60,
        now,
        top: 0,
        lang: Lang::current(),
    };
    let mut cv = Canvas::try_new(w, h).unwrap_or_else(|| std::process::exit(1));
    cv.set_square(true);
    draw(&mut cv, w as i32, h as i32, &scene);
    let mut buf = vec![0u8; w * h * 4];
    if !cv.blit_argb(&mut buf) || std::fs::write(path, &buf).is_err() {
        eprintln!("lunarnotes: could not write {path}");
        std::process::exit(1);
    }
    println!("lunarnotes: wrote {path} ({w}x{h} ARGB8888)");
    std::process::exit(0);
}

/// Super+N twice closes it: a second start signals the first and leaves.
fn toggle_or_lock() -> Option<std::fs::File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&dir).join("lunarnotes.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .ok()?;
    // SAFETY: flock on a descriptor this process owns.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
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
        Some("--dump") => dump(&args.next().unwrap_or_else(|| "lunarnotes.raw".into())),
        Some(_) => {
            eprintln!("usage: lunarnotes [--dump PATH:WxH]");
            std::process::exit(2);
        }
    }
    let _lock = toggle_or_lock();
    let (path, stamp) = notes::session_paths();
    let mut st = State {
        lang: Some(Lang::current()),
        entries: notes::load(&path),
        seen: notes::seen(&stamp),
        ..State::default()
    };
    // Opening the list is looking at it: the bar's chip loses its number. The
    // marks stay on screen for this showing, from the stamp read above.
    if let Some(newest) = st.entries.first() {
        let _ = notes::mark_seen(&stamp, newest.when.max(st.seen));
    }
    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunarnotes: no Wayland compositor found");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunarnotes: wayland roundtrip failed");
            std::process::exit(1);
        }
    }
    let (Some(comp), Some(ls)) = (st.compositor.clone(), st.layer_shell.clone()) else {
        eprintln!("lunarnotes: compositor lacks wlr-layer-shell");
        std::process::exit(1);
    };
    let surface = comp.create_surface(&qh, ());
    let layer = ls.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "notifications".into(),
        &qh,
        (),
    );
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

    #[test]
    fn the_panel_is_centred_and_fits_the_output() {
        for (w, h, n) in [
            (1920, 1080, 6),
            (1280, 720, 100),
            (800, 600, 0),
            (320, 240, 5),
        ] {
            let (px, py, pw, ph) = panel(w, h, n);
            assert!(px >= 0 && py >= 0, "{w}x{h}");
            assert!(
                px + pw <= w && py + ph <= h.max(HEAD_H + ENTRY_H + FOOT_H),
                "{w}x{h}: {px} {py} {pw} {ph}"
            );
            assert!((px - (w - px - pw)).abs() <= 1, "centred");
            assert!(visible(w, h, n) >= 1);
        }
        // A short history is a short panel.
        assert_eq!(visible(1920, 1080, 2), 2);
        assert!(visible(1280, 720, 100) < 100);
    }

    #[test]
    fn every_look_and_size_draws_without_a_panic() {
        let now = 1_000_000;
        let entries = sample(now);
        for (w, h) in [(1600, 900), (640, 480), (320, 240)] {
            for top in [0, 3] {
                let s = Scene {
                    entries: &entries,
                    seen: now - 600,
                    now,
                    top,
                    lang: Lang::Es,
                };
                let mut cv = Canvas::new(w, h);
                draw(&mut cv, w as i32, h as i32, &s);
            }
        }
        let s = Scene {
            entries: &[],
            seen: 0,
            now,
            top: 0,
            lang: Lang::En,
        };
        draw(&mut Canvas::new(800, 600), 800, 600, &s);
        assert_eq!(fit("abcdef", 4), "abc…");
    }
}
