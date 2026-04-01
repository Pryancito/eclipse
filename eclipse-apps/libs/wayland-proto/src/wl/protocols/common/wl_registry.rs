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
    Bind {
        name: u32,
        id: NewId,
    },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Bind { name, id } => RawMessage {
                sender,
                opcode: Opcode(0),
                args: smallvec![name.into(), id.into()],
            },
        }
    }

    fn from_raw(
        _con: Rc<RefCell<dyn Connection>>,
        m: &RawMessage,
    ) -> Result<Request, DeserializeError> {
        match m.opcode {
            Opcode(0) => Ok(Request::Bind {
                name: from_payload!(UInt, m.args[0]),
                id: from_payload!(NewId, m.args[1]),
            }),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Global {
        name: u32,
        interface: String,
        version: u32,
    },
    GlobalRemove {
        name: u32,
    },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Global {
                name,
                interface,
                version,
            } => RawMessage {
                sender,
                opcode: Opcode(0),
                args: smallvec![name.into(), interface.into(), version.into()],
            },
            Event::GlobalRemove { name } => RawMessage {
                sender,
                opcode: Opcode(1),
                args: smallvec![name.into()],
            },
        }
    }

    fn from_raw(
        _con: Rc<RefCell<dyn Connection>>,
        m: &RawMessage,
    ) -> Result<Event, DeserializeError> {
        match m.opcode {
            Opcode(0) => Ok(Event::Global {
                name: from_payload!(UInt, m.args[0]),
                interface: from_payload!(String, m.args[1]),
                version: from_payload!(UInt, m.args[2]),
            }),
            Opcode(1) => Ok(Event::GlobalRemove {
                name: from_payload!(UInt, m.args[0]),
            }),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct WlRegistry {
    con: Rc<RefCell<dyn Connection>>,
    object_id: ObjectId,
}

impl Interface for WlRegistry {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_registry";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[
            PayloadType::UInt,
            PayloadType::String,
            PayloadType::UInt,
            PayloadType::NewId,
        ], // bind: name, interface, version, id
        &[PayloadType::UInt], // (placeholder for global_remove event opcode)
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, object_id: ObjectId) -> WlRegistry {
        WlRegistry { con, object_id }
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

impl WlRegistry {
    pub fn bind(&mut self, name: u32, interface: &str, version: u32, id: NewId) -> Result<(), SendError> {
        self.con.borrow_mut().send(
            self.object_id,
            Opcode(0),
            &[
                name.into(),
                Payload::String(alloc::string::String::from(interface)),
                version.into(),
                id.into(),
            ],
            &[],
        )
    }
}
