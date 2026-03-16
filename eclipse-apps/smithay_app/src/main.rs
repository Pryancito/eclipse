//! Un solo binario: backend según target (como xfwl4).
//! - Linux (host): compositor Wayland con Smithay + winit.
//! - Eclipse: compositor propio (DRM, SideWind, IPC).

#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
extern crate eclipse_syscall;
#[cfg(not(target_vendor = "eclipse"))]
use smithay_app::smithay_wayland;

// ---- Entry point Linux: Smithay Wayland ----
#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    if let Err(e) = smithay_wayland::run() {
        eprintln!("[SMITHAY] Error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(target_vendor = "eclipse")]
use std::prelude::v1::*;
#[cfg(target_vendor = "eclipse")]
use smithay_app::libc;
#[cfg(target_vendor = "eclipse")]
use std::env;

// ---- Entry point Eclipse: compositor propio ----
#[cfg(target_vendor = "eclipse")]
#[cfg(not(test))]
fn main() {
    use smithay_app::state::SmithayState;
    use smithay_app::ipc::{query_input_service_pid, subscribe_to_input};

    // Stack allocation: evita fallo del heap allocator (GPF CR2=0 en Box::new).
    // La pila de userspace es 1MB; SmithayState cabe. Si hace falta heap después, mmap directo funciona.
    let mut state = SmithayState::new().expect("Failed to initialize Smithay State");

    if let Some(input_pid) = query_input_service_pid() {
        let self_pid = unsafe { libc::getpid() as u32 };
        let _ = subscribe_to_input(input_pid, self_pid);
    }

    let self_pid = unsafe { libc::getpid() as u32 };
    let _ = eclipse_syscall::call::register_log_hud(self_pid);

    #[cfg(feature = "trace-frames")]
    let _stats_before = libc::SystemStats {
        uptime_ticks: 0,
        idle_ticks: 0,
        total_mem_frames: 0,
        used_mem_frames: 0,
    };

    loop {
        state.handle_ipc();
        let need_render = state.update();
        if need_render {
            state.render();
        }
        // Throttle para evitar saturar la CPU; 16ms ≈ 60 FPS.
        // El kernel de Eclipse permite sleep() sin bloquear otros procesos.
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

#[cfg(test)]
#[cfg(target_vendor = "eclipse")]
mod tests {
    use smithay_app::state::SmithayState;

    #[test]
    fn main_loop_iterations_complete_without_hanging() {
        let mut state = SmithayState::new().expect("state");
        const N: u64 = 500;
        for _ in 0..N {
            while let Some(_event) = state.backend.poll_event() {
                state.handle_event(&_event);
            }
            state.update();
            state.render();
            state.backend.swap_buffers();
        }
        assert!(state.counter >= N, "counter should advance each update");
    }
}
