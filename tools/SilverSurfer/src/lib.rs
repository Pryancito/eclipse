//! SilverSurfer — the Eclipse OS browser.
//!
//! The library is the chrome, the address bar and the file dialog. The Servo
//! engine and the Wayland window are behind the `engine` feature, so the unit
//! tests and the benchmarks run without compiling the engine.

pub mod about;
pub mod bookmarks;
pub mod chrome;
pub mod downloads;
pub mod files;
pub mod filter;
pub mod history;
pub mod home;
pub mod location;
pub mod look;
pub mod menu;
pub mod passwords;
pub mod privacy;
pub mod session;
pub mod spa_compat;
pub mod spawn;
pub mod wl;

#[cfg(feature = "engine")]
pub mod clipboard;
#[cfg(feature = "engine")]
pub mod engine;
#[cfg(feature = "engine")]
pub mod window;

/// Parse argv and open the window. `--help` prints usage and returns.
#[cfg(feature = "engine")]
pub fn launch() -> Result<(), String> {
    let mut fresh = false;
    let mut private = false;
    let mut url_arg: Option<String> = None;
    for arg in std::env::args().skip(1) {
        if arg == "--content-process" {
            // Servo relaunches this binary as `--content-process <token>`.
            let token = std::env::args()
                .skip_while(|a| a != "--content-process")
                .nth(1)
                .unwrap_or_default();
            servo::run_content_process(token);
            return Ok(());
        }
        if arg == "--help" || arg == "-h" {
            println!(
                "\
SilverSurfer — navegador de Eclipse OS

Uso: silversurfer [--fresh|--private] [URL]

Sin URL restaura la sesión anterior (o la página de inicio).
--fresh ignora la sesión y abre una pestaña nueva.
--private ventana privada (sin sesión; limpia datos al cerrar).
Una dirección o host navega; palabras buscan (SILVERSURFER_SEARCH).
Ctrl+L barra, Ctrl+T/W pestaña, Ctrl+Tab cambiar, Ctrl+O lunarfile,
Ctrl+/-/0 zoom, Ctrl+D marcador, Ctrl+F buscar en página,
Ctrl+S descargar URL, Ctrl+J descargas, F11 pantalla completa,
Ctrl+N ventana nueva, Ctrl+Shift+P ventana privada,
Ctrl+Shift+Delete borrar datos, Ctrl+Shift+L guardar login,
arrastrar pestañas para reordenar,
Alt+Izquierda / Alt+Derecha historial, F5 recargar.
about:history · about:bookmarks · about:downloads · about:privacy ·
about:cookies · about:clear · about:filters · about:passwords.
Clic derecho en la página abre el menú contextual.
SILVERSURFER_ADBLOCK=0 desactiva el bloqueo lite."
            );
            return Ok(());
        }
        if arg == "--fresh" {
            fresh = true;
            continue;
        }
        if arg == "--private" {
            private = true;
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("opción desconocida: {arg}"));
        }
        url_arg = Some(arg);
    }

    if private {
        let url = match url_arg {
            Some(arg) => location::resolve_omnibox(&arg, &location::search_template())
                .map_err(|_| format!("dirección no válida: {arg}"))?,
            None => home::home_url(),
        };
        window::run(window::Start::Private(url))
    } else if let Some(arg) = url_arg {
        let url = location::resolve_omnibox(&arg, &location::search_template())
            .map_err(|_| format!("dirección no válida: {arg}"))?;
        window::run(window::Start::Url(url))
    } else if fresh {
        window::run(window::Start::Fresh)
    } else {
        window::run(window::Start::Restore)
    }
}

#[cfg(test)]
mod surfman_fallback {
    #[test]
    fn egl_init_failure_is_an_error_not_an_abort() {
        let aborts = |src: &str| {
            src.lines()
                .any(|line| line.trim_start().starts_with("assert_ne!(ok, egl::FALSE)"))
        };
        let wayland = include_str!("../vendor/surfman/src/wayland/connection.rs");
        let x11 = include_str!("../vendor/surfman/src/x11/connection.rs");
        assert!(!aborts(wayland), "wayland eglInitialize still aborts");
        assert!(
            wayland.contains("ok == egl::FALSE"),
            "wayland eglInitialize failure must return an error"
        );
        assert!(!aborts(x11), "x11 eglInitialize still aborts");
        assert!(
            x11.contains(
                "fn create_egl_display(display: *mut Display) -> Result<EGLDisplay, Error>"
            ),
            "x11 eglInitialize failure must return an error"
        );
    }
}
