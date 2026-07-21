//! lunarbar — Eclipse OS's native panel stack (its waybar replacement).
//!
//! Why not waybar: waybar is a GTK application. GTK's GApplication registers on
//! the session D-Bus at startup, so on a system with no session bus waybar
//! prints "Could not connect: Connection refused" and exits before it ever maps
//! its panel — and it further pulls in gdk-pixbuf, fontconfig and a UTF-8 locale
//! to draw anything. lunarbar instead is a single static musl binary over
//! wlr-layer-shell + wl_shm, with its own 5x7 font and /proc readers: NO GTK,
//! NO D-Bus, NO gdk-pixbuf, NO fontconfig, NO locale. It just works.
//!
//! Two bars per output:
//! - a TOP bar with system info — crescent launcher, uptime, load on the left;
//!   CPU%, MEM%, network, temperature, disk, battery and the clock (with load
//!   gauges) on the right;
//! - a BOTTOM taskbar listing every open window via wlr-foreign-toplevel-
//!   management (no D-Bus), click to focus/raise, the active window highlighted.
//!
//! Both bars reserve their height as an exclusive zone so maximised windows do
//! not cover them, and repaint once a second.
//!
//! Env knobs:
//! - `LUNARBAR_HEIGHT=N`   — bar height in px (default 30, applies to both).
//! - `LUNARBAR_TERMINAL=…` — command run on launcher click (default
//!   /usr/local/bin/eclipse-terminal).
//! - `LUNARBAR_DUMP=/path:WxH` — render one INFO bar to a raw XRGB8888 file and
//!   exit (no compositor needed), for offline verification.

mod draw;
mod sysinfo;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use draw::{Canvas, Rgb};
use sysinfo::{CpuMeter, NetMeter, NetRate};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
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

// ── Palette (matches the Eclipse-Dark labwc theme) ───────────────────────────
const BAR_BG: Rgb = (0x14, 0x10, 0x22); // deep night
const BAR_RULE: Rgb = (0x6b, 0x5a, 0xa8); // violet accent rule
const TEXT: Rgb = (0xe0, 0xdc, 0xf4); // lavender
const ACCENT: Rgb = (0x9b, 0x8a, 0xe0); // violet — launcher, clock, active
const DIM: Rgb = (0x83, 0x7d, 0xa0); // muted labels
const BTN_BG: Rgb = (0x1e, 0x19, 0x30); // idle task button
const BTN_ACTIVE_BG: Rgb = (0x2c, 0x24, 0x4a); // focused task button

const BUFFERS: usize = 2;

/// activated-state value from wlr-foreign-toplevel-management-unstable-v1
/// (the `state` array carries u32 enum values; Activated == 2).
const TOPLEVEL_STATE_ACTIVATED: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Top bar: system metrics.
    Info,
    /// Bottom bar: window list.
    Task,
}

struct Bar {
    role: Role,
    #[allow(dead_code)]
    output: wl_output::WlOutput,
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    width: u32,
    height: u32,
    map: *mut u8,
    map_len: usize,
    buffers: [Option<wl_buffer::WlBuffer>; BUFFERS],
    busy: [bool; BUFFERS],
    next: usize,
    configured: bool,
    /// x-range [x0,x1) of the launcher hitbox (Info bars only).
    launcher_hit: (i32, i32),
    /// (x0,x1,toplevel-index) click targets for each window button (Task bars).
    task_hits: Vec<(i32, i32, usize)>,
}

