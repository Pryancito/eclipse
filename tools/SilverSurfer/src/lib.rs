//! SilverSurfer — the Eclipse OS browser.
//!
//! The library is the chrome, the address bar and the file dialog. The Servo
//! engine and the Wayland window are behind the `engine` feature, so the unit
//! tests and the benchmarks run without compiling the engine.

pub mod chrome;
pub mod files;
pub mod home;
pub mod look;
pub mod location;
pub mod session;
pub mod spa_compat;
pub mod wl;

#[cfg(feature = "engine")]
pub mod engine;
#[cfg(feature = "engine")]
pub mod window;

/// Parse argv and open the window. `--help` prints usage and returns.
#[cfg(feature = "engine")]
pub fn launch() -> Result<(), String> {
    let mut fresh = false;
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

Uso: silversurfer [--fresh] [URL]

Sin URL restaura la sesión anterior (o la página de inicio).
--fresh ignora la sesión y abre una pestaña nueva.
Una dirección o host navega; palabras buscan (SILVERSURFER_SEARCH).
Ctrl+L barra, Ctrl+T/W pestaña, Ctrl+Tab cambiar, Ctrl+O lunarfile,
Alt+Izquierda / Alt+Derecha historial, F5 recargar."
            );
            return Ok(());
        }
        if arg == "--fresh" {
            fresh = true;
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("opción desconocida: {arg}"));
        }
        url_arg = Some(arg);
    }

    if let Some(arg) = url_arg {
        let url = location::resolve_omnibox(&arg, &location::search_template())
            .map_err(|_| format!("dirección no válida: {arg}"))?;
        window::run(window::Start::Url(url))
    } else if fresh {
        window::run(window::Start::Fresh)
    } else {
        window::run(window::Start::Restore)
    }
}
