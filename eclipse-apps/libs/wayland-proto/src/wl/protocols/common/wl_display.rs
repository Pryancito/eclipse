#![no_std]
#![allow(unused)]


use crate::wl::{
    Array, Connection, DeserializeError, Handle, Interface, Message, NewId, ObjectId, Opcode,
    Payload, PayloadType, RawMessage, SendError,
};
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use smallvec::smallvec;

macro_rules! from_payload {
    ($ty:ident, $v:expr) => {
        match ($v).clone() {
            Payload::$ty(value) => value.into(),
            _ => return Err(DeserializeError::UnexpectedType),
        }
    };
}

#[derive(Debug)]
pub enum Request {
    Sync {
        callback: NewId,
    },
    GetRegistry {
        registry: NewId,
    },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Sync { callback } => RawMessage {
                sender,
                opcode: Opcode(0),
                args: smallvec![callback.into()],
            },
            Request::GetRegistry { registry } => RawMessage {
                sender,
                opcode: Opcode(1),
                args: smallvec![registry.into()],
            },
        }
    }

    fn from_raw(
        _con: Rc<RefCell<dyn Connection>>,
        m: &RawMessage,
    ) -> Result<Request, DeserializeError> {
        match m.opcode {
            Opcode(0) => Ok(Request::Sync {
                callback: from_payload!(NewId, m.args[0]),
            }),
            Opcode(1) => Ok(Request::GetRegistry {
                registry: from_payload!(NewId, m.args[0]),
            }),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Error {
        object_id: ObjectId,
        code: u32,
        message: String,
    },
    DeleteId {
        id: u32,
    },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Error {
                object_id,
                code,
                message,
            } => RawMessage {
                sender,
                opcode: Opcode(0),
                args: smallvec![object_id.into(), code.into(), message.into()],
            },
            Event::DeleteId { id } => RawMessage {
                sender,
                opcode: Opcode(1),
                args: smallvec![id.into()],
            },
        }
    }

    fn from_raw(
        _con: Rc<RefCell<dyn Connection>>,
        m: &RawMessage,
    ) -> Result<Event, DeserializeError> {
        match m.opcode {
            Opcode(0) => Ok(Event::Error {
                object_id: from_payload!(ObjectId, m.args[0]),
                code: from_payload!(UInt, m.args[1]),
                message: from_payload!(String, m.args[2]),
            }),
            Opcode(1) => Ok(Event::DeleteId {
                id: from_payload!(UInt, m.args[0]),
            }),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Clone)]
pub struct WlDisplay {
    con: Rc<RefCell<dyn Connection>>,
    object_id: ObjectId,
}

impl Interface for WlDisplay {
    type Event = Event;
    type Request = Request;
    const NAME: &'static str = "wl_display";
    const VERSION: u32 = 1;
    /// Indexed by **client request opcode** (server-side deserialize). Opcode 0 = sync, 1 = get_registry.
    /// Opcodes 2–3 are display *events* (error, delete_id); clients must not send them as requests — leave empty.
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::NewId],
        &[PayloadType::NewId],
        &[],
        &[],
        &[],
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, object_id: ObjectId) -> WlDisplay {
        WlDisplay { con, object_id }
    }

    fn connection(&self) -> &Rc<RefCell<dyn Connection>> {
        &self.con
    }

    fn id(&self) -> ObjectId {
        self.object_id
    }

    fn as_new_id(&self) -> NewId {
        NewId(self.object_id.0)
    }
}

impl WlDisplay {
    pub fn get_registry(&mut self, registry: NewId) -> Result<(), SendError> {
        self.con.borrow_mut().send(
            self.object_id,
            Opcode(1),
            &[registry.into()],
            &[],
        )
    }
}