impl Drop for Bar {
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

/// One entry in the taskbar, mirrored from a foreign-toplevel handle.
struct Toplevel {
    handle: ZwlrForeignToplevelHandleV1,
    title: String,
    app_id: String,
    activated: bool,
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    foreign_mgr: Option<ZwlrForeignToplevelManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    pending_outputs: Vec<wl_output::WlOutput>,
    bars: Vec<Bar>,
    toplevels: Vec<Toplevel>,
    height: u32,
    terminal: String,
    cpu: CpuMeter,
    net: NetMeter,
    // pointer tracking for clicks
    ptr_x: f64,
    ptr_y: f64,
    ptr_bar: Option<u32>, // layer id the pointer is over
}

impl State {
    fn ensure_surfaces(&mut self, qh: &QueueHandle<State>) {
        let (Some(compositor), Some(layer_shell)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        let outputs: Vec<_> = self.pending_outputs.drain(..).collect();
        for output in outputs {
            // Each output gets a top info bar and a bottom taskbar.
            for (role, edge) in [(Role::Info, Anchor::Top), (Role::Task, Anchor::Bottom)] {
                let surface = compositor.create_surface(qh, ());
                let layer = layer_shell.get_layer_surface(
                    &surface,
                    Some(&output),
                    zwlr_layer_shell_v1::Layer::Top,
                    "panel".into(),
                    qh,
                    (),
                );
                layer.set_anchor(edge | Anchor::Left | Anchor::Right);
                layer.set_size(0, self.height);
                layer.set_exclusive_zone(self.height as i32);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                surface.commit();
                self.bars.push(Bar {
                    role,
                    output: output.clone(),
                    surface,
                    layer,
                    width: 0,
                    height: self.height,
                    map: std::ptr::null_mut(),
                    map_len: 0,
                    buffers: [None, None],
                    busy: [false, false],
                    next: 0,
                    configured: false,
                    launcher_hit: (0, 0),
                    task_hits: Vec::new(),
                });
            }
        }
    }

    fn bar_index(&self, layer_id: u32) -> Option<usize> {
        self.bars
            .iter()
            .position(|b| b.layer.id().protocol_id() == layer_id)
    }

    /// (Re)allocate the shm pool for a bar after a configure with a new size.
    fn configure(&mut self, qh: &QueueHandle<State>, layer_id: u32, w: u32, h: u32) {
        let Some(idx) = self.bar_index(layer_id) else {
            return;
        };
        let w = w.max(1);
        let h = h.max(1);
        if self.bars[idx].configured && self.bars[idx].width == w && self.bars[idx].height == h {
            self.render(layer_id);
            return;
        }
        let Some(shm) = &self.shm else { return };

        // Tear down any previous mapping/buffers.
        {
            let bar = &mut self.bars[idx];
            for b in bar.buffers.iter_mut() {
                if let Some(b) = b.take() {
                    b.destroy();
                }
            }
            if !bar.map.is_null() {
                unsafe { libc::munmap(bar.map as *mut libc::c_void, bar.map_len) };
                bar.map = std::ptr::null_mut();
            }
        }

        let stride = w as usize * 4;
        let frame_size = stride * h as usize;
        let total = frame_size * BUFFERS;

        let raw = unsafe {
            libc::memfd_create(b"lunarbar\0".as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC)
        };
        if raw < 0 {
            eprintln!("lunarbar: memfd_create failed");
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(raw, total as libc::off_t) } != 0 {
            eprintln!("lunarbar: ftruncate failed");
            return;
        }
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            eprintln!("lunarbar: mmap failed");
            return;
        }
        let map = map as *mut u8;

        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
        let mk = |i: usize| {
            pool.create_buffer(
                (i * frame_size) as i32,
                w as i32,
                h as i32,
                stride as i32,
                wl_shm::Format::Xrgb8888,
                qh,
                (layer_id, i),
            )
        };
        let buffers = [Some(mk(0)), Some(mk(1))];
        pool.destroy();

        let bar = &mut self.bars[idx];
        bar.width = w;
        bar.height = h;
        bar.map = map;
        bar.map_len = total;
        bar.buffers = buffers;
        bar.busy = [false, false];
        bar.next = 0;
        bar.configured = true;

        self.render(layer_id);
    }

    fn render(&mut self, layer_id: u32) {
        let Some(idx) = self.bar_index(layer_id) else {
            return;
        };
        if !self.bars[idx].configured || self.bars[idx].map.is_null() {
            return;
        }
        match self.bars[idx].role {
            Role::Info => self.render_info(idx),
            Role::Task => self.render_task(idx),
        }
    }

