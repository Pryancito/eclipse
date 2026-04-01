//! Lunas Desktop Environment — Entry point.
//! - Linux (host): mock mode for testing.
//! - Eclipse: native compositor (DRM, SideWind, IPC).


// ---- Entry point Linux: mock mode ----
#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    eprintln!("[LUNAS] Desktop environment requires Eclipse OS target.");
    eprintln!("[LUNAS] Use --target for Eclipse OS cross-compilation.");
    std::process::exit(1);
}


fn main() {
    use lunas::state::LunasState;

    let mut state = LunasState::new().expect("Failed to initialize Lunas Desktop");

    let _ = eclipse_syscall::call::set_process_name("lunas");

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
    }

    // wl_seat — keyboard + pointer seat
    {
        let kb_reg = state.keyboard_registry.clone();
        let w = state.backend.fb.info.width;
        let h = state.backend.fb.info.height;
        state.protocol.register_global_with_post_bind(
            "wl_seat", 7,
            move || {
                let seat = lunas::protocol::LunasSeat {
                    keyboard_registry: kb_reg.clone(),
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
            Some(alloc::boxed::Box::new(|obj_id, client| {
                // Send capabilities: keyboard present
                use wayland_proto::wl::protocols::common::wl_seat::{Event, CAP_KEYBOARD};
                client.send_event(obj_id, Event::Capabilities { capabilities: CAP_KEYBOARD })
                    .map_err(|_| wayland_proto::wl::server::objects::ServerError::IoError)
            })),
        );
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
            Some(alloc::boxed::Box::new(move |obj_id, client| {
                use wayland_proto::wl::protocols::common::wl_output::{
                    Event, SUBPIXEL_UNKNOWN, TRANSFORM_NORMAL, MODE_CURRENT,
                };
                client.send_event(obj_id, Event::Geometry {
                    x: 0, y: 0,
                    physical_width: 527, physical_height: 296,
                    subpixel: SUBPIXEL_UNKNOWN,
                    make: alloc::string::String::from("Eclipse OS"),
                    model: alloc::string::String::from("Virtual Display"),
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
    }

    // ── Start Wayland Unix socket server ─────────────────────────────────────
    let mut wayland_socket = lunas::wayland_socket::WaylandSocketServer::new("/tmp/wayland-0");
    if wayland_socket.is_none() {
        eprintln!("[LUNAS] Warning: could not bind /tmp/wayland-0 — standard Wayland clients won't connect");
    }

    let self_pid = unsafe { libc::getpid() as u32 };
    let _ = eclipse_syscall::call::register_log_hud(self_pid);

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
#[cfg(target_vendor = "eclipse")]
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
