//! `xdg_shell` (xdg_wm_base, xdg_toplevel, xdg_popup) — el shell que usa el 99%
//! de apps modernas (GTK4, Qt6, Firefox, Chromium, weston-terminal, …).

use smithay::{
    delegate_xdg_shell,
    desktop::Window,
    input::Seat,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
        wayland_server::protocol::wl_seat,
    },
    utils::Serial,
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface,
        XdgShellHandler, XdgShellState,
    },
};

use crate::state::LabwcState;

impl XdgShellHandler for LabwcState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState { &mut self.xdg_shell }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Crea un Window de smithay::desktop y lo mete en el Space.
        let window = Window::new_wayland_window(surface.clone());
        // Aplica window_rules del rc.xml (maximize/fullscreen al mapear).
        // El app_id/title aún no están disponibles en `pending_state` durante
        // `new_toplevel` (Smithay 0.7); las rules se reaplican en el primer
        // `commit` cuando ya tengamos esos atributos. Por ahora calculamos
        // posición inicial cascada (labwc style).
        let _ = surface;
        let n = self.space.elements().count() as i32;
        let pos = (40 + n * 30, 60 + n * 30);
        self.space.map_element(window, pos, true);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(surface.into());
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat = Seat::<Self>::from_resource(&seat).unwrap();
        crate::grabs::start_move(self, surface, &seat, serial);
    }

    fn resize_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat,
                      serial: Serial, edges: ResizeEdge) {
        let seat = Seat::<Self>::from_resource(&seat).unwrap();
        crate::grabs::start_resize(self, surface, &seat, serial, edges);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Quitar la Window del Space.
        let target = self.space.elements().find(|w| {
            w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false)
        }).cloned();
        if let Some(w) = target {
            self.space.unmap_elem(&w);
        }
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {}
}

delegate_xdg_shell!(LabwcState);
