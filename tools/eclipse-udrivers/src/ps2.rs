//! The PC's i8042 keyboard controller, with a keyboard on its first port
//! and a mouse on its aux port, as a process drives it: what
//! `drivers/src/input/ps2_input.rs` does in the kernel, port for port, but
//! through [`claim::port_in`] and [`claim::port_out`] and with timeouts in
//! time rather than in turns of a loop.
//!
//! Each byte the controller gives is the keyboard's or the mouse's, as
//! its status says: [`Keyboard`] turns the keyboard's scancodes (set 1, as
//! the controller translates them) into keys, [`Mouse`] the mouse's
//! packets into moves, buttons and wheels. Neither touches a port, so both
//! are tried here against bytes alone, and [`init`] against an i8042 that
//! is not there.

use std::collections::BTreeMap;

use crate::claim;
use crate::evdev::{Info, EV_KEY, EV_LED, EV_REL, EV_REP};

/// Status: a byte waits to be read; the controller has not yet taken the
/// last one written; the byte waiting is the aux port's.
const OUTPUT_FULL: u8 = 1 << 0;
const INPUT_FULL: u8 = 1 << 1;
const AUX_DATA: u8 = 1 << 5;

/// How long the controller, or a device behind it, gets to take a byte or
/// to answer.
pub const TIMEOUT_NS: u64 = 100_000_000;

/// The controller's two ports, as a driver reaches them.
pub trait Io {
    /// Port 0x64, read.
    fn status(&mut self) -> u8;
    /// Port 0x60, read.
    fn read(&mut self) -> u8;
    /// Port 0x60, written: to the keyboard, or after 0xD4 to the mouse.
    fn write_data(&mut self, value: u8);
    /// Port 0x64, written: commands of the controller.
    fn write_cmd(&mut self, value: u8);
    /// A clock, in ns, for the timeouts.
    fn now_ns(&mut self) -> u64;
}

/// The i8042 this process holds (`udriver=i8042@0`): ports 0x60 and 0x64
/// are offsets 0 and 4 of the keyboard's window.
pub struct Ports;

impl Ports {
    fn get(offset: usize) -> u8 {
        claim::port_in(claim::KIND_I8042, 0, offset, 1).unwrap_or(0) as u8
    }

    fn put(offset: usize, value: u8) {
        let _ = claim::port_out(claim::KIND_I8042, 0, offset, 1, value as u32);
    }
}

impl Io for Ports {
    fn status(&mut self) -> u8 {
        Ports::get(4)
    }

    fn read(&mut self) -> u8 {
        Ports::get(0)
    }

    fn write_data(&mut self, value: u8) {
        Ports::put(0, value)
    }

    fn write_cmd(&mut self, value: u8) {
        Ports::put(4, value)
    }

    fn now_ns(&mut self) -> u64 {
        crate::now_ns()
    }
}

/// Wait until `ready` holds of the status, for at most [`TIMEOUT_NS`].
fn wait(io: &mut impl Io, ready: impl Fn(u8) -> bool) -> bool {
    let start = io.now_ns();
    loop {
        if ready(io.status()) {
            return true;
        }
        if io.now_ns().saturating_sub(start) > TIMEOUT_NS {
            return false;
        }
        std::hint::spin_loop();
    }
}

fn wait_write(io: &mut impl Io) -> bool {
    wait(io, |s| s & INPUT_FULL == 0)
}

fn read_data(io: &mut impl Io) -> Option<u8> {
    wait(io, |s| s & OUTPUT_FULL != 0).then(|| io.read())
}

fn drain(io: &mut impl Io) {
    while io.status() & OUTPUT_FULL != 0 {
        let _ = io.read();
    }
}

fn command(io: &mut impl Io, cmd: u8) -> bool {
    let ok = wait_write(io);
    if ok {
        io.write_cmd(cmd);
    }
    ok
}

/// A byte for the mouse, and whether it said it took it (0xFA).
fn write_aux(io: &mut impl Io, byte: u8) -> bool {
    if !command(io, 0xD4) || !wait_write(io) {
        return false;
    }
    io.write_data(byte);
    read_data(io) == Some(ACK)
}

/// What a device behind the controller answers a byte it took, and one it
/// wants again.
const ACK: u8 = 0xFA;
const RESEND: u8 = 0xFE;

