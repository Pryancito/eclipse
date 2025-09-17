//! Driver USB para teclado
//! 
//! Implementa soporte completo para teclados USB con funcionalidades avanzadas.

use crate::drivers::framebuffer::{FramebufferDriver, Color};
use crate::syslog;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;

/// Códigos de teclas USB (HID Usage Tables)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsbKeyCode {
    // Teclas de función
    F1 = 0x3A, F2 = 0x3B, F3 = 0x3C, F4 = 0x3D,
    F5 = 0x3E, F6 = 0x3F, F7 = 0x40, F8 = 0x41,
    F9 = 0x42, F10 = 0x43, F11 = 0x44, F12 = 0x45,

    // Teclas de letras
    A = 0x04, B = 0x05, C = 0x06, D = 0x07, E = 0x08,
    F = 0x09, G = 0x0A, H = 0x0B, I = 0x0C, J = 0x0D,
    K = 0x0E, L = 0x0F, M = 0x10, N = 0x11, O = 0x12,
    P = 0x13, Q = 0x14, R = 0x15, S = 0x16, T = 0x17,
    U = 0x18, V = 0x19, W = 0x1A, X = 0x1B, Y = 0x1C, Z = 0x1D,

    // Números
    Num1 = 0x1E, Num2 = 0x1F, Num3 = 0x20, Num4 = 0x21, Num5 = 0x22,
    Num6 = 0x23, Num7 = 0x24, Num8 = 0x25, Num9 = 0x26, Num0 = 0x27,

    // Teclas especiales
    Enter = 0x28,
    Escape = 0x29,
    Backspace = 0x2A,
    Tab = 0x2B,
    Space = 0x2C,
    Minus = 0x2D,
    Equal = 0x2E,
    LeftBracket = 0x2F,
    RightBracket = 0x30,
    Backslash = 0x31,
    Semicolon = 0x33,
    Quote = 0x34,
    Grave = 0x35,
    Comma = 0x36,
    Period = 0x37,
    Slash = 0x38,

    // Teclas de control
    CapsLock = 0x39,
    LeftShift = 0xE1,
    RightShift = 0xE5,
    LeftCtrl = 0xE0,
    RightCtrl = 0xE4,
    LeftAlt = 0xE2,
    RightAlt = 0xE6,
    LeftMeta = 0xE3,
    RightMeta = 0xE7,

    // Teclas de navegación
    Up = 0x52,
    Down = 0x51,
    Left = 0x50,
    Right = 0x4F,
    Home = 0x4A,
    End = 0x4D,
    PageUp = 0x4B,
    PageDown = 0x4E,
    Insert = 0x49,
    Delete = 0x4C,

    // Teclado numérico
    NumLock = 0x53,
    NumDivide = 0x54,
    NumMultiply = 0x55,
    NumSubtract = 0x56,
    NumAdd = 0x57,
    NumEnter = 0x58,
    NumDecimal = 0x63,

    // Teclas de sistema
    PrintScreen = 0x46,
    ScrollLock = 0x47,
    Pause = 0x48,
    Menu = 0x65,

    // Teclas multimedia
    VolumeUp = 0x80,
    VolumeDown = 0x81,
    Mute = 0x7F,
    PlayPause = 0xCD,
    Stop = 0xB7,
    Previous = 0xB6,
    Next = 0xB5,

    Unknown = 0x00,
}

/// Estado de las teclas modificadoras
#[derive(Debug, Clone, Copy)]
pub struct ModifierState {
    pub left_shift: bool,
    pub right_shift: bool,
    pub left_ctrl: bool,
    pub right_ctrl: bool,
    pub left_alt: bool,
    pub right_alt: bool,
    pub left_meta: bool,
    pub right_meta: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
    pub scroll_lock: bool,
}

