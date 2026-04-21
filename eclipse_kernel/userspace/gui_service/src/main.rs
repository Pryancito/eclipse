//! GUI Service - Lanza el compositor Wayland desde disco.
//!
//! Por defecto (**labwc**): `execve("/usr/bin/labwc", [..., "-d"], environ)` para activar
//! el nivel de log de wlroots (`WLR_DEBUG`) sin depender de argv en `SYS_EXEC`.
//! Con feature **`compositor-lunas`**: abre **`file:/usr/bin/lunas`**, `mmap` + `exec` (ET_EXEC estático).
//!
//! Responsabilidades:
//! 1. labwc: ruta VFS absoluta + `execve` con `-d`.
//! 2. lunas: abrir vía `file:`, `mmap` + `exec` en el mismo flujo de arranque.
//! 3. Salir si falla; si tiene éxito, la imagen del proceso es ya el compositor.

#[cfg(not(feature = "compositor-lunas"))]
use std::ffi::CString;
#[cfg(not(feature = "compositor-lunas"))]
use std::os::raw::c_char;
use std::prelude::v1::*;
use eclipse_relibc as libc;

#[cfg(feature = "compositor-lunas")]
const COMPOSITOR_PATH: &str = "file:/usr/bin/lunas";

/// Ruta VFS para labwc (execve del kernel; no esquema `file:`).
#[cfg(not(feature = "compositor-lunas"))]
const LABWC_EXEC_PATH: &str = "/usr/bin/labwc";

fn main() {
    let pid = unsafe { libc::getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|              GUI SERVICE - Compositor Launcher               |");
    println!("+--------------------------------------------------------------+");
    // En Eclipse init siempre es PID 1.
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, unsafe { libc::getppid() });
    #[cfg(feature = "compositor-lunas")]
    println!("[GUI-SERVICE] exec compositor: {}", COMPOSITOR_PATH);
    #[cfg(not(feature = "compositor-lunas"))]
    println!(
        "[GUI-SERVICE] exec compositor: {} -d (execve)",
        LABWC_EXEC_PATH
    );

    // IMPORTANT: este proceso hace exec() y se transforma en el compositor.
    // Si esperamos a enviar READY después, nunca ocurrirá. Enviamos READY ahora.
    let ppid = unsafe { libc::getppid() };
    if ppid > 0 {
        let _ = libc::send_ipc(ppid as u32, 255, b"READY");
    }

    #[cfg(not(feature = "compositor-lunas"))]
    exec_labwc_debug();

    #[cfg(feature = "compositor-lunas")]
    exec_lunas_via_mmap();
}

/// labwc: `execve` con `-d` (argv real; `SYS_EXEC` no soporta argumentos).
#[cfg(not(feature = "compositor-lunas"))]
fn exec_labwc_debug() {
    // libinput usa udev + ficheros de quirks bajo /usr/share/libinput; en Eclipse
    // `libinput_udev_assign_seat` falla al *start* (antes de que WLR_LIBINPUT_NO_DEVICES
    // evite solo el caso "creado pero sin dispositivos"). Cargar solo DRM aquí; labwc
    // añade el backend headless después en server.c.
    //let _ = std::env::set_var("WLR_BACKENDS", "drm");
    //let _ = std::env::set_var("WLR_LIBINPUT_NO_DEVICES", "1");
    // Usar el nuevo backend nativo de Eclipse para libseat.
    let _ = std::env::set_var("LIBSEAT_BACKEND", "eclipse");
    // Forzar el uso de /dev/dri/card0 para saltar el bucle de espera de udev en wlroots.
    let _ = std::env::set_var("WLR_DRM_DEVICES", "/dev/dri/card0");

    //let _ = std::env::set_var("WLR_DRM_NO_ATOMIC", "1");
    //let _ = std::env::set_var("WLR_RENDERER", "pixman");

    let path = CString::new(LABWC_EXEC_PATH).expect("labwc path");
    let arg0 = CString::new(LABWC_EXEC_PATH).expect("argv0");
    let arg_d = CString::new("-d").expect("-d");
    let argv: [*const c_char; 3] = [arg0.as_ptr(), arg_d.as_ptr(), core::ptr::null()];
    let envp = libc::environ_ptr();
    unsafe {
        let r = libc::execve(path.as_ptr(), argv.as_ptr(), envp);
        println!("[GUI-SERVICE] FATAL: execve({}) failed: {}", LABWC_EXEC_PATH, r);
        libc::exit(1);
    }
}

#[cfg(feature = "compositor-lunas")]
fn exec_lunas_via_mmap() {
    unsafe {
        use eclipse_relibc::{
            eclipse_close, eclipse_open, lseek, mmap, munmap, MAP_PRIVATE, PROT_EXEC, PROT_READ,
        };
        const SEEK_SET: i32 = 0;
        const SEEK_END: i32 = 2;

        let fd = eclipse_open(COMPOSITOR_PATH, libc::O_RDONLY, 0);
        if fd < 0 {
            println!("[GUI-SERVICE] FATAL: Cannot open {} for exec", COMPOSITOR_PATH);
            libc::exit(1);
        }

        let sz = lseek(fd, 0, SEEK_END);
        let _ = lseek(fd, 0, SEEK_SET);
        if sz < 0 {
            println!(
                "[GUI-SERVICE] FATAL: lseek(SEEK_END) failed for {} for exec",
                COMPOSITOR_PATH
            );
            eclipse_close(fd);
            libc::exit(1);
        }
        if sz == 0 {
            println!("[GUI-SERVICE] FATAL: compositor file has size 0 (check FS CONTENT TLV / image build)");
            eclipse_close(fd);
            libc::exit(1);
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
            libc::exit(1);
        }

        let binary = core::slice::from_raw_parts(mapped as *const u8, size);
        let result = libc::exec(binary);

        println!("[GUI-SERVICE] FATAL: exec() failed with code {}", result);
        let _ = munmap(mapped, size);
        libc::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "compositor-lunas")]
    #[test]
    fn compositor_path_is_file_uri_lunas() {
        assert!(super::COMPOSITOR_PATH.starts_with("file:/usr/bin/"));
        assert!(super::COMPOSITOR_PATH.ends_with("/lunas"));
    }

    #[cfg(not(feature = "compositor-lunas"))]
    #[test]
    fn labwc_uses_vfs_exec_path() {
        assert_eq!(super::LABWC_EXEC_PATH, "/usr/bin/labwc");
    }
}
