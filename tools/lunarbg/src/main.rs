//! lunarbg — Eclipse OS's animated wallpaper client (its swaybg replacement).
//!
//! Why not swaybg: Alpine's swaybg decodes wallpapers through gdk-pixbuf,
//! whose loader registry is installed by an apk trigger that may never run
//! under Eclipse OS — leaving it unable to recognise ANY image format. And
//! swaybg is static anyway: lunarbg instead renders the animated cosmic
//! background of the original Eclipse smithay compositor (see `scene.rs`)
//! procedurally at each output's native resolution, over wlr-layer-shell +
//! wl_shm.
//!
//! Animation model: the cosmic base (gradient, stars, grid) is rendered once
//! per size; every compositor frame callback re-renders only the logo region
//! (crescent, orbiting text, arcs, rings, ticks) into one of two persistent
//! shm buffers and damages just that rectangle, so the software-rendered
//! compositor composites a small area per frame, not the whole screen.
//!
//! Pure-Rust Wayland stack (wayland-client's Rust backend): a single static
//! musl binary with no runtime library dependencies.
//!
//! Env knobs:
//! - `LUNARBG_STATIC=1` — draw one frame and stop animating.
//! - `LUNARBG_DUMP=/path[:WxH]` — render a frame offscreen to a raw
//!   XRGB8888 file and exit (no compositor needed).

mod scene;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::Instant;

use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

/// Two persistent frame buffers per output, alternated each frame.
const BUFFERS: usize = 2;

struct Frames {
    width: usize,
    height: usize,
    layout: scene::Layout,
    base: Vec<u8>,
    /// mmap of the pool holding BUFFERS frames back to back.
    map: *mut u8,
    map_len: usize,
    buffers: [wl_buffer::WlBuffer; BUFFERS],
    busy: [bool; BUFFERS],
    next: usize,
}

impl Drop for Frames {
    fn drop(&mut self) {
        for b in &self.buffers {
            b.destroy();
        }
        unsafe {
            libc::munmap(self.map as *mut libc::c_void, self.map_len);
        }
    }
}

struct Background {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    frames: Option<Frames>,
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    pending_outputs: Vec<wl_output::WlOutput>,
    backgrounds: Vec<Background>,
    start: Option<Instant>,
    animate: bool,
}

impl State {
    fn now_ms(&mut self) -> u32 {
        let start = self.start.get_or_insert_with(Instant::now);
        start.elapsed().as_millis() as u32
    }

