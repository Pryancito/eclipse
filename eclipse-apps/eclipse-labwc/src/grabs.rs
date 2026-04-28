//! Pointer grabs para move/resize interactivos — port adaptado de
//! `smithay/anvil/src/shell/grabs.rs`.
//!
//! labwc lanza un grab cuando:
//!   * El cliente envía `xdg_toplevel.move` (drag de titlebar).
//!   * El cliente envía `xdg_toplevel.resize` (drag de borde).
//!   * El usuario inicia con un mousebind del rc.xml (`<mousebind action="Drag">`).

use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        RelativeMotionEvent,
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    utils::{IsAlive, Logical, Point, Rectangle, Serial, Size},
};

use crate::state::LabwcState;

/// Grab que mueve la ventana siguiendo el cursor.
pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<LabwcState>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<LabwcState> for MoveSurfaceGrab {
    fn motion(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>,
              _focus: Option<(<LabwcState as smithay::input::SeatHandler>::PointerFocus, Point<f64, Logical>)>,
              event: &MotionEvent) {
        // Move event sin foco para que la ventana siga el cursor sin recibir motion.
        handle.motion(data, None, event);
        let delta = event.location - self.start_data.location;
        let new_loc = self.initial_window_location.to_f64() + delta;
        data.space.map_element(self.window.clone(), new_loc.to_i32_round(), true);
    }

    fn relative_motion(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>,
                       focus: Option<(<LabwcState as smithay::input::SeatHandler>::PointerFocus, Point<f64, Logical>)>,
                       event: &RelativeMotionEvent) {
        handle.relative_motion(data, focus, event);
    }

    fn button(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>, event: &ButtonEvent) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>, details: AxisFrame) { handle.axis(data, details); }
    fn frame(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>) { handle.frame(data); }

    // Gestures (Smithay 0.7+): delegate all to inner.
    fn gesture_swipe_begin(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureSwipeBeginEvent) { h.gesture_swipe_begin(d, e); }
    fn gesture_swipe_update(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureSwipeUpdateEvent) { h.gesture_swipe_update(d, e); }
    fn gesture_swipe_end(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureSwipeEndEvent) { h.gesture_swipe_end(d, e); }
    fn gesture_pinch_begin(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GesturePinchBeginEvent) { h.gesture_pinch_begin(d, e); }
    fn gesture_pinch_update(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GesturePinchUpdateEvent) { h.gesture_pinch_update(d, e); }
    fn gesture_pinch_end(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GesturePinchEndEvent) { h.gesture_pinch_end(d, e); }
    fn gesture_hold_begin(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureHoldBeginEvent) { h.gesture_hold_begin(d, e); }
    fn gesture_hold_end(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureHoldEndEvent) { h.gesture_hold_end(d, e); }

    fn start_data(&self) -> &PointerGrabStartData<LabwcState> { &self.start_data }
    fn unset(&mut self, _data: &mut LabwcState) {}
}

/// Grab que redimensiona desde un edge específico.
pub struct ResizeSurfaceGrab {
    pub start_data: PointerGrabStartData<LabwcState>,
    pub window: Window,
    pub edges: ResizeEdge,
    pub initial_rect: Rectangle<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
}

impl PointerGrab<LabwcState> for ResizeSurfaceGrab {
    fn motion(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>,
              _focus: Option<(<LabwcState as smithay::input::SeatHandler>::PointerFocus, Point<f64, Logical>)>,
              event: &MotionEvent) {
        handle.motion(data, None, event);
        if !self.window.alive() { handle.unset_grab(self, data, event.serial, event.time, true); return; }

        let delta = (event.location - self.start_data.location).to_i32_round::<i32>();
        let mut new_size = self.initial_rect.size;
        let mut new_loc  = self.initial_rect.loc;
        match self.edges {
            ResizeEdge::Top         => { new_size.h -= delta.y; new_loc.y += delta.y; }
            ResizeEdge::Bottom      => { new_size.h += delta.y; }
            ResizeEdge::Left        => { new_size.w -= delta.x; new_loc.x += delta.x; }
            ResizeEdge::Right       => { new_size.w += delta.x; }
            ResizeEdge::TopLeft     => { new_size.w -= delta.x; new_size.h -= delta.y; new_loc.x += delta.x; new_loc.y += delta.y; }
            ResizeEdge::TopRight    => { new_size.w += delta.x; new_size.h -= delta.y; new_loc.y += delta.y; }
            ResizeEdge::BottomLeft  => { new_size.w -= delta.x; new_size.h += delta.y; new_loc.x += delta.x; }
            ResizeEdge::BottomRight => { new_size.w += delta.x; new_size.h += delta.y; }
            _ => {}
        }
        new_size.w = new_size.w.max(80);
        new_size.h = new_size.h.max(60);

        if let Some(toplevel) = self.window.toplevel() {
            toplevel.with_pending_state(|s| { s.size = Some(new_size); });
            toplevel.send_pending_configure();
        }
        self.last_window_size = new_size;
        data.space.map_element(self.window.clone(), new_loc, true);
    }

