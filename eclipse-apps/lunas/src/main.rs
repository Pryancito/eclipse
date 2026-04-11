//! Lunas Desktop Environment — Entry point.
//! - Linux (host): mock mode for testing.
//! - Eclipse: native compositor (DRM, SideWind, IPC).


// ---- Entry point Linux: mock mode ----
#[cfg(not(target_os = "eclipse"))]
fn main() {
    eprintln!("[LUNAS] Desktop environment requires Eclipse OS target.");
    eprintln!("[LUNAS] Use --target for Eclipse OS cross-compilation.");
    std::process::exit(1);
}


fn main() {
    eprintln!("[LUNAS] Starting Lunas Desktop Environment...");
    use lunas::state::LunasState;

    let mut state = LunasState::new().expect("Failed to initialize Lunas Desktop");
    eprintln!("[LUNAS] State initialized, setting process name...");

    let _ = eclipse_syscall::call::set_process_name("lunas");
    eprintln!("[LUNAS] Process name set, registering globals...");

    // Register Wayland globals — closures capture the shared protocol channels
    // so that all protocol objects (LunasSurface, LunasShmPool, LunasBuffer)
    // route their side-effects through the same Rc<RefCell<…>> lists that
    // LunasState drains every frame.
    let pending_commits = state.pending_surface_commits.clone();
    let buffer_registry = state.buffer_registry.clone();

    {
        let c = pending_commits.clone();
        let b = buffer_registry.clone();
        state.protocol.register_global(
            "wl_compositor", 4,
            move || {
                let compositor = lunas::protocol::LunasCompositor {
                    pending_commits: c.clone(),
                    buffer_registry: b.clone(),
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(compositor))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::wl_compositor::WlCompositor
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered wl_compositor");
    }

    {
        let b = buffer_registry.clone();
        state.protocol.register_global(
            "wl_shm", 1,
            move || {
                let shm = lunas::protocol::LunasShm { buffer_registry: b.clone() };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(shm))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::wl_shm::WlShm
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered wl_shm");
    }

    // ── Register new standard Wayland globals ────────────────────────────────

    // xdg_wm_base — modern toplevel window management
    {
        let c = pending_commits.clone();
        let b = buffer_registry.clone();
        state.protocol.register_global(
            "xdg_wm_base", 2,
            move || {
                let xdg = lunas::protocol::LunasXdgWmBase {
                    pending_commits: c.clone(),
                    buffer_registry: b.clone(),
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(xdg))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::xdg_wm_base::XdgWmBase
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered xdg_wm_base");
    }

    // wl_seat — keyboard + pointer seat
    {
        let kb_reg = state.keyboard_registry.clone();
        let ptr_reg = state.pointer_registry.clone();
        let w = state.backend.fb.info.width;
        let h = state.backend.fb.info.height;
        state.protocol.register_global_with_post_bind(
            "wl_seat", 7,
            move || {
                let seat = lunas::protocol::LunasSeat {
                    keyboard_registry: kb_reg.clone(),
                    pointer_registry: ptr_reg.clone(),
                    screen_w: w,
                    screen_h: h,
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(seat))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::wl_seat::WlSeat
            >(id, inner),
            Some(Box::new(|obj_id, client: &mut wayland_proto::wl::server::client::Client| {
                // Send capabilities: keyboard + pointer
                use wayland_proto::wl::protocols::common::wl_seat::{Event, CAP_KEYBOARD, CAP_POINTER};
                client.send_event(obj_id, Event::Capabilities { capabilities: CAP_KEYBOARD | CAP_POINTER })
                    .map_err(|_| wayland_proto::wl::server::objects::ServerError::IoError)
            })),
        );
        eprintln!("[LUNAS] Registered wl_seat");
    }

    // wl_output — display info
    {
        let w = state.backend.fb.info.width as i32;
        let h = state.backend.fb.info.height as i32;
        state.protocol.register_global_with_post_bind(
            "wl_output", 4,
            move || {
                let out = lunas::protocol::LunasOutput {
                    screen_w: w as u32,
                    screen_h: h as u32,
                    refresh_mhz: 60_000,
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(out))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::wl_output::WlOutput
            >(id, inner),
            Some(Box::new(move |obj_id, client: &mut wayland_proto::wl::server::client::Client| {
                use wayland_proto::wl::protocols::common::wl_output::{
                    Event, SUBPIXEL_UNKNOWN, TRANSFORM_NORMAL, MODE_CURRENT,
                };
                client.send_event(obj_id, Event::Geometry {
                    x: 0, y: 0,
                    physical_width: 527, physical_height: 296,
                    subpixel: SUBPIXEL_UNKNOWN,
                    make: String::from("Eclipse OS"),
                    model: String::from("Virtual Display"),
                    transform: TRANSFORM_NORMAL,
                }).map_err(|_| wayland_proto::wl::server::objects::ServerError::IoError)?;
                client.send_event(obj_id, Event::Mode {
                    flags: MODE_CURRENT,
                    width: w,
                    height: h,
                    refresh: 60_000,
                }).map_err(|_| wayland_proto::wl::server::objects::ServerError::IoError)?;
                client.send_event(obj_id, Event::Scale { factor: 1 })
                    .map_err(|_| wayland_proto::wl::server::objects::ServerError::IoError)?;
                client.send_event(obj_id, Event::Done)
                    .map_err(|_| wayland_proto::wl::server::objects::ServerError::IoError)
            })),
        );
        eprintln!("[LUNAS] Registered wl_output");
    }

    // xwayland_shell_v1 — association of X11 windows to Wayland surfaces
    {
        let s = state.xwayland_serials.clone();
        state.protocol.register_global(
            "xwayland_shell_v1", 1,
            move || {
                let shell = lunas::protocol::LunasXwaylandShell {
                    xwayland_serials: s.clone(),
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(shell))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::xwayland_shell::XwaylandShellV1
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered xwayland_shell_v1");
    }

    // ── zxdg_decoration_manager_v1 — SSD negotiation (core labwc protocol) ──
    {
        state.protocol.register_global(
            "zxdg_decoration_manager_v1", 1,
            || {
                let mgr = lunas::protocol::LunasDecorationManager;
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(mgr))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::xdg_decoration::ZxdgDecorationManagerV1
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered zxdg_decoration_manager_v1");
    }

    // ── wl_shell — legacy shell for old GTK2/Qt4 clients ─────────────────
    {
        let c = pending_commits.clone();
        let b = buffer_registry.clone();
        state.protocol.register_global(
            "wl_shell", 1,
            move || {
                let shell = lunas::protocol::LunasWlShell {
                    pending_commits: c.clone(),
                    buffer_registry: b.clone(),
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(shell))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::wl_shell::WlShell
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered wl_shell");
    }

    // ── zxdg_output_manager_v1 — extended output info ─────────────────────
    {
        let w = state.backend.fb.info.width as u32;
        let h = state.backend.fb.info.height as u32;
        state.protocol.register_global(
            "zxdg_output_manager_v1", 3,
            move || {
                let mgr = lunas::protocol::LunasXdgOutputManager {
                    screen_w: w,
                    screen_h: h,
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(mgr))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::xdg_output::ZxdgOutputManagerV1
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered zxdg_output_manager_v1");
    }

    // ── zwlr_layer_shell_v1 — layer shell for panels / overlays ──────────
    {
        let c = pending_commits.clone();
        let b = buffer_registry.clone();
        let w = state.backend.fb.info.width as u32;
        let h = state.backend.fb.info.height as u32;
        state.protocol.register_global(
            "zwlr_layer_shell_v1", 4,
            move || {
                let shell = lunas::protocol::LunasLayerShell {
                    pending_commits: c.clone(),
                    buffer_registry: b.clone(),
                    screen_w: w,
                    screen_h: h,
                };
                wayland_proto::wl::server::objects::ObjectInner::Rc(
                    std::rc::Rc::new(core::cell::RefCell::new(shell))
                )
            },
            |id, inner| wayland_proto::wl::server::objects::Object::new::<
                wayland_proto::wl::protocols::common::zwlr_layer_shell::ZwlrLayerShellV1
            >(id, inner),
        );
        eprintln!("[LUNAS] Registered zwlr_layer_shell_v1");
    }

    eprintln!("[LUNAS] All globals registered, entering main loop...");

    // ── Start Wayland Unix socket server ─────────────────────────────────────
    eprintln!("[LUNAS] Binding Wayland socket...");
    let mut wayland_socket = lunas::wayland_socket::WaylandSocketServer::new("/tmp/wayland-0");
    if wayland_socket.is_none() {
        eprintln!("[LUNAS] Warning: could not bind /tmp/wayland-0 — standard Wayland clients won't connect");
    }

    eprintln!("[LUNAS] Getting self PID...");
    let self_pid = unsafe { libc::getpid() as u32 };
    eprintln!("[LUNAS] Self PID: {}", self_pid);

    eprintln!("[LUNAS] Registering HUD...");
    let _ = eclipse_syscall::call::register_log_hud(self_pid);
    eprintln!("[LUNAS] HUD registered.");

    loop {
        // Accept and process standard Wayland socket clients
        if let Some(ref mut sock) = wayland_socket {
            if sock.poll(&mut state.protocol) {
                state.dirty = true;
            }
        }
        state.handle_ipc();
        state.update();
        state.render();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

#[cfg(test)]
#[cfg(target_os = "eclipse")]
mod tests {
    use lunas::state::LunasState;

    #[test]
    fn main_loop_iterations_complete_without_hanging() {
        let mut state = LunasState::new().expect("state");
        const N: u64 = 10_000;
        for _ in 0..N {
            while let Some(_event) = state.backend.poll_event() {
                state.handle_event(&_event);
            }
            if state.update() {
                state.render();
            }
        }
        assert!(state.counter >= N, "counter should advance each update");
    }
}
