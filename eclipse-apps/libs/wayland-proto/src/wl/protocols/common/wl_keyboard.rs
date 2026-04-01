//! wl_keyboard — keyboard input device object.

use crate::wl::{ObjectId, NewId, Interface, Message, Connection, Payload, PayloadType, DeserializeError, RawMessage};
use crate::wl::connection::SendError;
use crate::wl::wire::{Array, Handle, Opcode};
use alloc::rc::Rc;
use core::cell::RefCell;
use smallvec::smallvec;

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Request {
    Release,
}

impl Message for Request {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        RawMessage { sender, opcode: Opcode(0), args: smallvec![] }
    }
    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 { 0 => Ok(Request::Release), _ => Err(DeserializeError::UnknownOpcode) }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

/// `wl_keyboard_keymap_format` values.
pub const KEYMAP_FORMAT_NO_KEYMAP: u32 = 0;
pub const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// `wl_keyboard_key_state` values.
pub const KEY_STATE_RELEASED: u32 = 0;
pub const KEY_STATE_PRESSED: u32 = 1;

#[derive(Debug)]
pub enum Event {
    /// opcode 0 — keyboard mapping (fd = -1 / size = 0 when format = NO_KEYMAP)
    Keymap { format: u32, fd: Handle, size: u32 },
    /// opcode 1 — keyboard focus entered `surface`
    Enter { serial: u32, surface: ObjectId, keys: Array },
    /// opcode 2 — keyboard focus left `surface`
    Leave { serial: u32, surface: ObjectId },
    /// opcode 3 — key press/release
    Key { serial: u32, time: u32, key: u32, state: u32 },
    /// opcode 4 — modifier state
    Modifiers { serial: u32, mods_depressed: u32, mods_latched: u32, mods_locked: u32, group: u32 },
    /// opcode 5 — key repeat rate and delay
    RepeatInfo { rate: i32, delay: i32 },
}

impl Message for Event {
    fn into_raw(self, sender: ObjectId) -> RawMessage {
        match self {
            Event::Keymap { format, fd, size } => RawMessage {
                sender, opcode: Opcode(0),
                args: smallvec![format.into(), fd.into(), size.into()],
            },
            Event::Enter { serial, surface, keys } => RawMessage {
                sender, opcode: Opcode(1),
                args: smallvec![serial.into(), surface.into(), keys.into()],
            },
            Event::Leave { serial, surface } => RawMessage {
                sender, opcode: Opcode(2),
                args: smallvec![serial.into(), surface.into()],
            },
            Event::Key { serial, time, key, state } => RawMessage {
                sender, opcode: Opcode(3),
                args: smallvec![serial.into(), time.into(), key.into(), state.into()],
            },
            Event::Modifiers { serial, mods_depressed, mods_latched, mods_locked, group } => RawMessage {
                sender, opcode: Opcode(4),
                args: smallvec![serial.into(), mods_depressed.into(), mods_latched.into(), mods_locked.into(), group.into()],
            },
            Event::RepeatInfo { rate, delay } => RawMessage {
                sender, opcode: Opcode(5),
                args: smallvec![rate.into(), delay.into()],
            },
        }
    }

    fn from_raw(_con: Rc<RefCell<dyn Connection>>, m: &RawMessage) -> Result<Self, DeserializeError> {
        match m.opcode.0 {
            3 => {
                let serial = match m.args.get(0) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let time   = match m.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let key    = match m.args.get(2) { Some(Payload::UInt(v)) => *v, _ => 0 };
                let state  = match m.args.get(3) { Some(Payload::UInt(v)) => *v, _ => 0 };
                Ok(Event::Key { serial, time, key, state })
            }
            _ => Err(DeserializeError::UnknownOpcode),
        }
    }
}

// ── Interface ─────────────────────────────────────────────────────────────────

pub struct WlKeyboard {
    con: Rc<RefCell<dyn Connection>>,
    id: ObjectId,
}

impl Interface for WlKeyboard {
    type Event = Event;
    type Request = Request;

    const NAME: &'static str = "wl_keyboard";
    const VERSION: u32 = 7;
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]] = &[
        &[], // 0: release
    ];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self { Self { con, id } }
    fn connection(&self) -> &Rc<RefCell<dyn Connection>> { &self.con }
    fn id(&self) -> ObjectId { self.id }
    fn as_new_id(&self) -> NewId { NewId(self.id.0) }
}

impl WlKeyboard {
    pub fn release(&mut self) -> Result<(), SendError> {
        self.con.borrow_mut().send(self.id, Opcode(0), &[], &[])
    }
}