    /// Create background surfaces for any outputs that appeared once the
    /// required globals are all bound. Safe to call repeatedly.
    fn ensure_surfaces(&mut self, qh: &QueueHandle<State>) {
        let (Some(compositor), Some(layer_shell)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        for output in self.pending_outputs.drain(..) {
            let surface = compositor.create_surface(qh, ());
            let layer = layer_shell.get_layer_surface(
                &surface,
                Some(&output),
                zwlr_layer_shell_v1::Layer::Background,
                "wallpaper".into(),
                qh,
                (),
            );
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_exclusive_zone(-1);
            layer.set_size(0, 0);
            surface.commit();
            self.backgrounds.push(Background {
                surface,
                layer,
                frames: None,
            });
        }
    }

    fn bg_index_by_layer(&self, layer_id: u32) -> Option<usize> {
        self.backgrounds
            .iter()
            .position(|b| b.layer.id().protocol_id() == layer_id)
    }

    /// (Re)build the per-size resources after a configure.
    fn configure(&mut self, qh: &QueueHandle<State>, layer_id: u32, w: u32, h: u32) {
        let t_ms = self.now_ms();
        let Some(idx) = self.bg_index_by_layer(layer_id) else {
            return;
        };
        let (w, h) = (w.max(1) as usize, h.max(1) as usize);
        if let Some(frames) = &self.backgrounds[idx].frames {
            if frames.width == w && frames.height == h {
                self.backgrounds[idx].surface.commit();
                return;
            }
        }
        let Some(shm) = &self.shm else { return };

        let stride = w * 4;
        let frame_size = stride * h;
        let total = frame_size * BUFFERS;

        let raw = unsafe {
            libc::memfd_create(
                b"lunarbg\0".as_ptr() as *const libc::c_char,
                libc::MFD_CLOEXEC,
            )
        };
        if raw < 0 {
            eprintln!("lunarbg: memfd_create failed");
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(raw, total as libc::off_t) } != 0 {
            eprintln!("lunarbg: ftruncate({total}) failed");
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
            eprintln!("lunarbg: mmap failed");
            return;
        }
        let map = map as *mut u8;

        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
        let make = |i: usize| {
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
        let buffers = [make(0), make(1)];
        // The pool object can go away; buffers keep the storage alive
        // server-side and the mapping outlives the closed fd.
        pool.destroy();

        let layout = scene::layout(w, h);
        let base = scene::render_base(w, h);

        // Seed BOTH buffers with the full base scene. Only buffer 0 used to
        // get it; buffer 1 stayed zeroed (memfd), and since ticks repaint just
        // the logo region, every frame shown from buffer 1 had BLACK outside
        // the logo — on the real monitor the wallpaper alternated between the
        // full cosmic scene and a dark screen with a floating square.
        let frame0: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(map, frame_size) };
        frame0.copy_from_slice(&base);
        scene::render_frame(frame0, w, &base, &layout, t_ms);
        let frame1: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(map.add(frame_size), frame_size) };
        frame1.copy_from_slice(&base);

        let bg = &mut self.backgrounds[idx];
        bg.frames = Some(Frames {
            width: w,
            height: h,
            layout,
            base,
            map,
            map_len: total,
            buffers,
            busy: [true, false],
            next: 1,
        });
        let frames = bg.frames.as_ref().unwrap();
        bg.surface.attach(Some(&frames.buffers[0]), 0, 0);
        bg.surface.damage_buffer(0, 0, w as i32, h as i32);
        bg.surface.commit();
        let _ = qh; // configure no longer schedules callbacks; the timer loop drives ticks
    }

    /// One animation step for a background, driven by the main loop's timer
    /// (NOT compositor frame callbacks: callback-paced rendering ran at the
    /// compositor's full rate, and on this software-rendered stack that
    /// overloaded the machine — libinput logged "event processing lagging,
    /// your system is too slow" right after session start).
    fn tick(&mut self, layer_id: u32) {
        let t_ms = self.now_ms();
        let Some(idx) = self.bg_index_by_layer(layer_id) else {
            return;
        };
        let bg = &mut self.backgrounds[idx];
        let Some(frames) = &mut bg.frames else { return };

        // Pick the next buffer; prefer a released one, but a busy buffer is
        // overwritten rather than stalling the animation (single-frame
        // artifacts beat a frozen wallpaper).
        let i = if !frames.busy[frames.next] {
            frames.next
        } else if !frames.busy[1 - frames.next] {
            1 - frames.next
        } else {
            frames.next
        };
        frames.next = 1 - i;
        frames.busy[i] = true;

        let frame_size = frames.width * frames.height * 4;
        let frame: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(frames.map.add(i * frame_size), frame_size)
        };
        // The buffer alternates, so it carries a stale logo region from two
        // frames ago; render_frame restores that region from the base first.
        scene::render_frame(frame, frames.width, &frames.base, &frames.layout, t_ms);

        let (rx, ry, rw, rh) = frames.layout.region;
        bg.surface.attach(Some(&frames.buffers[i]), 0, 0);
        bg.surface
            .damage_buffer(rx as i32, ry as i32, rw as i32, rh as i32);
        bg.surface.commit();
    }

    /// Render a tick on every configured background.
    fn tick_all(&mut self) {
        let ids: Vec<u32> = self
            .backgrounds
            .iter()
            .filter(|b| b.frames.is_some())
            .map(|b| b.layer.id().protocol_id())
            .collect();
        for id in ids {
            self.tick(id);
        }
    }
}

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
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, 1, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.pending_outputs.push(output);
                }
                _ => {}
            }
        }
        // GlobalRemove for an output ends in a Closed event on its layer
        // surface, handled there.
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
                if let Some(pos) = state.bg_index_by_layer(id) {
                    let bg = state.backgrounds.remove(pos);
                    bg.layer.destroy();
                    bg.surface.destroy();
                }
            }
            _ => {}
        }
    }
}

