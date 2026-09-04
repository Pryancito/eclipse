//! Console keyboard layout (the cooked VT path).
//!
//! Wayland/X11 interpret evdev scancodes in userspace via XKB. The text
//! console cannot: `handle_key_event` in [`super::stdio`] has to turn those
//! same scancodes into Unicode itself. This module holds the tables and the
//! live selection, exposed to userspace as `/proc/kbd`.
//!
//! Only layouts with an in-kernel table can be selected here (`es`, `us`).
//! labwc can take any XKB name; `eclipse-kbd` keeps the two in sync for the
//! layouts we actually ship.

use alloc::string::String;
use core::sync::atomic::{AtomicU8, Ordering};

use zcore_drivers::input::input_event_codes::key::*;

/// Sentinel: first reader pulls `kbd=` off the kernel cmdline.
const UNINIT: u8 = 0xff;

static LAYOUT: AtomicU8 = AtomicU8::new(UNINIT);

/// Console layout. Discriminant is the value stored in [`LAYOUT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Layout {
    Es = 0,
    Us = 1,
}

impl Layout {
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Es => "es",
            Layout::Us => "us",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "es" | "ES" => Some(Layout::Es),
            "us" | "US" => Some(Layout::Us),
            _ => None,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Layout::Us,
            _ => Layout::Es,
        }
    }
}

/// Modifier snapshot, matching Linux's `shift_state` (KG_SHIFT / KG_ALTGR /
/// KG_CTRL / KG_CAPSSHIFT) for the four XKB levels we honour.
#[derive(Clone, Copy)]
pub struct KeyMods {
    pub shift: bool,
    pub altgr: bool,
    pub caps: bool,
    pub ctrl: bool,
}

impl KeyMods {
    fn letter(self, lower: char) -> char {
        if self.caps ^ self.shift {
            lower.to_ascii_uppercase()
        } else {
            lower
        }
    }

    /// Four levels: base, Shift, AltGr, Shift+AltGr.
    fn pick(self, base: char, shifted: char, altgr: char, shift_altgr: char) -> char {
        if self.altgr && self.shift {
            shift_altgr
        } else if self.altgr {
            altgr
        } else if self.shift {
            shifted
        } else {
            base
        }
    }
}

fn ensure_init() {
    if LAYOUT.load(Ordering::Relaxed) != UNINIT {
        return;
    }
    let chosen = parse_cmdline(&kernel_hal::boot::cmdline()).unwrap_or(Layout::Es);
    let _ = LAYOUT.compare_exchange(UNINIT, chosen as u8, Ordering::Relaxed, Ordering::Relaxed);
}

/// `kbd=es` / `kbd=us` on the kernel cmdline (`:` or whitespace separated,
/// same token rules as `desktop=`).
pub fn parse_cmdline(cmdline: &str) -> Option<Layout> {
    cmdline
        .split(|c: char| c == ':' || c.is_whitespace())
        .find_map(|tok| tok.strip_prefix("kbd="))
        .and_then(Layout::from_name)
}

pub fn current() -> Layout {
    ensure_init();
    Layout::from_u8(LAYOUT.load(Ordering::Relaxed))
}

pub fn current_name() -> &'static str {
    current().as_str()
}

/// Text of `/proc/kbd`: `"es\n"`.
pub fn proc_content() -> String {
    alloc::format!("{}\n", current_name())
}

pub fn set(name: &str) -> rcore_fs::vfs::Result<()> {
    let layout = Layout::from_name(name).ok_or(rcore_fs::vfs::FsError::InvalidParam)?;
    LAYOUT.store(layout as u8, Ordering::Relaxed);
    Ok(())
}

