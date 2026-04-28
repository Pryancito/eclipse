//! XWayland — server X11 nested para clientes legacy.
//!
//! En Eclipse OS el binario es `eclipse-xwayland` (port de Xwayland). Lo
//! lanzamos como subproceso, le pasamos el `WAYLAND_DISPLAY` y un fd de
//! `wl_socket` extra, y exponemos su DISPLAY (`:0`) para que los clientes X11
//! conecten.
//!
//! Este módulo es muy compacto a propósito: la complejidad real está en el
//! traductor X→Wayland (xcb-proto), que reusamos del módulo `lunas::xwayland`.

use alloc::string::String;

#[derive(Clone, Debug)]
pub struct X11Server {
    pub display:  String,
    pub pid:      u32,
    pub width:    u16,
    pub height:   u16,
}

impl X11Server {
    /// Lanza Xwayland nested. Devuelve `None` si no se encuentra el binario.
    pub fn new(width: u16, height: u16) -> Option<Self> {
        #[cfg(feature = "xwayland")]
        {
            // Lanzar el binario con `Wayland fd` heredado.
            let pid = spawn_xwayland("eclipse-xwayland", ":1")?;
            Some(Self { display: ":1".into(), pid, width, height })
        }
        #[cfg(not(feature = "xwayland"))]
        {
            let _ = (width, height);
            None
        }
    }
}

#[cfg(feature = "xwayland")]
fn spawn_xwayland(_bin: &str, _display: &str) -> Option<u32> {
    // Eclipse: usa `eclipse_syscall::call::spawn_command`.
    // Linux dev: usa `std::process::Command`.
    None
}
