//! zwlr_layer_shell_v1 — wlroots layer shell.
//!
//! Allows clients to create surfaces that occupy a layer of the compositor
//! output (background, bottom, top, overlay).  Used by Waybar, swaylock,
//! swaybg, mako, and similar desktop-integration applications.
//!
//! Protocol: wlr-layer-shell-unstable-v1
//! Interface versions: zwlr_layer_shell_v1(4), zwlr_layer_surface_v1(5)

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use smallvec::smallvec;

// ─────────────────────────────────────────────────────────────────────────────
// Layer constants
// ─────────────────────────────────────────────────────────────────────────────

/// Layers, from bottom to top.
pub const LAYER_BACKGROUND: u32 = 0;
pub const LAYER_BOTTOM:     u32 = 1;
pub const LAYER_TOP:        u32 = 2;
pub const LAYER_OVERLAY:    u32 = 3;

/// Anchor edges (bitfield).
pub const ANCHOR_TOP:    u32 = 1;
pub const ANCHOR_BOTTOM: u32 = 2;
pub const ANCHOR_LEFT:   u32 = 4;
pub const ANCHOR_RIGHT:  u32 = 8;

/// Keyboard interactivity levels.
pub const KEYBOARD_INTERACTIVITY_NONE:       u32 = 0;
pub const KEYBOARD_INTERACTIVITY_EXCLUSIVE:  u32 = 1;
pub const KEYBOARD_INTERACTIVITY_ON_DEMAND:  u32 = 2;

// ─────────────────────────────────────────────────────────────────────────────
// zwlr_layer_shell_v1
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ShellRequest {
    /// opcode 0 — get_layer_surface(id, surface, output, layer, namespace)
    GetLayerSurface {
        id:        NewId,
        surface:   ObjectId,
        output:    ObjectId,  // may be null_id
        layer:     u32,
        namespace: String,
    },
    /// opcode 1 — destroy the shell object
    Destroy,
}

impl Message for ShellRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            ShellRequest::GetLayerSurface { id, surface, output, layer, namespace } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![
                    id.into(), surface.into(), output.into(),
                    layer.into(),
                    crate::wl::Payload::String(namespace),
                ],
            },
            ShellRequest::Destroy => RawMessage { sender, opcode: Opcode(1), args: smallvec![] },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let surface = match m.args.get(1) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let output = match m.args.get(2) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let layer = match m.args.get(3) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let namespace = match m.args.get(4) { Some(Payload::String(s)) => s.clone(), _ => String::new() };
                Ok(ShellRequest::GetLayerSurface { id, surface, output, layer, namespace })
            }
            1 => Ok(ShellRequest::Destroy),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

/// No events on zwlr_layer_shell_v1 itself.
#[derive(Debug)]
pub enum ShellEvent {}