impl Default for ModifierState {
    fn default() -> Self {
        Self {
            left_shift: false,
            right_shift: false,
            left_ctrl: false,
            right_ctrl: false,
            left_alt: false,
            right_alt: false,
            left_meta: false,
            right_meta: false,
            caps_lock: false,
            num_lock: false,
            scroll_lock: false,
        }
    }
}

/// Evento del teclado
#[derive(Debug, Clone)]
pub struct KeyboardEvent {
    pub key_code: UsbKeyCode,
    pub pressed: bool,
    pub modifiers: ModifierState,
    pub character: Option<char>,
    pub timestamp: u64,
}

/// Configuración del teclado
#[derive(Debug, Clone)]
pub struct KeyboardConfig {
    pub repeat_delay: u64,
    pub repeat_rate: u64,
    pub enable_caps_lock: bool,
    pub enable_num_lock: bool,
    pub enable_scroll_lock: bool,
    pub layout: KeyboardLayout,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardLayout {
    Qwerty,
    Azerty,
    Qwertz,
    Dvorak,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            repeat_delay: 500, // 500ms
            repeat_rate: 50,   // 50ms
            enable_caps_lock: true,
            enable_num_lock: true,
            enable_scroll_lock: true,
            layout: KeyboardLayout::Qwerty,
        }
    }
}

/// Driver USB para teclado
pub struct UsbKeyboardDriver {
    device_id: u32,
    config: KeyboardConfig,
    modifier_state: ModifierState,
    pressed_keys: BTreeMap<UsbKeyCode, u64>,
    event_queue: Vec<KeyboardEvent>,
    max_events: usize,
    is_initialized: bool,
    last_event_time: u64,
}

impl UsbKeyboardDriver {
    pub fn new(device_id: u32) -> Self {
        Self {
            device_id,
            config: KeyboardConfig::default(),
            modifier_state: ModifierState::default(),
            pressed_keys: BTreeMap::new(),
            event_queue: Vec::new(),
            max_events: 100,
            is_initialized: false,
            last_event_time: 0,
        }
    }

