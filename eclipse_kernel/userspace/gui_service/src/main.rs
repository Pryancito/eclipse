//! GUI Service - Launches the Sidewind compositor (smithay_app)
//!
//! Responsibilities:
//! 1. Wait for filesystem to be ready.
//! 2. Launch smithay_app from disk.
//! 3. Exit after successful launch (one-shot supervisor).

use std::prelude::v1::*;

/// Buffer to load compositor when mmap fails (e.g. file: scheme read path issues)
const MAX_COMPOSITOR_SIZE: usize = 16 * 1024 * 1024;
/// Spinlock-protected load buffer for thread-safe SMP access.
static LOAD_BUF: std::libc::Spinlock<[u8; MAX_COMPOSITOR_SIZE]> = std::libc::Spinlock::new([0; MAX_COMPOSITOR_SIZE]);

const COMPOSITOR_PATH: &str = "file:/usr/bin/smithay_app";

/// Load and spawn smithay_app. Returns the child PID or -1 on failure.
unsafe fn spawn_compositor() -> i32 {
    use std::libc::{eclipse_open, eclipse_close, eclipse_read, eclipse_spawn, lseek, mmap, munmap, PROT_READ, PROT_EXEC, MAP_PRIVATE};
    const SEEK_SET: i32 = 0;
    const SEEK_END: i32 = 2;
    let fd = eclipse_open(COMPOSITOR_PATH, std::libc::O_RDONLY, 0);
    if fd < 0 {
        println!("[GUI-SERVICE] ERROR: Cannot open {}", COMPOSITOR_PATH);
        return -1;
    }

    // Obtener tamaño del archivo usando lseek.
    let sz = lseek(fd, 0, SEEK_END);
    if sz <= 0 {
        println!("[GUI-SERVICE] ERROR: lseek(SEEK_END) failed for {}", COMPOSITOR_PATH);
        eclipse_close(fd);
        return -1;
    }
    let _ = lseek(fd, 0, SEEK_SET);
    let size = sz as usize;
    let pid = {
        let mapped = mmap(
            core::ptr::null_mut(),
            size,
            PROT_READ | PROT_EXEC,
            MAP_PRIVATE,
            fd,
            0,
        );
        if !mapped.is_null() && (mapped as isize) > 0 {
            println!("[GUI-SERVICE] Mapped compositor at {:p}", mapped);
            eclipse_close(fd);
            let binary = core::slice::from_raw_parts(mapped as *const u8, size);
            let pid = eclipse_spawn(binary, Some("smithay_app"));
            let _ = munmap(mapped, size);
            pid
        } else {
            // mmap falló, intentamos lectura manual al buffer protegido.
            let _ = lseek(fd, 0, SEEK_SET);
            let mut load_guard = LOAD_BUF.lock();
            let read_size = size.min(MAX_COMPOSITOR_SIZE);
            let n = {
                let buf = &mut load_guard[..read_size];
                let result = eclipse_read(fd as u32, buf);
                eclipse_close(fd);
                result
            };
            if n < 0 || n as usize != size {
                println!("[GUI-SERVICE] ERROR: read failed for {} (got {})", COMPOSITOR_PATH, n);
                return -1;
            }
            eclipse_spawn(&load_guard[..size], Some("smithay_app"))
        }
    };

    pid as i32
}

fn main() {
    let pid = unsafe { std::libc::getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|           GUI SERVICE - Sidewind Compositor Launcher         |");
    println!("+--------------------------------------------------------------+");
    let ppid = unsafe { std::libc::getppid() };
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, ppid);

    // En Eclipse init siempre es PID 1; no dependemos de getppid() para READY.
    let init_pid: u32 = 1;
    println!("[GUI-SERVICE] Sending READY to init (PID {})...", init_pid);
    let _ = std::libc::send_ipc(init_pid, 255, b"READY");

    // Proceso one-shot: intenta lanzar smithay_app y sale.
    let child_pid = unsafe { spawn_compositor() };
    if child_pid > 0 {
        println!("[GUI-SERVICE] smithay_app started with PID {}", child_pid);
        println!("[GUI-SERVICE] Launcher done; exiting.");
    } else {
        println!("[GUI-SERVICE] ERROR: Failed to start smithay_app");
    }
    loop {
        unsafe { std::libc::sleep_ms(1000); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositor_path_absolute() {
        assert!(COMPOSITOR_PATH.starts_with("file:/"));
        assert!(COMPOSITOR_PATH.contains("smithay_app"));
    }
}
