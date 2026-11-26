//! USB HID, as a driver of the controller under it sees it: what a boot
//! keyboard's reports say, what a pointer's report descriptor says its
//! reports hold, and both as evdev events and as the `Info` of the node
//! that serves them (`crate::evdev`).
//!
//! Nothing here touches a device: the controller's driver
//! (`crate::xhci`) fetches the descriptors and the reports, and this turns
//! them into events, tested on the host.

use std::collections::BTreeMap;

use crate::evdev::{Info, EV_ABS, EV_KEY, EV_LED, EV_REL, EV_REP};
use crate::ps2::{bitmap, LED_CAPSL, LED_NUML, LED_SCROLLL};

/// `BUS_USB`.
pub const BUS_USB: u16 = 0x03;
pub const BTN_LEFT: u16 = 0x110;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
/// `INPUT_PROP_POINTER`.
const PROP_POINTER: u16 = 0;
/// How many buttons a pointer has at most: left, right, middle, side and
/// extra, as `hid-input.c` numbers the first five of the button page.
const BUTTONS: u16 = 5;

/// HID's keyboard page, by usage, and the Linux key of each: Linux's own
/// table (`hid-input.c`), the keys a boot keyboard has.
const KEYS: [(u8, u16); 147] = [
    (0x04, 30),  // KEY_A
    (0x05, 48),  // KEY_B
    (0x06, 46),  // KEY_C
    (0x07, 32),  // KEY_D
    (0x08, 18),  // KEY_E
    (0x09, 33),  // KEY_F
    (0x0a, 34),  // KEY_G
    (0x0b, 35),  // KEY_H
    (0x0c, 23),  // KEY_I
    (0x0d, 36),  // KEY_J
    (0x0e, 37),  // KEY_K
    (0x0f, 38),  // KEY_L
    (0x10, 50),  // KEY_M
    (0x11, 49),  // KEY_N
    (0x12, 24),  // KEY_O
    (0x13, 25),  // KEY_P
    (0x14, 16),  // KEY_Q
    (0x15, 19),  // KEY_R
    (0x16, 31),  // KEY_S
    (0x17, 20),  // KEY_T
    (0x18, 22),  // KEY_U
    (0x19, 47),  // KEY_V
    (0x1a, 17),  // KEY_W
    (0x1b, 45),  // KEY_X
    (0x1c, 21),  // KEY_Y
    (0x1d, 44),  // KEY_Z
    (0x1e, 2),   // KEY_1
    (0x1f, 3),   // KEY_2
    (0x20, 4),   // KEY_3
    (0x21, 5),   // KEY_4
    (0x22, 6),   // KEY_5
    (0x23, 7),   // KEY_6
    (0x24, 8),   // KEY_7
    (0x25, 9),   // KEY_8
    (0x26, 10),  // KEY_9
    (0x27, 11),  // KEY_0
    (0x28, 28),  // KEY_ENTER
    (0x29, 1),   // KEY_ESC
    (0x2a, 14),  // KEY_BACKSPACE
    (0x2b, 15),  // KEY_TAB
    (0x2c, 57),  // KEY_SPACE
    (0x2d, 12),  // KEY_MINUS
    (0x2e, 13),  // KEY_EQUAL
    (0x2f, 26),  // KEY_LEFTBRACE
    (0x30, 27),  // KEY_RIGHTBRACE
    (0x31, 43),  // KEY_BACKSLASH
    (0x32, 43),  // KEY_BACKSLASH
    (0x33, 39),  // KEY_SEMICOLON
    (0x34, 40),  // KEY_APOSTROPHE
    (0x35, 41),  // KEY_GRAVE
    (0x36, 51),  // KEY_COMMA
    (0x37, 52),  // KEY_DOT
    (0x38, 53),  // KEY_SLASH
    (0x39, 58),  // KEY_CAPSLOCK
    (0x3a, 59),  // KEY_F1
    (0x3b, 60),  // KEY_F2
    (0x3c, 61),  // KEY_F3
    (0x3d, 62),  // KEY_F4
    (0x3e, 63),  // KEY_F5
    (0x3f, 64),  // KEY_F6
    (0x40, 65),  // KEY_F7
    (0x41, 66),  // KEY_F8
    (0x42, 67),  // KEY_F9
    (0x43, 68),  // KEY_F10
    (0x44, 87),  // KEY_F11
    (0x45, 88),  // KEY_F12
    (0x46, 99),  // KEY_SYSRQ
    (0x47, 70),  // KEY_SCROLLLOCK
    (0x48, 119), // KEY_PAUSE
    (0x49, 110), // KEY_INSERT
    (0x4a, 102), // KEY_HOME
    (0x4b, 104), // KEY_PAGEUP
    (0x4c, 111), // KEY_DELETE
    (0x4d, 107), // KEY_END
    (0x4e, 109), // KEY_PAGEDOWN
    (0x4f, 106), // KEY_RIGHT
    (0x50, 105), // KEY_LEFT
    (0x51, 108), // KEY_DOWN
    (0x52, 103), // KEY_UP
    (0x53, 69),  // KEY_NUMLOCK
    (0x54, 98),  // KEY_KPSLASH
    (0x55, 55),  // KEY_KPASTERISK
    (0x56, 74),  // KEY_KPMINUS
    (0x57, 78),  // KEY_KPPLUS
    (0x58, 96),  // KEY_KPENTER
    (0x59, 79),  // KEY_KP1
    (0x5a, 80),  // KEY_KP2
    (0x5b, 81),  // KEY_KP3
    (0x5c, 75),  // KEY_KP4
    (0x5d, 76),  // KEY_KP5
    (0x5e, 77),  // KEY_KP6
    (0x5f, 71),  // KEY_KP7
    (0x60, 72),  // KEY_KP8
    (0x61, 73),  // KEY_KP9
    (0x62, 82),  // KEY_KP0
    (0x63, 83),  // KEY_KPDOT
    (0x64, 86),  // KEY_102ND
    (0x65, 127), // KEY_COMPOSE
    (0x66, 116), // KEY_POWER
    (0x67, 117), // KEY_KPEQUAL
    (0x68, 183), // KEY_F13
    (0x69, 184), // KEY_F14
    (0x6a, 185), // KEY_F15
    (0x6b, 186), // KEY_F16
    (0x6c, 187), // KEY_F17
    (0x6d, 188), // KEY_F18
    (0x6e, 189), // KEY_F19
    (0x6f, 190), // KEY_F20
    (0x70, 191), // KEY_F21
    (0x71, 192), // KEY_F22
    (0x72, 193), // KEY_F23
    (0x73, 194), // KEY_F24
    (0x74, 134), // KEY_OPEN
    (0x75, 138), // KEY_HELP
    (0x76, 130), // KEY_PROPS
    (0x77, 132), // KEY_FRONT
    (0x78, 128), // KEY_STOP
    (0x79, 129), // KEY_AGAIN
    (0x7a, 131), // KEY_UNDO
    (0x7b, 137), // KEY_CUT
    (0x7c, 133), // KEY_COPY
    (0x7d, 135), // KEY_PASTE
    (0x7e, 136), // KEY_FIND
    (0x7f, 113), // KEY_MUTE
    (0x80, 115), // KEY_VOLUMEUP
    (0x81, 114), // KEY_VOLUMEDOWN
    (0x85, 121), // KEY_KPCOMMA
    (0x87, 89),  // KEY_RO
    (0x88, 93),  // KEY_KATAKANAHIRAGANA
    (0x89, 124), // KEY_YEN
    (0x8a, 92),  // KEY_HENKAN
    (0x8b, 94),  // KEY_MUHENKAN
    (0x8c, 95),  // KEY_KPJPCOMMA
    (0x90, 122), // KEY_HANGEUL
    (0x91, 123), // KEY_HANJA
    (0x92, 90),  // KEY_KATAKANA
    (0x93, 91),  // KEY_HIRAGANA
    (0xb6, 179), // KEY_KPLEFTPAREN
    (0xb7, 180), // KEY_KPRIGHTPAREN
    (0xe0, 29),  // KEY_LEFTCTRL
    (0xe1, 42),  // KEY_LEFTSHIFT
    (0xe2, 56),  // KEY_LEFTALT
    (0xe3, 125), // KEY_LEFTMETA
    (0xe4, 97),  // KEY_RIGHTCTRL
    (0xe5, 54),  // KEY_RIGHTSHIFT
    (0xe6, 100), // KEY_RIGHTALT
    (0xe7, 126), // KEY_RIGHTMETA
];