    fn relative_motion(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>,
                       focus: Option<(<LabwcState as smithay::input::SeatHandler>::PointerFocus, Point<f64, Logical>)>,
                       event: &RelativeMotionEvent) { handle.relative_motion(data, focus, event); }

    fn button(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>, event: &ButtonEvent) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>, details: AxisFrame) { handle.axis(data, details); }
    fn frame(&mut self, data: &mut LabwcState, handle: &mut PointerInnerHandle<'_, LabwcState>) { handle.frame(data); }

    fn gesture_swipe_begin(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureSwipeBeginEvent) { h.gesture_swipe_begin(d, e); }
    fn gesture_swipe_update(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureSwipeUpdateEvent) { h.gesture_swipe_update(d, e); }
    fn gesture_swipe_end(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureSwipeEndEvent) { h.gesture_swipe_end(d, e); }
    fn gesture_pinch_begin(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GesturePinchBeginEvent) { h.gesture_pinch_begin(d, e); }
    fn gesture_pinch_update(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GesturePinchUpdateEvent) { h.gesture_pinch_update(d, e); }
    fn gesture_pinch_end(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GesturePinchEndEvent) { h.gesture_pinch_end(d, e); }
    fn gesture_hold_begin(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureHoldBeginEvent) { h.gesture_hold_begin(d, e); }
    fn gesture_hold_end(&mut self, d: &mut LabwcState, h: &mut PointerInnerHandle<'_, LabwcState>, e: &GestureHoldEndEvent) { h.gesture_hold_end(d, e); }

    fn start_data(&self) -> &PointerGrabStartData<LabwcState> { &self.start_data }
    fn unset(&mut self, _data: &mut LabwcState) {}
}

/// Helper para arrancar el grab desde el handler `move_request`.
pub fn start_move(state: &mut LabwcState, surface: smithay::wayland::shell::xdg::ToplevelSurface,
                  seat: &smithay::input::Seat<LabwcState>, serial: Serial) {
    let pointer = match seat.get_pointer() { Some(p) => p, None => return };
    if !pointer.has_grab(serial) { return; }
    let start_data = match pointer.grab_start_data() { Some(s) => s, None => return };
    let window = match state.space.elements().find(|w|
        w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false)
    ).cloned() { Some(w) => w, None => return };
    let initial = state.space.element_location(&window).unwrap_or_default();
    pointer.set_grab(state, MoveSurfaceGrab { start_data, window, initial_window_location: initial }, serial,
                     smithay::input::pointer::Focus::Clear);
}

pub fn start_resize(state: &mut LabwcState, surface: smithay::wayland::shell::xdg::ToplevelSurface,
                    seat: &smithay::input::Seat<LabwcState>, serial: Serial, edges: ResizeEdge) {
    let pointer = match seat.get_pointer() { Some(p) => p, None => return };
    if !pointer.has_grab(serial) { return; }
    let start_data = match pointer.grab_start_data() { Some(s) => s, None => return };
    let window = match state.space.elements().find(|w|
        w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false)
    ).cloned() { Some(w) => w, None => return };
    let loc  = state.space.element_location(&window).unwrap_or_default();
    let size = window.geometry().size;
    pointer.set_grab(state, ResizeSurfaceGrab {
        start_data, window, edges,
        initial_rect: Rectangle::from_loc_and_size(loc, size),
        last_window_size: size,
    }, serial, smithay::input::pointer::Focus::Clear);
}
