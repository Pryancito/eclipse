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
/// Deliberately layout-blind, and **only the letters, the digits and the space
/// are actually layout-independent**. The comment here used to claim ES and US
/// agree on the punctuation too, and they do not: on a Spanish layout
/// `KEY_MINUS` (12) is the key printed `'` and `KEY_SLASH` (53) is the key
/// printed `-`, so the two were swapped for exactly the layout this image ships
/// with. Resolving them properly means libxkbcommon and a keymap fd.
///
/// So both of those keys give `-`, which is the character that actually turns up
/// in an application name (`gnome-terminal`, `libreoffice-writer`); `/` turns up
/// in none, and on a Spanish keyboard the key printed `-` producing a `-` is
/// what anyone would expect. The cost of being wrong here is one character in a
/// search filter, never a wrong launch, because the filter only ever narrows a
/// list the user then looks at.
///
/// Everything unmapped returns None and is ignored by the caller.
pub fn key_char(code: u32) -> Option<char> {
    Some(match code {
        2..=10 => (b'1' + (code - 2) as u8) as char, // KEY_1..KEY_9
        11 => '0',                                   // KEY_0
        16..=25 => b"qwertyuiop"[(code - 16) as usize] as char,
        30..=38 => b"asdfghjkl"[(code - 30) as usize] as char,
        44..=50 => b"zxcvbnm"[(code - 44) as usize] as char,
        57 => ' ', // KEY_SPACE
        51 => ',', // KEY_COMMA, `,` on ES too
        52 => '.', // KEY_DOT, `.` on ES too
        // KEY_MINUS and KEY_SLASH: `-` and `/` on US, `'` and `-` on ES.
        12 | 53 => '-',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_the_ones_linux_actually_sends() {
        // Straight out of `linux/input-event-codes.h`. Wayland hands out evdev
        // keycodes, so a wrong number here is a key that silently does nothing
        // (or, worse, does something else) and no compiler notices.
        assert_eq!(KEY_ESC_WL, 1);
        assert_eq!(KEY_BACKSPACE_WL, 14);
        assert_eq!(KEY_TAB_WL, 15);
        assert_eq!(KEY_ENTER_WL, 28);
        assert_eq!(KEY_KPENTER_WL, 96);
        assert_eq!(KEY_UP_WL, 103);
        assert_eq!(KEY_PGUP_WL, 104);
        assert_eq!(KEY_LEFT_WL, 105);
        assert_eq!(KEY_RIGHT_WL, 106);
        assert_eq!(KEY_DOWN_WL, 108);
        assert_eq!(KEY_PGDN_WL, 109);
        assert_eq!(BTN_LEFT, 0x110);
        assert_eq!(BTN_RIGHT, 0x111);
        assert_eq!(BTN_MIDDLE, 0x112);
        // Every one of them distinct, which a copy-paste would break.
        let all = [
            KEY_ESC_WL,
            KEY_BACKSPACE_WL,
            KEY_TAB_WL,
            KEY_ENTER_WL,
            KEY_KPENTER_WL,
            KEY_UP_WL,
            KEY_DOWN_WL,
            KEY_LEFT_WL,
            KEY_RIGHT_WL,
            KEY_PGUP_WL,
            KEY_PGDN_WL,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "two key codes are the same number");
    }

    #[test]
    fn no_navigation_key_is_also_a_printable_character() {
        // A code in both tables would type into the filter AND move the
        // selection, or take the place of the other -- the ranges below sit next
        // to the navigation codes (KEY_0 is 11 and KEY_BACKSPACE is 14).
        for k in [
            KEY_ESC_WL,
            KEY_BACKSPACE_WL,
            KEY_TAB_WL,
            KEY_ENTER_WL,
            KEY_KPENTER_WL,
            KEY_UP_WL,
            KEY_DOWN_WL,
            KEY_LEFT_WL,
            KEY_RIGHT_WL,
            KEY_PGUP_WL,
            KEY_PGDN_WL,
        ] {
            assert_eq!(key_char(k), None, "code {k} is both a key and a letter");
        }
    }

    #[test]
    fn the_letters_and_digits_come_out_in_the_right_order() {
        // Each range is indexed, so an off-by-one shifts a whole row of the
        // keyboard: typing "firefox" would filter on something else entirely.
        assert_eq!(key_char(2), Some('1'));
        assert_eq!(key_char(10), Some('9'));
        assert_eq!(key_char(11), Some('0'));
        let digits: String = (2..=11).filter_map(key_char).collect();
        assert_eq!(digits, "1234567890");
        let top: String = (16..=25).filter_map(key_char).collect();
        assert_eq!(top, "qwertyuiop");
        let home: String = (30..=38).filter_map(key_char).collect();
        assert_eq!(home, "asdfghjkl");
        let bottom: String = (44..=50).filter_map(key_char).collect();
        assert_eq!(bottom, "zxcvbnm");
        // And "firefox" really is typeable, letter by letter.
        let firefox: Vec<char> = "firefox".chars().collect();
        let codes = [33u32, 23, 19, 18, 33, 24, 45];
        let typed: Vec<char> = codes.iter().filter_map(|c| key_char(*c)).collect();
        assert_eq!(typed, firefox);
    }

    #[test]
    fn a_hyphen_is_typeable_on_both_layouts() {
        // The regression this mapping exists for: on a Spanish keyboard the key
        // printed `-` is KEY_SLASH, and it used to insert a `/`. Half the
        // application names on the image have a hyphen in them.
        assert_eq!(key_char(12), Some('-'), "the US hyphen key");
        assert_eq!(key_char(53), Some('-'), "the ES hyphen key");
        assert_eq!(key_char(51), Some(','));
        assert_eq!(key_char(52), Some('.'));
        assert_eq!(key_char(57), Some(' '));
        // `gnome-terminal` is typeable end to end.
        let codes = [34u32, 49, 24, 50, 18, 12, 20, 18, 19, 50, 23, 49, 30, 38];
        let typed: String = codes.iter().filter_map(|c| key_char(*c)).collect();
        assert_eq!(typed, "gnome-terminal");
    }

    #[test]
    fn nothing_outside_the_table_produces_a_character() {
        // Modifiers especially: a Shift that typed something would put a stray
        // character in the filter every time someone reached for a capital.
        for k in [
            0,
            13,
            26,
            27,
            29,
            39,
            40,
            41,
            42,
            43,
            54,
            55,
            56,
            58,
            59,
            87,
            97,
            100,
            125,
            126,
            1000,
            u32::MAX,
        ] {
            assert_eq!(key_char(k), None, "code {k} typed a character");
        }
        // And nothing in the whole range panics or returns a control character.
        for k in 0..=1024u32 {
            if let Some(c) = key_char(k) {
                assert!(!c.is_control(), "code {k} gave a control character");
                assert!(c.is_ascii(), "code {k} gave a non-ASCII character");
            }
        }
    }

    #[test]
    fn one_wheel_notch_is_the_surface_local_unit_wayland_sends() {
        // 15.0 is one detent of a classic wheel in wl_pointer.axis units; a
        // wrong value here makes the result lists scroll at the wrong speed in
        // BOTH clients at once.
        assert_eq!(WHEEL_NOTCH, 15.0);
        // A zero notch divides by zero in `scroll_top`; checked at compile time
        // so the build stops rather than a test.
        const { assert!(WHEEL_NOTCH > 0.0) };
    }
}