/// The Linux key of keyboard usage `u`.
pub fn usage_key(u: u8) -> Option<u16> {
    KEYS.iter().find(|&&(k, _)| k == u).map(|&(_, c)| c)
}

/// The keys of the modifier byte, from bit 0: left Ctrl, Shift, Alt, Meta,
/// then the right ones.
const MODIFIERS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];
/// Usages 1 to 3 of the keyboard page say an error (`ErrorRollOver` and
/// the like), not a key.
const KEY_ERROR_MAX: u8 = 3;

/// A boot keyboard: what its last report held.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keyboard {
    mods: u8,
    keys: [u8; 6],
}

impl Keyboard {
    /// The key events of boot report `r` (modifiers, a reserved byte, six
    /// keys), before its `SYN_REPORT`: each key that went down or up since
    /// the last. A report of rollover errors leaves the keys as they were,
    /// as Linux does: it says the keyboard does not know, not that
    /// everything came up.
    pub fn report(&mut self, r: &[u8]) -> Vec<(u16, u16, i32)> {
        let mut out = Vec::new();
        if r.len() < 3 {
            return out;
        }
        let mods = r[0];
        let mut keys = [0u8; 6];
        for (k, &b) in keys.iter_mut().zip(&r[2..]) {
            *k = b;
        }
        for (bit, &code) in MODIFIERS.iter().enumerate() {
            let (was, now) = (self.mods >> bit & 1, mods >> bit & 1);
            if was != now {
                out.push((EV_KEY, code, now as i32));
            }
        }
        self.mods = mods;
        if keys.iter().any(|&u| u != 0 && u <= KEY_ERROR_MAX) {
            return out;
        }
        for &u in self.keys.iter().filter(|&&u| u != 0 && !keys.contains(&u)) {
            out.extend(usage_key(u).map(|c| (EV_KEY, c, 0)));
        }
        for &u in keys.iter().filter(|&&u| u != 0 && !self.keys.contains(&u)) {
            out.extend(usage_key(u).map(|c| (EV_KEY, c, 1)));
        }
        self.keys = keys;
        out
    }

