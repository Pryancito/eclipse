//! GUI Service - Lanza la sesión gráfica desde disco.
//!
//! Por defecto (sin **`compositor-lunas`**): `execve("/usr/sbin/lightdm", [argv0], environ)`.
//! LightDM es un binario ELF (gestor de pantallas de inicio de sesión); debe estar en el rootfs
//! junto con sus dependencias (Xorg o Wayland, PAM/config, etc.).
//! Con feature **`compositor-lunas`**: abre **`file:/usr/bin/lunas`**, `mmap` + `exec` (ET_EXEC estático).
//!
//! Responsabilidades:
//! 1. lightdm: ruta VFS absoluta + `execve` (defecto).
//! 2. lunas: abrir vía `file:`, `mmap` + `exec` en el mismo flujo de arranque.
//! 3. Salir si falla; si tiene éxito, la imagen del proceso es ya el display manager.

#[cfg(not(feature = "compositor-lunas"))]
use std::ffi::CString;
#[cfg(not(feature = "compositor-lunas"))]
use std::os::raw::c_char;
use std::prelude::v1::*;
use eclipse_relibc as libc;

#[cfg(feature = "compositor-lunas")]
const COMPOSITOR_PATH: &str = "file:/usr/bin/lunas";

/// Binario LightDM (ELF). En algunas distros existe solo en `/usr/bin/lightdm`.
#[cfg(not(feature = "compositor-lunas"))]
const LIGHTDM_EXEC_PATH: &str = "/usr/bin/lightdm";

/// Variables de entorno opcionales para la sesión gráfica (X11 / Wayland en rootfs).
#[cfg(not(feature = "compositor-lunas"))]
fn apply_gui_session_env() {
    // Forzar el uso de /dev/dri/card0 para saltar el bucle de espera de udev en wlroots.
    //let _ = std::env::set_var("WLR_DRM_DEVICES", "/dev/dri/card0");
    //let _ = std::env::set_var("LIBINPUT_QUIRKS_DIR", "/usr/share/libinput");
    // Fontconfig: algunos builds vienen con rutas absolutas del host embebidas.
    // Forzamos el path runtime dentro del rootfs de Eclipse.
    //if std::env::var_os("FONTCONFIG_PATH").is_none() {
        //let _ = std::env::set_var("FONTCONFIG_PATH", "/etc/fonts");
    //}
    //if std::env::var_os("FONTCONFIG_FILE").is_none() {
        //let _ = std::env::set_var("FONTCONFIG_FILE", "/etc/fonts/fonts.conf");
    //}
    // Bring-up default: allow pixman/software renderer if EGL/GBM isn't ready yet.
    // If the user already set it, preserve their choice.
    //let _ = std::env::set_var("WLR_RENDERER", "pixman"); // pixman/software renderer if EGL/GBM isn't ready yet.
    //let _ = std::env::set_var("XDG_RUNTIME_DIR", "/run");
    //let _ = std::env::set_var("LD_LIBRARY_PATH", "/usr/lib");
}

/// Lanza LightDM (`execve` directo; el binario debe ser ELF en el VFS).
#[cfg(not(feature = "compositor-lunas"))]
fn exec_lightdm() {
    apply_gui_session_env();
    let path = CString::new(LIGHTDM_EXEC_PATH).expect("lightdm path");
    let arg0 = CString::new(LIGHTDM_EXEC_PATH).expect("argv0");
    let argv: [*const c_char; 2] = [arg0.as_ptr(), core::ptr::null()];
    let envp = libc::environ_ptr();
    unsafe {
        let r = libc::execve(path.as_ptr(), argv.as_ptr(), envp);
        println!(
            "[GUI-SERVICE] FATAL: execve({}) failed: {}",
            LIGHTDM_EXEC_PATH, r
        );
        libc::exit(1);
    }
}