    /// Paint the top info bar: launcher + system metrics.
    fn render_info(&mut self, idx: usize) {
        // Sample everything BEFORE borrowing the bar mutably.
        let m = InfoMetrics {
            cpu: self.cpu.sample(),
            mem: sysinfo::mem_percent(),
            clock: sysinfo::clock_hhmm(),
            net: self.net.sample(),
            uptime: sysinfo::uptime(),
            load: sysinfo::loadavg(),
            disk: sysinfo::disk_root_percent(),
            temp: sysinfo::temp_c(),
            batt: sysinfo::battery(),
        };

        let (w, h) = (self.bars[idx].width as usize, self.bars[idx].height as usize);
        let frame_size = w * h * 4;
        let bar = &mut self.bars[idx];
        let i = pick_buffer(bar);
        let data: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(bar.map.add(i * frame_size), frame_size) };
        let mut cv = Canvas { data, w, h };

        let launcher_hit = draw_info(&mut cv, w, h, &m);
        bar.launcher_hit = launcher_hit;

        commit_bar(bar, i, w, h);
    }

    /// Paint the bottom taskbar: one button per open window.
    fn render_task(&mut self, idx: usize) {
        // Snapshot the window list first (labels + active flag) to avoid holding
        // a borrow of self.toplevels while we mutate the bar.
        let items: Vec<(String, bool)> = self
            .toplevels
            .iter()
            .map(|t| (button_label(t), t.activated))
            .collect();

        let (w, h) = (self.bars[idx].width as usize, self.bars[idx].height as usize);
        let frame_size = w * h * 4;
        let bar = &mut self.bars[idx];
        let i = pick_buffer(bar);
        let data: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(bar.map.add(i * frame_size), frame_size) };
        let mut cv = Canvas { data, w, h };

        bar.task_hits = draw_task(&mut cv, w, h, &items);

        commit_bar(bar, i, w, h);
    }

    fn render_all(&mut self) {
        let ids: Vec<u32> = self
            .bars
            .iter()
            .filter(|b| b.configured)
            .map(|b| b.layer.id().protocol_id())
            .collect();
        for id in ids {
            self.render(id);
        }
    }

    fn render_task_bars(&mut self) {
        let ids: Vec<u32> = self
            .bars
            .iter()
            .filter(|b| b.configured && b.role == Role::Task)
            .map(|b| b.layer.id().protocol_id())
            .collect();
        for id in ids {
            self.render(id);
        }
    }

    /// Launcher click: spawn the terminal, detached.
    fn spawn_terminal(&self) {
        let cmd = self.terminal.clone();
        unsafe {
            let pid = libc::fork();
            if pid == 0 {
                libc::setsid();
                let sh = b"/bin/sh\0";
                let dashc = b"-c\0";
                let c = std::ffi::CString::new(cmd).unwrap();
                let argv = [
                    sh.as_ptr() as *const libc::c_char,
                    dashc.as_ptr() as *const libc::c_char,
                    c.as_ptr(),
                    std::ptr::null(),
                ];
                libc::execv(sh.as_ptr() as *const libc::c_char, argv.as_ptr());
                libc::_exit(127);
            }
            if pid > 0 {
                let mut st = 0;
                libc::waitpid(pid, &mut st, libc::WNOHANG);
            }
        }
    }

    /// Taskbar click: activate (focus + raise) the window under the pointer.
    fn activate_toplevel(&self, k: usize) {
        let (Some(seat), Some(t)) = (&self.seat, self.toplevels.get(k)) else {
            return;
        };
        t.handle.activate(seat);
    }
}

// ── Buffer helpers ───────────────────────────────────────────────────────────

/// Choose a released buffer (or overwrite a busy one rather than stall).
fn pick_buffer(bar: &mut Bar) -> usize {
    let i = if !bar.busy[bar.next] {
        bar.next
    } else if !bar.busy[1 - bar.next] {
        1 - bar.next
    } else {
        bar.next
    };
    bar.next = 1 - i;
    bar.busy[i] = true;
    i
}

fn commit_bar(bar: &mut Bar, i: usize, w: usize, h: usize) {
    if let Some(buf) = bar.buffers[i].as_ref() {
        bar.surface.attach(Some(buf), 0, 0);
        bar.surface.damage_buffer(0, 0, w as i32, h as i32);
        bar.surface.commit();
    }
}

