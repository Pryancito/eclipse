//! GUI Service - Lanza el compositor Wayland desde disco (exec vía mmap).
//!
//! Por defecto: **`file:/usr/bin/labwc`** (PIE musl + `PT_INTERP`; requiere intérprete en `/lib`).
//! Con feature **`compositor-lunas`**: **`file:/usr/bin/lunas`** (ET_EXEC estático).
//!
//! Responsabilidades:
//! 1. Abrir el ELF del compositor vía esquema `file:`.
//! 2. `mmap` + `exec` en el mismo flujo de arranque.
//! 3. Salir si falla; si tiene éxito, la imagen del proceso es ya el compositor.

use std::prelude::v1::*;

/// Buffer to load compositor when mmap fails (e.g. file: scheme read path issues)
const MAX_COMPOSITOR_SIZE: usize = 16 * 1024 * 1024;
/// Spinlock-protected load buffer for thread-safe SMP access.
static LOAD_BUF: std::libc::Spinlock<[u8; MAX_COMPOSITOR_SIZE]> = std::libc::Spinlock::new([0; MAX_COMPOSITOR_SIZE]);

const COMPOSITOR_PATH: &str = "file:/usr/bin/lunas";

fn main() {
    let pid = unsafe { std::libc::getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|              GUI SERVICE - Compositor Launcher               |");
    println!("+--------------------------------------------------------------+");
    // En Eclipse init siempre es PID 1.
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, unsafe { std::libc::getppid() });
    println!("[GUI-SERVICE] exec compositor: {}", COMPOSITOR_PATH);

    // Transformar este proceso en el compositor (labwc o lunas).
    // Al usar exec(), el PID se mantiene pero la imagen del proceso cambia.
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
    fn compositor_path_is_file_uri() {
        assert!(COMPOSITOR_PATH.starts_with("file:/usr/bin/"));
        #[cfg(feature = "compositor-lunas")]
        assert!(COMPOSITOR_PATH.ends_with("/lunas"));
        #[cfg(not(feature = "compositor-lunas"))]
        assert!(COMPOSITOR_PATH.ends_with("/labwc"));
    }
}