impl Message for ShellEvent {
    fn into_raw(self, _: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct ZwlrLayerShellV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for ZwlrLayerShellV1 {
    type Event = ShellEvent;
    type Request = ShellRequest;

    const NAME: &'static str = "zwlr_layer_shell_v1";
    const VERSION: u32 = 4;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        // 0: get_layer_surface — id, surface, output, layer, namespace
        &[PayloadType::NewId, PayloadType::ObjectId, PayloadType::ObjectId,
          PayloadType::UInt, PayloadType::String],
        &[], // 1: destroy
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

// ─────────────────────────────────────────────────────────────────────────────
// zwlr_layer_surface_v1
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SurfaceRequest {
    /// opcode 0 — set size (0,0 = compositor chooses)
    SetSize { width: u32, height: u32 },
    /// opcode 1 — set anchor edges (bitfield of ANCHOR_*)
    SetAnchor { anchor: u32 },
    /// opcode 2 — set exclusive zone (pixels to reserve at the anchor edge)
    SetExclusiveZone { zone: i32 },
    /// opcode 3 — set margin at each edge (top, right, bottom, left)
    SetMargin { top: i32, right: i32, bottom: i32, left: i32 },
    /// opcode 4 — set keyboard interactivity
    SetKeyboardInteractivity { keyboard_interactivity: u32 },
    /// opcode 5 — get popup role for this surface
    GetPopup { popup: ObjectId },
    /// opcode 6 — acknowledge a configure event
    AckConfigure { serial: u32 },
    /// opcode 7 — destroy the layer surface
    Destroy,
    /// opcode 8 — set which layer to render on (added in v2)
    SetLayer { layer: u32 },
    /// opcode 9 — set exclusive edge (added in v5, optional)
    SetExclusiveEdge { edge: u32 },
}

impl Message for SurfaceRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            SurfaceRequest::SetSize { width, height } => RawMessage { sender, opcode: Opcode(0), args: smallvec![width.into(), height.into()] },
            SurfaceRequest::SetAnchor { anchor } => RawMessage { sender, opcode: Opcode(1), args: smallvec![anchor.into()] },
            SurfaceRequest::SetExclusiveZone { zone } => RawMessage { sender, opcode: Opcode(2), args: smallvec![zone.into()] },
            SurfaceRequest::SetMargin { top, right, bottom, left } => RawMessage { sender, opcode: Opcode(3), args: smallvec![top.into(), right.into(), bottom.into(), left.into()] },
            SurfaceRequest::SetKeyboardInteractivity { keyboard_interactivity } => RawMessage { sender, opcode: Opcode(4), args: smallvec![keyboard_interactivity.into()] },
            SurfaceRequest::GetPopup { popup } => RawMessage { sender, opcode: Opcode(5), args: smallvec![popup.into()] },
            SurfaceRequest::AckConfigure { serial } => RawMessage { sender, opcode: Opcode(6), args: smallvec![serial.into()] },
            SurfaceRequest::Destroy => RawMessage { sender, opcode: Opcode(7), args: smallvec![] },
            SurfaceRequest::SetLayer { layer } => RawMessage { sender, opcode: Opcode(8), args: smallvec![layer.into()] },
            SurfaceRequest::SetExclusiveEdge { edge } => RawMessage { sender, opcode: Opcode(9), args: smallvec![edge.into()] },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let width  = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let height = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetSize { width, height })
            }
            1 => {
                let anchor = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetAnchor { anchor })
            }
            2 => {
                let zone = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetExclusiveZone { zone })
            }
            3 => {
                let top    = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let right  = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let bottom = match m.args.get(2) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let left   = match m.args.get(3) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetMargin { top, right, bottom, left })
            }
            4 => {
                let keyboard_interactivity = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetKeyboardInteractivity { keyboard_interactivity })
            }
            5 => {
                let popup = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::GetPopup { popup })
            }
            6 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::AckConfigure { serial })
            }
            7 => Ok(SurfaceRequest::Destroy),
            8 => {
                let layer = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetLayer { layer })
            }
            9 => {
                let edge = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetExclusiveEdge { edge })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum SurfaceEvent {
    /// opcode 0 — compositor requests a size (0,0 = as large as desired)
    Configure { serial: u32, width: u32, height: u32 },
    /// opcode 1 — compositor closed the layer surface
    Closed,
}

impl Message for SurfaceEvent {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            SurfaceEvent::Configure { serial, width, height } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![serial.into(), width.into(), height.into()],
            },
            SurfaceEvent::Closed => RawMessage { sender, opcode: Opcode(1), args: smallvec![] },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let width  = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let height = match m.args.get(2) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(SurfaceEvent::Configure { serial, width, height })
            }
            1 => Ok(SurfaceEvent::Closed),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct ZwlrLayerSurfaceV1 {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for ZwlrLayerSurfaceV1 {
    type Event = SurfaceEvent;
    type Request = SurfaceRequest;

    const NAME: &'static str = "zwlr_layer_surface_v1";
    const VERSION: u32 = 4;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::UInt, PayloadType::UInt],                             // 0: set_size
        &[PayloadType::UInt],                                                // 1: set_anchor
        &[PayloadType::Int],                                                 // 2: set_exclusive_zone
        &[PayloadType::Int, PayloadType::Int, PayloadType::Int, PayloadType::Int], // 3: set_margin
        &[PayloadType::UInt],                                                // 4: set_keyboard_interactivity
        &[PayloadType::ObjectId],                                            // 5: get_popup
        &[PayloadType::UInt],                                                // 6: ack_configure
        &[],                                                                 // 7: destroy
        &[PayloadType::UInt],                                                // 8: set_layer
        &[PayloadType::UInt],                                                // 9: set_exclusive_edge
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