/// Which of the mouse's protocols it was talked into: the plain one of
/// 3-byte packets, or a Microsoft one of 4 with a wheel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Proto {
    #[default]
    Ps2,
    /// IntelliMouse (id 3): byte 3 is the wheel.
    Imps,
    /// IntelliMouse Explorer (id 4): byte 3 has the wheel, buttons 4 and 5,
    /// or a tilted wheel.
    Imex,
}

impl Proto {
    fn packet_len(self) -> usize {
        match self {
            Proto::Ps2 => 3,
            _ => 4,
        }
    }
}

/// Set the controller up as the kernel's driver did: both ports on, both
/// interrupts, translation to set 1; then the mouse reset, talked into the
/// richest protocol it has, and streaming. The protocol it took.
pub fn init(io: &mut impl Io) -> Proto {
    drain(io);
    command(io, 0xAE);
    command(io, 0xA8);
    let mut config = 0;
    if command(io, 0x20) {
        config = read_data(io).unwrap_or(0);
    }
    // Interrupts of both ports, both clocks on, translation.
    config |= 0x01 | 0x02 | 0x40;
    config &= !(0x10 | 0x20);
    if command(io, 0x60) && wait_write(io) {
        io.write_data(config);
    }
    drain(io);
    // The reset first: a mouse may take nothing more before its self test
    // (0xAA) and id are out.
    if write_aux(io, 0xFF) {
        let _ = read_data(io);
        let _ = read_data(io);
    }
    let _ = write_aux(io, 0xF6);
    // The knock goes while the mouse is still quiet: streaming, its packets
    // would come between the answers.
    let proto = negotiate_wheel(io);
    let _ = write_aux(io, 0xF4);
    proto
}

fn mouse_id(io: &mut impl Io) -> Option<u8> {
    if !write_aux(io, 0xF2) {
        return None;
    }
    read_data(io)
}

fn set_rate(io: &mut impl Io, rate: u8) -> bool {
    write_aux(io, 0xF3) && write_aux(io, rate)
}

/// Linux's `intellimouse_detect` and `im_explorer_detect`: rates 200, 100,
/// 80 and the mouse says id 3; then 200, 200, 80 and it says 4.
fn negotiate_wheel(io: &mut impl Io) -> Proto {
    let knock = |io: &mut _, rates: [u8; 3]| rates.into_iter().all(|r| set_rate(io, r));
    let mut proto = Proto::Ps2;
    if knock(io, [200, 100, 80]) && mouse_id(io) == Some(3) {
        proto = Proto::Imps;
        if knock(io, [200, 200, 80]) && mouse_id(io) == Some(4) {
            proto = Proto::Imex;
        }
    }
    // 100 a second, psmouse's; the knock left it at 80.
    let _ = set_rate(io, 100);
    proto
}

/// Light the keyboard's LEDs: `mask` has Scroll Lock at bit 0, Num Lock at
/// 1, Caps Lock at 2. The keyboard's two acks come as bytes like any other,
/// and [`Keyboard`] drops them.
pub fn set_leds(io: &mut impl Io, mask: u8) {
    for byte in [0xED, mask & 7] {
        if !wait_write(io) {
            return;
        }
        io.write_data(byte);
    }
}

/// The mask [`set_leds`] takes, from what of `LED_NUML`, `LED_CAPSL` and
/// `LED_SCROLLL` is lit.
pub fn led_mask(num: bool, caps: bool, scroll: bool) -> u8 {
    scroll as u8 | (num as u8) << 1 | (caps as u8) << 2
}

/// Every byte waiting in the controller, the keyboard's and the mouse's
/// apart, in the order they came.
pub fn take(io: &mut impl Io, out: &mut Vec<(bool, u8)>) {
    loop {
        let status = io.status();
        if status & OUTPUT_FULL == 0 {
            return;
        }
        let byte = io.read();
        out.push((status & AUX_DATA != 0, byte));
    }
}

/// Linux's key codes for scancode set 1.
pub const KEY_PAUSE: u16 = 119;

