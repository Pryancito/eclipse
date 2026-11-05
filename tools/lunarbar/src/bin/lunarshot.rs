//! lunarshot — Eclipse OS's screenshots.
//!
//! The desktop had no way to take one: no `grim`, no `slurp`, no key. This
//! is the two of them in the shape of the rest of the crate, one static
//! binary over `wlr-screencopy-unstable-v1` (which labwc implements through
//! wlroots), wl_shm and wlr-layer-shell:
//!
//! - default (Print): every output, stitched where the outputs sit.
//! - `--region` (Shift+Print): the screen freezes under a dimmed overlay,
//!   you drag a rectangle (a click without dragging takes that whole
//!   output), Enter or the release takes it, Esc cancels.
//!
//! The picture is saved as a PNG in the pictures folder (`~/Imágenes`, or
//! `~/Pictures`, whichever exists; `XDG_PICTURES_DIR` first), put on the
//! clipboard as `image/png` through wlr-data-control, and announced through
//! the bar's notification spool with a button that opens the folder. The
//! process stays alive only while it owns the clipboard, as every Wayland
//! clipboard source has to: the next copy anywhere ends it.
//!
//! `--dump PATH:WxH`: render the region overlay to a raw ARGB8888 file, over
//! the PNG in `LUNARSHOT_BACKDROP` (or a plain ground) with the selection in
//! `LUNARSHOT_SEL=x,y,w,h`, for offline review.

use std::io::Write;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use lunarbar::draw::{Canvas, Rgb, GLYPH_H};
use lunarbar::fill_guard;
use lunarbar::hunter_notify::{self, Action as NotifyAction, Alert, Severity};
use lunarbar::i18n::{Key, Lang};
use lunarbar::keys::{KEY_ENTER_WL, KEY_ESC_WL, KEY_KPENTER_WL};
use lunarbar::proc::{self, map_shm_pool};
use tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

// ── look ─────────────────────────────────────────────────────────────────────

const GROUND: Rgb = (0x04, 0x07, 0x0d);
const SURFACE: Rgb = (0x0a, 0x14, 0x23);
const HAIRLINE: Rgb = (0x1d, 0x35, 0x50);
const TEXT: Rgb = (0xe6, 0xf1, 0xff);
const MUTED: Rgb = (0x8a, 0xa4, 0xc2);
const CYAN: Rgb = (0x38, 0xe1, 0xff);
const GOLD: Rgb = (0xf2, 0xc1, 0x4e);

/// How dark the frozen screen is outside the selection.
const DIM: f32 = 0.55;
const PLATE_PAD: i32 = 8;
const PLATE_GAP: i32 = 8;
const BRACKET_LEN: i32 = 16;
/// A drag shorter than this on either side is a click.
const MIN_DRAG: i32 = 4;

// ── geometry ─────────────────────────────────────────────────────────────────

