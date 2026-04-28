//! Resto de handlers — todos triviales (pure delegate). Se separan en archivos
//! para mantener la simetría con labwc upstream donde cada protocolo es un .c.

use smithay::{
    delegate_shm, delegate_seat, delegate_data_device, delegate_primary_selection,
    delegate_output, delegate_viewporter,
    input::{Seat, SeatHandler, pointer::CursorImageStatus},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::{
        buffer::BufferHandler,
        output::OutputHandler,
        selection::{
            data_device::{ClientDndGrabHandler, DataDeviceHandler, ServerDndGrabHandler},
            primary_selection::PrimarySelectionHandler,
            SelectionHandler,
        },
        shm::{ShmHandler, ShmState},
    },
};

use crate::state::LabwcState;

// ── ShmHandler ──────────────────────────────────────────────────────────────
impl BufferHandler for LabwcState {
    fn buffer_destroyed(&mut self, _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer) {}
}
impl ShmHandler for LabwcState {
    fn shm_state(&self) -> &ShmState { &self.shm }
}
delegate_shm!(LabwcState);

// ── SeatHandler ─────────────────────────────────────────────────────────────
impl SeatHandler for LabwcState {
    type KeyboardFocus = WlSurface;
    type PointerFocus  = WlSurface;
    type TouchFocus    = WlSurface;

    fn seat_state(&mut self) -> &mut smithay::input::SeatState<Self> { &mut self.seat_state }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        *self.cursor_status.lock().unwrap() = image;
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
}
delegate_seat!(LabwcState);

// ── DataDevice / Selection / Primary ────────────────────────────────────────
impl SelectionHandler for LabwcState { type SelectionUserData = (); }
impl DataDeviceHandler for LabwcState {
    fn data_device_state(&self) -> &smithay::wayland::selection::data_device::DataDeviceState {
        &self.data_device
    }
}
impl ClientDndGrabHandler for LabwcState {}
impl ServerDndGrabHandler for LabwcState {}
delegate_data_device!(LabwcState);

impl PrimarySelectionHandler for LabwcState {
    fn primary_selection_state(&self) -> &smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &self.primary_sel
    }
}
delegate_primary_selection!(LabwcState);

// ── Output ──────────────────────────────────────────────────────────────────
impl OutputHandler for LabwcState {}
delegate_output!(LabwcState);

// ── Viewporter ──────────────────────────────────────────────────────────────
delegate_viewporter!(LabwcState);