    /// Whether a key is held: then the next report that still holds it is
    /// not a press.
    pub fn held(&self) -> bool {
        self.mods != 0 || self.keys.iter().any(|&k| k != 0)
    }
}

/// The LED report of a boot keyboard for the LEDs evdev has lit: Num Lock,
/// Caps Lock and Scroll Lock are its bits 0 to 2, as evdev's codes are.
pub fn led_report(lit: &[bool; 3]) -> u8 {
    lit.iter()
        .enumerate()
        .fold(0, |m, (i, &on)| m | ((on as u8) << i))
}

/// A boot keyboard as a node says it is.
pub fn keyboard_info(name: &str, phys: &str, id: [u16; 3]) -> Info {
    let mut keys: Vec<u16> = KEYS.iter().map(|&(_, c)| c).collect();
    keys.extend(MODIFIERS);
    Info {
        name: name.into(),
        phys: phys.into(),
        id: [BUS_USB, id[0], id[1], id[2]],
        bits: BTreeMap::from([
            (EV_KEY, bitmap(keys)),
            (EV_LED, bitmap([LED_NUML, LED_CAPSL, LED_SCROLLL])),
            (EV_REP, Vec::new()),
        ]),
        ..Info::default()
    }
}

/// Where a value is in a report, in bits after the report ID.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Field {
    pub at: u32,
    pub size: u32,
    pub min: i32,
    pub max: i32,
    /// Absolute, not a move since the last report.
    pub abs: bool,
}

impl Field {
    /// Its value in report `r` (after the ID), signed when its minimum is
    /// below 0.
    fn read(&self, r: &[u8]) -> Option<i32> {
        // Both are the device's numbers: a sum that wrapped would pass for
        // a field at the report's start.
        let end = self.at.checked_add(self.size)?;
        if self.size == 0 || self.size > 32 || end.div_ceil(8) as usize > r.len() {
            return None;
        }
        let mut v: u64 = 0;
        for i in 0..self.size {
            let bit = self.at + i;
            if r[bit as usize / 8] >> (bit % 8) & 1 != 0 {
                v |= 1 << i;
            }
        }
        if self.min < 0 && self.size < 64 && v >> (self.size - 1) & 1 != 0 {
            v |= u64::MAX << self.size;
        }
        Some(v as i64 as i32)
    }
}

/// What a pointer's reports hold, from its report descriptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    /// The ID its reports start with, when the descriptor numbers them.
    pub id: Option<u8>,
    /// Buttons 1 to 5, each a bit.
    pub buttons: Vec<(u16, Field)>,
    pub x: Option<Field>,
    pub y: Option<Field>,
    pub wheel: Option<Field>,
    pub hwheel: Option<Field>,
}

