//! xdg_wm_base — toplevel window management base object.
//!
//! Required by all modern Wayland clients instead of the deprecated wl_shell.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ── Requests (client → compositor) ───────────────────────────────────────────

#[derive(Debug)]
pub enum Request {
    /// opcode 0
    Destroy,
    /// opcode 1 — create an xdg_positioner
    CreatePositioner { id: NewId },
    /// opcode 2 — create an xdg_surface for a wl_surface
    GetXdgSurface { id: NewId, surface: ObjectId },
    /// opcode 3 — reply to a ping event
    Pong { serial: u32 },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            Request::CreatePositioner { id } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![id.into()],
            },
            Request::GetXdgSurface { id, surface } => RawMessage {
                sender, opcode: Opcode(2),
                args: smallvec![id.into(), surface.into()],
            },
            Request::Pong { serial } => RawMessage {
                sender, opcode: Opcode(3),
                args: smallvec![serial.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Request::Destroy),
            1 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Request::CreatePositioner { id })
            }
            2 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let surface = match m.args.get(1) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Request::GetXdgSurface { id, surface })
            }
            3 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Request::Pong { serial })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Events (compositor → client) ─────────────────────────────────────────────

#[derive(Debug)]
pub enum Event {
    /// opcode 0 — compositor requests a pong
    Ping { serial: u32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Ping { serial } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![serial.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Event::Ping { serial })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Interface ─────────────────────────────────────────────────────────────────

pub struct XdgWmBase {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for XdgWmBase {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "xdg_wm_base";
    const VERSION: u32 = 3;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                          // 0: destroy
        &[PayloadType::NewId],                        // 1: create_positioner
        &[PayloadType::NewId, PayloadType::ObjectId], // 2: get_xdg_surface
        &[PayloadType::UInt],                         // 3: pong
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

impl XdgWmBase {
    /// Create an xdg_positioner.
    pub fn create_positioner(&mut self, id: NewId) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(1), &[id.into()], &[])
    }

    /// Send xdg_wm_base.get_xdg_surface — creates an xdg_surface for `surface`.
    pub fn get_xdg_surface(
        &mut self,
        id: NewId,
        surface: ObjectId,
    ) -> Result<super::xdg_surface::XdgSurface, SendError> {
        self.con.borrow_mut().send(
            self.id, Opcode(2),
            &[id.into(), surface.into()],
            &[],
        )?;
        Ok(super::xdg_surface::XdgSurface::new(self.con.clone(), id.as_id()))
    }

    /// Reply to a `ping` event.
    pub fn pong(&mut self, serial: u32) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(3), &[serial.into()], &[])
    }

    pub fn destroy(&mut self) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(0), &[], &[])
    }
}
