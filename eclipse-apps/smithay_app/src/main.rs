//! Un solo binario: backend según target (como xfwl4).
//! - Linux (host): compositor Wayland con Smithay + winit.
//! - Eclipse: compositor propio (DRM, SideWind, IPC).

#![cfg_attr(all(not(test), not(target_os = "linux")), no_main)]

#[cfg(not(target_os = "linux"))]
extern crate alloc;
#[cfg(not(target_os = "linux"))]
extern crate std;

#[cfg(not(target_os = "linux"))]
extern crate eclipse_syscall;

pub mod backend;
pub mod compositor;
pub mod damage;
pub mod input;
pub mod ipc;
pub mod render;
pub mod state;

// Compositor Wayland con Smithay (solo cuando target es Linux)
#[cfg(target_os = "linux")]
mod smithay_wayland;

// ---- Entry point Linux: Smithay Wayland ----
#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = smithay_wayland::run() {
        eprintln!("[SMITHAY] Error: {}", e);
        std::process::exit(1);
    }
}

// ---- Entry point Eclipse: compositor propio ----
#[cfg(not(target_os = "linux"))]
#[cfg(not(test))]
#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    use crate::state::SmithayState;
    use crate::ipc::{query_input_service_pid, subscribe_to_input};

    println!("[SMITHAY] Starting via Eclipse Runtime...");

    // Stack allocation: evita fallo del heap allocator (GPF CR2=0 en Box::new).
    // La pila de userspace es 1MB; SmithayState cabe. Si hace falta heap después, mmap directo funciona.
    let mut state = SmithayState::new().expect("Failed to initialize Smithay State");

    match query_input_service_pid() {
        Some(input_pid) => {
            if !state.backend.input_scheme_available() {
                let self_pid = unsafe { eclipse_libc::getpid() as u32 };
                if subscribe_to_input(input_pid, self_pid) {
                    println!("[SMITHAY] Subscribed to input service (PID {}) via IPC", input_pid);
                } else {
                    println!("[SMITHAY] Warning: subscription to input service PID {} failed", input_pid);
                }
            }
        }
        None => {
            println!("[SMITHAY] Warning: input service PID not available, input events may not work");
        }
    }

    let self_pid = unsafe { eclipse_libc::getpid() as u32 };
    if eclipse_syscall::call::register_log_hud(self_pid).is_err() {
        println!("[SMITHAY] Warning: register_log_hud failed (kernel may not support it yet)");
    }

    #[cfg(feature = "trace-frames")]
    let _stats_before = eclipse_libc::SystemStats {
        uptime_ticks: 0,
        idle_ticks: 0,
        total_mem_frames: 0,
        used_mem_frames: 0,
    };

    let mut last_render = std::time::Instant::now();

    loop {
        state.handle_ipc();
        let is_busy = state.update();
        let elapsed_since_render = last_render.elapsed();
        if is_busy || state.dirty || elapsed_since_render >= std::time::Duration::from_millis(500) {
            state.render();
            state.dirty = false;
            last_render = std::time::Instant::now();
        }

        let frame_target = std::time::Duration::from_millis(16);
        let elapsed = last_render.elapsed();
        if !is_busy && !state.dirty {
            std::thread::sleep(std::time::Duration::from_millis(4));
        } else if elapsed < frame_target {
            std::thread::sleep(frame_target - elapsed);
        }

        #[cfg(feature = "trace-frames")]
        if state.counter > 0 && state.counter % 600 == 0 {
            println!(
                "[SMITHAY] heartbeat counter={} messages={}",
                state.counter,
                state.backend.ipc.message_count
            );
        }
    }
}

#[cfg(test)]
#[cfg(not(target_os = "linux"))]
mod tests {
    use crate::state::SmithayState;

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
