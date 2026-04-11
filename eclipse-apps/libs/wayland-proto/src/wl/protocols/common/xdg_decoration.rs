//! zxdg_decoration_manager_v1 — XDG decoration manager (server-side decorations).
//!
//! This protocol lets the compositor tell clients whether to use server-side
//! decorations (SSD) or client-side decorations (CSD).  labwc is an SSD
//! compositor and uses this to request that clients use SSD.
//!
//! Protocol: xdg-decoration-unstable-v1
//! Interface versions: zxdg_decoration_manager_v1(1), zxdg_toplevel_decoration_v1(1)

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ─────────────────────────────────────────────────────────────────────────────
// zxdg_decoration_manager_v1
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ManagerRequest {
    /// opcode 0 — destroy the manager object
    Destroy,
    /// opcode 1 — get_toplevel_decoration(id: new_id, toplevel: object)
    GetToplevelDecoration { id: NewId, toplevel: ObjectId },
}

impl Message for ManagerRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            ManagerRequest::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            ManagerRequest::GetToplevelDecoration { id, toplevel } => RawMessage {
                sender, opcode: Opcode(1),
                args: smallvec![id.into(), toplevel.into()],
            },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(ManagerRequest::Destroy),
            1 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let toplevel = match m.args.get(1) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(ManagerRequest::GetToplevelDecoration { id, toplevel })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// No events on the manager itself.
#[derive(Debug)]
pub enum ManagerEvent {}

impl Message for ManagerEvent {
    fn into_raw(self, _: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct ZxdgDecorationManagerV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for ZxdgDecorationManagerV1 {
    type Event = ManagerEvent;
    type Request = ManagerRequest;

    const NAME: &'static str = "zxdg_decoration_manager_v1";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                              // 0: destroy
        &[PayloadType::NewId, PayloadType::ObjectId],    // 1: get_toplevel_decoration
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

// ─────────────────────────────────────────────────────────────────────────────
// zxdg_toplevel_decoration_v1
// ─────────────────────────────────────────────────────────────────────────────

/// Decoration mode sent by the compositor to the client.
/// `1` = no preference (client decides), `2` = server-side, `3` = client-side.
pub const MODE_CLIENT_SIDE: u32 = 1;
pub const MODE_SERVER_SIDE: u32 = 2;

#[derive(Debug)]
pub enum DecorationRequest {
    /// opcode 0 — destroy this decoration object
    Destroy,
    /// opcode 1 — client requests a specific mode (ignored by SSD compositors)
    SetMode { mode: u32 },
    /// opcode 2 — client unsets any mode preference
    UnsetMode,
}

impl Message for DecorationRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            DecorationRequest::Destroy   => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            DecorationRequest::SetMode { mode } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![mode.into()],
            },
            DecorationRequest::UnsetMode => RawMessage { sender, opcode: Opcode(2), args: smallvec![] },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(DecorationRequest::Destroy),
            1 => {
                let mode = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(DecorationRequest::SetMode { mode })
            }
            2 => Ok(DecorationRequest::UnsetMode),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum DecorationEvent {
    /// opcode 0 — compositor tells the client which mode to use
    Configure { mode: u32 },
}

impl Message for DecorationEvent {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            DecorationEvent::Configure { mode } => RawMessage {
                sender, opcode: Opcode(0), args: smallvec![mode.into()],
            },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let mode = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(DecorationEvent::Configure { mode })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct ZxdgToplevelDecorationV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for ZxdgToplevelDecorationV1 {
    type Event = DecorationEvent;
    type Request = DecorationRequest;

    const NAME: &'static str = "zxdg_toplevel_decoration_v1";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                    // 0: destroy
        &[PayloadType::UInt],   // 1: set_mode
        &[],                    // 2: unset_mode
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
