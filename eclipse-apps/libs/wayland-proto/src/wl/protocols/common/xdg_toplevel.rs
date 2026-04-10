//! xdg_toplevel — a toplevel application window.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::{Array, Opcode};
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;
use smallvec::smallvec;

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Request {
    Destroy,
    SetParent { parent: ObjectId },
    SetTitle { title: String },
    SetAppId { app_id: String },
    Move { seat: ObjectId, serial: u32 },
    Resize { seat: ObjectId, serial: u32, edges: u32 },
    SetMinSize { width: i32, height: i32 },
    SetMaxSize { width: i32, height: i32 },
    SetMaximized,
    UnsetMaximized,
    SetFullscreen { output: ObjectId },
    UnsetFullscreen,
    SetMinimized,
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::Destroy => RawMessage { sender, opcode: Opcode(0), args: smallvec![] },
            Request::SetParent { parent } => RawMessage { sender, opcode: Opcode(1), args: smallvec![parent.into()] },
            Request::SetTitle { title } => RawMessage { sender, opcode: Opcode(2), args: smallvec![title.into()] },
            Request::SetAppId { app_id } => RawMessage { sender, opcode: Opcode(3), args: smallvec![app_id.into()] },
            Request::Move { seat, serial } => RawMessage { sender, opcode: Opcode(4), args: smallvec![seat.into(), serial.into()] },
            Request::Resize { seat, serial, edges } => RawMessage { sender, opcode: Opcode(5), args: smallvec![seat.into(), serial.into(), edges.into()] },
            Request::SetMinSize { width, height } => RawMessage { sender, opcode: Opcode(7), args: smallvec![width.into(), height.into()] },
            Request::SetMaxSize { width, height } => RawMessage { sender, opcode: Opcode(8), args: smallvec![width.into(), height.into()] },
            Request::SetMaximized => RawMessage { sender, opcode: Opcode(9), args: smallvec![] },
            Request::UnsetMaximized => RawMessage { sender, opcode: Opcode(10), args: smallvec![] },
            Request::SetFullscreen { output } => RawMessage { sender, opcode: Opcode(11), args: smallvec![output.into()] },
            Request::UnsetFullscreen => RawMessage { sender, opcode: Opcode(12), args: smallvec![] },
            Request::SetMinimized => RawMessage { sender, opcode: Opcode(13), args: smallvec![] },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => Ok(Request::Destroy),
            1 => {
                let parent = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => ObjectId::null() };
                Ok(Request::SetParent { parent })
            }
            2 => {
                let title = match m.args.get(0) { Some(Payload::String(s)) => s.clone(), _ => String::new() };
                Ok(Request::SetTitle { title })
            }
            3 => {
                let app_id = match m.args.get(0) { Some(Payload::String(s)) => s.clone(), _ => String::new() };
                Ok(Request::SetAppId { app_id })
            }
            4 => {
                let seat = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => ObjectId::null() };
                let serial = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(Request::Move { seat, serial })
            }
            5 => {
                let seat = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => ObjectId::null() };
                let serial = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let edges = match m.args.get(2) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(Request::Resize { seat, serial, edges })
            }
            7 => {
                let width = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => 0 };
                let height = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => 0 };
                Ok(Request::SetMinSize { width, height })
            }
            8 => {
                let width = match m.args.get(0) { Some(Payload::Int(v)) => *v, _ => 0 };
                let height = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => 0 };
                Ok(Request::SetMaxSize { width, height })
            }
            9 => Ok(Request::SetMaximized),
            10 => Ok(Request::UnsetMaximized),
            11 => {
                let output = match m.args.get(0) { Some(Payload::ObjectId(v)) => *v, _ => ObjectId::null() };
                Ok(Request::SetFullscreen { output })
            }
            12 => Ok(Request::UnsetFullscreen),
            13 => Ok(Request::SetMinimized),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Event {
    /// compositor → client: new size/state (0,0 = use your preferred size)
    Configure { width: i32, height: i32, states: Array },
    /// compositor → client: window should close
    Close,
    /// compositor → client: suggested bounds (optional)
    ConfigureBounds { width: i32, height: i32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Configure { width, height, states } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![width.into(), height.into(), states.into()],
            },
            Event::Close => RawMessage { sender, opcode: Opcode(1), args: smallvec![] },
            Event::ConfigureBounds { width, height } => RawMessage {
                sender, opcode: Opcode(2),
                args: smallvec![width.into(), height.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                if m.args.len() < 3 { return Err(DeserializeError::InvalidLength); }
                let width  = match m.args[0] { Payload::Int(v) => v, _ => 0 };
                let height = match m.args[1] { Payload::Int(v) => v, _ => 0 };
                let states = match &m.args[2] { Payload::Array(v) => v.clone(), _ => Array(alloc::vec::Vec::new()) };
                Ok(Event::Configure { width, height, states })
            }
            1 => Ok(Event::Close),
            2 => {
                 if m.args.len() < 2 { return Err(DeserializeError::InvalidLength); }
                 let width  = match m.args[0] { Payload::Int(v) => v, _ => 0 };
                 let height = match m.args[1] { Payload::Int(v) => v, _ => 0 };
                 Ok(Event::ConfigureBounds { width, height })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Interface ─────────────────────────────────────────────────────────────────

pub struct XdgToplevel {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for XdgToplevel {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "xdg_toplevel";
    const VERSION: u32 = 2;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[],                                          // 0: destroy
        &[PayloadType::ObjectId],                     // 1: set_parent
        &[PayloadType::String],                       // 2: set_title
        &[PayloadType::String],                       // 3: set_app_id
        &[PayloadType::ObjectId, PayloadType::UInt],  // 4: move
        &[PayloadType::ObjectId, PayloadType::UInt, PayloadType::UInt], // 5: resize
        &[],                                          // 6: (reserved)
        &[PayloadType::Int, PayloadType::Int],        // 7: set_min_size
        &[PayloadType::Int, PayloadType::Int],        // 8: set_max_size
        &[],                                          // 9: set_maximized
        &[],                                          // 10: unset_maximized
        &[PayloadType::ObjectId],                     // 11: set_fullscreen
        &[],                                          // 12: unset_fullscreen
        &[],                                          // 13: set_minimized
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

impl XdgToplevel {
    pub fn set_title(&mut self, title: &str) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(2), &[Payload::String(String::from(title))], &[])
    }
    pub fn set_app_id(&mut self, app_id: &str) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(3), &[Payload::String(String::from(app_id))], &[])
    }
    pub fn destroy(&mut self) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(0), &[], &[])
    }
}