// ── Info-bar drawing (shared by render_info and the offscreen dump) ───────────

struct InfoMetrics {
    cpu: Option<u32>,
    mem: Option<u32>,
    clock: String,
    net: Option<NetRate>,
    uptime: Option<String>,
    load: Option<f32>,
    disk: Option<u32>,
    temp: Option<u32>,
    batt: Option<(u32, bool)>,
}

/// Draw a right-anchored metric: an optional load gauge then its label, the
/// whole unit ending at `right`. Returns the module's left x.
fn metric(
    cv: &mut Canvas,
    right: i32,
    ty: i32,
    h: i32,
    scale: i32,
    label: &str,
    gauge: Option<f32>,
    col: Rgb,
) -> i32 {
    let tw = Canvas::text_width(label, scale);
    let (gw, gpad) = if gauge.is_some() {
        (5 * scale, 4)
    } else {
        (0, 0)
    };
    let total = gw + gpad + tw;
    let x = right - total;
    if let Some(f) = gauge {
        let ghh = (2 * scale).max(4);
        let gy = (h - ghh) / 2;
        cv.gauge(x, gy, gw, ghh, f, DIM);
    }
    cv.text(label, x + gw + gpad, ty, scale, col);
    x
}

/// Draw the network module: ▼<down>  ▲<up>, right-anchored at `right`.
fn net_module(cv: &mut Canvas, right: i32, ty: i32, h: i32, scale: i32, n: &NetRate) -> i32 {
    let down = sysinfo::fmt_rate(n.down);
    let up = sysinfo::fmt_rate(n.up);
    let ts = (2 * scale + 1).min(h - 2);
    let ty_tri = (h - ts) / 2;
    let dw = Canvas::text_width(&down, scale);
    let uw = Canvas::text_width(&up, scale);
    let total = ts + 3 + dw + 8 + ts + 3 + uw;
    let x = right - total;
    let mut cx = x;
    cv.triangle(cx, ty_tri, ts, false, ACCENT); // download ▼
    cx += ts + 3;
    cx += cv.text(&down, cx, ty, scale, TEXT);
    cx += 8;
    cv.triangle(cx, ty_tri, ts, true, ACCENT); // upload ▲
    cx += ts + 3;
    cv.text(&up, cx, ty, scale, TEXT);
    x
}

