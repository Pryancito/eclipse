//! Driver de teclado para Eclipse OS
//! 
//! Define las interfaces y tipos básicos para drivers de teclado.

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType},
    manager::{Driver, DriverInfo, DriverResult, DriverError},
};

/// Códigos de tecla
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyCode {
    // Letras
    A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    
    // Números
    Key0, Key1, Key2, Key3, Key4, Key5, Key6, Key7, Key8, Key9,
    
    // Teclas especiales
    Space, Enter, Escape, Backspace, Tab, CapsLock, Shift, Ctrl, Alt,
    Left, Right, Up, Down, Home, End, PageUp, PageDown, Insert, Delete,
    
    // Teclas de función
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    
    // Símbolos
    Semicolon, Quote, Backslash, Comma, Period, Slash,
    LeftBracket, RightBracket, Minus, Equals, Grave,
    Equal, Apostrophe, PrintScreen, ScrollLock, Pause,
    
    // Teclas del teclado numérico
    Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9,
    NumLock, NumpadDivide, NumpadMultiply, NumpadSubtract, NumpadAdd, NumpadEnter, NumpadDecimal,
    
    // Teclas del sistema
    LeftShift, RightShift, LeftCtrl, RightCtrl, LeftAlt, RightAlt,
    LeftMeta, RightMeta, Menu,
    
    // Teclas adicionales
    None, Wheel,
    
    // Desconocida
    Unknown,
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
    pub modifiers: u8, // Bitmask de modificadores
}

impl KeyEvent {
    pub fn new(key: KeyCode, state: KeyState, modifiers: u8) -> Self {
        Self {
            key,
            state,
            modifiers,
        }
    }
}

/// Driver de teclado base
pub trait KeyboardDriver {
    /// Leer siguiente evento de teclado
    fn read_key(&mut self) -> Option<KeyEvent>;
    
    /// Verificar si una tecla está presionada
    fn is_key_pressed(&self, key: KeyCode) -> bool;
    
    /// Obtener modificadores actuales
    fn get_modifiers(&self) -> u8;
    
    /// Limpiar buffer de eventos
    fn clear_buffer(&mut self);
    
    /// Verificar si hay eventos pendientes
    fn has_key_events(&self) -> bool;
}

/// Driver de teclado básico
pub struct BasicKeyboardDriver {
    pub info: DriverInfo,
    pub is_initialized: bool,
}

impl BasicKeyboardDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("basic_keyboard");
        info.device_type = DeviceType::Input;
        info.version = 1;

        Self {
            info,
            is_initialized: false,
        }
    }
}

impl Driver for BasicKeyboardDriver {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        self.is_initialized = true;
        self.info.is_loaded = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        self.is_initialized = false;
        self.info.is_loaded = false;
        Ok(())
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        device_info.device_type == DeviceType::Input
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        device.driver_id = Some(self.info.id);
        Ok(())
    }

    fn detach_device(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }

    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }
}

impl KeyboardDriver for BasicKeyboardDriver {
    fn read_key(&mut self) -> Option<KeyEvent> {
        // Implementación básica - no hay eventos por defecto
        None
    }

    fn is_key_pressed(&self, _key: KeyCode) -> bool {
        false
    }

    fn get_modifiers(&self) -> u8 {
        0
    }

    fn clear_buffer(&mut self) {
        // No hay buffer que limpiar
    }

    fn has_key_events(&self) -> bool {
        false
    }
}
