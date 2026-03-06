#![cfg_attr(not(test), no_main)]
extern crate std;
extern crate alloc;
extern crate eclipse_syscall;

pub mod compositor;
pub mod render;
pub mod input;
pub mod ipc;
pub mod space;
pub mod backend;
pub mod state;

use crate::state::SmithayState;
use crate::ipc::query_input_service_pid;

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

    let _input_pid = query_input_service_pid().expect("Failed to find input service");

    #[cfg(feature = "trace-frames")]
    let mut stats_before = std::libc::SystemStats {
        uptime_ticks: 0,
        idle_ticks: 0,
        total_mem_frames: 0,
        used_mem_frames: 0,
    };

    loop {
        #[cfg(feature = "trace-frames")]
        {
            unsafe {
                let _ = std::libc::get_system_stats(&mut stats_before);
            }
        }

        // Poll events from the backend (IPC + Framebuffer)
        while let Some(event) = state.backend.poll_event() {
            state.handle_event(event);
        }

        // Update state logic (animations, window management)
        state.update();

        // Render current state to backbuffer
        state.render();

        // Swap backbuffer to front
        state.backend.swap_buffers();

        #[cfg(feature = "trace-frames")]
        {
            let mut stats_after = std::libc::SystemStats {
                uptime_ticks: 0,
                idle_ticks: 0,
                total_mem_frames: 0,
                used_mem_frames: 0,
            };
            unsafe {
                let _ = std::libc::get_system_stats(&mut stats_after);
            }
            let delta = stats_after.uptime_ticks.saturating_sub(stats_before.uptime_ticks);
            if delta > TRACE_FRAME_THRESHOLD_TICKS {
                println!(
                    "[SMITHAY] frame slow: {} ticks (counter={} recv_attempts={} messages={})",
                    delta,
                    state.counter,
                    state.backend.ipc.recv_attempts,
                    state.backend.ipc.message_count
                );
            }
            if state.counter > 0 && state.counter % TRACE_HEARTBEAT_EVERY == 0 {
                println!(
                    "[SMITHAY] heartbeat counter={} recv_attempts={} messages={}",
                    state.counter,
                    state.backend.ipc.recv_attempts,
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
