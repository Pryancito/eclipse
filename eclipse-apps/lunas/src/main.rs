//! Lunas Desktop Environment — Entry point.
//! - Linux (host): mock mode for testing.
//! - Eclipse: native compositor (DRM, SideWind, IPC).

#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
extern crate eclipse_syscall;

// ---- Entry point Linux: mock mode ----
#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    eprintln!("[LUNAS] Desktop environment requires Eclipse OS target.");
    eprintln!("[LUNAS] Use --target for Eclipse OS cross-compilation.");
    std::process::exit(1);
}

#[cfg(target_vendor = "eclipse")]
use std::prelude::v1::*;
#[cfg(target_vendor = "eclipse")]
use lunas::libc;

// ---- Entry point Eclipse: native desktop environment ----
#[cfg(target_vendor = "eclipse")]
#[cfg(not(test))]
fn main() {
    use lunas::state::LunasState;

    let mut state = LunasState::new().expect("Failed to initialize Lunas Desktop");

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
        const N: u64 = 500_000;
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
