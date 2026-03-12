//! Un solo binario: backend según target (como xfwl4).
//! - Linux (host): compositor Wayland con Smithay + winit.
//! - Eclipse: compositor propio (DRM, SideWind, IPC).

#![cfg_attr(not(target_os = "linux"), no_std)]

#[cfg(not(target_os = "linux"))]
extern crate alloc;
#[cfg(not(target_os = "linux"))]
extern crate eclipse_std as std;

#[cfg(not(target_os = "linux"))]
extern crate eclipse_syscall;
#[cfg(target_os = "linux")]
use smithay_app::smithay_wayland;

// ---- Entry point Linux: Smithay Wayland ----
#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = smithay_wayland::run() {
        eprintln!("[SMITHAY] Error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
use std::prelude::v1::*;
#[cfg(not(target_os = "linux"))]
use smithay_app::libc;

// ---- Entry point Eclipse: compositor propio ----
#[cfg(not(target_os = "linux"))]
#[cfg(not(test))]
fn main() {
    use smithay_app::state::SmithayState;
    use smithay_app::ipc::{query_input_service_pid, subscribe_to_input};
    use core::matches;

    println!("[SMITHAY] Starting via Eclipse Runtime...");

    // Stack allocation: evita fallo del heap allocator (GPF CR2=0 en Box::new).
    // La pila de userspace es 1MB; SmithayState cabe. Si hace falta heap después, mmap directo funciona.
    let mut state = SmithayState::new().expect("Failed to initialize Smithay State");

    match query_input_service_pid() {
        Some(input_pid) => {
            if !state.backend.input_scheme_available() {
                let self_pid = unsafe { libc::getpid() as u32 };
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

    let self_pid = unsafe { libc::getpid() as u32 };
    if eclipse_syscall::call::register_log_hud(self_pid).is_err() {
        println!("[SMITHAY] Warning: register_log_hud failed (kernel may not support it yet)");
    }

    #[cfg(feature = "trace-frames")]
    let _stats_before = libc::SystemStats {
        uptime_ticks: 0,
        idle_ticks: 0,
        total_mem_frames: 0,
        used_mem_frames: 0,
    };

    let mut last_render = std::time::Instant::now();

    // Diagnostics thresholds and accumulators — only active under the
    // `trace-frames` Cargo feature (zero overhead in default builds).
    #[cfg(feature = "trace-frames")]
    const SLOW_FRAME_THRESHOLD_MS: u64 = 500;
    #[cfg(feature = "trace-frames")]
    const HIGH_MEM_THRESHOLD: f32 = 0.85;
    #[cfg(feature = "trace-frames")]
    const MEM_WARNING_INTERVAL: u64 = 60;

    #[cfg(feature = "trace-frames")]
    let mut max_frame_ms: u64 = 0;

    loop {
        #[cfg(feature = "trace-frames")]
        let frame_start = std::time::Instant::now();

        state.handle_ipc();
        let is_busy = state.update();
        let elapsed_since_render = last_render.elapsed();
        if is_busy || state.dirty || elapsed_since_render >= std::time::Duration::from_millis(500) {
            state.render();
            state.dirty = false;
            last_render = std::time::Instant::now();
        }

        // ── Diagnostics ────────────────────────────────────────────────────────
        #[cfg(feature = "trace-frames")]
        {
            let frame_ms = frame_start.elapsed().as_millis() as u64;
            if frame_ms > max_frame_ms {
                max_frame_ms = frame_ms;
            }

            // Warn about individual slow frames that may indicate a hang.
            if frame_ms > SLOW_FRAME_THRESHOLD_MS {
                println!(
                    "[SMITHAY] SLOW_FRAME {}ms counter={} inputs={} messages={} attempts={} windows={}",
                    frame_ms,
                    state.counter,
                    state.input_event_count,
                    state.backend.ipc.message_count,
                    state.backend.ipc.recv_attempts,
                    state.space.window_count,
                );
            }

            // Warn about memory pressure. Use a single fold to compute both the
            // active surface count and total mapped MB in one pass.
            if state.mem_usage > HIGH_MEM_THRESHOLD && state.counter % MEM_WARNING_INTERVAL == 0 {
                let (active_surfaces, mapped_mb) = state.surfaces.iter()
                    .filter(|s| s.active)
                    .fold((0usize, 0f32), |(count, mb), s| {
                        (count + 1, mb + s.buffer_size as f32 / (1024.0 * 1024.0))
                    });
                println!(
                    "[SMITHAY] HIGH_MEM {:.1}% surfaces={} mapped={:.1}MB",
                    state.mem_usage * 100.0,
                    active_surfaces,
                    mapped_mb,
                );
            }
        }
        // ── End diagnostics ────────────────────────────────────────────────────

        let frame_target = std::time::Duration::from_millis(16);
        let elapsed = last_render.elapsed();
        if !is_busy && !state.dirty {
            std::thread::sleep(std::time::Duration::from_millis(4));
        } else if elapsed < frame_target {
            std::thread::sleep(frame_target - elapsed);
        }

        // Periodic heartbeat — emitted every ~10 s (600 iterations at ~16 ms/frame).
        // Use a single fold to compute active surface count and mapped MB in one pass.
        #[cfg(feature = "trace-frames")]
        if state.counter > 0 && state.counter % 600 == 0 {
            let (active_surfaces, mapped_mb) = state.surfaces.iter()
                .filter(|s| s.active)
                .fold((0usize, 0f32), |(count, mb), s| {
                    (count + 1, mb + s.buffer_size as f32 / (1024.0 * 1024.0))
                });
            println!(
                "[SMITHAY] heartbeat counter={} messages={} attempts={} inputs={} \
                 mem={:.1}% cpu={:.1}% windows={} surfaces={} mapped={:.1}MB max_frame={}ms",
                state.counter,
                state.backend.ipc.message_count,
                state.backend.ipc.recv_attempts,
                state.input_event_count,
                state.mem_usage * 100.0,
                state.cpu_usage * 100.0,
                state.space.window_count,
                active_surfaces,
                mapped_mb,
                max_frame_ms,
            );
            max_frame_ms = 0;
        }
    }
}

#[cfg(test)]
#[cfg(not(target_os = "linux"))]
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
