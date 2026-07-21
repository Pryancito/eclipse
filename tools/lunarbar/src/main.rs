//! lunarbar — Eclipse OS's native panel stack (its waybar replacement).
//!
//! Why not waybar (or Ironbar/Eww/Riftbar): they are GTK applications. GTK's
//! GApplication registers on the session D-Bus at startup, so on a system with
//! no session bus the panel prints "Could not connect: Connection refused" and
//! exits before it ever maps — and GTK further pulls in gdk-pixbuf, fontconfig
//! and a UTF-8 locale to draw anything. lunarbar instead is a single static
//! musl binary over wlr-layer-shell + wl_shm, with its own 5x7 font and /proc
//! readers: NO GTK, NO D-Bus, NO gdk-pixbuf, NO fontconfig, NO locale.
//!
//! Two bars per output, sharing the visual language of the waybar config this
//! replaces (rgba(15,12,26) ground, #6b5aa8 2px rule, rounded pills):
//! - BOTTOM (the classic waybar layout, replicated): ◑ launcher, then one
//!   rounded button per open window via wlr-foreign-toplevel-management
//!   (click to focus/raise, active button #3a3357 with white text); on the
//!   right `cpu N%`, `mem N%`, and the bold clock in its violet pill.
//! - TOP (system info): ☾ eclipse wordmark, uptime and load on the left;
//!   network ▼/▲ throughput, disk, temperature and battery (auto-hidden when
//!   absent) with load-tinted mini gauges, and the date pill on the right.
//!
//! Both bars reserve their height as an exclusive zone and repaint at 1 Hz.
//!
//! Env knobs:
//! - `LUNARBAR_HEIGHT=N`   — bar height in px (default 34, waybar's height).
//! - `LUNARBAR_TERMINAL=…` — command run on launcher click (default
//!   /usr/local/bin/eclipse-terminal).
//! - `LUNARBAR_DUMP=/path:WxH` — render both bars (top + bottom, wallpaper gap
//!   between) to a raw XRGB8888 file and exit, for offline verification.

mod draw;
mod sysinfo;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use draw::{Canvas, Rgb, GLYPH_H};
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

// ── Palette: lifted from the waybar CSS this bar replicates ──────────────────
const BAR_BG: Rgb = (0x0f, 0x0c, 0x1a); // window#waybar background
const BAR_RULE: Rgb = (0x6b, 0x5a, 0xa8); // 2px border, violet
const TEXT: Rgb = (0xe8, 0xe4, 0xf8); // primary text
const MUTED: Rgb = (0xc9, 0xc4, 0xe4); // module text (cpu/mem/buttons)
const LAUNCH: Rgb = (0xb9, 0xa8, 0xff); // launcher glyph
const PILL: Rgb = (0x29, 0x23, 0x3f); // clock/date pill background
const BTN_ACTIVE: Rgb = (0x3a, 0x33, 0x57); // active taskbar button
const WHITE: Rgb = (0xff, 0xff, 0xff); // active button text

const BUFFERS: usize = 2;

/// activated-state value from wlr-foreign-toplevel-management-unstable-v1
/// (the `state` array carries u32 enum values; Activated == 2).
const TOPLEVEL_STATE_ACTIVATED: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Top bar: system metrics.
    Info,
    /// Bottom bar: waybar-style launcher + taskbar + cpu/mem/clock.
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
    /// x-range [x0,x1) of the launcher hitbox.
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

/// One coherent snapshot of every metric both bars draw. Refreshed once per
/// 1 Hz tick (a single /proc pass; sampling CpuMeter twice per tick would
/// zero its delta).
#[derive(Default, Clone)]
struct Metrics {
    cpu: Option<u32>,
    mem: Option<u32>,
    clock: String,
    date: String,
    net: Option<NetRate>,
    uptime: Option<String>,
    load: Option<f32>,
    disk: Option<u32>,
    temp: Option<u32>,
    batt: Option<(u32, bool)>,
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
    metrics: Metrics,
    // pointer tracking for clicks
    ptr_x: f64,
    ptr_y: f64,
    ptr_bar: Option<u32>, // layer id the pointer is over
}

