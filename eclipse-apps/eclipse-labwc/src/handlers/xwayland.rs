//! XWayland — server X11 nested via `smithay::xwayland`.
//!
//! NOTA: el API exacto cambió entre Smithay 0.3 (anvil clásico) y 0.7. Este
//! archivo deja la integración como **stub funcional**: el `XwmHandler` está
//! implementado con cuerpos vacíos para que `delegate_xwm!` no falle, y el
//! lanzamiento real (`start_xwayland`) queda pendiente de adaptar al nuevo
//! constructor de Smithay 0.7 (`XWayland::spawn(&dh, …)`).
//!
//! Para una integración completa, ver `smithay/anvil/src/xwayland.rs` upstream
//! y reemplazar los TODOs con la lógica de mapeo X11 → Wayland surfaces.

use smithay::{
    utils::{Logical, Rectangle},
    xwayland::{
        xwm::{Reorder, ResizeEdge, XwmId},
        X11Surface, X11Wm, XwmHandler,
    },
    reexports::x11rb::protocol::xproto::Window as X11Window,
};

use crate::state::LabwcState;

impl XwmHandler for LabwcState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        unimplemented!("multi-XWM not supported by labwc")
    }
    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn map_window_request(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn unmapped_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn configure_request(&mut self, _xwm: XwmId, _window: X11Surface,
                         _x: Option<i32>, _y: Option<i32>, _w: Option<u32>, _h: Option<u32>,
                         _r: Option<Reorder>) {}
    fn configure_notify(&mut self, _xwm: XwmId, _window: X11Surface,
                        _geo: Rectangle<i32, Logical>, _above: Option<X11Window>) {}
    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface,
                      _button: u32, _resize_edge: ResizeEdge) {}
    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}

/// Lanza Xwayland nested. **TODO**: adaptar al constructor real de Smithay 0.7
/// que toma `&DisplayHandle` y devuelve un handle + canal de eventos.
pub fn start_xwayland(_state: &mut LabwcState) -> anyhow::Result<()> {
    // Pseudo-código de la integración real (a completar al portar):
    //
    //   let xwayland = XWayland::spawn(&state.display_handle, /* env */ vec![],
    //       /* listening fds */ true)?;
    //   state.loop_handle.insert_source(xwayland, |event, _, data| match event {
    //       XWaylandEvent::Ready { x11_socket, display_number, .. } => {
    //           let wm = X11Wm::start_wm(state.loop_handle.clone(), x11_socket, ...)?;
    //           // guardar `wm` en el state real
    //       }
    //       XWaylandEvent::Error => tracing::warn!("XWayland error"),
    //   })?;
    Ok(())
}
