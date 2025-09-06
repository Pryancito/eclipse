#![no_std]

use core::ptr;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::boxed::Box;

/// Driver de teclado USB para Eclipse OS
/// Implementa el protocolo HID (Human Interface Device) para teclados USB

/// Códigos de tecla USB estándar (HID Usage Page 0x07)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbKeyCode {
    // Teclas de función
    F1 = 0x3A, F2 = 0x3B, F3 = 0x3C, F4 = 0x3D,
    F5 = 0x3E, F6 = 0x3F, F7 = 0x40, F8 = 0x41,
    F9 = 0x42, F10 = 0x43, F11 = 0x44, F12 = 0x45,
    
    // Teclas de control
    LeftCtrl = 0xE0, RightCtrl = 0xE4,
    LeftShift = 0xE1, RightShift = 0xE5,
    LeftAlt = 0xE2, RightAlt = 0xE6,
    LeftGui = 0xE3, RightGui = 0xE7,
    
    // Teclas especiales
    Enter = 0x28, Escape = 0x29, Backspace = 0x2A,
    Tab = 0x2B, Space = 0x2C, CapsLock = 0x39,
    NumLock = 0x53, ScrollLock = 0x47,
    
    // Teclas de navegación
    Insert = 0x49, Home = 0x4A, PageUp = 0x4B,
    Delete = 0x4C, End = 0x4D, PageDown = 0x4E,
    Up = 0x52, Down = 0x51, Left = 0x50, Right = 0x4F,
    
    // Teclas numéricas
    Key0 = 0x27, Key1 = 0x1E, Key2 = 0x1F, Key3 = 0x20,
    Key4 = 0x21, Key5 = 0x22, Key6 = 0x23, Key7 = 0x24,
    Key8 = 0x25, Key9 = 0x26,
    
    // Teclas alfabéticas
    A = 0x04, B = 0x05, C = 0x06, D = 0x07, E = 0x08,
    F = 0x09, G = 0x0A, H = 0x0B, I = 0x0C, J = 0x0D,
    K = 0x0E, L = 0x0F, M = 0x10, N = 0x11, O = 0x12,
    P = 0x13, Q = 0x14, R = 0x15, S = 0x16, T = 0x17,
    U = 0x18, V = 0x19, W = 0x1A, X = 0x1B, Y = 0x1C, Z = 0x1D,
    
    // Teclas de símbolos
    Minus = 0x2D, Equal = 0x2E, LeftBracket = 0x2F,
    RightBracket = 0x30, Backslash = 0x31, Semicolon = 0x33,
    Quote = 0x34, Grave = 0x35, Comma = 0x36,
    Period = 0x37, Slash = 0x38,
    
    // Teclado numérico
    NumPad0 = 0x62, NumPad1 = 0x59, NumPad2 = 0x5A, NumPad3 = 0x5B,
    NumPad4 = 0x5C, NumPad5 = 0x5D, NumPad6 = 0x5E, NumPad7 = 0x5F,
    NumPad8 = 0x60, NumPad9 = 0x61, NumPadEnter = 0x58,
    NumPadPlus = 0x57, NumPadMinus = 0x56, NumPadStar = 0x55,
    NumPadSlash = 0x54, NumPadDot = 0x63,
    
    // Teclas multimedia
    VolumeUp = 0x80, VolumeDown = 0x81, Mute = 0x7F,
    PlayPause = 0xE8, Stop = 0xE9, Previous = 0xEA, Next = 0xEB,
    
    Unknown = 0x00,
}

