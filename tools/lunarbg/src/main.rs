//! lunarbg — Eclipse OS's swaybg replacement.
//!
//! Why not swaybg: Alpine's swaybg decodes wallpapers through gdk-pixbuf,
//! whose loader registry is installed by an apk trigger that may never run
//! under Eclipse OS — leaving it unable to recognise ANY image format.
//! lunarbg needs no image files at all: it renders the Eclipse night scene
//! procedurally (see `scene.rs`) at each output's native resolution and hands
//! it to the compositor over wl_shm as a wlr-layer-shell background surface.
//!
//! Pure-Rust Wayland stack (wayland-client's Rust backend): a single static
//! musl binary with no runtime library dependencies.

mod scene;

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::{
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    /// Outputs announced but not yet carrying a background surface.
    pending_outputs: Vec<wl_output::WlOutput>,
    backgrounds: Vec<Background>,
}

struct Background {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// Last size we rendered, to skip redundant redraws on repeat configures.
    drawn: Option<(u32, u32)>,
}

impl State {
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
            // Size 0x0 + all anchors: the compositor tells us the real size in
            // the configure event.
            layer.set_size(0, 0);
            surface.commit();
            self.backgrounds.push(Background {
                surface,
                layer,
                drawn: None,
            });
        }
    }

    fn draw(&mut self, qh: &QueueHandle<State>, layer_id: u32, w: u32, h: u32) {
        let Some(shm) = &self.shm else { return };
        let Some(bg) = self
            .backgrounds
            .iter_mut()
            .find(|b| b.layer.id().protocol_id() == layer_id)
        else {
            return;
        };
        if bg.drawn == Some((w, h)) {
            // Same size re-configure: nothing to redraw, previous buffer is
            // still attached and valid.
            bg.surface.commit();
            return;
        }
        let (w, h) = (w.max(1) as usize, h.max(1) as usize);
        let stride = w * 4;
        let size = stride * h;

        let pixels = scene::render_xrgb(w, h);

        // Anonymous shared memory via memfd (no /tmp files, works on
        // Eclipse OS which implements memfd_create + no-op seals).
        let raw = unsafe {
            libc::memfd_create(b"lunarbg\0".as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC)
        };
        if raw < 0 {
            eprintln!("lunarbg: memfd_create failed");
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(raw, size as libc::off_t) } != 0 {
            eprintln!("lunarbg: ftruncate({size}) failed");
            return;
        }
        unsafe {
            let map = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            );
            if map == libc::MAP_FAILED {
                eprintln!("lunarbg: mmap failed");
                return;
            }
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), map as *mut u8, size);
            libc::munmap(map, size);
        }

        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            stride as i32,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );
        // The pool object can go away immediately; the buffer keeps the
        // backing storage alive server-side, and `fd` closes on drop.
        pool.destroy();

        bg.surface.attach(Some(&buffer), 0, 0);
        bg.surface.damage_buffer(0, 0, w as i32, h as i32);
        bg.surface.commit();
        bg.drawn = Some((w as u32, h as u32));
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
                    state.compositor =
                        Some(registry.bind(name, version.min(4), qh, ()));
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
                state.draw(qh, layer.id().protocol_id(), width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                let id = layer.id().protocol_id();
                if let Some(pos) = state
                    .backgrounds
                    .iter()
                    .position(|b| b.layer.id().protocol_id() == id)
                {
                    let bg = state.backgrounds.remove(pos);
                    bg.layer.destroy();
                    bg.surface.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
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

fn main() {
    // Offscreen debug mode: LUNARBG_DUMP=/path[:WxH] renders the scene to a
    // raw XRGB8888 file and exits, no compositor needed.
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
        std::fs::write(&path, scene::render_xrgb(w, h)).expect("write dump");
        eprintln!("lunarbg: dumped {w}x{h} XRGB8888 to {path}");
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

    let mut state = State::default();
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

    loop {
        state.ensure_surfaces(&qh);
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            eprintln!("lunarbg: connection lost: {e}");
            std::process::exit(1);
        }
    }
}
