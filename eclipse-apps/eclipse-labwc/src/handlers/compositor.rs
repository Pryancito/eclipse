//! `wl_compositor`, `wl_subcompositor`, `wl_surface` — handler.

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor,
    reexports::wayland_server::{Client, protocol::wl_surface::WlSurface},
    wayland::compositor::{
        CompositorClientState, CompositorHandler, CompositorState,
        get_parent, is_sync_subsurface,
    },
};

use crate::state::{ClientState, LabwcState};

impl CompositorHandler for LabwcState {
    fn compositor_state(&mut self) -> &mut CompositorState { &mut self.compositor }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        client.get_data::<ClientState>().unwrap().compositor_client_state()
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // Si el commit corresponde a un toplevel xdg, propagamos al espacio de Smithay
        // para que se redibuje y aplicamos `windowRules` del rc.xml.
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(p) = get_parent(&root) { root = p; }
            // Smithay's Space: cuando el cliente envía buffer, marcamos la window dirty.
            self.space.elements().for_each(|w| {
                if w.toplevel().map(|t| t.wl_surface() == &root).unwrap_or(false) {
                    w.on_commit();
                }
            });
        }
    }
}

// `ClientState` debe exponer un `CompositorClientState` (Smithay lo necesita).
impl ClientState {
    pub fn compositor_client_state(&self) -> &CompositorClientState {
        // Stub: en producción el ClientState almacena el state real;
        // aquí usamos un static thread-local para simplificar el bootstrap.
        thread_local! {
            static STATE: CompositorClientState = CompositorClientState::default();
        }
        STATE.with(|s| unsafe { &*(s as *const CompositorClientState) })
    }
}

delegate_compositor!(LabwcState);
