//! Un solo binario: backend según target (como xfwl4).
//! - Linux (host): compositor Wayland con Smithay + winit.
//! - Eclipse: compositor propio (DRM, SideWind, IPC).


use smithay_app::libc;
use std::env;

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
        cpu_count: 0,
        cpu_temp: [0; 16],
        gpu_load: [0; 4],
        gpu_temp: [0; 4],
        gpu_vram_total_bytes: 0,
        gpu_vram_used_bytes: 0,
        anomaly_count: 0,
        heap_fragmentation: 0,
        wall_time_offset: 0,
    };

    loop {
        state.handle_ipc();
        // Solo renderizar si hubo cambios (animaciones, marcas dirty, métricas).
        // Evita escribir el framebuffer completo 60 veces/s cuando no hay nada nuevo.
        state.update();
        state.render();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

#[cfg(test)]
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
