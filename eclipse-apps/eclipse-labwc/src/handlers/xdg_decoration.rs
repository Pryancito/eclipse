//! `zxdg_decoration_manager_v1` — labwc por defecto fuerza SSD (server-side
//! decorations). Esto matches lo que hace upstream:
//!   `<core><decoration>server</decoration></core>` en `rc.xml`.

use smithay::{
    delegate_xdg_decoration,
    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    wayland::shell::xdg::{
        decoration::XdgDecorationHandler,
        ToplevelSurface,
    },
};

use crate::config::DecorationMode;
use crate::state::LabwcState;

impl XdgDecorationHandler for LabwcState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(match self.config.core.decoration {
                DecorationMode::Server => Mode::ServerSide,
                DecorationMode::Client => Mode::ClientSide,
            });
        });
        toplevel.send_pending_configure();
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: Mode) {
        // labwc respeta la elección global: si rc.xml dice "server", forzamos
        // ServerSide aunque el cliente pida ClientSide.
        let final_mode = match self.config.core.decoration {
            DecorationMode::Server => Mode::ServerSide,
            DecorationMode::Client => mode,
        };
        toplevel.with_pending_state(|s| s.decoration_mode = Some(final_mode));
        toplevel.send_pending_configure();
    }
    fn unset_mode(&mut self, _toplevel: ToplevelSurface) {}
}

delegate_xdg_decoration!(LabwcState);