/// Paint the whole info bar. Returns the launcher hitbox (x0,x1).
fn draw_info(cv: &mut Canvas, w: usize, h: usize, m: &InfoMetrics) -> (i32, i32) {
    cv.clear(BAR_BG);
    cv.hline(0, h as i32 - 1, w as i32, BAR_RULE, 0.85); // bottom accent rule

    let scale = ((h as i32 - 8) / 7).clamp(2, 4);
    let gh = 7 * scale;
    let ty = (h as i32 - gh) / 2;
    let pad = 8;

    // ── left: crescent launcher + label, then uptime and load ──
    let icon_d = gh;
    cv.crescent(pad, ty, icon_d, ACCENT);
    let after_icon = pad + icon_d + 6;
    let label_w = cv.text("ECLIPSE", after_icon, ty, scale, TEXT);
    let launch_x1 = after_icon + label_w + 6;
    let launcher_hit = (pad, launch_x1);

    let mut lx = launch_x1 + 6;
    if let Some(up) = &m.uptime {
        cv.sep(lx, h as i32, DIM);
        lx += 8;
        lx += cv.text(&format!("UP {up}"), lx, ty, scale, DIM);
        lx += 8;
    }
    if let Some(load) = m.load {
        cv.sep(lx, h as i32, DIM);
        lx += 8;
        cv.text(&format!("LOAD {load:.2}"), lx, ty, scale, DIM);
    }

    // ── right: clock, mem, cpu, temp, disk, net, battery (right-to-left) ──
    let hi = h as i32;
    let mut rx = w as i32 - pad;

    rx = metric(cv, rx, ty, hi, scale, &m.clock, None, ACCENT);

    rx -= 8;
    cv.sep(rx, hi, DIM);
    rx -= 8;
    let mem_s = format!("MEM {}%", opt(m.mem));
    rx = metric(cv, rx, ty, hi, scale, &mem_s, m.mem.map(|v| v as f32 / 100.0), TEXT);

    rx -= 8;
    cv.sep(rx, hi, DIM);
    rx -= 8;
    let cpu_s = format!("CPU {}%", opt(m.cpu));
    rx = metric(cv, rx, ty, hi, scale, &cpu_s, m.cpu.map(|v| v as f32 / 100.0), TEXT);

    if let Some(t) = m.temp {
        rx -= 8;
        cv.sep(rx, hi, DIM);
        rx -= 8;
        rx = metric(cv, rx, ty, hi, scale, &format!("{t}C"), None, TEXT);
    }
    if let Some(d) = m.disk {
        rx -= 8;
        cv.sep(rx, hi, DIM);
        rx -= 8;
        rx = metric(cv, rx, ty, hi, scale, &format!("DISK {d}%"), Some(d as f32 / 100.0), TEXT);
    }
    if let Some(n) = &m.net {
        if n.link {
            rx -= 8;
            cv.sep(rx, hi, DIM);
            rx -= 8;
            rx = net_module(cv, rx, ty, hi, scale, n);
        }
    }
    if let Some((b, ch)) = m.batt {
        rx -= 8;
        cv.sep(rx, hi, DIM);
        rx -= 8;
        let label = if ch {
            format!("BAT {b}% CH")
        } else {
            format!("BAT {b}%")
        };
        let _ = metric(cv, rx, ty, hi, scale, &label, Some(b as f32 / 100.0), TEXT);
    }

    launcher_hit
}

/// Paint the taskbar: one button per open window, active one highlighted.
/// Returns the click hitboxes (x0,x1,index). Shared by render_task and the
/// offscreen preview.
fn draw_task(cv: &mut Canvas, w: usize, h: usize, items: &[(String, bool)]) -> Vec<(i32, i32, usize)> {
    cv.clear(BAR_BG);
    cv.hline(0, 0, w as i32, BAR_RULE, 0.85); // top accent rule

    let scale = ((h as i32 - 8) / 7).clamp(2, 4);
    let gh = 7 * scale;
    let ty = (h as i32 - gh) / 2;
    let btn_pad = 8;
    let btn_h = gh + 6;
    let btn_y = (h as i32 - btn_h) / 2;
    let gap = 6;
    let pad = 8;

    let mut hits = Vec::new();
    let mut x = pad;
    for (k, (label, active)) in items.iter().enumerate() {
        let tw = Canvas::text_width(label, scale);
        let bw = tw + 2 * btn_pad;
        if x + bw > w as i32 - pad {
            break; // out of room; stop rather than overflow
        }
        let bg = if *active { BTN_ACTIVE_BG } else { BTN_BG };
        cv.fill_rect(x, btn_y, bw, btn_h, bg);
        if *active {
            // accent underline for the focused window
            cv.hline(x, btn_y + btn_h - 1, bw, ACCENT, 1.0);
            cv.hline(x, btn_y + btn_h - 2, bw, ACCENT, 0.5);
        }
        let fg = if *active { TEXT } else { DIM };
        cv.text(label, x + btn_pad, ty, scale, fg);
        hits.push((x, x + bw, k));
        x += bw + gap;
    }
    hits
}

/// "42" or "--" for an optional percentage.
fn opt(v: Option<u32>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "--".into())
}

/// A short, font-renderable button label for a window: prefer app_id, fall back
/// to the title, capped so buttons stay a sane width.
fn button_label(t: &Toplevel) -> String {
    let src = if !t.app_id.is_empty() {
        &t.app_id
    } else {
        &t.title
    };
    let src = src.trim();
    let src = if src.is_empty() { "WINDOW" } else { src };
    let mut s: String = src.chars().take(18).collect();
    if src.chars().count() > 18 {
        s.push('.');
    }
    s
}