impl State {
    /// One metrics pass per tick, shared by every bar render until the next.
    fn refresh_metrics(&mut self) {
        self.metrics = Metrics {
            cpu: self.cpu.sample(),
            mem: sysinfo::mem_percent(),
            clock: sysinfo::clock_hhmm(),
            date: sysinfo::date_dm(),
            net: self.net.sample(),
            uptime: sysinfo::uptime(),
            load: sysinfo::loadavg(),
            disk: sysinfo::disk_root_percent(),
            temp: sysinfo::temp_c(),
            batt: sysinfo::battery(),
        };
    }

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

        // Tear down any previous mapping/buffers. Mark unconfigured until the
        // replacement is fully in place: if an allocation below fails and we
        // bail, render() must not run against the torn-down state (and stale
        // task_hits must not keep resolving clicks against a frozen frame).
        {
            let bar = &mut self.bars[idx];
            bar.configured = false;
            bar.task_hits.clear();
            bar.launcher_hit = (0, 0);
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
        let m = self.metrics.clone();
        match self.bars[idx].role {
            Role::Info => {
                let (w, h) = (self.bars[idx].width as usize, self.bars[idx].height as usize);
                let frame_size = w * h * 4;
                let bar = &mut self.bars[idx];
                let Some(i) = pick_buffer(bar) else {
                    return; // both buffers held by the compositor; retry next tick
                };
                let mut cv = Canvas::new(w, h);
                let launcher_hit = draw_info(&mut cv, w, h, &m);
                let bar = &mut self.bars[idx];
                let data: &mut [u8] = unsafe {
                    std::slice::from_raw_parts_mut(bar.map.add(i * frame_size), frame_size)
                };
                cv.blit_xrgb(data);
                bar.launcher_hit = launcher_hit;
                commit_bar(bar, i, w, h);
            }
            Role::Task => {
                // Snapshot the window list first (labels + active flag) to
                // avoid borrowing self.toplevels while mutating the bar.
                let items: Vec<(String, bool)> = self
                    .toplevels
                    .iter()
                    .map(|t| (button_label(t), t.activated))
                    .collect();
                let (w, h) = (self.bars[idx].width as usize, self.bars[idx].height as usize);
                let frame_size = w * h * 4;
                let bar = &mut self.bars[idx];
                let Some(i) = pick_buffer(bar) else {
                    return; // both buffers held by the compositor; retry next tick
                };
                let mut cv = Canvas::new(w, h);
                let (launcher_hit, hits) = draw_task(&mut cv, w, h, &items, &m);
                let bar = &mut self.bars[idx];
                let data: &mut [u8] = unsafe {
                    std::slice::from_raw_parts_mut(bar.map.add(i * frame_size), frame_size)
                };
                cv.blit_xrgb(data);
                bar.launcher_hit = launcher_hit;
                bar.task_hits = hits;
                commit_bar(bar, i, w, h);
            }
        }
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

    /// Launcher click: spawn the terminal, detached via double-fork. The
    /// intermediate child exits immediately (parent reaps it with a blocking
    /// waitpid, which returns at once), so the grandchild running the terminal
    /// is reparented to init — lunarbar never accumulates zombies.
    fn spawn_terminal(&self) {
        let cmd = self.terminal.clone();
        unsafe {
            let pid = libc::fork();
            if pid == 0 {
                // Intermediate child: new session, fork the real child, exit.
                libc::setsid();
                if libc::fork() == 0 {
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
                libc::_exit(0);
            }
            if pid > 0 {
                let mut st = 0;
                libc::waitpid(pid, &mut st, 0);
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

/// Choose a released buffer. Returns None when the compositor still holds
/// both — the caller skips this frame (the next 1 Hz tick retries) instead of
/// writing into shm the compositor may be reading, which both tears and
/// corrupts the busy[] accounting via the stale Release that would follow a
/// double-attach.
fn pick_buffer(bar: &mut Bar) -> Option<usize> {
    let i = if !bar.busy[bar.next] {
        bar.next
    } else if !bar.busy[1 - bar.next] {
        1 - bar.next
    } else {
        return None;
    };
    bar.next = 1 - i;
    bar.busy[i] = true;
    Some(i)
}

fn commit_bar(bar: &mut Bar, i: usize, w: usize, h: usize) {
    if let Some(buf) = bar.buffers[i].as_ref() {
        bar.surface.attach(Some(buf), 0, 0);
        bar.surface.damage_buffer(0, 0, w as i32, h as i32);
        bar.surface.commit();
    }
}

// ── Bottom bar: the waybar layout, replicated ────────────────────────────────

/// Paint the taskbar exactly like the waybar config it replaces:
/// `◑ | [window buttons] … cpu N%  mem N%  [HH:MM]`. Returns the launcher
/// hitbox and per-button hitboxes.
fn draw_task(
    cv: &mut Canvas,
    w: usize,
    h: usize,
    items: &[(String, bool)],
    m: &Metrics,
) -> ((i32, i32), Vec<(i32, i32, usize)>) {
    cv.clear(BAR_BG);
    // border-top: 2px solid #6b5aa8
    cv.hline(0, 0, w as i32, BAR_RULE, 1.0);
    cv.hline(0, 1, w as i32, BAR_RULE, 1.0);

    let ty = (h as i32 + 2 - GLYPH_H) / 2; // text cell top, below the border
    let btn_h = h as i32 - 10; // waybar: margin 3px + 2px border
    let btn_y = (h as i32 - btn_h) / 2 + 1;

    // ── left: ◑ launcher (padding 0 10px, like #custom-launcher) ──
    let d = (h as i32 * 18) / 34; // ≈18px glyph in a 34px bar
    let ly = (h as i32 - d) / 2;
    cv.disc_half(10, ly, d, LAUNCH);
    let launcher_hit = (0, 10 + d + 10);

    // ── right side first, so the taskbar knows where to stop ──
    // modules-right: cpu, memory, clock  →  right-to-left: clock pill, mem, cpu.
    // On very narrow outputs, drop modules that would cross into the launcher
    // instead of overprinting it (lowest-priority module drops first).
    let left_min = launcher_hit.1 + 8;
    let mut rx = w as i32 - 4;
    {
        // clock: rounded pill, bold, #29233f / #e8e4f8
        let pw = Canvas::text_width(&m.clock) + 20;
        if rx - pw >= left_min {
            rx -= pw;
            cv.round_rect(rx, btn_y, pw, btn_h, 6, PILL);
            cv.text_bold(&m.clock, rx + 10, ty, TEXT);
            rx -= 10;
        }

        let mem_s = format!("mem {}%", opt(m.mem));
        let mw = Canvas::text_width(&mem_s) + 10;
        if rx - mw >= left_min {
            rx -= mw;
            cv.text(&mem_s, rx, ty, MUTED);
            rx -= 10;
        }

        let cpu_s = format!("cpu {}%", opt(m.cpu));
        let cw = Canvas::text_width(&cpu_s) + 10;
        if rx - cw >= left_min {
            rx -= cw;
            cv.text(&cpu_s, rx, ty, MUTED);
            rx -= 10;
        }
    }

    // ── taskbar buttons: rounded 6px, active #3a3357 + white ──
    let mut hits = Vec::new();
    let mut x = launcher_hit.1;
    for (k, (label, active)) in items.iter().enumerate() {
        let tw = Canvas::text_width(label);
        let bw = tw + 16; // padding 0 8px
        if x + bw > rx - 8 {
            break; // out of room; stop rather than overflow
        }
        if *active {
            cv.round_rect(x, btn_y, bw, btn_h, 6, BTN_ACTIVE);
        }
        let fg = if *active { WHITE } else { MUTED };
        cv.text(label, x + 8, ty, fg);
        hits.push((x, x + bw, k));
        x += bw + 4; // margin 3px 2px
    }

    (launcher_hit, hits)
}

// ── Top bar: system info in the same visual language ─────────────────────────

/// Draw a right-anchored module (optional mini gauge + label), ending at
/// `right`. Skipped (returns None) when it would cross `min_x` — narrow
/// outputs drop right modules instead of overprinting the left group.
fn metric(
    cv: &mut Canvas,
    right: i32,
    min_x: i32,
    ty: i32,
    h: i32,
    label: &str,
    gauge: Option<f32>,
    col: Rgb,
) -> Option<i32> {
    let tw = Canvas::text_width(label);
    let (gw, gpad) = if gauge.is_some() { (26, 6) } else { (0, 0) };
    let total = gw + gpad + tw;
    let x = right - total;
    if x < min_x {
        return None;
    }
    if let Some(f) = gauge {
        let ghh = 7;
        let gy = (h - ghh) / 2;
        cv.gauge(x, gy, gw, ghh, f, PILL);
    }
    cv.text(label, x + gw + gpad, ty, col);
    Some(x)
}

/// Draw the network module: ▼<down> ▲<up>, right-anchored at `right`.
/// Skipped (returns None) when it would cross `min_x`.
fn net_module(cv: &mut Canvas, right: i32, min_x: i32, ty: i32, h: i32, n: &NetRate) -> Option<i32> {
    let down = sysinfo::fmt_rate(n.down);
    let up = sysinfo::fmt_rate(n.up);
    let ts = 9;
    let ty_tri = (h - ts) / 2;
    let dw = Canvas::text_width(&down);
    let uw = Canvas::text_width(&up);
    let total = ts + 4 + dw + 10 + ts + 4 + uw;
    let x = right - total;
    if x < min_x {
        return None;
    }
    let mut cx = x;
    cv.triangle(cx, ty_tri, ts, false, LAUNCH); // download ▼
    cx += ts + 4;
    cx += cv.text(&down, cx, ty, MUTED);
    cx += 10;
    cv.triangle(cx, ty_tri, ts, true, LAUNCH); // upload ▲
    cx += ts + 4;
    cv.text(&up, cx, ty, MUTED);
    Some(x)
}

/// Paint the top info bar. Returns the launcher hitbox (x0,x1).
fn draw_info(cv: &mut Canvas, w: usize, h: usize, m: &Metrics) -> (i32, i32) {
    cv.clear(BAR_BG);
    // border-bottom: 2px solid #6b5aa8 (mirrors the bottom bar's top rule)
    cv.hline(0, h as i32 - 1, w as i32, BAR_RULE, 1.0);
    cv.hline(0, h as i32 - 2, w as i32, BAR_RULE, 1.0);

    let ty = (h as i32 - 2 - GLYPH_H) / 2; // text cell top, above the border
    let hi = h as i32;
    let btn_h = hi - 10;
    let btn_y = (hi - btn_h) / 2;

    // ── left: ☾ + eclipse wordmark, uptime, load ──
    let d = (hi * 18) / 34;
    let ly = (hi - d) / 2;
    cv.crescent(10, ly, d, LAUNCH);
    let mut lx = 10 + d + 8;
    lx += cv.text_bold("eclipse", lx, ty, TEXT);
    let launcher_hit = (0, lx + 4);

    if let Some(up) = &m.uptime {
        lx += 12;
        cv.vrule(lx, hi, BAR_RULE);
        lx += 12;
        lx += cv.text(&format!("up {up}"), lx, ty, MUTED);
    }
    if let Some(load) = m.load {
        lx += 12;
        cv.vrule(lx, hi, BAR_RULE);
        lx += 12;
        lx += cv.text(&format!("load {load:.2}"), lx, ty, MUTED);
    }

    // ── right: date pill, battery, temp, disk, net (right-to-left) ──
    // Modules that would cross into the left group are dropped, lowest
    // priority (leftmost) first — narrow outputs degrade instead of garbling.
    let min_x = lx + 12;
    let mut rx = w as i32 - 4;
    {
        let pw = Canvas::text_width(&m.date) + 20;
        if rx - pw >= min_x {
            rx -= pw;
            cv.round_rect(rx, btn_y, pw, btn_h, 6, PILL);
            cv.text_bold(&m.date, rx + 10, ty, TEXT);
            rx -= 10;
        }
    }
    if let Some((b, ch)) = m.batt {
        let label = if ch { format!("bat {b}% +") } else { format!("bat {b}%") };
        if let Some(x) = metric(cv, rx - 10, min_x, ty, hi, &label, Some(b as f32 / 100.0), MUTED) {
            rx = x - 12;
            cv.vrule(rx, hi, BAR_RULE);
        }
    }
    if let Some(t) = m.temp {
        if let Some(x) = metric(cv, rx - 10, min_x, ty, hi, &format!("{t}°c"), None, MUTED) {
            rx = x - 12;
            cv.vrule(rx, hi, BAR_RULE);
        }
    }
    if let Some(dk) = m.disk {
        if let Some(x) = metric(
            cv,
            rx - 10,
            min_x,
            ty,
            hi,
            &format!("disk {dk}%"),
            Some(dk as f32 / 100.0),
            MUTED,
        ) {
            rx = x - 12;
            cv.vrule(rx, hi, BAR_RULE);
        }
    }
    if let Some(n) = &m.net {
        if n.link {
            net_module(cv, rx - 10, min_x, ty, hi, n);
        }
    }

    launcher_hit
}

/// "42" or "--" for an optional percentage.
fn opt(v: Option<u32>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "--".into())
}

/// A short, font-renderable button label for a window: prefer the title (what
/// waybar's `{title:.18}` showed), fall back to app_id, capped so buttons stay
/// a sane width.
fn button_label(t: &Toplevel) -> String {
    let src = if !t.title.trim().is_empty() {
        &t.title
    } else {
        &t.app_id
    };
    let src = sanitize_title(src);
    let src = src.trim();
    let src = if src.is_empty() { "window" } else { src };
    let mut s: String = src.chars().take(18).collect();
    if src.chars().count() > 18 {
        s.push('.');
    }
    s
}

/// Map a raw toplevel title onto FONT_9X15's ISO-8859-1 repertoire. Titles
/// arrive as arbitrary UTF-8: em/en dashes (Firefox's "page — browser"),
/// curly quotes, emoji, CJK, even control chars. Untranslatable glyphs would
/// each render as a full-width '?' — and a stray '\n' would draw a second
/// text line bleeding out of the button — so translate what has an ASCII
/// cousin, drop what is invisible, and collapse the rest into a single '?'.
fn sanitize_title(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for ch in src.chars() {
        match ch {
            '\u{2010}'..='\u{2015}' => out.push('-'),        // hyphens/dashes
            '\u{2018}' | '\u{2019}' => out.push('\''),       // curly single quotes
            '\u{201c}' | '\u{201d}' => out.push('"'),        // curly double quotes
            '\u{2026}' => out.push_str("..."),               // ellipsis
            // Invisible: zero-width chars, variation selectors, combining marks.
            '\u{200b}'..='\u{200f}' | '\u{fe00}'..='\u{fe0f}' | '\u{0300}'..='\u{036f}' => {}
            c if c.is_control() => out.push(' '),            // incl. \n, \t
            c if (c as u32) < 0x100 => out.push(c),          // Latin-1: has a glyph
            _ => {
                // Everything else has no glyph; collapse runs (a CJK title
                // becomes one '?', not one per codepoint).
                if !out.ends_with('?') {
                    out.push('?');
                }
            }
        }
    }
    out
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
                    // The pointer is created from the Capabilities event, not
                    // here: requesting one from a pointer-less seat is a
                    // protocol violation on strict compositors.
                    state.seat = Some(registry.bind(name, version.min(5), qh, ()));
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
                            let (hx0, hx1) = state.bars[idx].launcher_hit;
                            if x >= hx0 && x < hx1 {
                                state.spawn_terminal();
                            } else if state.bars[idx].role == Role::Task {
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
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        // Create/drop the pointer as the seat's POINTER capability toggles.
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            let has_pointer = caps.contains(wl_seat::Capability::Pointer);
            if has_pointer && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            } else if !has_pointer {
                if let Some(p) = state.pointer.take() {
                    // release() exists from wl_pointer v3; below that the
                    // object just stays inert.
                    if p.version() >= 3 {
                        p.release();
                    }
                }
                state.ptr_bar = None;
            }
        }
    }
}
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

fn main() {
    // Minimum 26: the 15px font plus the h-10 pill height — anything shorter
    // draws glyphs taller than the pills that frame them.
    let height: u32 = std::env::var("LUNARBAR_HEIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|h| (26..=64).contains(h))
        .unwrap_or(34); // waybar's configured height
    let terminal = std::env::var("LUNARBAR_TERMINAL")
        .unwrap_or_else(|_| "/usr/local/bin/eclipse-terminal".into());

    // Offscreen preview for offline verification: render BOTH bars as they'd
    // sit on screen — top info bar, wallpaper gap, bottom taskbar — to a raw
    // XRGB8888 file. Spec is `path:WxH`; H is the full preview height.
    if let Ok(spec) = std::env::var("LUNARBAR_DUMP") {
        let (path, w, full_h) = match spec.rsplit_once(':') {
            Some((p, dims)) if dims.contains('x') => {
                let (ws, hs) = dims.split_once('x').unwrap();
                (p.to_string(), ws.parse().unwrap_or(1280), hs.parse().unwrap_or(220))
            }
            _ => (spec, 1280usize, 220usize),
        };
        // Clamp degenerate specs ("0x220", "1280x1") instead of tripping the
        // blit size assert — Canvas clamps its pixmap to >=1px but the
        // destination slices below are sized from the raw values.
        let w = w.clamp(64, 16384);
        let full_h = full_h.clamp(2 * height as usize, 16384);
        let bh = (height as usize).min(full_h / 2);
        let mut buf = vec![0u8; w * full_h * 4];

        // Wallpaper-tone fill for the gap between the bars (XRGB: B,G,R,X).
        const WALL: Rgb = (0x0c, 0x0a, 0x18);
        for px in buf.chunks_exact_mut(4) {
            px[0] = WALL.2;
            px[1] = WALL.1;
            px[2] = WALL.0;
            px[3] = 0xff;
        }

        let mut cpu = CpuMeter::default();
        let mut net = NetMeter::default();
        let _ = cpu.sample();
        let _ = net.sample();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let m = Metrics {
            cpu: cpu.sample(),
            mem: sysinfo::mem_percent(),
            clock: sysinfo::clock_hhmm(),
            date: sysinfo::date_dm(),
            net: net.sample(),
            uptime: sysinfo::uptime(),
            load: sysinfo::loadavg(),
            disk: sysinfo::disk_root_percent(),
            temp: sysinfo::temp_c(),
            batt: sysinfo::battery(),
        };

        // Top info bar occupies rows [0, bh).
        {
            let mut cv = Canvas::new(w, bh);
            draw_info(&mut cv, w, bh, &m);
            cv.blit_xrgb(&mut buf[..w * bh * 4]);
        }
        // Bottom taskbar occupies rows [full_h-bh, full_h) with sample windows
        // (one accented, to exercise the ISO-8859-1 font).
        {
            let off = (full_h - bh) * w * 4;
            let mut cv = Canvas::new(w, bh);
            let sample = [
                ("foot".to_string(), true),
                ("configuración".to_string(), false),
                ("lunar editor".to_string(), false),
            ];
            draw_task(&mut cv, w, bh, &sample, &m);
            cv.blit_xrgb(&mut buf[off..off + w * bh * 4]);
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
    // Prime the delta-based meters now: the registry roundtrip below provides
    // a natural sampling window, so the FIRST committed frame already shows a
    // real cpu% instead of a "cpu --%" flash.
    let _ = state.cpu.sample();
    let _ = state.net.sample();
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
    state.refresh_metrics();

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
            state.refresh_metrics();
            state.render_all();
            next_tick += interval;
            let now = std::time::Instant::now();
            if next_tick < now {
                next_tick = now + interval;
            }
        }
    }
}