impl UsbKeyCode {
    /// Convertir código USB a carácter ASCII
    pub fn to_ascii(&self, shift_pressed: bool, caps_lock: bool) -> Option<char> {
        let is_uppercase = shift_pressed ^ caps_lock;
        
        match self {
            // Teclas alfabéticas
            UsbKeyCode::A => Some(if is_uppercase { 'A' } else { 'a' }),
            UsbKeyCode::B => Some(if is_uppercase { 'B' } else { 'b' }),
            UsbKeyCode::C => Some(if is_uppercase { 'C' } else { 'c' }),
            UsbKeyCode::D => Some(if is_uppercase { 'D' } else { 'd' }),
            UsbKeyCode::E => Some(if is_uppercase { 'E' } else { 'e' }),
            UsbKeyCode::F => Some(if is_uppercase { 'F' } else { 'f' }),
            UsbKeyCode::G => Some(if is_uppercase { 'G' } else { 'g' }),
            UsbKeyCode::H => Some(if is_uppercase { 'H' } else { 'h' }),
            UsbKeyCode::I => Some(if is_uppercase { 'I' } else { 'i' }),
            UsbKeyCode::J => Some(if is_uppercase { 'J' } else { 'j' }),
            UsbKeyCode::K => Some(if is_uppercase { 'K' } else { 'k' }),
            UsbKeyCode::L => Some(if is_uppercase { 'L' } else { 'l' }),
            UsbKeyCode::M => Some(if is_uppercase { 'M' } else { 'm' }),
            UsbKeyCode::N => Some(if is_uppercase { 'N' } else { 'n' }),
            UsbKeyCode::O => Some(if is_uppercase { 'O' } else { 'o' }),
            UsbKeyCode::P => Some(if is_uppercase { 'P' } else { 'p' }),
            UsbKeyCode::Q => Some(if is_uppercase { 'Q' } else { 'q' }),
            UsbKeyCode::R => Some(if is_uppercase { 'R' } else { 'r' }),
            UsbKeyCode::S => Some(if is_uppercase { 'S' } else { 's' }),
            UsbKeyCode::T => Some(if is_uppercase { 'T' } else { 't' }),
            UsbKeyCode::U => Some(if is_uppercase { 'U' } else { 'u' }),
            UsbKeyCode::V => Some(if is_uppercase { 'V' } else { 'v' }),
            UsbKeyCode::W => Some(if is_uppercase { 'W' } else { 'w' }),
            UsbKeyCode::X => Some(if is_uppercase { 'X' } else { 'x' }),
            UsbKeyCode::Y => Some(if is_uppercase { 'Y' } else { 'y' }),
            UsbKeyCode::Z => Some(if is_uppercase { 'Z' } else { 'z' }),
            
            // Teclas numéricas
            UsbKeyCode::Key0 => Some(if shift_pressed { ')' } else { '0' }),
            UsbKeyCode::Key1 => Some(if shift_pressed { '!' } else { '1' }),
            UsbKeyCode::Key2 => Some(if shift_pressed { '@' } else { '2' }),
            UsbKeyCode::Key3 => Some(if shift_pressed { '#' } else { '3' }),
            UsbKeyCode::Key4 => Some(if shift_pressed { '$' } else { '4' }),
            UsbKeyCode::Key5 => Some(if shift_pressed { '%' } else { '5' }),
            UsbKeyCode::Key6 => Some(if shift_pressed { '^' } else { '6' }),
            UsbKeyCode::Key7 => Some(if shift_pressed { '&' } else { '7' }),
            UsbKeyCode::Key8 => Some(if shift_pressed { '*' } else { '8' }),
            UsbKeyCode::Key9 => Some(if shift_pressed { '(' } else { '9' }),
            
            // Teclas de símbolos
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
    
    /// Obtener nombre legible de la tecla
    pub fn name(&self) -> &'static str {
        match self {
            UsbKeyCode::F1 => "F1", UsbKeyCode::F2 => "F2", UsbKeyCode::F3 => "F3", UsbKeyCode::F4 => "F4",
            UsbKeyCode::F5 => "F5", UsbKeyCode::F6 => "F6", UsbKeyCode::F7 => "F7", UsbKeyCode::F8 => "F8",
            UsbKeyCode::F9 => "F9", UsbKeyCode::F10 => "F10", UsbKeyCode::F11 => "F11", UsbKeyCode::F12 => "F12",
            UsbKeyCode::LeftCtrl => "LeftCtrl", UsbKeyCode::RightCtrl => "RightCtrl",
            UsbKeyCode::LeftShift => "LeftShift", UsbKeyCode::RightShift => "RightShift",
            UsbKeyCode::LeftAlt => "LeftAlt", UsbKeyCode::RightAlt => "RightAlt",
            UsbKeyCode::LeftGui => "LeftGui", UsbKeyCode::RightGui => "RightGui",
            UsbKeyCode::Enter => "Enter", UsbKeyCode::Escape => "Escape", UsbKeyCode::Backspace => "Backspace",
            UsbKeyCode::Tab => "Tab", UsbKeyCode::Space => "Space", UsbKeyCode::CapsLock => "CapsLock",
            UsbKeyCode::NumLock => "NumLock", UsbKeyCode::ScrollLock => "ScrollLock",
            UsbKeyCode::Insert => "Insert", UsbKeyCode::Home => "Home", UsbKeyCode::PageUp => "PageUp",
            UsbKeyCode::Delete => "Delete", UsbKeyCode::End => "End", UsbKeyCode::PageDown => "PageDown",
            UsbKeyCode::Up => "Up", UsbKeyCode::Down => "Down", UsbKeyCode::Left => "Left", UsbKeyCode::Right => "Right",
            UsbKeyCode::A => "A", UsbKeyCode::B => "B", UsbKeyCode::C => "C", UsbKeyCode::D => "D",
            UsbKeyCode::E => "E", UsbKeyCode::F => "F", UsbKeyCode::G => "G", UsbKeyCode::H => "H",
            UsbKeyCode::I => "I", UsbKeyCode::J => "J", UsbKeyCode::K => "K", UsbKeyCode::L => "L",
            UsbKeyCode::M => "M", UsbKeyCode::N => "N", UsbKeyCode::O => "O", UsbKeyCode::P => "P",
            UsbKeyCode::Q => "Q", UsbKeyCode::R => "R", UsbKeyCode::S => "S", UsbKeyCode::T => "T",
            UsbKeyCode::U => "U", UsbKeyCode::V => "V", UsbKeyCode::W => "W", UsbKeyCode::X => "X",
            UsbKeyCode::Y => "Y", UsbKeyCode::Z => "Z",
            UsbKeyCode::Unknown => "Unknown",
            _ => "Other",
        }
    }
}

/// Estado de las teclas modificadoras
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModifierState {
    pub left_ctrl: bool,
    pub right_ctrl: bool,
    pub left_shift: bool,
    pub right_shift: bool,
    pub left_alt: bool,
    pub right_alt: bool,
    pub left_gui: bool,
    pub right_gui: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
    pub scroll_lock: bool,
}

impl ModifierState {
    pub fn new() -> Self {
        Self {
            left_ctrl: false,
            right_ctrl: false,
            left_shift: false,
            right_shift: false,
            left_alt: false,
            right_alt: false,
            left_gui: false,
            right_gui: false,
            caps_lock: false,
            num_lock: false,
            scroll_lock: false,
        }
    }
    
