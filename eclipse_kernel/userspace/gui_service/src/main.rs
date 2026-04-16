//! GUI Service - Lanza el compositor Wayland desde disco (exec vía mmap).
//!
//! Por defecto: **`file:/usr/bin/labwc`** (PIE musl + `PT_INTERP`; requiere intérprete en `/lib`).
//! Con feature **`compositor-lunas`**: **`file:/usr/bin/lunas`** (ET_EXEC estático).
//!
//! Responsabilidades:
//! 1. Lanzar `/sbin/seatd` como proceso daemon (requerido por wlroots/labwc para abrir la sesión DRM).
//! 2. Abrir el ELF del compositor vía esquema `file:`.
//! 3. `mmap` + `exec` en el mismo flujo de arranque.
//! 4. Salir si falla; si tiene éxito, la imagen del proceso es ya el compositor.

use std::prelude::v1::*;
use eclipse_relibc as libc;

/// Buffer to load compositor when mmap fails (e.g. file: scheme read path issues)
const MAX_COMPOSITOR_SIZE: usize = 16 * 1024 * 1024;
/// Spinlock-protected load buffer for thread-safe SMP access.
static LOAD_BUF: libc::Spinlock<[u8; MAX_COMPOSITOR_SIZE]> = libc::Spinlock::new([0; MAX_COMPOSITOR_SIZE]);

/// Path to the seatd seat-management daemon (required by wlroots/labwc).
const SEATD_PATH: &str = "file:/sbin/seatd";
/// Seconds to wait after forking seatd before exec'ing the compositor, giving
/// seatd time to create and listen on `/run/seatd.sock`.
const SEATD_WAIT_SECS: u64 = 2;

#[cfg(feature = "compositor-lunas")]
const COMPOSITOR_PATH: &str = "file:/usr/bin/labwc";
#[cfg(not(feature = "compositor-lunas"))]
const COMPOSITOR_PATH: &str = "file:/usr/bin/labwc";

/// Fork a child process and exec `/sbin/seatd` inside it.
///
/// Returns `true` when the child was launched.  On any error (binary not
/// found, mmap failure, fork failure) it prints a warning and returns `false`
/// so the compositor can still be attempted without a seat manager.
///
/// The caller is responsible for sleeping long enough that seatd has time to
/// bind its Unix-domain socket before the compositor tries to connect.
unsafe fn spawn_seatd() -> bool {
    use eclipse_relibc::{eclipse_open, eclipse_close, lseek, mmap, PROT_READ, PROT_EXEC, MAP_PRIVATE};
    const SEEK_SET: i32 = 0;
    const SEEK_END: i32 = 2;

    let fd = eclipse_open(SEATD_PATH, libc::O_RDONLY, 0);
    if fd < 0 {
        println!("[GUI-SERVICE] seatd not found at {} (skipping)", SEATD_PATH);
        return false;
    }

    let sz = lseek(fd, 0, SEEK_END);
    let _ = lseek(fd, 0, SEEK_SET);
    if sz <= 0 {
        eclipse_close(fd);
        println!("[GUI-SERVICE] seatd: file empty or lseek failed (skipping)");
        return false;
    }

    let size = sz as usize;
    let mapped = mmap(core::ptr::null_mut(), size, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
    eclipse_close(fd);
    if mapped.is_null() || (mapped as isize) <= 0 {
        println!("[GUI-SERVICE] seatd: mmap failed (skipping)");
        return false;
    }

    let pid = libc::fork();
    if pid < 0 {
        println!("[GUI-SERVICE] seatd: fork() failed (skipping)");
        return false;
    }

    if pid == 0 {
        // Child process: replace image with seatd and never return.
        let binary = core::slice::from_raw_parts(mapped as *const u8, size);
        let _ = libc::exec(binary);
        libc::exit(1);
    }

    // Parent: seatd child is starting up.
    println!("[GUI-SERVICE] seatd launched (PID {})", pid);
    true
}

fn main() {
    let pid = unsafe { libc::getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|              GUI SERVICE - Compositor Launcher               |");
    println!("+--------------------------------------------------------------+");
    // En Eclipse init siempre es PID 1.
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, unsafe { libc::getppid() });
    println!("[GUI-SERVICE] exec compositor: {}", COMPOSITOR_PATH);

    // Launch seatd before the compositor so that wlroots can open a DRM session.
    // spawn_seatd() forks quickly; we sleep afterwards to give seatd time to bind
    // /run/seatd.sock before labwc tries to connect.
    let seatd_launched = unsafe { spawn_seatd() };

    // IMPORTANT: este proceso hace exec() y se transforma en el compositor.
    // Si esperamos a enviar READY después, nunca ocurrirá. Enviamos READY ahora.
    let ppid = unsafe { libc::getppid() };
    if ppid > 0 {
        let _ = libc::send_ipc(ppid as u32, 255, b"READY");
    }

    // Wait for seatd to bind its socket before starting the compositor.
    if seatd_launched {
        println!("[GUI-SERVICE] waiting {}s for seatd to bind /run/seatd.sock…", SEATD_WAIT_SECS);
        std::thread::sleep(std::time::Duration::from_secs(SEATD_WAIT_SECS));
    }

    // Transformar este proceso en el compositor (labwc o lunas).
    // Al usar exec(), el PID se mantiene pero la imagen del proceso cambia.
    unsafe {
        use eclipse_relibc::{eclipse_open, eclipse_close, lseek, mmap, munmap, PROT_READ, PROT_EXEC, MAP_PRIVATE};
        const SEEK_SET: i32 = 0;
        const SEEK_END: i32 = 2;

        let fd = eclipse_open(COMPOSITOR_PATH, libc::O_RDONLY, 0);
        if fd < 0 {
            println!("[GUI-SERVICE] FATAL: Cannot open {} for exec", COMPOSITOR_PATH);
            unsafe { libc::exit(1); }
        }

        // lseek returns (off_t)-1 on error; 0 is valid for an empty file.
        let sz = lseek(fd, 0, SEEK_END);
        // Always reset position; mmap uses explicit offset=0 so this is also diagnostic.
        let _ = lseek(fd, 0, SEEK_SET);
        if sz < 0 {
            println!("[GUI-SERVICE] FATAL: lseek(SEEK_END) failed for {} for exec", COMPOSITOR_PATH);
            eclipse_close(fd);
            unsafe { libc::exit(1); }
        }
        if sz == 0 {
            println!("[GUI-SERVICE] FATAL: compositor file has size 0 (check FS CONTENT TLV / image build)");
            eclipse_close(fd);
            unsafe { libc::exit(1); }
        }
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
            unsafe { libc::exit(1); }
        }

        let binary = core::slice::from_raw_parts(mapped as *const u8, size);
        let result = libc::exec(binary);
        
        // Si exec() falla, llegamos aquí:
        println!("[GUI-SERVICE] FATAL: exec() failed with code {}", result);
        let _ = munmap(mapped, size);
        unsafe { libc::exit(1); }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn compositor_path_is_file_uri() {
        assert!(super::COMPOSITOR_PATH.starts_with("file:/usr/bin/"));
        #[cfg(feature = "compositor-lunas")]
        assert!(super::COMPOSITOR_PATH.ends_with("/lunas"));
        #[cfg(not(feature = "compositor-lunas"))]
        assert!(super::COMPOSITOR_PATH.ends_with("/labwc"));
    }

    #[test]
    fn seatd_path_is_file_uri() {
        assert!(super::SEATD_PATH.starts_with("file:/sbin/seatd"));
    }
}