// ── Wayland dispatch ─────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
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
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "zwlr_foreign_toplevel_manager_v1" => {
                    state.foreign_mgr = Some(registry.bind(name, version.min(3), qh, ()))
                }
                "wl_output" => {
                    let o: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.pending_outputs.push(o);
                }
                "wl_seat" => {
                    let seat: wl_seat::WlSeat = registry.bind(name, version.min(5), qh, ());
                    state.pointer = Some(seat.get_pointer(qh, ()));
                    state.seat = Some(seat);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
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
                state.configure(qh, layer.id().protocol_id(), width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                let id = layer.id().protocol_id();
                if let Some(pos) = state.bar_index(id) {
                    state.bars.remove(pos);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (u32, usize)> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        (layer_id, i): &(u32, usize),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(idx) = state.bar_index(*layer_id) {
                state.bars[idx].busy[*i] = false;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.ptr_x = surface_x;
                state.ptr_y = surface_y;
                state.ptr_bar = state
                    .bars
                    .iter()
                    .find(|b| b.surface.id() == surface.id())
                    .map(|b| b.layer.id().protocol_id());
            }
            wl_pointer::Event::Leave { .. } => state.ptr_bar = None,
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.ptr_x = surface_x;
                state.ptr_y = surface_y;
            }
            wl_pointer::Event::Button {
                button, state: bs, ..
            } => {
                // BTN_LEFT = 0x110; act on press.
                let pressed = matches!(bs, WEnum::Value(wl_pointer::ButtonState::Pressed));
                if pressed && button == 0x110 {
                    if let Some(id) = state.ptr_bar {
                        if let Some(idx) = state.bar_index(id) {
                            let x = state.ptr_x as i32;
                            match state.bars[idx].role {
                                Role::Info => {
                                    let (hx0, hx1) = state.bars[idx].launcher_hit;
                                    if x >= hx0 && x < hx1 {
                                        state.spawn_terminal();
                                    }
                                }
                                Role::Task => {
                                    let hit = state.bars[idx]
                                        .task_hits
                                        .iter()
                                        .find(|(x0, x1, _)| x >= *x0 && x < *x1)
                                        .map(|(_, _, k)| *k);
                                    if let Some(k) = hit {
                                        state.activate_toplevel(k);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ── Foreign-toplevel (taskbar source) ────────────────────────────────────────

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(Toplevel {
                handle: toplevel,
                title: String::new(),
                app_id: String::new(),
                activated: false,
            });
        }
    }

    // The `toplevel` event creates a new handle object; tell wayland-client how
    // to type its user-data.
    wayland_client::event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event as E;
        let id = handle.id();
        match event {
            E::Title { title } => {
                if let Some(t) = state.toplevels.iter_mut().find(|t| t.handle.id() == id) {
                    t.title = title;
                }
            }
            E::AppId { app_id } => {
                if let Some(t) = state.toplevels.iter_mut().find(|t| t.handle.id() == id) {
                    t.app_id = app_id;
                }
            }
            E::State { state: bytes } => {
                let mut activated = false;
                for c in bytes.chunks_exact(4) {
                    let v = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                    if v == TOPLEVEL_STATE_ACTIVATED {
                        activated = true;
                    }
                }
                if let Some(t) = state.toplevels.iter_mut().find(|t| t.handle.id() == id) {
                    t.activated = activated;
                }
            }
            E::Done => state.render_task_bars(),
            E::Closed => {
                handle.destroy();
                state.toplevels.retain(|t| t.handle.id() != id);
                state.render_task_bars();
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
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

fn main() {
    let height: u32 = std::env::var("LUNARBAR_HEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|h| (16..=64).contains(h))
        .unwrap_or(30);
    let terminal = std::env::var("LUNARBAR_TERMINAL")
        .unwrap_or_else(|_| "/usr/local/bin/eclipse-terminal".into());

    // Offscreen preview for offline verification: render BOTH bars as they'd sit
    // on screen — top info bar, wallpaper gap, bottom taskbar — to a raw
    // XRGB8888 file. Spec is `path:WxH`; H is the full preview height (the bars
    // are `LUNARBAR_HEIGHT` tall each, top-anchored and bottom-anchored).
    if let Ok(spec) = std::env::var("LUNARBAR_DUMP") {
        let (path, w, full_h) = match spec.rsplit_once(':') {
            Some((p, dims)) if dims.contains('x') => {
                let (ws, hs) = dims.split_once('x').unwrap();
                (p.to_string(), ws.parse().unwrap_or(1280), hs.parse().unwrap_or(220))
            }
            _ => (spec, 1280usize, 220usize),
        };
        let bh = (height as usize).min(full_h / 2);
        let mut buf = vec![0u8; w * full_h * 4];

        // Wallpaper-tone fill for the gap between the bars.
        const WALL: Rgb = (0x0c, 0x0a, 0x18);
        {
            let mut cv = Canvas { data: &mut buf, w, h: full_h };
            cv.clear(WALL);
        }

        let mut cpu = CpuMeter::default();
        let mut net = NetMeter::default();
        let _ = cpu.sample();
        let _ = net.sample();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let m = InfoMetrics {
            cpu: cpu.sample(),
            mem: sysinfo::mem_percent(),
            clock: sysinfo::clock_hhmm(),
            net: net.sample(),
            uptime: sysinfo::uptime(),
            load: sysinfo::loadavg(),
            disk: sysinfo::disk_root_percent(),
            temp: sysinfo::temp_c(),
            batt: sysinfo::battery(),
        };

        // Top info bar occupies rows [0, bh).
        {
            let top = &mut buf[..w * bh * 4];
            let mut cv = Canvas { data: top, w, h: bh };
            draw_info(&mut cv, w, bh, &m);
        }
        // Bottom taskbar occupies rows [full_h-bh, full_h) with sample windows.
        {
            let off = (full_h - bh) * w * 4;
            let bot = &mut buf[off..off + w * bh * 4];
            let mut cv = Canvas { data: bot, w, h: bh };
            let sample = [
                ("FOOT".to_string(), true),
                ("ECLIPSE-FILES".to_string(), false),
                ("LUNARBG".to_string(), false),
            ];
            draw_task(&mut cv, w, bh, &sample);
        }

        std::fs::write(&path, buf).expect("write dump");
        eprintln!("lunarbar: dumped {w}x{full_h} XRGB8888 (both bars) to {path}");
        return;
    }

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lunarbar: cannot connect to the Wayland compositor: {e}");
            std::process::exit(1);
        }
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = State {
        height,
        terminal,
        ..State::default()
    };
    if let Err(e) = queue.roundtrip(&mut state) {
        eprintln!("lunarbar: initial roundtrip failed: {e}");
        std::process::exit(1);
    }
    if state.layer_shell.is_none() {
        eprintln!("lunarbar: compositor lacks zwlr_layer_shell_v1");
        std::process::exit(1);
    }
    if state.compositor.is_none() || state.shm.is_none() {
        eprintln!("lunarbar: missing wl_compositor or wl_shm");
        std::process::exit(1);
    }
    if state.foreign_mgr.is_none() {
        eprintln!("lunarbar: no wlr-foreign-toplevel-management — taskbar will be empty");
    }

    // 1 Hz repaint: enough for clock/cpu/mem/net, negligible load on the
    // software-rendered stack.
    let interval = std::time::Duration::from_secs(1);
    let mut next_tick = std::time::Instant::now() + interval;

    loop {
        state.ensure_surfaces(&qh);
        if let Err(e) = queue.flush() {
            eprintln!("lunarbar: connection lost: {e}");
            std::process::exit(1);
        }
        if let Some(guard) = queue.prepare_read() {
            let timeout_ms = next_tick
                .saturating_duration_since(std::time::Instant::now())
                .as_millis()
                .min(1000) as i32;
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms.max(0)) };
            if ready > 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        if let Err(e) = queue.dispatch_pending(&mut state) {
            eprintln!("lunarbar: protocol error: {e}");
            std::process::exit(1);
        }
        if std::time::Instant::now() >= next_tick {
            state.render_all();
            next_tick += interval;
            let now = std::time::Instant::now();
            if next_tick < now {
                next_tick = now + interval;
            }
        }
    }
}
