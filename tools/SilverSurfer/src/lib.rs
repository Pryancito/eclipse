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
pub mod wl;

#[cfg(feature = "engine")]
pub mod engine;
#[cfg(feature = "engine")]
pub mod window;

/// Parse argv and open the window. `--help` prints usage and returns.
#[cfg(feature = "engine")]
pub fn launch() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        // Servo relaunches this binary as `--content-process <token>`. That
        // process must enter the engine and stop. Treating the flag as a URL
        // opens another window, which spawns more content processes.
        if arg == "--content-process" {
            let token = args.next().unwrap_or_default();
            servo::run_content_process(token);
            return Ok(());
        }
        if arg == "--help" || arg == "-h" {
            println!(
                "\
SilverSurfer — navegador de Eclipse OS

Uso: silversurfer [URL]

Sin URL abre la página de inicio. Una dirección sin esquema lleva https://.
Ctrl+L barra, Ctrl+T pestaña, Ctrl+W cerrar, Ctrl+O lunarfile,
Alt+Izquierda / Alt+Derecha historial, F5 recargar."
            );
            return Ok(());
        }
        let url = location::parse_location(&arg).map_err(|_| format!("dirección no válida: {arg}"))?;
        window::run(url)
    } else {
        window::run(home::home_url())
    }
}