    /// Verificar si alguna tecla Ctrl está presionada
    pub fn ctrl_pressed(&self) -> bool {
        self.left_ctrl || self.right_ctrl
    }
    
    /// Verificar si alguna tecla Shift está presionada
    pub fn shift_pressed(&self) -> bool {
        self.left_shift || self.right_shift
    }
    
    /// Verificar si alguna tecla Alt está presionada
    pub fn alt_pressed(&self) -> bool {
        self.left_alt || self.right_alt
    }
    
    /// Verificar si alguna tecla Gui (Windows/Command) está presionada
    pub fn gui_pressed(&self) -> bool {
        self.left_gui || self.right_gui
    }
}

/// Evento de teclado
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardEvent {
    KeyPress { key: UsbKeyCode, modifiers: ModifierState },
    KeyRelease { key: UsbKeyCode, modifiers: ModifierState },
    KeyRepeat { key: UsbKeyCode, modifiers: ModifierState },
}

/// Información del teclado USB
#[derive(Debug, Clone)]
pub struct UsbKeyboardInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: String,
    pub product: String,
    pub version: u16,
    pub max_packet_size: u8,
    pub polling_interval: u8,
    pub num_leds: u8,
    pub num_keys: u8,
}

/// Driver de teclado USB
#[derive(Debug)]
pub struct UsbKeyboardDriver {
    pub info: UsbKeyboardInfo,
    pub device_address: u8,
    pub endpoint_address: u8,
    pub modifier_state: ModifierState,
    pub key_buffer: VecDeque<KeyboardEvent>,
    pub led_state: u8,
    pub initialized: bool,
    pub error_count: u32,
}

