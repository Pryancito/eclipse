use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

#[derive(Debug)]
pub enum Request {
    Destroy,
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Destroy => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Request::Destroy),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Release,
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Release => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Event::Release),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct WlBuffer {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlBuffer {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_buffer";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[], // destroy
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self {
        Self { con, id }
    }

    fn connection(&self) -> &Rc<RefCell<dyn Connection>> {
        &self.con
    }

    fn id(&self) -> ObjectId {
        self.id
    }

    fn as_new_id(&self) -> NewId {
        NewId(self.id.0)
    }
}
