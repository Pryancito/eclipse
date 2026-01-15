//! Driver de teclado para Eclipse OS
//!
//! Driver PS/2 real usando Port I/O y decodificación de scancodes (Set 1).

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType},
    manager::{Driver, DriverInfo, DriverResult},
};

/// Wrapper para Port I/O usando ensamblador inline
#[derive(Debug, Clone, Copy)]
pub struct Port {
    port: u16,
}

impl Port {
    pub const fn new(port: u16) -> Self {
        Self { port }
    }

    /// Leer un byte del puerto
    pub unsafe fn read(&self) -> u8 {
        let value: u8;
        core::arch::asm!("in al, dx", out("al") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        value
    }

    /// Escribir un byte al puerto
    pub unsafe fn write(&self, value: u8) {
        core::arch::asm!("out dx, al", in("dx") self.port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Puertos del controlador PS/2
const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;

/// Códigos de tecla
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    // Letras
    A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    // Números
    Key0, Key1, Key2, Key3, Key4, Key5, Key6, Key7, Key8, Key9,
    // Teclas especiales
    Space, Enter, Escape, Backspace, Tab, CapsLock, Shift, Ctrl, Alt, 
    Left, Right, Up, Down, Home, End, PageUp, PageDown, Delete, Insert,
    // Teclas de Función
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    // Símbolos
    Semicolon, Quote, Backslash, Comma, Period, Slash, Minus, Equal, Apostrophe,
    LeftBracket, RightBracket, Grave, PrintScreen, ScrollLock, Pause,
    // Teclas del teclado numérico
    Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9,
    NumLock, NumpadDivide, NumpadMultiply, NumpadSubtract, NumpadAdd, NumpadEnter, NumpadDecimal,
    // Modificadores específicos (para compatibilidad)
    LeftShift, RightShift, LeftCtrl, RightCtrl, LeftAlt, RightAlt, LeftMeta, RightMeta, Menu,
    // Otros
    Unknown, None, Wheel
}

impl KeyCode {
    pub fn to_char(&self, shift: bool) -> Option<char> {
        match self {
            KeyCode::A => Some(if shift { 'A' } else { 'a' }),
            KeyCode::B => Some(if shift { 'B' } else { 'b' }),
            KeyCode::C => Some(if shift { 'C' } else { 'c' }),
            KeyCode::D => Some(if shift { 'D' } else { 'd' }),
            KeyCode::E => Some(if shift { 'E' } else { 'e' }),
            KeyCode::F => Some(if shift { 'F' } else { 'f' }),
            KeyCode::G => Some(if shift { 'G' } else { 'g' }),
            KeyCode::H => Some(if shift { 'H' } else { 'h' }),
            KeyCode::I => Some(if shift { 'I' } else { 'i' }),
            KeyCode::J => Some(if shift { 'J' } else { 'j' }),
            KeyCode::K => Some(if shift { 'K' } else { 'k' }),
            KeyCode::L => Some(if shift { 'L' } else { 'l' }),
            KeyCode::M => Some(if shift { 'M' } else { 'm' }),
            KeyCode::N => Some(if shift { 'N' } else { 'n' }),
            KeyCode::O => Some(if shift { 'O' } else { 'o' }),
            KeyCode::P => Some(if shift { 'P' } else { 'p' }),
            KeyCode::Q => Some(if shift { 'Q' } else { 'q' }),
            KeyCode::R => Some(if shift { 'R' } else { 'r' }),
            KeyCode::S => Some(if shift { 'S' } else { 's' }),
            KeyCode::T => Some(if shift { 'T' } else { 't' }),
            KeyCode::U => Some(if shift { 'U' } else { 'u' }),
            KeyCode::V => Some(if shift { 'V' } else { 'v' }),
            KeyCode::W => Some(if shift { 'W' } else { 'w' }),
            KeyCode::X => Some(if shift { 'X' } else { 'x' }),
            KeyCode::Y => Some(if shift { 'Y' } else { 'y' }),
            KeyCode::Z => Some(if shift { 'Z' } else { 'z' }),
            
            KeyCode::Key0 => Some(if shift { ')' } else { '0' }),
            KeyCode::Key1 => Some(if shift { '!' } else { '1' }),
            KeyCode::Key2 => Some(if shift { '@' } else { '2' }),
            KeyCode::Key3 => Some(if shift { '#' } else { '3' }),
            KeyCode::Key4 => Some(if shift { '$' } else { '4' }),
            KeyCode::Key5 => Some(if shift { '%' } else { '5' }),
            KeyCode::Key6 => Some(if shift { '^' } else { '6' }),
            KeyCode::Key7 => Some(if shift { '&' } else { '7' }),
            KeyCode::Key8 => Some(if shift { '*' } else { '8' }),
            KeyCode::Key9 => Some(if shift { '(' } else { '9' }),

            KeyCode::Space => Some(' '),
            KeyCode::Enter => Some('\n'),
            KeyCode::Tab => Some('\t'),
            KeyCode::Minus => Some(if shift { '_' } else { '-' }),
            KeyCode::Equal => Some(if shift { '+' } else { '=' }),
            KeyCode::Comma => Some(if shift { '<' } else { ',' }),
            KeyCode::Period => Some(if shift { '>' } else { '.' }),
            KeyCode::Slash => Some(if shift { '?' } else { '/' }),
            KeyCode::Semicolon => Some(if shift { ':' } else { ';' }),
            KeyCode::Quote => Some(if shift { '"' } else { '\'' }),
            KeyCode::Apostrophe => Some(if shift { '~' } else { '`' }), // Approximation for now
            KeyCode::Backslash => Some(if shift { '|' } else { '\\' }),
            
            _ => None,
        }
    }
}

/// Estado de una tecla
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyState {
    Pressed,
    Released,
}

/// Evento de teclado
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyEvent {
    pub key: KeyCode,
    pub state: KeyState,
    pub modifiers: u8,
}

/// Driver de teclado base
pub trait KeyboardDriver {
    fn read_key(&mut self) -> Option<KeyEvent>;
    fn read_char(&mut self) -> Option<char>;
    fn is_key_pressed(&self, key: KeyCode) -> bool;
    fn get_modifiers(&self) -> u8;
    fn clear_buffer(&mut self);
    fn has_key_events(&self) -> bool;
}

/// Driver PS/2 básico
pub struct BasicKeyboardDriver {
    pub info: DriverInfo,
    data_port: Port,
    status_port: Port,
    shift_pressed: bool,
    last_scancode: u8, // Para manejar códigos extendidos en el futuro
}

impl BasicKeyboardDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("ps2_keyboard");
        info.device_type = DeviceType::Input;
        info.version = 1;

        Self {
            info,
            data_port: Port::new(DATA_PORT),
            status_port: Port::new(STATUS_PORT),
            shift_pressed: false,
            last_scancode: 0,
        }
    }

    /// Verificar si hay datos disponibles para leer
    fn has_data(&self) -> bool {
        unsafe { (self.status_port.read() & 1) != 0 }
    }

    /// Mapeo simple de Scancode Set 1 (US QWERTY) a KeyCode
    fn scancode_to_keycode(&self, scancode: u8) -> Option<KeyCode> {
        let key = match scancode {
            0x02 => KeyCode::Key1, 0x03 => KeyCode::Key2, 0x04 => KeyCode::Key3, 0x05 => KeyCode::Key4,
            0x06 => KeyCode::Key5, 0x07 => KeyCode::Key6, 0x08 => KeyCode::Key7, 0x09 => KeyCode::Key8,
            0x0A => KeyCode::Key9, 0x0B => KeyCode::Key0, 0x0C => KeyCode::Minus, 0x0D => KeyCode::Equal,
            0x0E => KeyCode::Backspace, 0x0F => KeyCode::Tab,
            
            0x10 => KeyCode::Q, 0x11 => KeyCode::W, 0x12 => KeyCode::E, 0x13 => KeyCode::R,
            0x14 => KeyCode::T, 0x15 => KeyCode::Y, 0x16 => KeyCode::U, 0x17 => KeyCode::I,
            0x18 => KeyCode::O, 0x19 => KeyCode::P, 0x1A => KeyCode::LeftBracket, 0x1B => KeyCode::RightBracket,
            0x1C => KeyCode::Enter, 0x1D => KeyCode::Ctrl,
            
            0x1E => KeyCode::A, 0x1F => KeyCode::S, 0x20 => KeyCode::D, 0x21 => KeyCode::F,
            0x22 => KeyCode::G, 0x23 => KeyCode::H, 0x24 => KeyCode::J, 0x25 => KeyCode::K,
            0x26 => KeyCode::L, 0x27 => KeyCode::Semicolon, 0x28 => KeyCode::Quote, 0x29 => KeyCode::Grave,
            0x2A => KeyCode::Shift, 0x2B => KeyCode::Backslash,
            
            0x2C => KeyCode::Z, 0x2D => KeyCode::X, 0x2E => KeyCode::C, 0x2F => KeyCode::V,
            0x30 => KeyCode::B, 0x31 => KeyCode::N, 0x32 => KeyCode::M, 0x33 => KeyCode::Comma,
            0x34 => KeyCode::Period, 0x35 => KeyCode::Slash, 0x36 => KeyCode::Shift,
            
            0x39 => KeyCode::Space,
            0x38 => KeyCode::Alt,
            0x3A => KeyCode::CapsLock,
            0x01 => KeyCode::Escape,
            
            0x4B => KeyCode::Left, 0x4D => KeyCode::Right, 0x48 => KeyCode::Up, 0x50 => KeyCode::Down,
            
            _ => return None,
        };
        Some(key)
    }
}

impl Driver for BasicKeyboardDriver {
    fn get_info(&self) -> &DriverInfo { &self.info }
    fn initialize(&mut self) -> DriverResult<()> { self.info.is_loaded = true; Ok(()) }
    fn cleanup(&mut self) -> DriverResult<()> { self.info.is_loaded = false; Ok(()) }
    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool { device_info.device_type == DeviceType::Input }
    fn attach_device(&mut self, _device: &mut Device) -> DriverResult<()> { Ok(()) }
    fn detach_device(&mut self, _device_id: u32) -> DriverResult<()> { Ok(()) }
    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> { Ok(()) }
}

impl KeyboardDriver for BasicKeyboardDriver {
    fn read_key(&mut self) -> Option<KeyEvent> {
        if !self.has_data() {
            return None;
        }

        let scancode = unsafe { self.data_port.read() };
        
        // Ignorar códigos extendidos por ahora (0xE0)
        if scancode == 0xE0 {
            return None;
        }

        let pressed = (scancode & 0x80) == 0;
        let actual_code = scancode & 0x7F;

        if let Some(keycode) = self.scancode_to_keycode(actual_code) {
             // Actualizar estado de Shift
             if keycode == KeyCode::Shift {
                self.shift_pressed = pressed;
            }

            if pressed {
                return Some(KeyEvent {
                    key: keycode,
                    state: KeyState::Pressed,
                    modifiers: if self.shift_pressed { 1 } else { 0 },
                });
            }
        }
        
        None
    }

    /// Helper bloqueante (polling) para obtener un caracter
    fn read_char(&mut self) -> Option<char> {
        // Bucle de polling simple
        // NOTA: En un sistema real esto bloquearía el kernel, pero como
        // no tenemos multitarea preemptiva completa ni interrupciones configuradas,
        // esto es lo correcto para la shell ahora.
        for _ in 0..100000 { 
            if let Some(event) = self.read_key() {
                if event.key != KeyCode::Shift && event.key != KeyCode::Ctrl && event.key != KeyCode::Alt {
                     return event.key.to_char(self.shift_pressed);
                }
            }
            // Pequeña pausa para no quemar CPU a lo loco (spin loop hint)
            core::hint::spin_loop(); 
        }
        None
    }

    fn is_key_pressed(&self, _key: KeyCode) -> bool {
        // No soportado en el driver básico sin buffer de estado
        false
    }
    
    fn get_modifiers(&self) -> u8 {
        if self.shift_pressed { 2 } else { 0 } // 2 = Shift bit
    }
    
    fn clear_buffer(&mut self) {
        while self.has_data() {
            unsafe { self.data_port.read() };
        }
    }
    
    fn has_key_events(&self) -> bool {
        self.has_data()
    }
}
