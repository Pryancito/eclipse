#![no_std]
#![allow(unused)]

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

#[derive(Debug)]
pub enum Request {}

impl Message for Request {
    fn into_raw(self, _sender: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

#[derive(Debug)]
pub enum Event {
    Done { callback_data: u32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Done { callback_data } => RawMessage {
                sender,
                opcode: Opcode(0),
                args: smallvec![callback_data.into()],
            },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                if m.args.is_empty() { return Err(DeserializeError::InvalidLength); }
                let data = match m.args[0] {
                    Payload::UInt(v) => v,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Event::Done { callback_data: data })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct WlCallback {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlCallback {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_callback";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