/// A rectangle in pixels, `w` and `h` never negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    /// The rectangle two corners span, whichever way the drag went.
    fn span(a: (i32, i32), b: (i32, i32)) -> Rect {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    /// Kept inside `w` x `h`.
    fn clamp(self, w: i32, h: i32) -> Rect {
        let x0 = self.x.clamp(0, w);
        let y0 = self.y.clamp(0, h);
        let x1 = (self.x + self.w).clamp(0, w);
        let y1 = (self.y + self.h).clamp(0, h);
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    /// From surface pixels to capture pixels, `k` capture pixels per surface
    /// pixel on each axis.
    fn scale(self, kx: f32, ky: f32) -> Rect {
        let x0 = (self.x as f32 * kx).round() as i32;
        let y0 = (self.y as f32 * ky).round() as i32;
        let x1 = ((self.x + self.w) as f32 * kx).round() as i32;
        let y1 = ((self.y + self.h) as f32 * ky).round() as i32;
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    fn is_drag(self) -> bool {
        self.w >= MIN_DRAG && self.h >= MIN_DRAG
    }
}

/// The canvas that holds every output at its place: `(x, y, w, h)` of each,
/// in compositor coordinates. Returns the bounding box.
fn bounds(places: &[(i32, i32, i32, i32)]) -> Rect {
    let x0 = places.iter().map(|p| p.0).min().unwrap_or(0);
    let y0 = places.iter().map(|p| p.1).min().unwrap_or(0);
    let x1 = places.iter().map(|p| p.0 + p.2).max().unwrap_or(0);
    let y1 = places.iter().map(|p| p.1 + p.3).max().unwrap_or(0);
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

// ── pixels ───────────────────────────────────────────────────────────────────

/// A wl_shm XRGB8888/ARGB8888 frame (little endian: B, G, R, X per pixel) as
/// an opaque pixmap, flipped when the compositor said `y_invert`.
fn to_pixmap(data: &[u8], w: u32, h: u32, stride: u32, y_invert: bool) -> Option<Pixmap> {
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h {
        let src = if y_invert { h - 1 - row } else { row };
        let line = data.get((src * stride) as usize..(src * stride + w * 4) as usize)?;
        for px in line.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    Pixmap::from_vec(rgba, IntSize::from_wh(w, h)?)
}

/// `src`'s part inside `r`, as its own pixmap.
fn crop(src: &Pixmap, r: Rect) -> Option<Pixmap> {
    let r = r.clamp(src.width() as i32, src.height() as i32);
    let mut out = Pixmap::new(r.w.max(1) as u32, r.h.max(1) as u32)?;
    out.draw_pixmap(-r.x, -r.y, src.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    Some(out)
}

/// `src` drawn at `w` x `h`.
fn resized(src: &Pixmap, w: u32, h: u32) -> Option<Pixmap> {
    if src.width() == w && src.height() == h {
        return Some(src.clone());
    }
    let mut out = Pixmap::new(w.max(1), h.max(1))?;
    let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
    let t = Transform::from_scale(w as f32 / src.width() as f32, h as f32 / src.height() as f32);
    out.draw_pixmap(0, 0, src.as_ref(), &paint, t, None);
    Some(out)
}

// ── where it goes ────────────────────────────────────────────────────────────

/// The pictures folder: `XDG_PICTURES_DIR`, else the first of `~/Imágenes`
/// and `~/Pictures` that exists, else the one the language names.
fn pictures_dir(home: &Path, xdg: Option<&str>, lang: Lang) -> PathBuf {
    if let Some(x) = xdg.filter(|x| !x.trim().is_empty()) {
        let x = x.trim().trim_matches('"');
        return match x.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(x),
        };
    }
    for name in ["Imágenes", "Pictures"] {
        if home.join(name).is_dir() {
            return home.join(name);
        }
    }
    home.join(lang.s(Key::pictures_folder))
}

/// `captura-2026-11-05-052612.png` for a moment, as local time.
fn file_name(t: i64, lang: Lang) -> String {
    let tt = t as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` it is handed.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    let stem = match lang {
        Lang::Es => "captura",
        Lang::En => "screenshot",
    };
    format!(
        "{stem}-{:04}-{:02}-{:02}-{:02}{:02}{:02}.png",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// A path that is not taken yet: `name.png`, then `name-2.png`, ...
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let stem = name.trim_end_matches(".png");
    (2..1000).map(|n| dir.join(format!("{stem}-{n}.png"))).find(|p| !p.exists()).unwrap_or(p)
}

/// Single quotes for `/bin/sh`, so a path with spaces stays one word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn announce(path: &Path, w: u32, h: u32, lang: Lang) {
    let dir = path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let (title, open, size) = match lang {
        Lang::Es => ("Captura guardada", "Abrir carpeta", format!("{w} × {h} · copiada al portapapeles")),
        Lang::En => ("Screenshot saved", "Open folder", format!("{w} × {h} · copied to the clipboard")),
    };
    let alert = Alert {
        app: String::new(),
        severity: Severity::Info,
        title: title.into(),
        body: format!("{}\n{size}", path.display()),
        actions: vec![NotifyAction {
            id: "open".into(),
            label: open.into(),
            run: Some(format!("lunarfile --dir {}", sh_quote(&dir))),
        }],
        timeout: None,
        reply: false,
    };
    let _ = hunter_notify::post(Path::new(hunter_notify::SPOOL), &alert);
}

// ── the overlay ──────────────────────────────────────────────────────────────

/// What one output's overlay shows.
struct View<'a> {
    /// The frozen screen, already at the surface's size.
    shot: &'a Pixmap,
    /// The selection, in surface pixels, when there is one on this output.
    sel: Option<Rect>,
    /// Its size in real pixels, for the readout.
    real: (i32, i32),
    /// Show the hint (no drag started anywhere yet).
    hint: bool,
    lang: Lang,
}

fn draw_overlay(cv: &mut Canvas, w: i32, h: i32, v: &View) {
    cv.pixmap(0, 0, v.shot);
    match v.sel {
        Some(s) if s.w > 0 && s.h > 0 => {
            // Dim the four bands around the selection, not the selection.
            cv.fill_rect_a(0, 0, w, s.y, GROUND, DIM);
            cv.fill_rect_a(0, s.y + s.h, w, h - s.y - s.h, GROUND, DIM);
            cv.fill_rect_a(0, s.y, s.x, s.h, GROUND, DIM);
            cv.fill_rect_a(s.x + s.w, s.y, w - s.x - s.w, s.h, GROUND, DIM);
            for (x, y, rw, rh) in [(s.x, s.y, s.w, 1), (s.x, s.y + s.h - 1, s.w, 1), (s.x, s.y, 1, s.h), (s.x + s.w - 1, s.y, 1, s.h)] {
                cv.fill_rect_a(x, y, rw, rh, CYAN, 1.0);
            }
            cv.brackets(s.x, s.y, s.w, s.h, BRACKET_LEN, 2, GOLD);
            // The readout, under the selection or above it near the bottom.
            let label = format!("{} × {}", v.real.0, v.real.1);
            let pw = Canvas::text_width(&label) + 2 * PLATE_PAD;
            let ph = GLYPH_H + 2 * PLATE_PAD;
            let below = s.y + s.h + PLATE_GAP;
            let py = if below + ph <= h { below } else { (s.y - PLATE_GAP - ph).max(0) };
            let px = s.x.clamp(0, (w - pw).max(0));
            plate(cv, px, py, pw, ph);
            cv.text(&label, px + PLATE_PAD, py + PLATE_PAD, TEXT);
        }
        _ => cv.fill_rect_a(0, 0, w, h, GROUND, DIM),
    }
    if v.hint {
        let (a, b) = match v.lang {
            Lang::Es => ("Arrastra para elegir una zona", "clic: esta pantalla · Intro: tomar · Esc: cancelar"),
            Lang::En => ("Drag to choose an area", "click: this screen · Enter: take · Esc: cancel"),
        };
        let pw = Canvas::text_width(b).max(Canvas::text_width(a)) + 2 * PLATE_PAD + 8;
        let ph = 2 * GLYPH_H + 4 + 2 * PLATE_PAD;
        let (px, py) = ((w - pw) / 2, 48);
        plate(cv, px, py, pw, ph);
        cv.brackets(px, py, pw, ph, 8, 1, GOLD);
        cv.text_bold(a, px + (pw - Canvas::text_width(a)) / 2, py + PLATE_PAD, TEXT);
        cv.text(b, px + (pw - Canvas::text_width(b)) / 2, py + PLATE_PAD + GLYPH_H + 4, MUTED);
    }
}

fn plate(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    cv.fill_rect_a(x, y, w, h, SURFACE, 0.94);
    for (rx, ry, rw, rh) in [(x, y, w, 1), (x, y + h - 1, w, 1), (x, y, 1, h), (x + w - 1, y, 1, h)] {
        cv.fill_rect_a(rx, ry, rw, rh, HAIRLINE, 1.0);
    }
}

// ── wayland ──────────────────────────────────────────────────────────────────

struct Output {
    wl: wl_output::WlOutput,
    /// Position in the compositor's layout.
    x: i32,
    y: i32,
    /// The capture: buffer, pixels, and how it went.
    frame: Option<Capture>,
    shot: Option<Pixmap>,
    failed: bool,
}

struct Capture {
    format: Option<(wl_shm::Format, u32, u32, u32)>,
    map: *mut u8,
    len: usize,
    buffer: Option<wl_buffer::WlBuffer>,
    y_invert: bool,
}

/// One output's overlay in `--region`.
struct Cover {
    out: usize,
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    w: u32,
    h: u32,
    map: *mut u8,
    len: usize,
    buffer: Option<wl_buffer::WlBuffer>,
    scaled: Option<Pixmap>,
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    copy_mgr: Option<ZwlrScreencopyManagerV1>,
    clip_mgr: Option<ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: Vec<Output>,

    covers: Vec<Cover>,
    lang: Option<Lang>,
    /// The cover under the pointer, and where on it.
    ptr_cover: Option<usize>,
    ptr: (i32, i32),
    /// Where the drag started (cover, point), and where it is now.
    drag: Option<(usize, (i32, i32))>,
    sel: Option<(usize, Rect)>,
    /// What was chosen: the cover and the rectangle in its surface pixels.
    chosen: Option<(usize, Rect)>,
    cancelled: bool,

    /// The PNG the clipboard source hands out, while it owns the selection.
    png: Vec<u8>,
    source: Option<ZwlrDataControlSourceV1>,
    clip_done: bool,
}

// SAFETY-adjacent note: the raw `map` pointers are mappings this process
// owns for its whole life; nothing crosses threads.

impl State {
    fn lang(&self) -> Lang {
        self.lang.unwrap_or(Lang::Es)
    }

    /// Ask for every output's frame.
    fn capture_all(&mut self, qh: &QueueHandle<State>) {
        let Some(mgr) = self.copy_mgr.clone() else {
            return;
        };
        for (i, o) in self.outputs.iter_mut().enumerate() {
            // 0: the cursor is not in the picture.
            mgr.capture_output(0, &o.wl, qh, i);
            o.frame = Some(Capture { format: None, map: std::ptr::null_mut(), len: 0, buffer: None, y_invert: false });
        }
    }

    fn captures_pending(&self) -> bool {
        self.outputs.iter().any(|o| o.shot.is_none() && !o.failed)
    }

    /// The frame told us its buffer: make one and ask for the copy.
    fn copy_into_buffer(&mut self, qh: &QueueHandle<State>, i: usize, frame: &ZwlrScreencopyFrameV1) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let Some(o) = self.outputs.get_mut(i) else {
            return;
        };
        let Some(c) = o.frame.as_mut() else {
            return;
        };
        if c.buffer.is_some() {
            return;
        }
        let Some((format, w, h, stride)) = c.format else {
            o.failed = true;
            return;
        };
        let len = stride as usize * h as usize;
        let Some((map, fd)) = map_shm_pool(len, "lunarshot") else {
            o.failed = true;
            return;
        };
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, ());
        pool.destroy();
        frame.copy(&buffer);
        c.map = map;
        c.len = len;
        c.buffer = Some(buffer);
    }

    fn frame_ready(&mut self, i: usize) {
        let Some(o) = self.outputs.get_mut(i) else {
            return;
        };
        let Some(c) = o.frame.take() else {
            return;
        };
        if let Some((_, w, h, stride)) = c.format {
            if !c.map.is_null() {
                // SAFETY: `map` is the `len` bytes this process mapped for it.
                let data = unsafe { std::slice::from_raw_parts(c.map, c.len) };
                o.shot = to_pixmap(data, w, h, stride, c.y_invert);
            }
        }
        if o.shot.is_none() {
            o.failed = true;
        }
        if let Some(b) = c.buffer {
            b.destroy();
        }
        if !c.map.is_null() {
            unsafe { libc::munmap(c.map as *mut libc::c_void, c.len) };
        }
    }

    /// Every output at its place, as one picture.
    fn stitched(&self) -> Option<Pixmap> {
        let shots: Vec<(&Output, &Pixmap)> = self.outputs.iter().filter_map(|o| Some((o, o.shot.as_ref()?))).collect();
        if let [(_, one)] = shots.as_slice() {
            return Some((*one).clone());
        }
        let places: Vec<_> = shots.iter().map(|(o, p)| (o.x, o.y, p.width() as i32, p.height() as i32)).collect();
        let b = bounds(&places);
        fill_guard::check_buffer(b.w as usize, b.h as usize).ok()?;
        let mut out = Pixmap::new(b.w as u32, b.h as u32)?;
        out.fill(tiny_skia::Color::BLACK);
        for (o, p) in shots {
            out.draw_pixmap(o.x - b.x, o.y - b.y, p.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
        Some(out)
    }

    fn map_covers(&mut self, qh: &QueueHandle<State>) {
        let (Some(comp), Some(ls)) = (self.compositor.clone(), self.layer_shell.clone()) else {
            return;
        };
        for (i, o) in self.outputs.iter().enumerate() {
            if o.shot.is_none() {
                continue;
            }
            let n = self.covers.len();
            let surface = comp.create_surface(qh, ());
            let layer = ls.get_layer_surface(&surface, Some(&o.wl), zwlr_layer_shell_v1::Layer::Overlay, "screenshot".into(), qh, n);
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_size(0, 0);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(if n == 0 { KeyboardInteractivity::Exclusive } else { KeyboardInteractivity::None });
            surface.commit();
            self.covers.push(Cover { out: i, surface, layer, w: 0, h: 0, map: std::ptr::null_mut(), len: 0, buffer: None, scaled: None });
        }
    }

    fn configure_cover(&mut self, qh: &QueueHandle<State>, n: usize, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let Some(c) = self.covers.get_mut(n) else {
            return;
        };
        let Some(shot) = self.outputs.get(c.out).and_then(|o| o.shot.as_ref()) else {
            return;
        };
        let (w, h) = (if w == 0 { shot.width() } else { w }, if h == 0 { shot.height() } else { h });
        if c.w != w || c.h != h {
            let Some(geom) = proc::pool_geometry(w, h) else {
                return;
            };
            // One buffer: the overlay is redrawn only on pointer motion, and a
            // frame drawn while the compositor still reads it costs a tear
            // nobody sees over a frozen picture.
            let Some((map, fd)) = map_shm_pool(geom.frame_size, "lunarshot") else {
                return;
            };
            if let Some(b) = c.buffer.take() {
                b.destroy();
            }
            if !c.map.is_null() {
                unsafe { libc::munmap(c.map as *mut libc::c_void, c.len) };
            }
            let pool = shm.create_pool(fd.as_fd(), geom.frame_size as i32, qh, ());
            c.buffer = Some(pool.create_buffer(0, w as i32, h as i32, geom.stride as i32, wl_shm::Format::Argb8888, qh, ()));
            pool.destroy();
            c.map = map;
            c.len = geom.frame_size;
            c.w = w;
            c.h = h;
            c.scaled = resized(shot, w, h);
        }
        self.paint(n);
    }

    fn paint(&mut self, n: usize) {
        let hint = self.drag.is_none() && self.sel.is_none();
        let lang = self.lang();
        let sel = self.sel.filter(|(c, _)| *c == n).map(|(_, r)| r);
        let Some(c) = self.covers.get(n) else {
            return;
        };
        let (Some(scaled), Some(buf)) = (c.scaled.as_ref(), c.buffer.as_ref()) else {
            return;
        };
        let shot = &self.outputs[c.out].shot;
        let (kx, ky) = match shot {
            Some(s) => (s.width() as f32 / c.w as f32, s.height() as f32 / c.h as f32),
            None => (1.0, 1.0),
        };
        let real = sel.map(|r| r.scale(kx, ky)).map(|r| (r.w, r.h)).unwrap_or((0, 0));
        let Some(mut cv) = Canvas::try_new(c.w as usize, c.h as usize) else {
            return;
        };
        draw_overlay(&mut cv, c.w as i32, c.h as i32, &View { shot: scaled, sel, real, hint, lang });
        // SAFETY: `map` is the frame this cover mapped, `len` bytes.
        let data = unsafe { std::slice::from_raw_parts_mut(c.map, c.len) };
        if cv.blit_argb(data) {
            c.surface.attach(Some(buf), 0, 0);
            c.surface.damage_buffer(0, 0, c.w as i32, c.h as i32);
            c.surface.commit();
        }
    }

    fn repaint_all(&mut self) {
        for n in 0..self.covers.len() {
            self.paint(n);
        }
    }

    /// The chosen part, in capture pixels.
    fn chosen_shot(&self) -> Option<Pixmap> {
        let (n, r) = self.chosen?;
        let c = self.covers.get(n)?;
        let shot = self.outputs.get(c.out)?.shot.as_ref()?;
        if !r.is_drag() {
            return Some(shot.clone());
        }
        let r = r.scale(shot.width() as f32 / c.w as f32, shot.height() as f32 / c.h as f32);
        crop(shot, r)
    }

    fn close_covers(&mut self) {
        for c in self.covers.drain(..) {
            if let Some(b) = c.buffer {
                b.destroy();
            }
            c.layer.destroy();
            c.surface.destroy();
            if !c.map.is_null() {
                unsafe { libc::munmap(c.map as *mut libc::c_void, c.len) };
            }
        }
    }

    /// Own the clipboard with `png` as `image/png`.
    fn offer_png(&mut self, qh: &QueueHandle<State>, png: Vec<u8>) -> bool {
        let (Some(mgr), Some(seat)) = (self.clip_mgr.clone(), self.seat.clone()) else {
            return false;
        };
        let source = mgr.create_data_source(qh, ());
        source.offer("image/png".into());
        let device = mgr.get_data_device(&seat, qh, ());
        device.set_selection(Some(&source));
        self.png = png;
        self.source = Some(source);
        true
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer_shell = Some(reg.bind(name, version.min(4), qh, ())),
                "zwlr_screencopy_manager_v1" => st.copy_mgr = Some(reg.bind(name, version.min(3), qh, ())),
                "zwlr_data_control_manager_v1" => st.clip_mgr = Some(reg.bind(name, version.min(2), qh, ())),
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(5), qh, ())),
                "wl_output" if st.outputs.len() < fill_guard::MAX_OUTPUTS => {
                    let i = st.outputs.len();
                    let wl = reg.bind(name, version.min(4), qh, i);
                    st.outputs.push(Output { wl, x: 0, y: 0, frame: None, shot: None, failed: false });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(st: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, &i: &usize, _: &Connection, _: &QueueHandle<State>) {
        if let wl_output::Event::Geometry { x, y, .. } = ev {
            if let Some(o) = st.outputs.get_mut(i) {
                o.x = x;
                o.y = y;
            }
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, usize> for State {
    fn event(st: &mut Self, frame: &ZwlrScreencopyFrameV1, ev: zwlr_screencopy_frame_v1::Event, &i: &usize, _: &Connection, qh: &QueueHandle<State>) {
        use zwlr_screencopy_frame_v1::Event;
        match ev {
            Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                let usable = matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);
                if let Some(c) = st.outputs.get_mut(i).and_then(|o| o.frame.as_mut()) {
                    if usable && c.format.is_none() {
                        c.format = Some((format, width, height, stride));
                    }
                }
                // Version 1 and 2 send no buffer_done: copy at once.
                if frame.version() < 3 {
                    st.copy_into_buffer(qh, i, frame);
                }
            }
            Event::BufferDone => st.copy_into_buffer(qh, i, frame),
            Event::Flags { flags: WEnum::Value(f) } => {
                if let Some(c) = st.outputs.get_mut(i).and_then(|o| o.frame.as_mut()) {
                    c.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            Event::Ready { .. } => {
                st.frame_ready(i);
                frame.destroy();
            }
            Event::Failed => {
                if let Some(o) = st.outputs.get_mut(i) {
                    o.failed = true;
                }
                frame.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(st: &mut Self, layer: &ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, &n: &usize, _: &Connection, qh: &QueueHandle<State>) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                st.configure_cover(qh, n, width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => st.cancelled = true,
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

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(st: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } = ev {
            match key {
                KEY_ESC_WL => st.cancelled = true,
                KEY_ENTER_WL | KEY_KPENTER_WL => {
                    // Enter takes the selection, or the screen under the
                    // pointer when there is none.
                    st.chosen = st.sel.or_else(|| Some((st.ptr_cover.unwrap_or(0), Rect::default())));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(st: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match ev {
            wl_pointer::Event::Enter { surface, surface_x, surface_y, .. } => {
                st.ptr_cover = st.covers.iter().position(|c| c.surface == surface);
                st.ptr = (surface_x as i32, surface_y as i32);
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                st.ptr = (surface_x as i32, surface_y as i32);
                if let Some((n, start)) = st.drag {
                    if st.ptr_cover == Some(n) {
                        let c = &st.covers[n];
                        st.sel = Some((n, Rect::span(start, st.ptr).clamp(c.w as i32, c.h as i32)));
                        st.repaint_all();
                    }
                }
            }
            wl_pointer::Event::Button { state: WEnum::Value(s), .. } => match s {
                wl_pointer::ButtonState::Pressed => {
                    if let Some(n) = st.ptr_cover {
                        st.drag = Some((n, st.ptr));
                        st.sel = None;
                        st.repaint_all();
                    }
                }
                wl_pointer::ButtonState::Released => {
                    if let Some((n, start)) = st.drag.take() {
                        let r = Rect::span(start, st.ptr);
                        st.chosen = Some((n, if r.is_drag() { st.sel.map(|s| s.1).unwrap_or(r) } else { Rect::default() }));
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(st: &mut Self, src: &ZwlrDataControlSourceV1, ev: zwlr_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match ev {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                // The fd is ours; dropping the file closes it, which is how
                // the reader learns the picture is complete.
                let mut f = std::fs::File::from(fd);
                if mime_type == "image/png" {
                    let _ = f.write_all(&st.png);
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                src.destroy();
                st.source = None;
                st.clip_done = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrDataControlDeviceV1, _: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}

    wayland_client::event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

wayland_client::delegate_noop!(State: ignore ZwlrDataControlOfferV1);
wayland_client::delegate_noop!(State: ignore ZwlrDataControlManagerV1);
wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

// ── main ─────────────────────────────────────────────────────────────────────

fn fail(msg: &str) -> ! {
    eprintln!("lunarshot: {msg}");
    std::process::exit(1);
}

fn dump(spec: &str) -> ! {
    let (path, dims) = spec.rsplit_once(':').unwrap_or((spec, "1600x900"));
    let (w, h) = dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .unwrap_or((1600, 900));
    if fill_guard::check_buffer(w as usize, h as usize).is_err() {
        fail("too large");
    }
    let shot = std::env::var("LUNARSHOT_BACKDROP")
        .ok()
        .and_then(|p| Pixmap::load_png(p).ok())
        .and_then(|p| resized(&p, w, h))
        .unwrap_or_else(|| {
            let mut p = Pixmap::new(w, h).unwrap();
            p.fill(tiny_skia::Color::from_rgba8(GROUND.0, GROUND.1, GROUND.2, 255));
            p
        });
    let sel = std::env::var("LUNARSHOT_SEL").ok().and_then(|s| {
        let v: Vec<i32> = s.split(',').filter_map(|n| n.trim().parse().ok()).collect();
        (v.len() == 4).then(|| Rect { x: v[0], y: v[1], w: v[2], h: v[3] })
    });
    let mut cv = Canvas::try_new(w as usize, h as usize).unwrap_or_else(|| fail("no canvas"));
    cv.set_square(true);
    let real = sel.map(|r| (r.w, r.h)).unwrap_or((0, 0));
    draw_overlay(&mut cv, w as i32, h as i32, &View { shot: &shot, sel, real, hint: sel.is_none(), lang: Lang::current() });
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    if !cv.blit_argb(&mut buf) || std::fs::write(path, &buf).is_err() {
        fail("could not write the dump");
    }
    println!("lunarshot: wrote {path} ({w}x{h} ARGB8888)");
    std::process::exit(0);
}

fn main() {
    let mut region = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--region" | "-r" => region = true,
            "--dump" => dump(&args.next().unwrap_or_else(|| "lunarshot.raw".into())),
            _ => {
                eprintln!("usage: lunarshot [--region] | --dump PATH:WxH");
                std::process::exit(2);
            }
        }
    }
    let lang = Lang::current();

    let Ok(conn) = lunarbar::wl::connect() else {
        fail("no Wayland compositor found (is the session up?)");
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = State { lang: Some(lang), ..State::default() };
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            fail("wayland roundtrip failed");
        }
    }
    if st.copy_mgr.is_none() || st.shm.is_none() {
        fail("the compositor offers no wlr-screencopy");
    }
    if st.outputs.is_empty() {
        fail("no outputs");
    }

    st.capture_all(&qh);
    while st.captures_pending() {
        if queue.blocking_dispatch(&mut st).is_err() {
            fail("wayland dispatch failed");
        }
    }

    let picture = if region {
        if st.layer_shell.is_none() || st.compositor.is_none() {
            fail("the compositor lacks wlr-layer-shell");
        }
        st.map_covers(&qh);
        while st.chosen.is_none() && !st.cancelled {
            if queue.blocking_dispatch(&mut st).is_err() {
                fail("wayland dispatch failed");
            }
        }
        let shot = if st.cancelled { None } else { st.chosen_shot() };
        st.close_covers();
        let _ = queue.roundtrip(&mut st);
        match shot {
            Some(s) => s,
            None => std::process::exit(1),
        }
    } else {
        st.stitched().unwrap_or_else(|| fail("no output could be captured"))
    };

    let png = picture.encode_png().unwrap_or_else(|_| fail("could not encode the PNG"));
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()));
    let dir = pictures_dir(&home, std::env::var("XDG_PICTURES_DIR").ok().as_deref(), lang);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        fail(&format!("{}: {e}", dir.display()));
    }
    let path = free_path(&dir, &file_name(lunarbar::files::now_secs(), lang));
    if let Err(e) = std::fs::write(&path, &png) {
        fail(&format!("{}: {e}", path.display()));
    }
    println!("{}", path.display());

    let on_clipboard = st.offer_png(&qh, png);
    announce(&path, picture.width(), picture.height(), lang);
    if !on_clipboard {
        return;
    }
    // Serve the clipboard until someone else takes it.
    let _ = conn.flush();
    while !st.clip_done {
        if queue.blocking_dispatch(&mut st).is_err() {
            break;
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_spans_the_same_rectangle_whichever_way_it_went() {
        let r = Rect { x: 10, y: 20, w: 30, h: 40 };
        assert_eq!(Rect::span((10, 20), (40, 60)), r);
        assert_eq!(Rect::span((40, 60), (10, 20)), r);
        assert_eq!(Rect::span((40, 20), (10, 60)), r);
        assert!(!Rect::span((5, 5), (7, 40)).is_drag(), "a twitch is a click");
        assert_eq!(Rect { x: -5, y: 90, w: 20, h: 20 }.clamp(100, 100), Rect { x: 0, y: 90, w: 15, h: 10 });
        assert_eq!(r.scale(2.0, 2.0), Rect { x: 20, y: 40, w: 60, h: 80 });
    }

    #[test]
    fn outputs_are_stitched_where_they_sit() {
        let b = bounds(&[(0, 0, 1920, 1080), (1920, 0, 1280, 1024)]);
        assert_eq!(b, Rect { x: 0, y: 0, w: 3200, h: 1080 });
        let b = bounds(&[(-1280, 100, 1280, 720), (0, 0, 1920, 1080)]);
        assert_eq!(b, Rect { x: -1280, y: 0, w: 3200, h: 1080 });
    }

    #[test]
    fn a_frame_becomes_an_opaque_picture_the_right_way_up() {
        // 2x2 XRGB: blue, green / red, white; stride with 4 bytes of padding.
        let data = [
            255, 0, 0, 0, 0, 255, 0, 0, 9, 9, 9, 9, //
            0, 0, 255, 0, 255, 255, 255, 0, 9, 9, 9, 9,
        ];
        let p = to_pixmap(&data, 2, 2, 12, false).unwrap();
        let px = |p: &Pixmap, x, y| {
            let c = p.pixel(x, y).unwrap();
            (c.red(), c.green(), c.blue(), c.alpha())
        };
        assert_eq!(px(&p, 0, 0), (0, 0, 255, 255));
        assert_eq!(px(&p, 0, 1), (255, 0, 0, 255));
        let flipped = to_pixmap(&data, 2, 2, 12, true).unwrap();
        assert_eq!(px(&flipped, 0, 0), (255, 0, 0, 255));
        assert!(to_pixmap(&data[..19], 2, 2, 12, false).is_none(), "a short buffer is refused");
        let c = crop(&p, Rect { x: 1, y: 1, w: 5, h: 5 }).unwrap();
        assert_eq!((c.width(), c.height()), (1, 1));
        assert_eq!(px(&c, 0, 0), (255, 255, 255, 255));
    }

    #[test]
    fn the_picture_goes_to_the_pictures_folder_under_a_dated_name() {
        let home = std::env::temp_dir().join(format!("lunarshot-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("Pictures")).unwrap();
        assert_eq!(pictures_dir(&home, None, Lang::Es), home.join("Pictures"), "the folder that exists wins");
        assert_eq!(pictures_dir(&home, Some("$HOME/Capturas"), Lang::Es), home.join("Capturas"));
        std::fs::remove_dir_all(home.join("Pictures")).unwrap();
        assert_eq!(pictures_dir(&home, None, Lang::Es), home.join("Imágenes"));
        let name = file_name(0, Lang::Es);
        assert!(name.starts_with("captura-") && name.ends_with(".png") && name.len() == "captura-1970-01-01-000000.png".len(), "{name}");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("a.png"), b"").unwrap();
        assert_eq!(free_path(&home, "a.png"), home.join("a-2.png"));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(sh_quote("/root/Mis fotos/it's"), "'/root/Mis fotos/it'\\''s'");
    }

    #[test]
    fn the_overlay_draws_with_and_without_a_selection_inside_any_screen() {
        let shot = Pixmap::new(640, 480).unwrap();
        for sel in [None, Some(Rect { x: 100, y: 100, w: 200, h: 120 }), Some(Rect { x: 500, y: 400, w: 140, h: 80 })] {
            for lang in [Lang::Es, Lang::En] {
                let mut cv = Canvas::new(640, 480);
                draw_overlay(&mut cv, 640, 480, &View { shot: &shot, sel, real: (200, 120), hint: sel.is_none(), lang });
            }
        }
    }
}