/// The bits of the longest report a descriptor may describe: 64 KiB, the
/// most a transfer carries.
const REPORT_BITS_MAX: u32 = 8 * 65536;
/// The values a descriptor may name in all. Values of no bits fit any
/// report, so their number has a bound of its own.
const VALUES_MAX: usize = 4096;

/// One input item's values: the usage of each, and where it is.
fn usages_of(
    local: &[u32],
    range: Option<(u32, u32)>,
    count: u32,
) -> impl Iterator<Item = Option<u32>> + '_ {
    (0..count).map(move |i| match range {
        Some((lo, hi)) => Some(lo.saturating_add(i).min(hi)),
        None => local.get(i as usize).or(local.last()).copied(),
    })
}

/// Sign-extend an item's data of `len` bytes.
fn signed(v: u32, len: usize) -> i32 {
    match len {
        1 => v as u8 as i8 as i32,
        2 => v as u16 as i16 as i32,
        _ => v as i32,
    }
}

/// Read a pointer's report descriptor: the first report that has an X,
/// with its buttons, Y and wheels. `None` when no report has an X.
pub fn parse_pointer(desc: &[u8]) -> Option<Pointer> {
    #[derive(Clone, Copy, Default)]
    struct Global {
        page: u32,
        min: i32,
        max: i32,
        size: u32,
        count: u32,
        id: u8,
    }
    let mut g = Global::default();
    let mut stack: Vec<Global> = Vec::new();
    let mut local: Vec<u32> = Vec::new();
    let (mut lo, mut hi) = (None, None);
    // Bits so far, by report ID.
    let mut at: BTreeMap<u8, u32> = BTreeMap::new();
    let mut numbered = false;
    // Every input value: (report ID, full usage, field).
    let mut values: Vec<(u8, u32, Field)> = Vec::new();
    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xfe {
            // A long item: its size is the next byte.
            let len = *desc.get(i + 1)? as usize;
            i += 3 + len;
            continue;
        }
        let len = [0, 1, 2, 4][(b & 3) as usize];
        let data = desc.get(i + 1..i + 1 + len)?;
        let v = data.iter().rev().fold(0u32, |v, &d| v << 8 | d as u32);
        i += 1 + len;
        let full = |u: u32, page: u32| if len == 4 { u } else { page << 16 | u };
        match (b >> 2 & 3, b >> 4) {
            // Main: Input.
            (0, 0x8) => {
                // The descriptor is the device's to write: a count or a
                // report beyond what a report can hold is no pointer's,
                // and is not walked value by value.
                let wide = g.size.checked_mul(g.count)?;
                if g.count as usize > VALUES_MAX && v & 1 == 0 && g.size == 0 {
                    return None;
                }
                let bits = at.entry(g.id).or_insert(0);
                let end = bits.checked_add(wide).filter(|&e| e <= REPORT_BITS_MAX)?;
                if v & 1 == 0 {
                    for (k, u) in usages_of(&local, lo.zip(hi), g.count).enumerate() {
                        if let Some(u) = u {
                            let field = Field {
                                at: *bits + k as u32 * g.size,
                                size: g.size,
                                min: g.min,
                                max: g.max,
                                abs: v & 4 == 0,
                            };
                            if values.len() == VALUES_MAX {
                                return None;
                            }
                            values.push((g.id, u, field));
                        }
                    }
                }
                *bits = end;
                local.clear();
                (lo, hi) = (None, None);
            }
            // Main: anything else clears the locals.
            (0, _) => {
                local.clear();
                (lo, hi) = (None, None);
            }
            (1, 0x0) => g.page = v,
            (1, 0x1) => g.min = signed(v, len),
            (1, 0x2) => g.max = if g.min < 0 { signed(v, len) } else { v as i32 },
            (1, 0x7) => g.size = v,
            (1, 0x8) => {
                g.id = v as u8;
                numbered = true;
            }
            (1, 0x9) => g.count = v,
            (1, 0xa) => stack.push(g),
            (1, 0xb) => g = stack.pop().unwrap_or(g),
            (2, 0x0) => local.push(full(v, g.page)),
            (2, 0x1) => lo = Some(full(v, g.page)),
            (2, 0x2) => hi = Some(full(v, g.page)),
            _ => {}
        }
    }
    const X: u32 = 0x0001_0030;
    let &(id, _, _) = values.iter().find(|&&(_, u, _)| u == X)?;
    let mut p = Pointer {
        id: numbered.then_some(id),
        ..Pointer::default()
    };
    for &(_, u, f) in values.iter().filter(|v| v.0 == id) {
        match u {
            0x0001_0030 => p.x = p.x.or(Some(f)),
            0x0001_0031 => p.y = p.y.or(Some(f)),
            0x0001_0038 => p.wheel = p.wheel.or(Some(f)),
            0x000c_0238 => p.hwheel = p.hwheel.or(Some(f)),
            u if u >> 16 == 9 && (1..=BUTTONS as u32).contains(&(u & 0xffff)) => {
                p.buttons.push((BTN_LEFT + (u & 0xffff) as u16 - 1, f))
            }
            _ => {}
        }
    }
    Some(p)
}

