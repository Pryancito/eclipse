use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::protocols::common::wl_surface::WlSurface;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

#[derive(Debug)]
pub enum Request {
    CreateSurface { id: NewId },
    CreateRegion { id: NewId },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::CreateSurface { id } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![id.into()],
            },
            Request::CreateRegion { id } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(1),
                args: smallvec![id.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                if m.args.len() != 1 { return Err(DeserializeError::InvalidLength); }
                let id = match m.args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Request::CreateSurface { id })
            }
            1 => {
                if m.args.len() != 1 { return Err(DeserializeError::InvalidLength); }
                let id = match m.args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Request::CreateRegion { id })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub enum Event {}

impl Message for Event {
    fn into_raw(self, _sender: ObjectId) -> RawMessage {
        unreachable!()
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, _m: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct WlCompositor {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlCompositor {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_compositor";
    const VERSION: u32 = 4;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::NewId], // create_surface
        &[PayloadType::NewId], // create_region
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

impl WlCompositor {
    pub fn create_surface(&mut self, id: NewId) -> Result<WlSurface, SendError> {
        self.con.borrow_mut().send(
            self.id,
            crate::wl::Opcode(0),
            &[id.into()],
            &[],
        )?;
        Ok(WlSurface::new(self.con.clone(), id.as_id()))
    }

    pub fn create_region(&mut self, id: NewId) -> Result<(), SendError> {
        self.con.borrow_mut().send(
            self.id,
            crate::wl::Opcode(1),
            &[id.into()],
            &[],
        )
    }
}
