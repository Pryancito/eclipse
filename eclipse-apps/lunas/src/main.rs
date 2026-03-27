//! Lunas — Entorno de Escritorio para Eclipse OS.
//!
//! Un binario, dos backends según el target:
//! - Eclipse OS: compositor propio (DRM + SideWind + IPC), con soporte Wayland y XWayland.
//! - Linux (host): stub informativo (el DE real corre sobre Eclipse).

#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_syscall;

// ---- Entry point Linux ----
#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    eprintln!("[LUNAS] Lunas es el entorno de escritorio de Eclipse OS.");
    eprintln!("[LUNAS] Para ejecutarlo compila con --target x86_64-unknown-eclipse.");
    std::process::exit(1);
}

// ---- Entry point Eclipse ----
#[cfg(target_vendor = "eclipse")]
use std::prelude::v1::*;
#[cfg(target_vendor = "eclipse")]
use lunas::libc;

#[cfg(target_vendor = "eclipse")]
#[cfg(not(test))]
fn main() {
    use lunas::state::LunasState;

    // Inicializar el estado en la pila para evitar fallos del heap allocator.
    let mut state = LunasState::new().expect("Failed to initialize Lunas Desktop Environment");

    let self_pid = unsafe { libc::getpid() as u32 };
    let _ = eclipse_syscall::call::register_log_hud(self_pid);

    loop {
        // 1. Drenar IPC al inicio del frame para no llenar el buzón del kernel.
        state.drain_ipc(64);
        // 2. Procesar todos los eventos pendientes (input + IPC).
        while let Some(event) = state.backend.poll_event() {
            state.handle_event(&event);
        }
        // 3. Actualizar animaciones, métricas y shell.
        state.update();
        // 4. Renderizar el frame si hubo cambios.
        state.render();

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

#[cfg(test)]
#[cfg(target_vendor = "eclipse")]
mod tests {
    use lunas::state::LunasState;

    #[test]
    fn main_loop_runs_without_hanging() {
        let mut state = LunasState::new().expect("state");
        const N: u64 = 100_000;
        for _ in 0..N {
            state.drain_ipc(16);
            while let Some(event) = state.backend.poll_event() {
                state.handle_event(&event);
            }
            state.update();
            state.render();
        }
        assert!(state.frame_counter >= N);
    }
}
