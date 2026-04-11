//! Minimal `wl_region` protocol stubs.
//!
//! `wl_region` is created by `wl_compositor.create_region` and used by clients
//! to describe input/opaque regions.  The compositor treats all regions as
//! infinite (i.e. it ignores them), so every request is a no-op.
#![allow(unused)]

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

#[derive(Debug)]
pub enum Request {
    Destroy,
    Add     { x: i32, y: i32, width: i32, height: i32 },
    Subtract{ x: i32, y: i32, width: i32, height: i32 },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Destroy => RawMessage { sender, opcode: crate::wl::Opcode(0), args: smallvec![] },
            Request::Add { x, y, width, height } => RawMessage {
                sender, opcode: crate::wl::Opcode(1),
                args: smallvec![x.into(), y.into(), width.into(), height.into()],
            },
            Request::Subtract { x, y, width, height } => RawMessage {
                sender, opcode: crate::wl::Opcode(2),
                args: smallvec![x.into(), y.into(), width.into(), height.into()],
            },
        }
    }
    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Request::Destroy),
            1 | 2 => {
                if m.args.len() < 4 { return Err(DeserializeError::InvalidLength); }
                let x = match m.args[0] { Payload::Int(v) => v, _ => return Err(DeserializeError::UnexpectedType) };
                let y = match m.args[1] { Payload::Int(v) => v, _ => return Err(DeserializeError::UnexpectedType) };
                let w = match m.args[2] { Payload::Int(v) => v, _ => return Err(DeserializeError::UnexpectedType) };
                let h = match m.args[3] { Payload::Int(v) => v, _ => return Err(DeserializeError::UnexpectedType) };
                if m.opcode.0 == 1 { Ok(Request::Add { x, y, width: w, height: h }) }
                else { Ok(Request::Subtract { x, y, width: w, height: h }) }
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

/// `wl_region` has no events.
#[derive(Debug)]
pub enum Event {}

impl Message for Event {
    fn into_raw(self, _sender: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct WlRegion {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlRegion {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_region";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                                                          // 0: destroy
        &[PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int],   // 1: add
        &[PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int],   // 2: subtract
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