/// The key of an `E0` scancode: Linux's `atkbd` table, translated.
fn extended(code: u8) -> Option<u16> {
    Some(match code {
        0x10 => 165, // KEY_PREVIOUSSONG
        0x19 => 163, // KEY_NEXTSONG
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x20 => 113, // KEY_MUTE
        0x21 => 140, // KEY_CALC
        0x22 => 164, // KEY_PLAYPAUSE
        0x24 => 166, // KEY_STOPCD
        0x2E => 114, // KEY_VOLUMEDOWN
        0x30 => 115, // KEY_VOLUMEUP
        0x32 => 172, // KEY_HOMEPAGE
        0x35 => 98,  // KEY_KPSLASH
        0x37 => 99,  // KEY_SYSRQ
        0x38 => 100, // KEY_RIGHTALT
        0x46 => 119, // KEY_PAUSE (Ctrl+Break)
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA
        0x5C => 126, // KEY_RIGHTMETA
        0x5D => 127, // KEY_COMPOSE
        0x5E => 116, // KEY_POWER
        0x5F => 142, // KEY_SLEEP
        0x63 => 143, // KEY_WAKEUP
        _ => return None,
    })
}

/// The key of a plain scancode: set 1 is laid out as Linux's codes are,
/// up to F12.
fn plain(code: u8) -> Option<u16> {
    match code {
        0x01..=0x53 | 0x56..=0x58 => Some(code as u16),
        _ => None,
    }
}

/// Every key [`Keyboard`] can give.
pub fn keys() -> impl Iterator<Item = u16> {
    (0..0x80u8)
        .filter_map(plain)
        .chain((0..0x80u8).filter_map(extended))
        .chain([KEY_PAUSE])
}

/// What the keyboard said: scancodes, one byte at a time, made into keys.
#[derive(Default)]
pub struct Keyboard {
    /// The prefix of the scancode being read: `E0`, or how many bytes of
    /// Pause's `E1 1D 45` (or `E1 9D C5`) are still to come.
    e0: bool,
    e1: u8,
    /// Whether the Pause sequence being read is the key's going down.
    e1_make: bool,
}

impl Keyboard {
    /// The key that one more byte finishes, and whether it went down.
    pub fn byte(&mut self, b: u8) -> Option<(u16, bool)> {
        if self.e1 > 0 {
            if self.e1 == 2 {
                self.e1_make = b & 0x80 == 0;
            }
            self.e1 -= 1;
            return (self.e1 == 0).then_some((KEY_PAUSE, self.e1_make));
        }
        match b {
            0xE0 => {
                self.e0 = true;
                return None;
            }
            0xE1 => {
                self.e1 = 2;
                return None;
            }
            // Its answers to commands, LEDs above all.
            ACK | RESEND => return None,
            _ => {}
        }
        let down = b & 0x80 == 0;
        let code = b & 0x7f;
        let key = if std::mem::take(&mut self.e0) {
            // 2A and 36 behind E0 are the shifts the keyboard fakes around
            // the grey keys: nothing was pressed.
            extended(code)
        } else {
            plain(code)
        };
        key.map(|k| (k, down))
    }
}

/// One packet of the mouse's, as evdev counts: Y down, wheel away from the
/// user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Packet {
    pub dx: i32,
    pub dy: i32,
    pub wheel: i32,
    pub hwheel: i32,
    /// Left, right, middle, side, extra, from bit 0.
    pub buttons: u8,
}

/// The mouse's packet in `bytes`, as Linux's `psmouse_process_byte` reads
/// it: every wheel negated, the Explorer's fields sign-extended.
pub fn decode(proto: Proto, bytes: &[u8; 4]) -> Packet {
    let flags = bytes[0];
    let signed = |v: u8, neg: bool| v as i32 - if neg { 256 } else { 0 };
    let mut p = Packet {
        dx: signed(bytes[1], flags & 0x10 != 0),
        dy: -signed(bytes[2], flags & 0x20 != 0),
        buttons: flags & 7,
        ..Packet::default()
    };
    let z = bytes[3];
    match proto {
        Proto::Ps2 => {}
        Proto::Imps => p.wheel = -(z as i8 as i32),
        Proto::Imex => match z & 0xC0 {
            0x80 => p.wheel = (z & 32) as i32 - (z & 31) as i32,
            0x40 => p.hwheel = (z & 32) as i32 - (z & 31) as i32,
            _ => {
                p.wheel = (z & 8) as i32 - (z & 7) as i32;
                p.buttons |= ((z >> 4) & 1) << 3 | ((z >> 5) & 1) << 4;
            }
        },
    }
    p
}