fn main() {
    let pid = unsafe { libc::getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|              GUI SERVICE - Compositor Launcher               |");
    println!("+--------------------------------------------------------------+");
    // En Eclipse init siempre es PID 1.
    println!("[GUI-SERVICE] PID={}, PPID={}", pid, unsafe { libc::getppid() });
    //#[cfg(feature = "compositor-lunas")]
    //println!("[GUI-SERVICE] exec compositor: {}", COMPOSITOR_PATH);
    //println!("[GUI-SERVICE] exec bash: {}", BASH_EXEC_PATH);
    //#[cfg(not(feature = "compositor-lunas"))]
    //{
    //    println!(
    //        "[GUI-SERVICE] exec compositor: {} -d (execve)",
    //        LIGHTDM_EXEC_PATH
    //    );
    //}

    // IMPORTANT: este proceso hace exec() y se transforma en el compositor.
    // Si esperamos a enviar READY después, nunca ocurrirá. Enviamos READY ahora.
    let ppid = unsafe { libc::getppid() };
    if ppid > 0 {
        let _ = libc::send_ipc(ppid as u32, 255, b"READY");
    }

    // Activar strace para este proceso (se conserva tras execve).
    // pid=0 => proceso actual.
    //unsafe {
        // syscall Linux 545 en nuestro kernel: sys_strace(pid, enable)
        // A través de la libc: eclipse_relibc expone syscall() genérica.
        //let _ = syscall(545, 0usize, 1usize);
    //}

    #[cfg(not(feature = "compositor-lunas"))]
    exec_lightdm();

    #[cfg(feature = "compositor-lunas")]
    exec_lunas_via_mmap();

    //#[cfg(not(feature = "compositor-lunas"))]
    //exec_bash();
}

#[cfg(feature = "compositor-lunas")]
fn exec_lunas_via_mmap() {
    unsafe {
        use eclipse_relibc::{
            eclipse_close, eclipse_open, lseek, mmap, munmap, MAP_FAILED, MAP_PRIVATE, PROT_READ,
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

        // Intento 1: mmap del ejecutable. Puede fallar si el esquema `file:` no implementa fmap.
        // En ese caso hacemos fallback a leer el fichero completo.
        let mapped = mmap(
            core::ptr::null_mut(),
            size,
            PROT_READ,
            MAP_PRIVATE,
            fd,
            0,
        );
        if mapped != MAP_FAILED {
            eclipse_close(fd);
            let binary = core::slice::from_raw_parts(mapped as *const u8, size);
            let result = libc::exec(binary);

            println!("[GUI-SERVICE] FATAL: exec() failed with code {}", result);
            let _ = munmap(mapped, size);
            libc::exit(1);
        }

        // mmap falló: imprimimos errno y hacemos fallback a leer en memoria.
        let errno = *libc::__errno_location();
        println!(
            "[GUI-SERVICE] mmap failed for {} (size={}): errno={}. Falling back to read().",
            COMPOSITOR_PATH, size, errno
        );

        // Intento 2: read() del ELF completo y SYS_EXEC desde buffer.
        // NOTA: en algunos esquemas `fstat()` puede no reportar tamaño (0). Reusamos `size`
        // obtenido por `lseek(SEEK_END)` que sí funciona con `file:`.
        let total = size;
        // Asegurar puntero de fichero al principio.
        let _ = lseek(fd, 0, SEEK_SET);

        let mut buf: Vec<u8> = Vec::with_capacity(total);
        buf.set_len(total);

        let mut off = 0usize;
        while off < total {
            let n = libc::read(
                fd,
                buf.as_mut_ptr().add(off) as *mut _,
                (total - off) as libc::size_t,
            );
            if n < 0 {
                let e = *libc::__errno_location();
                println!(
                    "[GUI-SERVICE] FATAL: read failed for {} at off {}: errno={}",
                    COMPOSITOR_PATH, off, e
                );
                eclipse_close(fd);
                libc::exit(1);
            }
            if n == 0 {
                println!(
                    "[GUI-SERVICE] FATAL: unexpected EOF reading {} at off {} (expected {})",
                    COMPOSITOR_PATH, off, total
                );
                eclipse_close(fd);
                libc::exit(1);
            }
            off += n as usize;
        }
        eclipse_close(fd);

        let result = libc::exec(&buf);
        println!("[GUI-SERVICE] FATAL: exec() failed with code {}", result);
        libc::exit(1);
    }
}
/*#[cfg(not(feature = "compositor-lunas"))]
fn exec_bash() {
    let _ = std::env::set_var("XDG_RUNTIME_DIR", "/run");
    let _ = std::env::set_var("WLR_BACKENDS", "drm");
    let _ = std::env::set_var("WLR_DEVICES", "/dev/dri/card0");
    let path = CString::new(BASH_EXEC_PATH).expect("bash path");
    let arg0 = CString::new(BASH_EXEC_PATH).expect("argv0");
    // `-i`: modo interactivo.
    let arg1 = CString::new("/bin/foot").expect("argv1");
    let argv: [*const c_char; 3] = [arg0.as_ptr(), arg1.as_ptr(), core::ptr::null()];
    let envp = libc::environ_ptr();
    unsafe {
        let r = libc::execve(path.as_ptr(), argv.as_ptr(), envp);
        println!("[GUI-SERVICE] FATAL: execve({}) failed: {}", BASH_EXEC_PATH, r);
        libc::exit(1);
    }
}*/

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
    fn lightdm_uses_vfs_exec_path() {
        assert_eq!(super::LIGHTDM_EXEC_PATH, "/usr/sbin/lightdm");
    }
}