/// The layout of a boot mouse's report: three buttons, then X and Y as a
/// signed byte each, and a wheel when the report has a fourth byte.
pub fn boot_mouse() -> Pointer {
    let rel = |at| Field {
        at,
        size: 8,
        min: -127,
        max: 127,
        abs: false,
    };
    let button = |n: u32| Field {
        at: n,
        size: 1,
        min: 0,
        max: 1,
        abs: false,
    };
    Pointer {
        id: None,
        buttons: (0..3).map(|n| (BTN_LEFT + n as u16, button(n))).collect(),
        x: Some(rel(8)),
        y: Some(rel(16)),
        wheel: Some(rel(24)),
        hwheel: None,
    }
}

impl Pointer {
    /// Whether it says where it is, not how far it moved: a tablet.
    pub fn absolute(&self) -> bool {
        self.x.is_some_and(|f| f.abs)
    }

    /// The events of report `r`, before its `SYN_REPORT`: every button
    /// (evdev drops the ones that did not change), the position or the
    /// move, and the wheels. Nothing for a report of another ID.
    pub fn report(&self, r: &[u8]) -> Vec<(u16, u16, i32)> {
        let r = match self.id {
            Some(id) if r.first() != Some(&id) => return Vec::new(),
            Some(_) => &r[1..],
            None => r,
        };
        let mut out = Vec::new();
        let axes = if self.absolute() {
            [(EV_ABS, ABS_X, self.x), (EV_ABS, ABS_Y, self.y)]
        } else {
            [(EV_REL, REL_X, self.x), (EV_REL, REL_Y, self.y)]
        };
        for (kind, code, f) in axes {
            if let Some(v) = f.and_then(|f| f.read(r)) {
                if kind == EV_ABS || v != 0 {
                    out.push((kind, code, v));
                }
            }
        }
        for &(code, f) in &self.buttons {
            if let Some(v) = f.read(r) {
                out.push((EV_KEY, code, (v != 0) as i32));
            }
        }
        for (code, f) in [(REL_WHEEL, self.wheel), (REL_HWHEEL, self.hwheel)] {
            // A wheel is always a move, whatever its item says.
            match f.and_then(|f| f.read(r)) {
                Some(v) if v != 0 => out.push((EV_REL, code, v)),
                _ => {}
            }
        }
        out
    }

    /// The pointer as a node says it is.
    pub fn info(&self, name: &str, phys: &str, id: [u16; 3]) -> Info {
        let mut bits = BTreeMap::from([(EV_KEY, bitmap(self.buttons.iter().map(|&(c, _)| c)))]);
        let mut rel: Vec<u16> = Vec::new();
        let mut abs = BTreeMap::new();
        if self.absolute() {
            for (code, f) in [(ABS_X, self.x), (ABS_Y, self.y)] {
                if let Some(f) = f {
                    abs.insert(code, [0, f.min, f.max, 0, 0, 0]);
                }
            }
            bits.insert(EV_ABS, bitmap(abs.keys().copied()));
        } else {
            rel.extend([REL_X, REL_Y]);
        }
        rel.extend(self.wheel.map(|_| REL_WHEEL));
        rel.extend(self.hwheel.map(|_| REL_HWHEEL));
        if !rel.is_empty() {
            bits.insert(EV_REL, bitmap(rel));
        }
        Info {
            name: name.into(),
            phys: phys.into(),
            id: [BUS_USB, id[0], id[1], id[2]],
            props: if self.absolute() {
                Vec::new()
            } else {
                bitmap([PROP_POINTER])
            },
            bits,
            abs,
            ..Info::default()
        }
    }
}

#[cfg(test)]
mod tests;
