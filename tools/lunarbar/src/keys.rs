//! Raw Linux evdev codes as they arrive over `wl_keyboard.key`, and the
//! subset of them that maps to a character.
//!
//! Wayland hands out evdev keycodes, not symbols: resolving them properly
//! means libxkbcommon, a keymap fd and a compose state — a shared library and
//! several hundred kB of tables for what a search field needs. These are the
//! codes both clients care about, in one place so their popups agree on what
//! Enter is.

/// `wl_keyboard.key` codes (evdev, i.e. `KEY_*` from `linux/input-event-codes.h`).
pub const KEY_ESC_WL: u32 = 1;
pub const KEY_BACKSPACE_WL: u32 = 14;
pub const KEY_TAB_WL: u32 = 15;
pub const KEY_ENTER_WL: u32 = 28;
pub const KEY_KPENTER_WL: u32 = 96;
pub const KEY_UP_WL: u32 = 103;
pub const KEY_DOWN_WL: u32 = 108;
pub const KEY_LEFT_WL: u32 = 105;
pub const KEY_RIGHT_WL: u32 = 106;
pub const KEY_PGUP_WL: u32 = 104;
pub const KEY_PGDN_WL: u32 = 109;

/// `wl_pointer.button` codes (`BTN_*`).
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// One wheel notch in `wl_pointer.axis` surface-local units.
pub const WHEEL_NOTCH: f64 = 15.0;

/// The printable character an evdev code stands for on a QWERTY-ish layout.
///
/// Deliberately layout-blind: ES and US agree on letters, digits and the two
/// punctuation keys below, which is all a search filter needs, and being
/// wrong about `-` on some third layout costs one character in a filter
/// rather than a wrong launch. Everything unmapped returns None and is
/// ignored by the caller.
pub fn key_char(code: u32) -> Option<char> {
    Some(match code {
        2..=10 => (b'1' + (code - 2) as u8) as char, // KEY_1..KEY_9
        11 => '0',                                   // KEY_0
        16..=25 => b"qwertyuiop"[(code - 16) as usize] as char,
        30..=38 => b"asdfghjkl"[(code - 30) as usize] as char,
        44..=50 => b"zxcvbnm"[(code - 44) as usize] as char,
        57 => ' ',
        12 => '-',
        52 => '.',
        53 => '/',
        _ => return None,
    })
}
