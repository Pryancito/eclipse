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

    let self_pid = unsafe { libc::getpid() as u32 };
    let _ = eclipse_syscall::call::register_log_hud(self_pid);

    loop {
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
