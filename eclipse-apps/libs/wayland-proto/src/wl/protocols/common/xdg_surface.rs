//! xdg_surface — a role-less surface that can become a toplevel or popup.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Request {
    Destroy,
    GetToplevel { id: NewId },
    /// set_window_geometry(x, y, width, height)
    SetWindowGeometry { x: i32, y: i32, width: i32, height: i32 },
    AckConfigure { serial: u32 },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            Request::GetToplevel { id } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![id.into()],
            },
            Request::SetWindowGeometry { x, y, width, height } => RawMessage {
                sender, opcode: Opcode(3),
                args: smallvec![x.into(), y.into(), width.into(), height.into()],
            },
            Request::AckConfigure { serial } => RawMessage {
                sender, opcode: Opcode(4), args: smallvec![serial.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Request::Destroy),
            1 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Request::GetToplevel { id })
            }
            3 => Ok(Request::SetWindowGeometry { x: 0, y: 0, width: 0, height: 0 }),
            4 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Request::AckConfigure { serial })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Event {
    /// Compositor sends this after geometry has been applied.
    Configure { serial: u32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Configure { serial } => RawMessage {
                sender, opcode: Opcode(0), args: smallvec![serial.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Event::Configure { serial })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Interface ─────────────────────────────────────────────────────────────────

pub struct XdgSurface {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for XdgSurface {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "xdg_surface";
    const VERSION: u32 = 2;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                                                      // 0: destroy
        &[PayloadType::NewId],                                                    // 1: get_toplevel
        &[PayloadType::NewId, PayloadType::ObjectId, PayloadType::ObjectId],      // 2: get_popup
        &[PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int],// 3: set_window_geometry
        &[PayloadType::UInt],                                                     // 4: ack_configure
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

impl XdgSurface {
    pub fn get_toplevel(&mut self, id: NewId) -> Result<super::xdg_toplevel::XdgToplevel, SendError> {
        self.con.borrow_mut().send(self.id, Opcode(1), &[id.into()], &[])?;
        Ok(super::xdg_toplevel::XdgToplevel::new(self.con.clone(), id.as_id()))
    }

    pub fn ack_configure(&mut self, serial: u32) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(4), &[serial.into()], &[])
    }

    pub fn destroy(&mut self) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(0), &[], &[])
    }
}