impl UsbKeyboardDriver {
    /// Crear nuevo driver de teclado USB
    pub fn new(vendor_id: u16, product_id: u16, device_address: u8, endpoint_address: u8) -> Self {
        Self {
            info: UsbKeyboardInfo {
                vendor_id,
                product_id,
                manufacturer: String::new(),
                product: String::new(),
                version: 0,
                max_packet_size: 8,
                polling_interval: 10,
                num_leds: 3,
                num_keys: 6,
            },
            device_address,
            endpoint_address,
            modifier_state: ModifierState::new(),
            key_buffer: VecDeque::new(),
            led_state: 0,
            initialized: false,
            error_count: 0,
        }
    }
    
    /// Inicializar el teclado USB
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Configurar endpoint de interrupción
        self.configure_endpoint()?;
        
        // Configurar LEDs iniciales
        self.set_leds(0)?;
        
        // Habilitar polling
        self.start_polling()?;
        
        self.initialized = true;
        Ok(())
    }
    
    /// Configurar endpoint de interrupción
    fn configure_endpoint(&mut self) -> Result<(), &'static str> {
        // En una implementación real, aquí se configuraría el endpoint USB
        // Por ahora simulamos la configuración
        Ok(())
    }
    
    /// Iniciar polling del teclado
    fn start_polling(&mut self) -> Result<(), &'static str> {
        // En una implementación real, aquí se configuraría el polling USB
        // Por ahora simulamos el inicio del polling
        Ok(())
    }
    
    /// Procesar datos recibidos del teclado
    pub fn process_keyboard_data(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() < 8 {
            return Err("Datos insuficientes");
        }
        
        // Parsear datos HID del teclado
        let modifiers = data[0];
        let _reserved = data[1];
        let key_codes = &data[2..8];
        
        // Actualizar estado de modificadores
        self.update_modifier_state(modifiers);
        
        // Procesar códigos de tecla
        self.process_key_codes(key_codes);
        
        Ok(())
    }
    
    /// Actualizar estado de teclas modificadoras
    fn update_modifier_state(&mut self, modifiers: u8) {
        self.modifier_state.left_ctrl = (modifiers & 0x01) != 0;
        self.modifier_state.left_shift = (modifiers & 0x02) != 0;
        self.modifier_state.left_alt = (modifiers & 0x04) != 0;
        self.modifier_state.left_gui = (modifiers & 0x08) != 0;
        self.modifier_state.right_ctrl = (modifiers & 0x10) != 0;
        self.modifier_state.right_shift = (modifiers & 0x20) != 0;
        self.modifier_state.right_alt = (modifiers & 0x40) != 0;
        self.modifier_state.right_gui = (modifiers & 0x80) != 0;
    }
    
    /// Procesar códigos de tecla
    fn process_key_codes(&mut self, key_codes: &[u8]) {
        // Detectar teclas presionadas y liberadas
        // En una implementación real, aquí se compararía con el estado anterior
        // Por simplicidad, asumimos que todas las teclas están siendo presionadas
        
        for &key_code in key_codes {
            if key_code != 0 {
                let key = self.usb_code_to_key(key_code);
                let event = KeyboardEvent::KeyPress {
                    key,
                    modifiers: self.modifier_state,
                };
                self.key_buffer.push_back(event);
            }
        }
    }
    
    /// Convertir código USB a enum de tecla
    fn usb_code_to_key(&self, code: u8) -> UsbKeyCode {
        match code {
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
            0x1E => UsbKeyCode::Key1,
            0x1F => UsbKeyCode::Key2,
            0x20 => UsbKeyCode::Key3,
            0x21 => UsbKeyCode::Key4,
            0x22 => UsbKeyCode::Key5,
            0x23 => UsbKeyCode::Key6,
            0x24 => UsbKeyCode::Key7,
            0x25 => UsbKeyCode::Key8,
            0x26 => UsbKeyCode::Key9,
            0x27 => UsbKeyCode::Key0,
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
            0x47 => UsbKeyCode::ScrollLock,
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
            0x54 => UsbKeyCode::NumPadSlash,
            0x55 => UsbKeyCode::NumPadStar,
            0x56 => UsbKeyCode::NumPadMinus,
            0x57 => UsbKeyCode::NumPadPlus,
            0x58 => UsbKeyCode::NumPadEnter,
            0x59 => UsbKeyCode::NumPad1,
            0x5A => UsbKeyCode::NumPad2,
            0x5B => UsbKeyCode::NumPad3,
            0x5C => UsbKeyCode::NumPad4,
            0x5D => UsbKeyCode::NumPad5,
            0x5E => UsbKeyCode::NumPad6,
            0x5F => UsbKeyCode::NumPad7,
            0x60 => UsbKeyCode::NumPad8,
            0x61 => UsbKeyCode::NumPad9,
            0x62 => UsbKeyCode::NumPad0,
            0x63 => UsbKeyCode::NumPadDot,
            0xE0 => UsbKeyCode::LeftCtrl,
            0xE1 => UsbKeyCode::LeftShift,
            0xE2 => UsbKeyCode::LeftAlt,
            0xE3 => UsbKeyCode::LeftGui,
            0xE4 => UsbKeyCode::RightCtrl,
            0xE5 => UsbKeyCode::RightShift,
            0xE6 => UsbKeyCode::RightAlt,
            0xE7 => UsbKeyCode::RightGui,
            _ => UsbKeyCode::Unknown,
        }
    }
    
    /// Obtener siguiente evento del buffer
    pub fn get_next_event(&mut self) -> Option<KeyboardEvent> {
        self.key_buffer.pop_front()
    }
    
    /// Verificar si hay eventos pendientes
    pub fn has_events(&self) -> bool {
        !self.key_buffer.is_empty()
    }
    
    /// Configurar LEDs del teclado
    pub fn set_leds(&mut self, leds: u8) -> Result<(), &'static str> {
        self.led_state = leds & 0x07; // Solo 3 LEDs soportados
        // En una implementación real, aquí se enviaría el comando SET_REPORT
        Ok(())
    }
    
    /// Obtener estado actual de los LEDs
    pub fn get_led_state(&self) -> u8 {
        self.led_state
    }
    
    /// Obtener estado de las teclas modificadoras
    pub fn get_modifier_state(&self) -> ModifierState {
        self.modifier_state
    }
    
    /// Verificar si el teclado está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    
    /// Obtener información del teclado
    pub fn get_info(&self) -> &UsbKeyboardInfo {
        &self.info
    }
    
    /// Limpiar buffer de eventos
    pub fn clear_buffer(&mut self) {
        self.key_buffer.clear();
    }
    
    /// Obtener número de eventos en el buffer
    pub fn event_count(&self) -> usize {
        self.key_buffer.len()
    }
}

/// Función de conveniencia para crear un driver de teclado USB
pub fn create_usb_keyboard_driver(vendor_id: u16, product_id: u16, device_address: u8, endpoint_address: u8) -> UsbKeyboardDriver {
    UsbKeyboardDriver::new(vendor_id, product_id, device_address, endpoint_address)
}
