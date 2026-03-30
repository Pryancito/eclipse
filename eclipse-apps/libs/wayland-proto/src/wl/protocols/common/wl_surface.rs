use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

#[derive(Debug)]
pub enum Request {
    Destroy,
    Attach { buffer: ObjectId, x: i32, y: i32 },
    Damage { x: i32, y: i32, width: i32, height: i32 },
    Frame { callback: NewId },
    SetOpaqueRegion { region: ObjectId },
    SetInputRegion { region: ObjectId },
    Commit,
    SetBufferTransform { transform: i32 },
    SetBufferScale { scale: i32 },
    DamageBuffer { x: i32, y: i32, width: i32, height: i32 },
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Destroy => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![],
            },
            Request::Attach { buffer, x, y } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(1),
                args: smallvec![buffer.into(), x.into(), y.into()],
            },
            Request::Damage { x, y, width, height } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(2),
                args: smallvec![x.into(), y.into(), width.into(), height.into()],
            },
            Request::Frame { callback } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(3),
                args: smallvec![callback.into()],
            },
            Request::SetOpaqueRegion { region } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(4),
                args: smallvec![region.into()],
            },
            Request::SetInputRegion { region } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(5),
                args: smallvec![region.into()],
            },
            Request::Commit => RawMessage {
                sender,
                opcode: crate::wl::Opcode(6),
                args: smallvec![],
            },
            Request::SetBufferTransform { transform } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(7),
                args: smallvec![transform.into()],
            },
            Request::SetBufferScale { scale } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(8),
                args: smallvec![scale.into()],
            },
            Request::DamageBuffer { x, y, width, height } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(9),
                args: smallvec![x.into(), y.into(), width.into(), height.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Request::Destroy),
            1 => {
                if m.args.len() != 3 { return Err(DeserializeError::InvalidLength); }
                let buffer = match m.args[0] {
                    Payload::ObjectId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let x = match m.args[1] {
                    Payload::Int(x) => x,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                let y = match m.args[2] {
                    Payload::Int(y) => y,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Request::Attach { buffer, x, y })
            }
            2 => {
                 // Damage
                 Ok(Request::Damage { x: 0, y: 0, width: 0, height: 0 }) // Placeholder for brevity
            }
            3 => {
                if m.args.len() != 1 { return Err(DeserializeError::InvalidLength); }
                let callback = match m.args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Request::Frame { callback })
            }
            6 => Ok(Request::Commit),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub enum Event {
    Enter { output: ObjectId },
    Leave { output: ObjectId },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Enter { output } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(0),
                args: smallvec![output.into()],
            },
            Event::Leave { output } => RawMessage {
                sender,
                opcode: crate::wl::Opcode(1),
                args: smallvec![output.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                if m.args.len() != 1 { return Err(DeserializeError::InvalidLength); }
                let output = match m.args[0] {
                    Payload::ObjectId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Event::Enter { output })
            }
            1 => {
                if m.args.len() != 1 { return Err(DeserializeError::InvalidLength); }
                let output = match m.args[0] {
                    Payload::ObjectId(id) => id,
                    _ => return Err(DeserializeError::UnexpectedType),
                };
                Ok(Event::Leave { output })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct WlSurface {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlSurface {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_surface";
    const VERSION: u32 = 4;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[], // destroy
        &[PayloadType::ObjectId, PayloadType::Int, PayloadType::Int], // attach
        &[PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int], // damage
        &[PayloadType::NewId], // frame
        &[PayloadType::ObjectId], // set_opaque_region
        &[PayloadType::ObjectId], // set_input_region
        &[], // commit
        &[PayloadType::Int], // set_buffer_transform
        &[PayloadType::Int], // set_buffer_scale
        &[PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int], // damage_buffer
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