/// Cycle `es` → `us` → `es`. Initialises from the cmdline if needed so a
/// write of `toggle` before any read still has a well-defined start.
pub fn toggle() -> Layout {
    ensure_init();
    loop {
        let now = LAYOUT.load(Ordering::Relaxed);
        if now == UNINIT {
            continue;
        }
        let cur = Layout::from_u8(now);
        let next = match cur {
            Layout::Es => Layout::Us,
            Layout::Us => Layout::Es,
        };
        if LAYOUT
            .compare_exchange(now, next as u8, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

/// Keycode + modifiers → printable character for the **current** layout.
pub fn to_char(code: u16, mods: KeyMods) -> Option<char> {
    to_char_for(current(), code, mods)
}

fn to_char_for(layout: Layout, code: u16, mods: KeyMods) -> Option<char> {
    if let Some(c) = shared_keys(code, mods) {
        return Some(c);
    }
    match layout {
        Layout::Es => punct_es(code, mods),
        Layout::Us => punct_us(code, mods),
    }
}

/// Letters, keypad and editing keys that do not differ between `es` and `us`.
fn shared_keys(code: u16, mods: KeyMods) -> Option<char> {
    match code {
        KEY_A => Some(mods.letter('a')),
        KEY_B => Some(mods.letter('b')),
        KEY_C => Some(mods.letter('c')),
        KEY_D => Some(mods.letter('d')),
        KEY_E => Some(mods.letter('e')),
        KEY_F => Some(mods.letter('f')),
        KEY_G => Some(mods.letter('g')),
        KEY_H => Some(mods.letter('h')),
        KEY_I => Some(mods.letter('i')),
        KEY_J => Some(mods.letter('j')),
        KEY_K => Some(mods.letter('k')),
        KEY_L => Some(mods.letter('l')),
        KEY_M => Some(mods.letter('m')),
        KEY_N => Some(mods.letter('n')),
        KEY_O => Some(mods.letter('o')),
        KEY_P => Some(mods.letter('p')),
        KEY_Q => Some(mods.letter('q')),
        KEY_R => Some(mods.letter('r')),
        KEY_S => Some(mods.letter('s')),
        KEY_T => Some(mods.letter('t')),
        KEY_U => Some(mods.letter('u')),
        KEY_V => Some(mods.letter('v')),
        KEY_W => Some(mods.letter('w')),
        KEY_X => Some(mods.letter('x')),
        KEY_Y => Some(mods.letter('y')),
        KEY_Z => Some(mods.letter('z')),
        KEY_ENTER | KEY_KPENTER => Some('\r'),
        KEY_SPACE => Some(' '),
        KEY_BACKSPACE => Some('\x7f'),
        KEY_TAB => Some('\t'),
        KEY_KP0 => Some('0'),
        KEY_KP1 => Some('1'),
        KEY_KP2 => Some('2'),
        KEY_KP3 => Some('3'),
        KEY_KP4 => Some('4'),
        KEY_KP5 => Some('5'),
        KEY_KP6 => Some('6'),
        KEY_KP7 => Some('7'),
        KEY_KP8 => Some('8'),
        KEY_KP9 => Some('9'),
        KEY_KPSLASH => Some('/'),
        KEY_KPASTERISK => Some('*'),
        KEY_KPMINUS => Some('-'),
        KEY_KPPLUS => Some('+'),
        _ => None,
    }
}

/// QWERTY español (España), aligned with `symbols/es` of xkeyboard-config.
fn punct_es(code: u16, mods: KeyMods) -> Option<char> {
    match code {
        KEY_1 => Some(mods.pick('1', '!', '|', '|')),
        KEY_2 => Some(mods.pick('2', '"', '@', '@')),
        KEY_3 => Some(mods.pick('3', '·', '#', '#')),
        KEY_4 => Some(mods.pick('4', '$', '~', '~')),
        KEY_5 => Some(mods.pick('5', '%', '€', '€')),
        KEY_6 => Some(mods.pick('6', '&', '¬', '¬')),
        KEY_7 => Some(mods.pick('7', '/', '{', '{')),
        KEY_8 => Some(mods.pick('8', '(', '[', '[')),
        KEY_9 => Some(mods.pick('9', ')', ']', ']')),
        KEY_0 => Some(mods.pick('0', '=', '}', '}')),
        KEY_MINUS => Some(mods.pick('\'', '?', '\\', '|')),
        KEY_EQUAL => Some(mods.pick('¡', '¿', '¡', '¿')),
        KEY_GRAVE => Some(mods.pick('º', 'ª', 'º', 'ª')),
        KEY_LEFTBRACE => Some(mods.pick('`', '^', '[', '{')),
        KEY_RIGHTBRACE => Some(mods.pick('+', '*', ']', '}')),
        KEY_BACKSLASH => Some(mods.pick('\\', '|', '|', '|')),
        KEY_SEMICOLON => Some(mods.pick('ñ', 'Ñ', '~', '`')),
        KEY_APOSTROPHE => Some(mods.pick('´', '¨', '{', '}')),
        KEY_102ND => Some(mods.pick('<', '>', '\\', '|')),
        KEY_COMMA => Some(mods.pick(',', ';', ',', ';')),
        KEY_DOT | KEY_KPDOT => Some(mods.pick('.', ':', '.', ':')),
        KEY_SLASH => Some(mods.pick('-', '_', '-', '_')),
        _ => None,
    }
}

/// ANSI US QWERTY. AltGr is ignored (levels 3/4 copy 1/2); ISO `KEY_102ND`
/// is the extra `<>` key some US-on-ISO boards still send.
fn punct_us(code: u16, mods: KeyMods) -> Option<char> {
    match code {
        KEY_1 => Some(mods.pick('1', '!', '1', '!')),
        KEY_2 => Some(mods.pick('2', '@', '2', '@')),
        KEY_3 => Some(mods.pick('3', '#', '3', '#')),
        KEY_4 => Some(mods.pick('4', '$', '4', '$')),
        KEY_5 => Some(mods.pick('5', '%', '5', '%')),
        KEY_6 => Some(mods.pick('6', '^', '6', '^')),
        KEY_7 => Some(mods.pick('7', '&', '7', '&')),
        KEY_8 => Some(mods.pick('8', '*', '8', '*')),
        KEY_9 => Some(mods.pick('9', '(', '9', '(')),
        KEY_0 => Some(mods.pick('0', ')', '0', ')')),
        KEY_MINUS => Some(mods.pick('-', '_', '-', '_')),
        KEY_EQUAL => Some(mods.pick('=', '+', '=', '+')),
        KEY_GRAVE => Some(mods.pick('`', '~', '`', '~')),
        KEY_LEFTBRACE => Some(mods.pick('[', '{', '[', '{')),
        KEY_RIGHTBRACE => Some(mods.pick(']', '}', ']', '}')),
        KEY_BACKSLASH => Some(mods.pick('\\', '|', '\\', '|')),
        KEY_SEMICOLON => Some(mods.pick(';', ':', ';', ':')),
        KEY_APOSTROPHE => Some(mods.pick('\'', '"', '\'', '"')),
        KEY_102ND => Some(mods.pick('<', '>', '\\', '|')),
        KEY_COMMA => Some(mods.pick(',', '<', ',', '<')),
        KEY_DOT | KEY_KPDOT => Some(mods.pick('.', '>', '.', '>')),
        KEY_SLASH => Some(mods.pick('/', '?', '/', '?')),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, altgr: bool) -> KeyMods {
        KeyMods {
            shift,
            altgr,
            caps: false,
            ctrl: false,
        }
    }

    #[test]
    fn parse_kbd_token() {
        assert_eq!(parse_cmdline("LOG=error:kbd=us:desktop=labwc"), Some(Layout::Us));
        assert_eq!(parse_cmdline("kbd=es ROOT=/dev/vda"), Some(Layout::Es));
        assert_eq!(parse_cmdline("desktop=labwc"), None);
        assert_eq!(parse_cmdline("kbd=de"), None);
    }

    #[test]
    fn tables_es_and_us() {
        assert_eq!(to_char_for(Layout::Es, KEY_SEMICOLON, mods(false, false)), Some('ñ'));
        assert_eq!(to_char_for(Layout::Us, KEY_SEMICOLON, mods(false, false)), Some(';'));
        assert_eq!(to_char_for(Layout::Us, KEY_2, mods(true, false)), Some('@'));
        assert_eq!(to_char_for(Layout::Es, KEY_2, mods(true, false)), Some('"'));
        assert_eq!(to_char_for(Layout::Es, KEY_A, mods(false, false)), Some('a'));
    }

    #[test]
    fn set_rejects_unknown() {
        assert!(set("de").is_err());
        set("es").unwrap();
        assert_eq!(current_name(), "es");
        set("us").unwrap();
        assert_eq!(current_name(), "us");
        assert_eq!(toggle(), Layout::Es);
        assert_eq!(current_name(), "es");
    }
}
