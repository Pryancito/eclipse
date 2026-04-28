//! `zwlr_layer_shell_v1` — panels, docks, fondos, lockscreen.
//!
//! Versión simplificada para Smithay 0.7. La integración completa con
//! `LayerSurface` + `layer_map_for_output` se delega al usuario al portar el
//! patrón de `anvil/src/shell/layer.rs`.

use smithay::{
    delegate_layer_shell,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    wayland::shell::wlr_layer::{
        Layer, WlrLayerShellHandler, WlrLayerShellState,
    },
};

use crate::state::LabwcState;

impl WlrLayerShellHandler for LabwcState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState { &mut self.layer_shell }

    fn new_layer_surface(
        &mut self,
        _surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        _wl_output: Option<WlOutput>,
        _layer: Layer,
        _namespace: String,
    ) {
        // TODO: usar `layer_map_for_output(&output).map_layer(...)`
        // como hace anvil/src/shell/layer.rs.
    }

    fn layer_destroyed(&mut self, _surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        // TODO: simétrico a new_layer_surface.
    }
}

delegate_layer_shell!(LabwcState);