/// Buffers: udata is (layer id, buffer index), to clear the busy flag.
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
            if let Some(idx) = state.bg_index_by_layer(*layer_id) {
                if let Some(frames) = &mut state.backgrounds[idx].frames {
                    frames.busy[*i] = false;
                }
            }
        }
    }
}

// Globals whose events carry nothing we need.
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

// The WEnum import keeps signatures readable if event matching grows later.
#[allow(unused_imports)]
use WEnum as _;

fn main() {
    // Offscreen debug mode: LUNARBG_DUMP=/path[:WxH] renders one animation
    // frame to a raw XRGB8888 file and exits, no compositor needed.
    if let Ok(spec) = std::env::var("LUNARBG_DUMP") {
        let (path, w, h) = match spec.rsplit_once(':') {
            Some((p, dims)) if dims.contains('x') => {
                let (w, h) = dims.split_once('x').unwrap();
                (
                    p.to_string(),
                    w.parse().unwrap_or(1920),
                    h.parse().unwrap_or(1080),
                )
            }
            _ => (spec, 1920, 1080),
        };
        let lay = scene::layout(w, h);
        let base = scene::render_base(w, h);
        let mut frame = base.clone();
        let t_ms = std::env::var("LUNARBG_DUMP_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0u32);
        scene::render_frame(&mut frame, w, &base, &lay, t_ms);
        std::fs::write(&path, frame).expect("write dump");
        eprintln!("lunarbg: dumped {w}x{h} XRGB8888 (t={t_ms}ms) to {path}");
        return;
    }

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lunarbg: cannot connect to the Wayland compositor: {e}");
            std::process::exit(1);
        }
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    display.get_registry(&qh, ());

    let mut state = State {
        animate: std::env::var("LUNARBG_STATIC").map_or(true, |v| v != "1"),
        ..State::default()
    };
    if let Err(e) = queue.roundtrip(&mut state) {
        eprintln!("lunarbg: initial roundtrip failed: {e}");
        std::process::exit(1);
    }
    if state.layer_shell.is_none() {
        eprintln!("lunarbg: compositor lacks zwlr_layer_shell_v1");
        std::process::exit(1);
    }
    if state.compositor.is_none() || state.shm.is_none() {
        eprintln!("lunarbg: missing wl_compositor or wl_shm");
        std::process::exit(1);
    }

    // Timer-paced animation loop. LUNARBG_FPS (default 12) bounds the load:
    // this stack composites in software and scans out by copying, so pacing
    // at the compositor's frame-callback rate overloaded the whole machine.
    let fps: u32 = std::env::var("LUNARBG_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|f| (1..=60).contains(f))
        .unwrap_or(12);
    let interval = std::time::Duration::from_millis(1000 / fps as u64);
    let mut next_tick = std::time::Instant::now() + interval;

    loop {
        state.ensure_surfaces(&qh);
        if let Err(e) = queue.flush() {
            eprintln!("lunarbg: connection lost: {e}");
            std::process::exit(1);
        }

        // Wait for server events OR the next animation tick, whichever first.
        if let Some(guard) = queue.prepare_read() {
            let timeout_ms: i32 = if state.animate {
                next_tick
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis()
                    .min(1000) as i32
            } else {
                1000
            };
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
            eprintln!("lunarbg: protocol error: {e}");
            std::process::exit(1);
        }

        if state.animate && std::time::Instant::now() >= next_tick {
            state.tick_all();
            next_tick += interval;
            // If we fell behind (system busy), resync rather than bursting.
            let now = std::time::Instant::now();
            if next_tick < now {
                next_tick = now + interval;
            }
        }
    }
}
