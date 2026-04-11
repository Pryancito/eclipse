//! wl_shell / wl_shell_surface — legacy (deprecated) Wayland shell interface.
//!
//! Still used by many older GTK2-era, Qt4, and EFL applications.  An SSD
//! compositor like labwc should implement it so those clients can connect
//! and get a managed window with server-side decorations.
//!
//! NOTE: new clients should use `xdg_wm_base` instead.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use smallvec::smallvec;

// ─────────────────────────────────────────────────────────────────────────────
// wl_shell
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ShellRequest {
    /// opcode 0 — get_shell_surface(id: new_id, surface: object)
    GetShellSurface { id: NewId, surface: ObjectId },
}

impl Message for ShellRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            ShellRequest::GetShellSurface { id, surface } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![id.into(), surface.into()],
            },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let id = match m.args.get(0) { Some(Payload::NewId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let surface = match m.args.get(1) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(ShellRequest::GetShellSurface { id, surface })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// No events on wl_shell itself.
#[derive(Debug)]
pub enum ShellEvent {}

impl Message for ShellEvent {
    fn into_raw(self, _: ObjectId) -> RawMessage { unreachable!() }
    fn from_raw(_: Rc<RefCell<dyn Connection>>, _: &RawMessage) -> Result<Self, DeserializeError> {
        Err(DeserializeError::UnknownOpcode)
    }
}

pub struct WlShell {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlShell {
    type Event = ShellEvent;
    type Request = ShellRequest;

    const NAME: &'static str = "wl_shell";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::NewId, PayloadType::ObjectId], // 0: get_shell_surface
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

// ─────────────────────────────────────────────────────────────────────────────
// wl_shell_surface
// ─────────────────────────────────────────────────────────────────────────────

/// Resize edges for wl_shell_surface.resize.
pub const RESIZE_NONE: u32 = 0;
pub const RESIZE_TOP: u32 = 1;
pub const RESIZE_BOTTOM: u32 = 2;
pub const RESIZE_LEFT: u32 = 4;
pub const RESIZE_TOP_LEFT: u32 = 5;
pub const RESIZE_BOTTOM_LEFT: u32 = 6;
pub const RESIZE_RIGHT: u32 = 8;
pub const RESIZE_TOP_RIGHT: u32 = 9;
pub const RESIZE_BOTTOM_RIGHT: u32 = 10;

/// Fullscreen method for set_fullscreen.
pub const FULLSCREEN_METHOD_DEFAULT: u32 = 0;

/// Transient flags.
pub const TRANSIENT_INACTIVE: u32 = 0x1;

#[derive(Debug)]
pub enum SurfaceRequest {
    /// opcode 0 — acknowledge a ping (keep-alive)
    Pong { serial: u32 },
    /// opcode 1 — interactive move
    Move { seat: ObjectId, serial: u32 },
    /// opcode 2 — interactive resize
    Resize { seat: ObjectId, serial: u32, edges: u32 },
    /// opcode 3 — become a toplevel window
    SetToplevel,
    /// opcode 4 — become a transient window
    SetTransient { parent: ObjectId, x: i32, y: i32, flags: u32 },
    /// opcode 5 — become fullscreen
    SetFullscreen { method: u32, framerate: u32, output: ObjectId },
    /// opcode 6 — become a popup
    SetPopup { seat: ObjectId, serial: u32, parent: ObjectId, x: i32, y: i32, flags: u32 },
    /// opcode 7 — become maximized
    SetMaximized { output: ObjectId },
    /// opcode 8 — set window title
    SetTitle { title: String },
    /// opcode 9 — set application class (like app_id)
    SetClass { class: String },
}

impl Message for SurfaceRequest {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            SurfaceRequest::Pong { serial } => RawMessage { sender, opcode: Opcode(0), args: smallvec![serial.into()] },
            SurfaceRequest::Move { seat, serial } => RawMessage { sender, opcode: Opcode(1), args: smallvec![seat.into(), serial.into()] },
            SurfaceRequest::Resize { seat, serial, edges } => RawMessage { sender, opcode: Opcode(2), args: smallvec![seat.into(), serial.into(), edges.into()] },
            SurfaceRequest::SetToplevel => RawMessage { sender, opcode: Opcode(3), args: smallvec![] },
            SurfaceRequest::SetTransient { parent, x, y, flags } => RawMessage { sender, opcode: Opcode(4), args: smallvec![parent.into(), x.into(), y.into(), flags.into()] },
            SurfaceRequest::SetFullscreen { method, framerate, output } => RawMessage { sender, opcode: Opcode(5), args: smallvec![method.into(), framerate.into(), output.into()] },
            SurfaceRequest::SetPopup { seat, serial, parent, x, y, flags } => RawMessage { sender, opcode: Opcode(6), args: smallvec![seat.into(), serial.into(), parent.into(), x.into(), y.into(), flags.into()] },
            SurfaceRequest::SetMaximized { output } => RawMessage { sender, opcode: Opcode(7), args: smallvec![output.into()] },
            SurfaceRequest::SetTitle { title } => RawMessage { sender, opcode: Opcode(8), args: smallvec![title.into()] },
            SurfaceRequest::SetClass { class } => RawMessage { sender, opcode: Opcode(9), args: smallvec![class.into()] },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::Pong { serial })
            }
            1 => {
                let seat = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let serial = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::Move { seat, serial })
            }
            2 => {
                let seat = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let serial = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let edges = match m.args.get(2) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::Resize { seat, serial, edges })
            }
            3 => Ok(SurfaceRequest::SetToplevel),
            4 => {
                let parent = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let x = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let y = match m.args.get(2) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let flags = match m.args.get(3) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetTransient { parent, x, y, flags })
            }
            5 => {
                let method = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let framerate = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let output = match m.args.get(2) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetFullscreen { method, framerate, output })
            }
            6 => {
                let seat = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let serial = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let parent = match m.args.get(2) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let x = match m.args.get(3) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let y = match m.args.get(4) { Some(Payload::Int(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                let flags = match m.args.get(5) { Some(Payload::UInt(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetPopup { seat, serial, parent, x, y, flags })
            }
            7 => {
                let output = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetMaximized { output })
            }
            8 => {
                let title = match m.args.get(0) { Some(Payload::String(s)) => s.clone(), _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetTitle { title })
            }
            9 => {
                let class = match m.args.get(0) { Some(Payload::String(s)) => s.clone(), _ => return Err(DeserializeError::UnexpectedType) };
                Ok(SurfaceRequest::SetClass { class })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

#[derive(Debug)]
pub enum SurfaceEvent {
    /// opcode 0 — compositor sends a ping; client must reply with pong
    Ping { serial: u32 },
    /// opcode 1 — compositor asks client to resize
    Configure { edges: u32, width: i32, height: i32 },
    /// opcode 2 — popup grab was cancelled
    PopupDone,
}

impl Message for SurfaceEvent {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            SurfaceEvent::Ping { serial } => RawMessage { sender, opcode: Opcode(0), args: smallvec![serial.into()] },
            SurfaceEvent::Configure { edges, width, height } => RawMessage {
                sender, opcode: Opcode(1), args: smallvec![edges.into(), width.into(), height.into()],
            },
            SurfaceEvent::PopupDone => RawMessage { sender, opcode: Opcode(2), args: smallvec![] },
        }
    }

    fn from_raw(_: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(SurfaceEvent::Ping { serial })
            }
            1 => {
                let edges = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let width = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => 0 };
                let height = match m.args.get(2) { Some(Payload::Int(v)) => *v, _ => 0 };
                Ok(SurfaceEvent::Configure { edges, width, height })
            }
            2 => Ok(SurfaceEvent::PopupDone),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

pub struct WlShellSurface {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlShellSurface {
    type Event = SurfaceEvent;
    type Request = SurfaceRequest;

    const NAME: &'static str = "wl_shell_surface";
    const VERSION: u32 = 1;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::UInt],                                                           // 0: pong
        &[PayloadType::ObjectId, PayloadType::UInt],                                    // 1: move
        &[PayloadType::ObjectId, PayloadType::UInt, PayloadType::UInt],                // 2: resize
        &[],                                                                            // 3: set_toplevel
        &[PayloadType::ObjectId, PayloadType::Int, PayloadType::Int, PayloadType::UInt], // 4: set_transient
        &[PayloadType::UInt, PayloadType::UInt, PayloadType::ObjectId],                // 5: set_fullscreen
        &[PayloadType::ObjectId, PayloadType::UInt, PayloadType::ObjectId, PayloadType::Int, PayloadType::Int, PayloadType::UInt], // 6: set_popup
        &[PayloadType::ObjectId],                                                       // 7: set_maximized
        &[PayloadType::String],                                                         // 8: set_title
        &[PayloadType::String],                                                         // 9: set_class
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}
