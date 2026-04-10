//! wl_seat — group of input devices (keyboard, pointer, touch).

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Request {
    GetPointer  { id: NewId },
    GetKeyboard { id: NewId },
    GetTouch    { id: NewId },
    Release,
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::GetPointer  { id } => RawMessage { sender, opcode: Opcode(0), args: smallvec![id.into()] },
            Request::GetKeyboard { id } => RawMessage { sender, opcode: Opcode(1), args: smallvec![id.into()] },
            Request::GetTouch    { id } => RawMessage { sender, opcode: Opcode(2), args: smallvec![id.into()] },
            Request::Release         => RawMessage { sender, opcode: Opcode(3), args: smallvec![] },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => { let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) }; Ok(Request::GetPointer { id }) }
            1 => { let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) }; Ok(Request::GetKeyboard { id }) }
            2 => { let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) }; Ok(Request::GetTouch { id }) }
            3 => Ok(Request::Release),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

/// Seat capability flags (bit field).
pub const CAP_POINTER:  u32 = 1;
pub const CAP_KEYBOARD: u32 = 2;
pub const CAP_TOUCH:    u32 = 4;

#[derive(Debug)]
pub enum Event {
    /// opcode 0 — announce available input devices
    Capabilities { capabilities: u32 },
    /// opcode 1 — human-readable name
    Name { name: alloc::string::String },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Capabilities { capabilities } => RawMessage {
                sender, opcode: Opcode(0), args: smallvec![capabilities.into()],
            },
            Event::Name { name } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![Payload::String(name)],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                 let c = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                 Ok(Event::Capabilities { capabilities: c })
            }
            1 => {
                 let name = match m.args.get(0) { Some(Payload::String(s)) => s.clone(), _ => return Err(DeserializeError::UnexpectedType) };
                 Ok(Event::Name { name })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Interface ─────────────────────────────────────────────────────────────────

pub struct WlSeat {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlSeat {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_seat";
    const VERSION: u32 = 7;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::NewId], // 0: get_pointer
        &[PayloadType::NewId], // 1: get_keyboard
        &[PayloadType::NewId], // 2: get_touch
        &[],                   // 3: release
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

impl WlSeat {
    pub fn get_pointer(&mut self, id: NewId) -> Result<super::wl_pointer::WlPointer, SendError> {
        self.con.borrow_mut().send(self.id, Opcode(0), &[id.into()], &[])?;
        Ok(super::wl_pointer::WlPointer::new(self.con.clone(), id.as_id()))
    }
    pub fn get_keyboard(&mut self, id: NewId) -> Result<super::wl_keyboard::WlKeyboard, SendError> {
        self.con.borrow_mut().send(self.id, Opcode(1), &[id.into()], &[])?;
        Ok(super::wl_keyboard::WlKeyboard::new(self.con.clone(), id.as_id()))
    }
    pub fn release(&mut self) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(3), &[], &[])
    }
}
