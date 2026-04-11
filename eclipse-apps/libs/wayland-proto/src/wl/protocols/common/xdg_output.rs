//! zxdg_output_manager_v1 — extended output information.
//!
//! Provides logical (compositor-space) output coordinates and sizes,
//! separate from the physical output properties in `wl_output`.
//! Used by waybar, swaybg, foot, and many other Wayland clients.
//!
//! Protocol: xdg-output-unstable-v1
//! Interface versions: zxdg_output_manager_v1(3), zxdg_output_v1(3)

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use smallvec::smallvec;

// ─────────────────────────────────────────────────────────────────────────────
// zxdg_output_manager_v1
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ManagerRequest {
    /// opcode 0 — destroy the manager
    Destroy,
    /// opcode 1 — get_xdg_output(id: new_id, output: object)
    GetXdgOutput { id: NewId, output: ObjectId },
}

impl Message for ManagerRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            ManagerRequest::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            ManagerRequest::GetXdgOutput { id, output } => RawMessage {
                sender, opcode: Opcode(1),
                args: smallvec![id.into(), output.into()],
            },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(ManagerRequest::Destroy),
            1 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let output = match m.args.get(1) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(ManagerRequest::GetXdgOutput { id, output })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

/// No events on the manager.
#[derive(Debug)]
pub enum ManagerEvent {}

impl Message for ManagerEvent {
    fn into_raw(self, _: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct ZxdgOutputManagerV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for ZxdgOutputManagerV1 {
    type Event = ManagerEvent;
    type Request = ManagerRequest;

    const NAME: &'static str = "zxdg_output_manager_v1";
    const VERSION: u32 = 3;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                              // 0: destroy
        &[PayloadType::NewId, PayloadType::ObjectId],    // 1: get_xdg_output
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

// ─────────────────────────────────────────────────────────────────────────────
// zxdg_output_v1
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum OutputRequest {
    /// opcode 0 — destroy this output info object
    Destroy,
}

impl Message for OutputRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            OutputRequest::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(OutputRequest::Destroy),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum OutputEvent {
    /// opcode 0 — logical position of the output in compositor coordinate space
    LogicalPosition { x: i32, y: i32 },
    /// opcode 1 — logical size of the output in compositor coordinate space
    LogicalSize { width: i32, height: i32 },
    /// opcode 2 — all data sent
    Done,
    /// opcode 3 — human-readable name for the output (e.g. "HDMI-1")
    Name { name: String },
    /// opcode 4 — human-readable description
    Description { description: String },
}

impl Message for OutputEvent {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            OutputEvent::LogicalPosition { x, y } => RawMessage {
                sender, opcode: Opcode(0), args: smallvec![x.into(), y.into()],
            },
            OutputEvent::LogicalSize { width, height } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![width.into(), height.into()],
            },
            OutputEvent::Done => RawMessage { sender, opcode: Opcode(2), args: smallvec![] },
            OutputEvent::Name { name } => RawMessage {
                sender, opcode: Opcode(3),
                args: smallvec![crate::wl::Payload::String(name)],
            },
            OutputEvent::Description { description } => RawMessage {
                sender, opcode: Opcode(4),
                args: smallvec![crate::wl::Payload::String(description)],
            },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        // Compositor → client only; clients don't send these events.
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct ZxdgOutputV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for ZxdgOutputV1 {
    type Event = OutputEvent;
    type Request = OutputRequest;

    const NAME: &'static str = "zxdg_output_v1";
    const VERSION: u32 = 3;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[], // 0: destroy
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