/// What the mouse said: bytes gathered into packets.
pub struct Mouse {
    proto: Proto,
    have: usize,
    bytes: [u8; 4],
}

impl Mouse {
    pub fn new(proto: Proto) -> Mouse {
        Mouse {
            proto,
            have: 0,
            bytes: [0; 4],
        }
    }

    /// The packet one more byte finishes. A first byte without bit 3,
    /// which every first byte has, is a byte lost or a stray one (a late
    /// self test): it is dropped, so the packets after it line up again.
    pub fn byte(&mut self, b: u8) -> Option<Packet> {
        if self.have == 0 && b & 0x08 == 0 {
            return None;
        }
        self.bytes[self.have] = b;
        self.have += 1;
        if self.have < self.proto.packet_len() {
            return None;
        }
        self.have = 0;
        let mut bytes = self.bytes;
        if self.proto == Proto::Ps2 {
            bytes[3] = 0;
        }
        Some(decode(self.proto, &bytes))
    }
}

pub const BTN_LEFT: u16 = 0x110;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;
pub const LED_NUML: u16 = 0;
pub const LED_CAPSL: u16 = 1;
pub const LED_SCROLLL: u16 = 2;
/// `BUS_I8042`, and the vendors psmouse and atkbd give.
const BUS_I8042: u16 = 0x11;

pub(crate) fn bitmap(codes: impl IntoIterator<Item = u16>) -> Vec<u8> {
    let mut map = Vec::new();
    for c in codes {
        let (byte, bit) = (c as usize / 8, c % 8);
        if map.len() <= byte {
            map.resize(byte + 1, 0);
        }
        map[byte] |= 1 << bit;
    }
    map
}

/// The keyboard, as Linux's `atkbd` names it.
pub fn keyboard_info() -> Info {
    Info {
        name: "AT Translated Set 2 keyboard".into(),
        phys: "isa0060/serio0/input0".into(),
        id: [BUS_I8042, 1, 1, 0xab41],
        bits: BTreeMap::from([
            (EV_KEY, bitmap(keys())),
            (EV_LED, bitmap([LED_NUML, LED_CAPSL, LED_SCROLLL])),
            (EV_REP, Vec::new()),
        ]),
        ..Info::default()
    }
}

/// The mouse, as Linux's `psmouse` names it for `proto`.
pub fn mouse_info(proto: Proto) -> Info {
    let (name, product, buttons) = match proto {
        Proto::Ps2 => ("PS/2 Generic Mouse", 1, 3),
        Proto::Imps => ("ImPS/2 Generic Wheel Mouse", 3, 3),
        Proto::Imex => ("ImExPS/2 Generic Explorer Mouse", 4, 5),
    };
    let mut rel = vec![REL_X, REL_Y];
    if proto != Proto::Ps2 {
        rel.extend([REL_WHEEL, REL_WHEEL_HI_RES]);
    }
    if proto == Proto::Imex {
        rel.extend([REL_HWHEEL, REL_HWHEEL_HI_RES]);
    }
    Info {
        name: name.into(),
        phys: "isa0060/serio1/input0".into(),
        id: [BUS_I8042, 2, product, 0],
        bits: BTreeMap::from([
            (EV_KEY, bitmap((0..buttons).map(|b| BTN_LEFT + b))),
            (EV_REL, bitmap(rel)),
        ]),
        ..Info::default()
    }
}

/// What a packet says, as events, before its `SYN_REPORT`: every button
/// (evdev drops the ones that did not change) and what moved.
pub fn packet_events(p: &Packet) -> Vec<(u16, u16, i32)> {
    let mut out = Vec::new();
    for (code, v) in [(REL_X, p.dx), (REL_Y, p.dy)] {
        if v != 0 {
            out.push((EV_REL, code, v));
        }
    }
    for b in 0..5 {
        out.push((EV_KEY, BTN_LEFT + b, ((p.buttons >> b) & 1) as i32));
    }
    for (lo, hi, v) in [
        (REL_WHEEL, REL_WHEEL_HI_RES, p.wheel),
        (REL_HWHEEL, REL_HWHEEL_HI_RES, p.hwheel),
    ] {
        if v != 0 {
            out.push((EV_REL, lo, v));
            out.push((EV_REL, hi, v * 120));
        }
    }
    out
}

#[cfg(test)]
mod tests;
