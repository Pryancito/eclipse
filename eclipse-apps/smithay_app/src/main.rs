#![cfg_attr(not(test), no_main)]
extern crate std;
extern crate alloc;
extern crate eclipse_syscall;

pub mod backend;
pub mod compositor;
pub mod damage;
pub mod input;
pub mod ipc;
pub mod render;
pub mod state;

use crate::state::SmithayState;
use crate::ipc::{query_input_service_pid, subscribe_to_input};

/// Umbral en ticks: si un frame tarda más, se loguea (diagnóstico de trompicones).
#[cfg(feature = "trace-frames")]
const TRACE_FRAME_THRESHOLD_TICKS: u64 = 100;
/// Cada N frames se imprime heartbeat (counter, recv_attempts, messages).
#[cfg(feature = "trace-frames")]
const TRACE_HEARTBEAT_EVERY: u64 = 600;

#[cfg(not(test))]
#[no_mangle]
pub extern "Rust" fn main() -> i32 {
    println!("[SMITHAY] Starting via Eclipse Runtime...");

    let mut state = SmithayState::new().expect("Failed to initialize Smithay State");

    // Obtain the input service PID from init. This is a best-effort lookup:
    // if init is still starting up or the input service has not been spawned
    // yet, query_input_service_pid() returns None and the compositor continues
    // without subscribing, relying on the "input:" scheme instead.
    //
    // If the "input:" scheme failed to open (input_fd == None), subscribing to
    // the input_service via IPC is the only way to receive hardware events, so
    // we always attempt the subscription in that case.
    match query_input_service_pid() {
        Some(input_pid) => {
            // Only subscribe via IPC when the direct "input:" scheme is not
            // available.  Subscribing while also reading the scheme would
            // deliver every hardware event twice (once from the scheme queue,
            // once from the IPC mailbox) causing duplicate cursor moves and
            // key presses.
            if !state.backend.input_scheme_available() {
                // eclipse_std does not implement std::process::id(); use the
                // POSIX getpid() FFI wrapper which is always safe on this OS.
                let self_pid = unsafe { std::libc::getpid() as u32 };
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

    // Registrar este proceso como destinatario de líneas de log del kernel (HUD).
    // Cuando display_service dibuja el logo llama sys_stop_progress; a partir de ahí
    // las líneas de serial/progress se envían por IPC aquí en lugar de dibujarse en el kernel FB.
    let self_pid = unsafe { std::libc::getpid() as u32 };
    if let Err(_) = eclipse_syscall::call::register_log_hud(self_pid) {
        println!("[SMITHAY] Warning: register_log_hud failed (kernel may not support it yet)");
    }

    #[cfg(feature = "trace-frames")]
    let mut stats_before = std::libc::SystemStats {
        uptime_ticks: 0,
        idle_ticks: 0,
        total_mem_frames: 0,
        used_mem_frames: 0,
    };

    let mut last_render = std::time::Instant::now();

    loop {
        // 1. Process all pending IPC messages (low latency polling)
        state.handle_ipc();

        // 2. Update logic and check if we need to redraw
        let is_busy = state.update();

        // 3. Render if something changed OR if too much time passed (keep-alive)
        let elapsed_since_render = last_render.elapsed();
        if is_busy || state.dirty || elapsed_since_render >= std::time::Duration::from_millis(500) {
            state.render();
            // render() ya hace present_damaged internamente; no hacer present de nuevo
            state.dirty = false;
            last_render = std::time::Instant::now();
        }

        // 4. Sleep to maintain ~60 FPS and prevent 100% CPU usage
        let frame_target = std::time::Duration::from_millis(16); // ~60 FPS
        let elapsed = last_render.elapsed();
        
        if !is_busy && !state.dirty {
            // Idle: Sleep a bit longer to save CPU, but short enough to stay snappy
            std::thread::sleep(std::time::Duration::from_millis(4));
        } else if elapsed < frame_target {
            // Animating/Busy: Cap at 60 FPS
            std::thread::sleep(frame_target - elapsed);
        }

        #[cfg(feature = "trace-frames")]
        {
            if state.counter > 0 && state.counter % TRACE_HEARTBEAT_EVERY == 0 {
                println!(
                    "[SMITHAY] heartbeat counter={} messages={}",
                    state.counter,
                    state.backend.ipc.message_count
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ejecuta N iteraciones del bucle principal con backend mock.
    /// Si el bucle se colgara (p. ej. recv bloqueante), este test no terminaría.
    #[test]
    fn main_loop_iterations_complete_without_hanging() {
        let mut state = SmithayState::new().expect("state");
        const N: u64 = 500;
        for _ in 0..N {
            while let Some(_event) = state.backend.poll_event() {
                state.handle_event(_event);
            }
            state.update();
            state.render();
            state.backend.swap_buffers();
        }
        assert!(state.counter >= N, "counter should advance each update");
    }
}