    /// Inicializar el driver del teclado
    pub fn initialize(&mut self) -> Result<(), String> {
        if self.is_initialized {
            return Err("El driver del teclado ya está inicializado".to_string());
        }

        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_KEYBOARD", &alloc::format!(
            "Inicializando driver USB para teclado (ID: {})",
            self.device_id
        ));

        // Simular inicialización del dispositivo USB
        self.is_initialized = true;

        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_KEYBOARD", "Driver USB para teclado inicializado correctamente");
        Ok(())
    }

    /// Procesar datos del teclado USB
    pub fn process_keyboard_data(&mut self, data: &[u8]) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del teclado no está inicializado".to_string());
        }

        if data.len() < 8 {
            return Err("Datos del teclado insuficientes".to_string());
        }

        // Parsear datos del teclado (formato estándar USB HID)
        let modifier_byte = data[0];
        let _reserved = data[1];
        let key_codes = &data[2..8];

        // Actualizar estado de modificadores
        self.update_modifier_state(modifier_byte);

        // Procesar teclas presionadas
        for &key_code in key_codes {
            if key_code != 0 {
                self.process_key_press(key_code);
            }
        }

        // Procesar teclas liberadas
        self.process_key_releases(&key_codes);

        Ok(())
    }

    /// Obtener estado de las teclas modificadoras
    pub fn get_modifier_state(&self) -> ModifierState {
        self.modifier_state
    }

    /// Verificar si una tecla está presionada
    pub fn is_key_pressed(&self, key_code: UsbKeyCode) -> bool {
        self.pressed_keys.contains_key(&key_code)
    }

    /// Obtener siguiente evento del teclado
    pub fn get_next_event(&mut self) -> Option<KeyboardEvent> {
        self.event_queue.pop()
    }

    /// Verificar si hay eventos pendientes
    pub fn has_events(&self) -> bool {
        !self.event_queue.is_empty()
    }

    /// Obtener número de eventos pendientes
    pub fn event_count(&self) -> usize {
        self.event_queue.len()
    }

    /// Limpiar eventos pendientes
    pub fn clear_events(&mut self) {
        self.event_queue.clear();
    }

    /// Actualizar configuración del teclado
    pub fn update_config(&mut self, new_config: KeyboardConfig) {
        self.config = new_config;
    }

    /// Obtener configuración actual
    pub fn get_config(&self) -> &KeyboardConfig {
        &self.config
    }

    /// Obtener ID del dispositivo
    pub fn get_device_id(&self) -> u32 {
        self.device_id
    }

    /// Verificar si el driver está inicializado
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }

    /// Obtener estadísticas del teclado
    pub fn get_stats(&self) -> String {
        alloc::format!(
            "Teclado USB (ID: {}) - Teclas presionadas: {}, Eventos: {}, Modificadores: Shift:{} Ctrl:{} Alt:{}",
            self.device_id,
            self.pressed_keys.len(),
            self.event_count(),
            self.modifier_state.left_shift || self.modifier_state.right_shift,
            self.modifier_state.left_ctrl || self.modifier_state.right_ctrl,
            self.modifier_state.left_alt || self.modifier_state.right_alt
        )
    }

    /// Simular presión de tecla (para testing)
    pub fn simulate_key_press(&mut self, key_code: UsbKeyCode) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del teclado no está inicializado".to_string());
        }

        self.process_key_press(key_code as u8);
        Ok(())
    }

    /// Simular liberación de tecla (para testing)
    pub fn simulate_key_release(&mut self, key_code: UsbKeyCode) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del teclado no está inicializado".to_string());
        }

        self.process_key_release(key_code);
        Ok(())
    }

    /// Simular secuencia de teclas (para testing)
    pub fn simulate_key_sequence(&mut self, keys: &[UsbKeyCode]) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del teclado no está inicializado".to_string());
        }

        for &key in keys {
            self.simulate_key_press(key)?;
            self.simulate_key_release(key)?;
        }

        Ok(())
    }

    // Métodos auxiliares privados

    fn update_modifier_state(&mut self, modifier_byte: u8) {
        self.modifier_state.left_ctrl = (modifier_byte & 0x01) != 0;
        self.modifier_state.left_shift = (modifier_byte & 0x02) != 0;
        self.modifier_state.left_alt = (modifier_byte & 0x04) != 0;
        self.modifier_state.left_meta = (modifier_byte & 0x08) != 0;
        self.modifier_state.right_ctrl = (modifier_byte & 0x10) != 0;
        self.modifier_state.right_shift = (modifier_byte & 0x20) != 0;
        self.modifier_state.right_alt = (modifier_byte & 0x40) != 0;
        self.modifier_state.right_meta = (modifier_byte & 0x80) != 0;
    }

    fn process_key_press(&mut self, key_code: u8) {
        let usb_key = self.key_code_to_enum(key_code);
        let timestamp = self.get_timestamp();

        if !self.pressed_keys.contains_key(&usb_key) {
            self.pressed_keys.insert(usb_key, timestamp);

            let character = self.get_character_for_key(usb_key);
            let event = KeyboardEvent {
                key_code: usb_key,
                pressed: true,
                modifiers: self.modifier_state,
                character,
                timestamp,
            };

            self.add_event(event);
        }
    }

    fn process_key_releases(&mut self, current_keys: &[u8]) {
        let current_key_set: alloc::collections::BTreeSet<u8> = current_keys.iter().cloned().collect();
        let mut keys_to_release = Vec::new();

        for (&key_code, &timestamp) in &self.pressed_keys {
            let key_byte = key_code as u8;
            if !current_key_set.contains(&key_byte) {
                keys_to_release.push(key_code);
            }
        }

        for key_code in keys_to_release {
            self.process_key_release(key_code);
        }
    }

    fn process_key_release(&mut self, key_code: UsbKeyCode) {
        if self.pressed_keys.remove(&key_code).is_some() {
            let character = self.get_character_for_key(key_code);
            let event = KeyboardEvent {
                key_code,
                pressed: false,
                modifiers: self.modifier_state,
                character,
                timestamp: self.get_timestamp(),
            };

            self.add_event(event);
        }
    }

    fn key_code_to_enum(&self, key_code: u8) -> UsbKeyCode {
        match key_code {
            0x04 => UsbKeyCode::A,
            0x05 => UsbKeyCode::B,
            0x06 => UsbKeyCode::C,
            0x07 => UsbKeyCode::D,
            0x08 => UsbKeyCode::E,
            0x09 => UsbKeyCode::F,
            0x0A => UsbKeyCode::G,
            0x0B => UsbKeyCode::H,
            0x0C => UsbKeyCode::I,
            0x0D => UsbKeyCode::J,
            0x0E => UsbKeyCode::K,
            0x0F => UsbKeyCode::L,
            0x10 => UsbKeyCode::M,
            0x11 => UsbKeyCode::N,
            0x12 => UsbKeyCode::O,
            0x13 => UsbKeyCode::P,
            0x14 => UsbKeyCode::Q,
            0x15 => UsbKeyCode::R,
            0x16 => UsbKeyCode::S,
            0x17 => UsbKeyCode::T,
            0x18 => UsbKeyCode::U,
            0x19 => UsbKeyCode::V,
            0x1A => UsbKeyCode::W,
            0x1B => UsbKeyCode::X,
            0x1C => UsbKeyCode::Y,
            0x1D => UsbKeyCode::Z,
            0x1E => UsbKeyCode::Num1,
            0x1F => UsbKeyCode::Num2,
            0x20 => UsbKeyCode::Num3,
            0x21 => UsbKeyCode::Num4,
            0x22 => UsbKeyCode::Num5,
            0x23 => UsbKeyCode::Num6,
            0x24 => UsbKeyCode::Num7,
            0x25 => UsbKeyCode::Num8,
            0x26 => UsbKeyCode::Num9,
            0x27 => UsbKeyCode::Num0,
            0x28 => UsbKeyCode::Enter,
            0x29 => UsbKeyCode::Escape,
            0x2A => UsbKeyCode::Backspace,
            0x2B => UsbKeyCode::Tab,
            0x2C => UsbKeyCode::Space,
            0x2D => UsbKeyCode::Minus,
            0x2E => UsbKeyCode::Equal,
            0x2F => UsbKeyCode::LeftBracket,
            0x30 => UsbKeyCode::RightBracket,
            0x31 => UsbKeyCode::Backslash,
            0x33 => UsbKeyCode::Semicolon,
            0x34 => UsbKeyCode::Quote,
            0x35 => UsbKeyCode::Grave,
            0x36 => UsbKeyCode::Comma,
            0x37 => UsbKeyCode::Period,
            0x38 => UsbKeyCode::Slash,
            0x39 => UsbKeyCode::CapsLock,
            0x3A => UsbKeyCode::F1,
            0x3B => UsbKeyCode::F2,
            0x3C => UsbKeyCode::F3,
            0x3D => UsbKeyCode::F4,
            0x3E => UsbKeyCode::F5,
            0x3F => UsbKeyCode::F6,
            0x40 => UsbKeyCode::F7,
            0x41 => UsbKeyCode::F8,
            0x42 => UsbKeyCode::F9,
            0x43 => UsbKeyCode::F10,
            0x44 => UsbKeyCode::F11,
            0x45 => UsbKeyCode::F12,
            0x46 => UsbKeyCode::PrintScreen,
            0x47 => UsbKeyCode::ScrollLock,
            0x48 => UsbKeyCode::Pause,
            0x49 => UsbKeyCode::Insert,
            0x4A => UsbKeyCode::Home,
            0x4B => UsbKeyCode::PageUp,
            0x4C => UsbKeyCode::Delete,
            0x4D => UsbKeyCode::End,
            0x4E => UsbKeyCode::PageDown,
            0x4F => UsbKeyCode::Right,
            0x50 => UsbKeyCode::Left,
            0x51 => UsbKeyCode::Down,
            0x52 => UsbKeyCode::Up,
            0x53 => UsbKeyCode::NumLock,
            0x54 => UsbKeyCode::NumDivide,
            0x55 => UsbKeyCode::NumMultiply,
            0x56 => UsbKeyCode::NumSubtract,
            0x57 => UsbKeyCode::NumAdd,
            0x58 => UsbKeyCode::NumEnter,
            0x59 => UsbKeyCode::Num1,
            0x5A => UsbKeyCode::Num2,
            0x5B => UsbKeyCode::Num3,
            0x5C => UsbKeyCode::Num4,
            0x5D => UsbKeyCode::Num5,
            0x5E => UsbKeyCode::Num6,
            0x5F => UsbKeyCode::Num7,
            0x60 => UsbKeyCode::Num8,
            0x61 => UsbKeyCode::Num9,
            0x62 => UsbKeyCode::Num0,
            0x63 => UsbKeyCode::NumDecimal,
            0x65 => UsbKeyCode::Menu,
            0x7F => UsbKeyCode::Mute,
            0x80 => UsbKeyCode::VolumeUp,
            0x81 => UsbKeyCode::VolumeDown,
            0xB5 => UsbKeyCode::Next,
            0xB6 => UsbKeyCode::Previous,
            0xB7 => UsbKeyCode::Stop,
            0xCD => UsbKeyCode::PlayPause,
            0xE0 => UsbKeyCode::LeftCtrl,
            0xE1 => UsbKeyCode::LeftShift,
            0xE2 => UsbKeyCode::LeftAlt,
            0xE3 => UsbKeyCode::LeftMeta,
            0xE4 => UsbKeyCode::RightCtrl,
            0xE5 => UsbKeyCode::RightShift,
            0xE6 => UsbKeyCode::RightAlt,
            0xE7 => UsbKeyCode::RightMeta,
            _ => UsbKeyCode::Unknown,
        }
    }

    fn get_character_for_key(&self, key_code: UsbKeyCode) -> Option<char> {
        let shift_pressed = self.modifier_state.left_shift || self.modifier_state.right_shift;
        let caps_lock = self.modifier_state.caps_lock;

        match key_code {
            UsbKeyCode::A => Some(if shift_pressed ^ caps_lock { 'A' } else { 'a' }),
            UsbKeyCode::B => Some(if shift_pressed ^ caps_lock { 'B' } else { 'b' }),
            UsbKeyCode::C => Some(if shift_pressed ^ caps_lock { 'C' } else { 'c' }),
            UsbKeyCode::D => Some(if shift_pressed ^ caps_lock { 'D' } else { 'd' }),
            UsbKeyCode::E => Some(if shift_pressed ^ caps_lock { 'E' } else { 'e' }),
            UsbKeyCode::F => Some(if shift_pressed ^ caps_lock { 'F' } else { 'f' }),
            UsbKeyCode::G => Some(if shift_pressed ^ caps_lock { 'G' } else { 'g' }),
            UsbKeyCode::H => Some(if shift_pressed ^ caps_lock { 'H' } else { 'h' }),
            UsbKeyCode::I => Some(if shift_pressed ^ caps_lock { 'I' } else { 'i' }),
            UsbKeyCode::J => Some(if shift_pressed ^ caps_lock { 'J' } else { 'j' }),
            UsbKeyCode::K => Some(if shift_pressed ^ caps_lock { 'K' } else { 'k' }),
            UsbKeyCode::L => Some(if shift_pressed ^ caps_lock { 'L' } else { 'l' }),
            UsbKeyCode::M => Some(if shift_pressed ^ caps_lock { 'M' } else { 'm' }),
            UsbKeyCode::N => Some(if shift_pressed ^ caps_lock { 'N' } else { 'n' }),
            UsbKeyCode::O => Some(if shift_pressed ^ caps_lock { 'O' } else { 'o' }),
            UsbKeyCode::P => Some(if shift_pressed ^ caps_lock { 'P' } else { 'p' }),
            UsbKeyCode::Q => Some(if shift_pressed ^ caps_lock { 'Q' } else { 'q' }),
            UsbKeyCode::R => Some(if shift_pressed ^ caps_lock { 'R' } else { 'r' }),
            UsbKeyCode::S => Some(if shift_pressed ^ caps_lock { 'S' } else { 's' }),
            UsbKeyCode::T => Some(if shift_pressed ^ caps_lock { 'T' } else { 't' }),
            UsbKeyCode::U => Some(if shift_pressed ^ caps_lock { 'U' } else { 'u' }),
            UsbKeyCode::V => Some(if shift_pressed ^ caps_lock { 'V' } else { 'v' }),
            UsbKeyCode::W => Some(if shift_pressed ^ caps_lock { 'W' } else { 'w' }),
            UsbKeyCode::X => Some(if shift_pressed ^ caps_lock { 'X' } else { 'x' }),
            UsbKeyCode::Y => Some(if shift_pressed ^ caps_lock { 'Y' } else { 'y' }),
            UsbKeyCode::Z => Some(if shift_pressed ^ caps_lock { 'Z' } else { 'z' }),
            UsbKeyCode::Num1 => Some(if shift_pressed { '!' } else { '1' }),
            UsbKeyCode::Num2 => Some(if shift_pressed { '@' } else { '2' }),
            UsbKeyCode::Num3 => Some(if shift_pressed { '#' } else { '3' }),
            UsbKeyCode::Num4 => Some(if shift_pressed { '$' } else { '4' }),
            UsbKeyCode::Num5 => Some(if shift_pressed { '%' } else { '5' }),
            UsbKeyCode::Num6 => Some(if shift_pressed { '^' } else { '6' }),
            UsbKeyCode::Num7 => Some(if shift_pressed { '&' } else { '7' }),
            UsbKeyCode::Num8 => Some(if shift_pressed { '*' } else { '8' }),
            UsbKeyCode::Num9 => Some(if shift_pressed { '(' } else { '9' }),
            UsbKeyCode::Num0 => Some(if shift_pressed { ')' } else { '0' }),
            UsbKeyCode::Space => Some(' '),
            UsbKeyCode::Enter => Some('\n'),
            UsbKeyCode::Tab => Some('\t'),
            UsbKeyCode::Backspace => Some('\x08'),
            UsbKeyCode::Escape => Some('\x1B'),
            UsbKeyCode::Minus => Some(if shift_pressed { '_' } else { '-' }),
            UsbKeyCode::Equal => Some(if shift_pressed { '+' } else { '=' }),
            UsbKeyCode::LeftBracket => Some(if shift_pressed { '{' } else { '[' }),
            UsbKeyCode::RightBracket => Some(if shift_pressed { '}' } else { ']' }),
            UsbKeyCode::Backslash => Some(if shift_pressed { '|' } else { '\\' }),
            UsbKeyCode::Semicolon => Some(if shift_pressed { ':' } else { ';' }),
            UsbKeyCode::Quote => Some(if shift_pressed { '"' } else { '\'' }),
            UsbKeyCode::Grave => Some(if shift_pressed { '~' } else { '`' }),
            UsbKeyCode::Comma => Some(if shift_pressed { '<' } else { ',' }),
            UsbKeyCode::Period => Some(if shift_pressed { '>' } else { '.' }),
            UsbKeyCode::Slash => Some(if shift_pressed { '?' } else { '/' }),
            _ => None,
        }
    }

    fn add_event(&mut self, event: KeyboardEvent) {
        if self.event_queue.len() >= self.max_events {
            self.event_queue.remove(0); // Remover evento más antiguo
        }
        self.event_queue.push(event);
    }

    fn get_timestamp(&mut self) -> u64 {
        self.last_event_time += 1;
        self.last_event_time
    }
}