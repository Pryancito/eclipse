//! Minimal `xdg_positioner` and `xdg_popup` protocol stubs.
//!
//! `xdg_positioner` describes the geometry/anchor for a popup window.
//! `xdg_popup` is a transient/popup surface (context menus, tooltips).
//!
//! Both are created and configured by apps but the compositor implementation
//! here is simplified: popups are treated as regular surfaces with no special
//! positioning.  This lets apps proceed past the "create positioner" step.
#![allow(unused)]

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ─────────────────────────────────────────────────────────────────────────────
// xdg_positioner
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum PositionerRequest {
    Destroy,
    SetSize            { width: i32, height: i32 },
    SetAnchorRect      { x: i32, y: i32, width: i32, height: i32 },
    SetAnchor          { anchor: u32 },
    SetGravity         { gravity: u32 },
    SetConstraintAdjustment { constraint_adjustment: u32 },
    SetOffset          { x: i32, y: i32 },
    SetReactive,
    SetParentSize      { parent_width: i32, parent_height: i32 },
    SetParentConfigure { serial: u32 },
}

impl Message for PositionerRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Self::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            Self::SetSize { width, height } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![width.into(), height.into()],
            },
            Self::SetAnchorRect { x, y, width, height } => RawMessage {
                sender, opcode: Opcode(2), args: smallvec![x.into(), y.into(), width.into(), height.into()],
            },
            Self::SetAnchor { anchor } => RawMessage {
                sender, opcode: Opcode(3), args: smallvec![anchor.into()],
            },
            Self::SetGravity { gravity } => RawMessage {
                sender, opcode: Opcode(4), args: smallvec![gravity.into()],
            },
            Self::SetConstraintAdjustment { constraint_adjustment } => RawMessage {
                sender, opcode: Opcode(5), args: smallvec![constraint_adjustment.into()],
            },
            Self::SetOffset { x, y } => RawMessage {
                sender, opcode: Opcode(6), args: smallvec![x.into(), y.into()],
            },
            Self::SetReactive => RawMessage { sender, opcode: Opcode(7), args: smallvec![] },
            Self::SetParentSize { parent_width, parent_height } => RawMessage {
                sender, opcode: Opcode(8), args: smallvec![parent_width.into(), parent_height.into()],
            },
            Self::SetParentConfigure { serial } => RawMessage {
                sender, opcode: Opcode(9), args: smallvec![serial.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Self::Destroy),
            1 => {
                let w = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let h = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetSize { width: w, height: h })
            }
            2 => {
                let x = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let y = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let w = match m.args.get(2) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let h = match m.args.get(3) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetAnchorRect { x, y, width: w, height: h })
            }
            3 => {
                let v = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetAnchor { anchor: v })
            }
            4 => {
                let v = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetGravity { gravity: v })
            }
            5 => {
                let v = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetConstraintAdjustment { constraint_adjustment: v })
            }
            6 => {
                let x = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let y = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetOffset { x, y })
            }
            7 => Ok(Self::SetReactive),
            8 => {
                let pw = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let ph = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetParentSize { parent_width: pw, parent_height: ph })
            }
            9 => {
                let s = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::SetParentConfigure { serial: s })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum PositionerEvent {}

impl Message for PositionerEvent {
    fn into_raw(self, _: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct XdgPositioner {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for XdgPositioner {
    type Event = PositionerEvent;
    type Request = PositionerRequest;
    const NAME: &'static str = "xdg_positioner";
    const VERSION: u32 = 3;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                                                                              // 0: destroy
        &[PayloadType::Int, PayloadType::Int],                                                           // 1: set_size
        &[PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int],                       // 2: set_anchor_rect
        &[PayloadType::UInt],                                                                            // 3: set_anchor
        &[PayloadType::UInt],                                                                            // 4: set_gravity
        &[PayloadType::UInt],                                                                            // 5: set_constraint_adjustment
        &[PayloadType::Int, PayloadType::Int],                                                           // 6: set_offset
        &[],                                                                                             // 7: set_reactive
        &[PayloadType::Int, PayloadType::Int],                                                           // 8: set_parent_size
        &[PayloadType::UInt],                                                                            // 9: set_parent_configure
    ];
    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

// ─────────────────────────────────────────────────────────────────────────────
// xdg_popup
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum PopupRequest {
    Destroy,
    Grab    { seat: ObjectId, serial: u32 },
    Reposition { positioner: ObjectId, token: u32 },
}

impl Message for PopupRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Self::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            Self::Grab { seat, serial } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![seat.into(), serial.into()],
            },
            Self::Reposition { positioner, token } => RawMessage {
                sender, opcode: Opcode(2), args: smallvec![positioner.into(), token.into()],
            },
        }
    }
    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Self::Destroy),
            1 => {
                let seat = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let serial = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::Grab { seat, serial })
            }
            2 => {
                let pos = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let tok = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::Reposition { positioner: pos, token: tok })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum PopupEvent {
    /// opcode 0 — compositor tells popup its final position
    Configure { x: i32, y: i32, width: i32, height: i32 },
    /// opcode 1 — popup dismissed
    PopupDone,
    /// opcode 2 — repositioned
    Repositioned { token: u32 },
}

impl Message for PopupEvent {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Self::Configure { x, y, width, height } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![x.into(), y.into(), width.into(), height.into()],
            },
            Self::PopupDone => RawMessage { sender, opcode: Opcode(1), args: smallvec![] },
            Self::Repositioned { token } => RawMessage {
                sender, opcode: Opcode(2), args: smallvec![token.into()],
            },
        }
    }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let x = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let y = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let w = match m.args.get(2) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let h = match m.args.get(3) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::Configure { x, y, width: w, height: h })
            }
            1 => Ok(Self::PopupDone),
            2 => {
                let t = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(Self::Repositioned { token: t })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct XdgPopup {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for XdgPopup {
    type Event = PopupEvent;
    type Request = PopupRequest;
    const NAME: &'static str = "xdg_popup";
    const VERSION: u32 = 3;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                                                   // 0: destroy
        &[PayloadType::ObjectId, PayloadType::UInt],                          // 1: grab
        &[PayloadType::ObjectId, PayloadType::UInt],                          // 2: reposition
    ];
    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
