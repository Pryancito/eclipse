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
#[cfg(all(not(target_vendor = "eclipse"), feature = "wayland"))]
use smithay_app::smithay_wayland;

// ---- Entry point Linux con feature "wayland": Smithay Wayland ----
#[cfg(all(not(target_vendor = "eclipse"), feature = "wayland"))]
fn main() {
    if let Err(e) = smithay_wayland::run() {
        eprintln!("[SMITHAY] Error: {}", e);
        std::process::exit(1);
    }
}

// ---- Entry point Linux sin feature "wayland": informar al usuario ----
#[cfg(all(not(target_vendor = "eclipse"), not(feature = "wayland")))]
fn main() {
    eprintln!("[SMITHAY] Backend Wayland no habilitado. Compile con --features wayland para usar el compositor Wayland en Linux.");
    std::process::exit(1);
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

    // Stack allocation: evita fallo del heap allocator (GPF CR2=0 en Box::new).
    // La pila de userspace es 1MB; SmithayState cabe. Si hace falta heap después, mmap directo funciona.
    let mut state = SmithayState::new().expect("Failed to initialize Smithay State");

    // Ratón y teclado solo por el scheme input: (open("input:")), no por IPC.

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
        // Only render when something changed: events arrived, animations are
        // running, or metrics were refreshed.  Skipping the render on truly
        // idle frames avoids a full framebuffer blit + present at 60 Hz when
        // the desktop is static, which was the main source of high CPU usage
        // on real hardware.
        if state.update() {
            state.render();
        }
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
        const N: u64 = 500000;
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
