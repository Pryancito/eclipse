//! Teclado español (España), ISO 105, para códigos físicos PS/2 Set 1 / evdev.
//!
//! Basado en la disposición habitual `es` (QWERTY + ñ/ç/º/¡/¿/·). Las teclas no
//! listadas se delegan a [`pc_keyboard::layouts::Us104Key`].

use pc_keyboard::layouts::Us104Key;
use pc_keyboard::{DecodedKey, HandleControl, KeyCode, KeyboardLayout, Modifiers};

#[derive(Debug, Clone, Copy)]
pub struct Es105Key;

impl KeyboardLayout for Es105Key {
    fn map_keycode(
        &self,
        keycode: KeyCode,
        modifiers: &Modifiers,
        handle_ctrl: HandleControl,
    ) -> DecodedKey {
        match keycode {
            KeyCode::Oem8 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('\u{00AA}') // ª
                } else {
                    DecodedKey::Unicode('\u{00BA}') // º
                }
            }
            KeyCode::Key2 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('"')
                } else {
                    DecodedKey::Unicode('2')
                }
            }
            KeyCode::Key3 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('\u{00B7}') // ·
                } else {
                    DecodedKey::Unicode('3')
                }
            }
            KeyCode::Key4 | KeyCode::Key5 => {
                Us104Key.map_keycode(keycode, modifiers, handle_ctrl)
            }
            KeyCode::Key6 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('&')
                } else {
                    DecodedKey::Unicode('6')
                }
            }
            KeyCode::Key7 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('/')
                } else {
                    DecodedKey::Unicode('7')
                }
            }
            KeyCode::Key8 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('(')
                } else {
                    DecodedKey::Unicode('8')
                }
            }
            KeyCode::Key9 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode(')')
                } else {
                    DecodedKey::Unicode('9')
                }
            }
            KeyCode::Key0 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('=')
                } else {
                    DecodedKey::Unicode('0')
                }
            }
            KeyCode::OemMinus => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('?')
                } else {
                    DecodedKey::Unicode('\'')
                }
            }
            KeyCode::OemPlus => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('\u{00BF}') // ¿
                } else {
                    DecodedKey::Unicode('\u{00A1}') // ¡
                }
            }
            KeyCode::Q => {
                let map_to_unicode = handle_ctrl == HandleControl::MapLettersToUnicode;
                if map_to_unicode && modifiers.is_ctrl() {
                    DecodedKey::Unicode('\u{0011}')
                } else if modifiers.is_altgr() {
                    DecodedKey::Unicode('@')
                } else if modifiers.is_caps() {
                    DecodedKey::Unicode('Q')
                } else {
                    DecodedKey::Unicode('q')
                }
            }
            KeyCode::E => {
                let map_to_unicode = handle_ctrl == HandleControl::MapLettersToUnicode;
                if map_to_unicode && modifiers.is_ctrl() {
                    DecodedKey::Unicode('\u{0005}')
                } else if modifiers.is_altgr() {
                    DecodedKey::Unicode('\u{20AC}') // €
                } else if modifiers.is_caps() {
                    DecodedKey::Unicode('E')
                } else {
                    DecodedKey::Unicode('e')
                }
            }
            KeyCode::C => {
                let map_to_unicode = handle_ctrl == HandleControl::MapLettersToUnicode;
                if map_to_unicode && modifiers.is_ctrl() {
                    DecodedKey::Unicode('\u{0003}')
                } else if modifiers.is_altgr() {
                    DecodedKey::Unicode(if modifiers.is_shifted() {
                        'Ç'
                    } else {
                        'ç'
                    })
                } else if modifiers.is_caps() {
                    DecodedKey::Unicode('C')
                } else {
                    DecodedKey::Unicode('c')
                }
            }
            KeyCode::Oem1 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('Ñ')
                } else {
                    DecodedKey::Unicode('ñ')
                }
            }
            KeyCode::Oem3 => {
                if modifiers.is_altgr() {
                    if modifiers.is_shifted() {
                        DecodedKey::Unicode('{')
                    } else {
                        DecodedKey::Unicode('[')
                    }
                } else if modifiers.is_shifted() {
                    DecodedKey::Unicode('\u{00A8}') // ¨
                } else {
                    DecodedKey::Unicode('\u{00B4}') // ´
                }
            }
            KeyCode::Oem4 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('*')
                } else {
                    DecodedKey::Unicode('+')
                }
            }
            KeyCode::Oem6 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('*')
                } else {
                    DecodedKey::Unicode('+')
                }
            }
            KeyCode::Oem7 => {
                if modifiers.is_altgr() {
                    DecodedKey::Unicode('\\')
                } else if modifiers.is_shifted() {
                    DecodedKey::Unicode('~')
                } else {
                    DecodedKey::Unicode('#')
                }
            }
            KeyCode::Oem5 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('>')
                } else if modifiers.is_altgr() {
                    DecodedKey::Unicode('|')
                } else {
                    DecodedKey::Unicode('<')
                }
            }
            KeyCode::OemComma => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode(';')
                } else {
                    DecodedKey::Unicode(',')
                }
            }
            KeyCode::OemPeriod => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode(':')
                } else {
                    DecodedKey::Unicode('.')
                }
            }
            KeyCode::Oem2 => {
                if modifiers.is_shifted() {
                    DecodedKey::Unicode('_')
                } else {
                    DecodedKey::Unicode('-')
                }
            }
            e => Us104Key.map_keycode(e, modifiers, handle_ctrl),
        }
    }
}
