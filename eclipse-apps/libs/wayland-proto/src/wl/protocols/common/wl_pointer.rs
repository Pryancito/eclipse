//! wl_pointer — pointer input device object.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::Opcode;
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Request {
    /// opcode 0 — set the pointer cursor surface
    SetCursor { serial: u32, surface: ObjectId, hotspot_x: i32, hotspot_y: i32 },
    /// opcode 1 — release the pointer object
    Release,
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Request::SetCursor { serial, surface, hotspot_x, hotspot_y } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![serial.into(), surface.into(), hotspot_x.into(), hotspot_y.into()],
            },
            Request::Release => RawMessage { sender, opcode: Opcode(1), args: smallvec![] },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let serial     = match m.args.get(0) { Some(Payload::UInt(v))    => *v, _ => 0 };
                let surface    = match m.args.get(1) { Some(Payload::ObjectId(v))=> *v, _ => ObjectId::null() };
                let hotspot_x  = match m.args.get(2) { Some(Payload::Int(v))     => *v, _ => 0 };
                let hotspot_y  = match m.args.get(3) { Some(Payload::Int(v))     => *v, _ => 0 };
                Ok(Request::SetCursor { serial, surface, hotspot_x, hotspot_y })
            }
            1 => Ok(Request::Release),
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

/// `wl_pointer_button_state` values.
pub const BTN_STATE_RELEASED: u32 = 0;
pub const BTN_STATE_PRESSED:  u32 = 1;

/// `wl_pointer_axis` values.
pub const AXIS_VERTICAL_SCROLL:   u32 = 0;
pub const AXIS_HORIZONTAL_SCROLL: u32 = 1;

/// `wl_pointer_axis_source` values.
pub const AXIS_SOURCE_WHEEL: u32 = 0;
pub const AXIS_SOURCE_FINGER: u32 = 1;

#[derive(Debug)]
pub enum Event {
    /// opcode 0 — pointer entered a surface (focus gained)
    Enter   { serial: u32, surface: ObjectId, surface_x: f32, surface_y: f32 },
    /// opcode 1 — pointer left a surface (focus lost)
    Leave   { serial: u32, surface: ObjectId },
    /// opcode 2 — pointer moved
    Motion  { time: u32, surface_x: f32, surface_y: f32 },
    /// opcode 3 — button pressed/released
    Button  { serial: u32, time: u32, button: u32, state: u32 },
    /// opcode 4 — scroll axis
    Axis    { time: u32, axis: u32, value: f32 },
    /// opcode 5 — end of pointer event group
    Frame,
    /// opcode 6 — source of axis events
    AxisSource { axis_source: u32 },
    /// opcode 7 — axis movement stopped
    AxisStop { time: u32, axis: u32 },
    /// opcode 8 — discrete axis steps
    AxisDiscrete { axis: u32, discrete: i32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Enter { serial, surface, surface_x, surface_y } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![serial.into(), surface.into(), Payload::Fixed(surface_x), Payload::Fixed(surface_y)],
            },
            Event::Leave { serial, surface } => RawMessage {
                sender, opcode: Opcode(1),
                args: smallvec![serial.into(), surface.into()],
            },
            Event::Motion { time, surface_x, surface_y } => RawMessage {
                sender, opcode: Opcode(2),
                args: smallvec![time.into(), Payload::Fixed(surface_x), Payload::Fixed(surface_y)],
            },
            Event::Button { serial, time, button, state } => RawMessage {
                sender, opcode: Opcode(3),
                args: smallvec![serial.into(), time.into(), button.into(), state.into()],
            },
            Event::Axis { time, axis, value } => RawMessage {
                sender, opcode: Opcode(4),
                args: smallvec![time.into(), axis.into(), Payload::Fixed(value)],
            },
            Event::Frame => RawMessage { sender, opcode: Opcode(5), args: smallvec![] },
            Event::AxisSource { axis_source } => RawMessage {
                sender, opcode: Opcode(6),
                args: smallvec![axis_source.into()],
            },
            Event::AxisStop { time, axis } => RawMessage {
                sender, opcode: Opcode(7),
                args: smallvec![time.into(), axis.into()],
            },
            Event::AxisDiscrete { axis, discrete } => RawMessage {
                sender, opcode: Opcode(8),
                args: smallvec![axis.into(), discrete.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            0 => {
                let serial    = match m.args.get(0) { Some(Payload::UInt(v))  => *v, _ => 0 };
                let surface   = match m.args.get(1) { Some(Payload::ObjectId(v))=> *v, _ => ObjectId::null() };
                let surface_x = match m.args.get(2) { Some(Payload::Fixed(v)) => *v, _ => 0.0 };
                let surface_y = match m.args.get(3) { Some(Payload::Fixed(v)) => *v, _ => 0.0 };
                Ok(Event::Enter { serial, surface, surface_x, surface_y })
            }
            1 => {
                let serial    = match m.args.get(0) { Some(Payload::UInt(v))  => *v, _ => 0 };
                let surface   = match m.args.get(1) { Some(Payload::ObjectId(v))=> *v, _ => ObjectId::null() };
                Ok(Event::Leave { serial, surface })
            }
            2 => {
                let time      = match m.args.get(0) { Some(Payload::UInt(v))  => *v, _ => 0 };
                let surface_x = match m.args.get(1) { Some(Payload::Fixed(v)) => *v, _ => 0.0 };
                let surface_y = match m.args.get(2) { Some(Payload::Fixed(v)) => *v, _ => 0.0 };
                Ok(Event::Motion { time, surface_x, surface_y })
            }
            3 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let time   = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let button = match m.args.get(2) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let state  = match m.args.get(3) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(Event::Button { serial, time, button, state })
            }
            4 => {
                let time   = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let axis   = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let value  = match m.args.get(2) { Some(Payload::Fixed(v))=> *v, _ => 0.0 };
                Ok(Event::Axis { time, axis, value })
            }
            5 => Ok(Event::Frame),
            6 => {
                let axis_source = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(Event::AxisSource { axis_source })
            }
            7 => {
                let time = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let axis = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(Event::AxisStop { time, axis })
            }
            8 => {
                let axis = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let discrete = match m.args.get(1) { Some(Payload::Int(v)) => *v, _ => 0 };
                Ok(Event::AxisDiscrete { axis, discrete })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Interface ─────────────────────────────────────────────────────────────────

pub struct WlPointer {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlPointer {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_pointer";
    const VERSION: u32 = 7;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[PayloadType::UInt, PayloadType::ObjectId, PayloadType::Int, PayloadType::Int], // 0: set_cursor
        &[], // 1: release
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

impl WlPointer {
    pub fn release(&mut self) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(1), &[], &[])
    }
}
