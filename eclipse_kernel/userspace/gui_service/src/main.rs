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

const COMPOSITOR_PATH: &str = "file:/usr/bin/lunas";

// No longer needed: we use exec() in main directly

fn main() {
    let pid = unsafe { std::libc::getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|           GUI SERVICE - Sidewind Compositor Launcher         |");
    println!("+--------------------------------------------------------------+");
    // En Eclipse init siempre es PID 1.
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, unsafe { std::libc::getppid() });
    println!("[GUI-SERVICE] Transforming into smithay_app via exec...");

    // Transformar este proceso en smithay_app.
    // Al usar exec(), el PID se mantiene pero la imagen del proceso cambia.
    // 'init' recibirá el READY enviado por smithay_app en su propio arranque.
    unsafe {
        use std::libc::{eclipse_open, eclipse_close, lseek, mmap, munmap, PROT_READ, PROT_EXEC, MAP_PRIVATE};
        const SEEK_SET: i32 = 0;
        const SEEK_END: i32 = 2;

        let fd = eclipse_open(COMPOSITOR_PATH, std::libc::O_RDONLY, 0);
        if fd < 0 {
            println!("[GUI-SERVICE] FATAL: Cannot open {} for exec", COMPOSITOR_PATH);
            unsafe { std::libc::exit(1); }
        }

        let sz = lseek(fd, 0, SEEK_END);
        if sz <= 0 {
            println!("[GUI-SERVICE] FATAL: lseek(SEEK_END) failed for {} for exec", COMPOSITOR_PATH);
            eclipse_close(fd);
            unsafe { std::libc::exit(1); }
        }
        let _ = lseek(fd, 0, SEEK_SET);
        let size = sz as usize;

        let mapped = mmap(
            core::ptr::null_mut(),
            size,
            PROT_READ | PROT_EXEC,
            MAP_PRIVATE,
            fd,
            0,
        );
        eclipse_close(fd);

        if mapped.is_null() || (mapped as isize) <= 0 {
            println!("[GUI-SERVICE] FATAL: mmap failed for {} for exec", COMPOSITOR_PATH);
            unsafe { std::libc::exit(1); }
        }

        let binary = core::slice::from_raw_parts(mapped as *const u8, size);
        let result = std::libc::exec(binary);
        
        // Si exec() falla, llegamos aquí:
        println!("[GUI-SERVICE] FATAL: exec() failed with code {}", result);
        let _ = munmap(mapped, size);
        unsafe { std::libc::exit(1); }
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
