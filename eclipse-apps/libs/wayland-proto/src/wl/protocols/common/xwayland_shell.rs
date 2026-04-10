//! xwayland_shell_v1 — Xwayland window management protocol.
//!
//! This protocol allows Xwayland to associate X11 windows with Wayland surfaces.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ── xwayland_shell_v1 (Manager Interface) ──────────────────────────────────

#[derive(Debug)]
pub enum Request {
    /// opcode 0 — destroy the manager
    Destroy,
    /// opcode 1 — create an xwayland_surface for a wl_surface
    GetXwaylandSurface { id: NewId, surface: ObjectId },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            Request::GetXwaylandSurface { id, surface } => RawMessage {
                sender, opcode: Opcode(1),
                args: smallvec![id.into(), surface.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Request::Destroy),
            1 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let surface = match m.args.get(1) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Request::GetXwaylandSurface { id, surface })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub enum Event {}
impl Message for Event {
    fn into_raw(self, _sender: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_con: Rc<RefCell<dyn Connection>>, _m: &RawMessage) -> Result<Self, DeserializeError> { Err(DeserializeError::UnknownOpcode) }
}

pub struct XwaylandShellV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for XwaylandShellV1 {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "xwayland_shell_v1";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                          // 0: destroy
        &[PayloadType::NewId, PayloadType::ObjectId], // 1: get_xwayland_surface
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

// ── xwayland_surface_v1 (Surface Interface) ────────────────────────────────

#[derive(Debug)]
pub enum SurfaceRequest {
    /// opcode 0 — destroy the surface association
    Destroy,
    /// opcode 1 — associate X11 window ID with Wayland surface
    SetSerial { serial_lo: u32, serial_hi: u32 },
}

impl Message for SurfaceRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            SurfaceRequest::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            SurfaceRequest::SetSerial { serial_lo, serial_hi } => RawMessage {
                sender, opcode: Opcode(1),
                args: smallvec![serial_lo.into(), serial_hi.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(SurfaceRequest::Destroy),
            1 => {
                let s_lo = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let s_hi = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetSerial { serial_lo: s_lo, serial_hi: s_hi })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct XwaylandSurfaceV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

pub enum SurfaceEvent {}
impl Message for SurfaceEvent {
    fn into_raw(self, _sender: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_con: Rc<RefCell<dyn Connection>>, _m: &RawMessage) -> Result<Self, DeserializeError> { Err(DeserializeError::UnknownOpcode) }
}

impl Interface for XwaylandSurfaceV1 {
    type Event = SurfaceEvent;
    type Request = SurfaceRequest;

    const NAME: &'static str = "xwayland_surface_v1";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                          // 0: destroy
        &[PayloadType::UInt, PayloadType::UInt],      // 1: set_serial
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xwayland_shell_payload_types() {
        assert_eq!(XwaylandShellV1::PAYLOAD_TYPES.len(), 2);
        assert_eq!(XwaylandShellV1::PAYLOAD_TYPES[0], &[]);
        assert_eq!(XwaylandShellV1::PAYLOAD_TYPES[1], &[PayloadType::NewId, PayloadType::ObjectId]);
    }

    #[test]
    fn test_xwayland_surface_payload_types() {
        assert_eq!(XwaylandSurfaceV1::PAYLOAD_TYPES.len(), 2);
        assert_eq!(XwaylandSurfaceV1::PAYLOAD_TYPES[0], &[]);
        assert_eq!(XwaylandSurfaceV1::PAYLOAD_TYPES[1], &[PayloadType::UInt, PayloadType::UInt]);
    }

    #[test]
    fn test_xwayland_shell_request_into_raw() {
        let req = Request::GetXwaylandSurface { id: NewId(10), surface: ObjectId(5) };
        let raw = req.into_raw(ObjectId(2));
        assert_eq!(raw.sender, ObjectId(2));
        assert_eq!(raw.opcode, Opcode(1));
        assert_eq!(raw.args.len(), 2);
        assert!(matches!(raw.args[0], Payload::NewId(NewId(10))));
        assert!(matches!(raw.args[1], Payload::ObjectId(ObjectId(5))));
    }

    #[test]
    fn test_xwayland_surface_request_into_raw() {
        let req = SurfaceRequest::SetSerial { serial_lo: 0x1234, serial_hi: 0x5678 };
        let raw = req.into_raw(ObjectId(5));
        assert_eq!(raw.sender, ObjectId(5));
        assert_eq!(raw.opcode, Opcode(1));
        assert_eq!(raw.args.len(), 2);
        assert!(matches!(raw.args[0], Payload::UInt(0x1234)));
        assert!(matches!(raw.args[1], Payload::UInt(0x5678)));
    }
}

