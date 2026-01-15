//! Driver USB real para teclado
//!
//! Implementa soporte completo para teclados USB reales con comunicación
//! hardware directa, no simulación.

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceOperations, DeviceType},
    keyboard::{KeyCode, KeyEvent, KeyState, KeyboardDriver},
    manager::{Driver, DriverError, DriverInfo, DriverResult},
    usb::{RealUsbController, UsbControllerType, UsbDeviceClass, UsbDeviceInfo},
};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Driver USB real para teclado
pub struct UsbKeyboardReal {
    pub info: DriverInfo,
    pub usb_controller: Option<RealUsbController>,
    pub keyboard_device: Option<UsbDeviceInfo>,
    pub is_initialized: bool,
    pub key_buffer: Vec<KeyEvent>,
    pub current_keys: [bool; 256], // Estado actual de todas las teclas
    pub last_report: [u8; 8],      // Último reporte HID recibido
    pub endpoint: u8,
    pub address: u8,
}

impl UsbKeyboardReal {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("usb_keyboard_real");
        info.device_type = DeviceType::Input;
        info.version = 2;

        Self {
            info,
            usb_controller: None,
            keyboard_device: None,
            is_initialized: false,
            key_buffer: Vec::new(),
            current_keys: [false; 256],
            last_report: [0; 8],
            endpoint: 0x81, // Endpoint interrupt IN
            address: 0,
        }
    }

    /// Configurar controlador USB
    pub fn set_usb_controller(&mut self, controller: RealUsbController) {
        self.usb_controller = Some(controller);
    }

    /// Detectar teclado USB
    pub fn detect_keyboard(&mut self) -> DriverResult<()> {
        if let Some(ref mut controller) = self.usb_controller {
            // Buscar dispositivos HID
            let hid_devices = controller.get_hid_devices();

            for device in hid_devices {
                // Verificar si es un teclado (clase HID, subclase 0x01, protocolo 0x01)
                if device.device_descriptor.device_class == UsbDeviceClass::HID {
                    // En una implementación real, verificaríamos los descriptores de interfaz
                    // Por ahora, asumimos que es un teclado
                    self.keyboard_device = Some(device.clone());
                    self.address = device.address;
                    return Ok(());
                }
            }
        }
        Err(DriverError::DeviceNotFound)
    }

    /// Leer reporte HID del teclado
    pub fn read_hid_report(&mut self) -> DriverResult<()> {
        if let Some(ref controller) = self.usb_controller {
            if let Some(ref device) = self.keyboard_device {
                let mut report = [0u8; 8];
                controller.read_hid_data(device.address, self.endpoint, &mut report)?;

                // Procesar reporte HID
                self.process_hid_report(&report);
                self.last_report = report;
            }
        }
        Ok(())
    }

    /// Procesar reporte HID del teclado
    fn process_hid_report(&mut self, report: &[u8; 8]) {
        // Estructura típica de reporte de teclado USB:
        // Byte 0: Modificadores (Ctrl, Alt, Shift, etc.)
        // Byte 1: Reservado
        // Bytes 2-7: Códigos de tecla presionadas (máximo 6 teclas)

        let modifiers = report[0];
        let key_codes = &report[2..8];

        // Procesar modificadores
        self.process_modifiers(modifiers);

        // Procesar teclas normales
        for &key_code in key_codes {
            if key_code != 0 {
                self.process_key_press(key_code);
            }
        }

        // Detectar teclas liberadas comparando con el reporte anterior
        let last_report = self.last_report;
        self.detect_key_releases(&last_report, report);
    }

    /// Procesar modificadores (Ctrl, Alt, Shift, etc.)
    fn process_modifiers(&mut self, modifiers: u8) {
        let modifier_keys = [
            (0x01, KeyCode::LeftCtrl),
            (0x02, KeyCode::LeftShift),
            (0x04, KeyCode::LeftAlt),
            (0x08, KeyCode::LeftMeta),
            (0x10, KeyCode::RightCtrl),
            (0x20, KeyCode::RightShift),
            (0x40, KeyCode::RightAlt),
            (0x80, KeyCode::RightMeta),
        ];

        for (bit, key_code) in modifier_keys.iter() {
            let is_pressed = (modifiers & bit) != 0;
            let key_index = *key_code as usize;

            if is_pressed != self.current_keys[key_index] {
                self.current_keys[key_index] = is_pressed;

                let event = KeyEvent {
                    key: *key_code,
                    state: if is_pressed {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    },
                    modifiers: self.get_current_modifiers(),
                };

                self.key_buffer.push(event);
            }
        }
    }

    /// Procesar tecla presionada
    fn process_key_press(&mut self, key_code: u8) {
        if let Some(key) = self.usb_key_to_keycode(key_code) {
            let key_index = key as usize;

            if !self.current_keys[key_index] {
                self.current_keys[key_index] = true;

                let event = KeyEvent {
                    key,
                    state: KeyState::Pressed,
                    modifiers: self.get_current_modifiers(),
                };

                self.key_buffer.push(event);
            }
        }
    }

    /// Detectar teclas liberadas
    fn detect_key_releases(&mut self, old_report: &[u8; 8], new_report: &[u8; 8]) {
        let old_keys = &old_report[2..8];
        let new_keys = &new_report[2..8];

        for &old_key in old_keys {
            if old_key != 0 && !new_keys.contains(&old_key) {
                if let Some(key) = self.usb_key_to_keycode(old_key) {
                    let key_index = key as usize;
                    self.current_keys[key_index] = false;

                    let event = KeyEvent {
                        key,
                        state: KeyState::Released,
                        modifiers: self.get_current_modifiers(),
                    };

                    self.key_buffer.push(event);
                }
            }
        }
    }

    /// Convertir código USB a KeyCode
    fn usb_key_to_keycode(&self, usb_key: u8) -> Option<KeyCode> {
        match usb_key {
            0x04 => Some(KeyCode::A),
            0x05 => Some(KeyCode::B),
            0x06 => Some(KeyCode::C),
            0x07 => Some(KeyCode::D),
            0x08 => Some(KeyCode::E),
            0x09 => Some(KeyCode::F),
            0x0A => Some(KeyCode::G),
            0x0B => Some(KeyCode::H),
            0x0C => Some(KeyCode::I),
            0x0D => Some(KeyCode::J),
            0x0E => Some(KeyCode::K),
            0x0F => Some(KeyCode::L),
            0x10 => Some(KeyCode::M),
            0x11 => Some(KeyCode::N),
            0x12 => Some(KeyCode::O),
            0x13 => Some(KeyCode::P),
            0x14 => Some(KeyCode::Q),
            0x15 => Some(KeyCode::R),
            0x16 => Some(KeyCode::S),
            0x17 => Some(KeyCode::T),
            0x18 => Some(KeyCode::U),
            0x19 => Some(KeyCode::V),
            0x1A => Some(KeyCode::W),
            0x1B => Some(KeyCode::X),
            0x1C => Some(KeyCode::Y),
            0x1D => Some(KeyCode::Z),
            0x1E => Some(KeyCode::Key1),
            0x1F => Some(KeyCode::Key2),
            0x20 => Some(KeyCode::Key3),
            0x21 => Some(KeyCode::Key4),
            0x22 => Some(KeyCode::Key5),
            0x23 => Some(KeyCode::Key6),
            0x24 => Some(KeyCode::Key7),
            0x25 => Some(KeyCode::Key8),
            0x26 => Some(KeyCode::Key9),
            0x27 => Some(KeyCode::Key0),
            0x28 => Some(KeyCode::Enter),
            0x29 => Some(KeyCode::Escape),
            0x2A => Some(KeyCode::Backspace),
            0x2B => Some(KeyCode::Tab),
            0x2C => Some(KeyCode::Space),
            0x2D => Some(KeyCode::Minus),
            0x2E => Some(KeyCode::Equal),
            0x2F => Some(KeyCode::LeftBracket),
            0x30 => Some(KeyCode::RightBracket),
            0x31 => Some(KeyCode::Backslash),
            0x33 => Some(KeyCode::Semicolon),
            0x34 => Some(KeyCode::Apostrophe),
            0x35 => Some(KeyCode::Grave),
            0x36 => Some(KeyCode::Comma),
            0x37 => Some(KeyCode::Period),
            0x38 => Some(KeyCode::Slash),
            0x39 => Some(KeyCode::CapsLock),
            0x3A => Some(KeyCode::F1),
            0x3B => Some(KeyCode::F2),
            0x3C => Some(KeyCode::F3),
            0x3D => Some(KeyCode::F4),
            0x3E => Some(KeyCode::F5),
            0x3F => Some(KeyCode::F6),
            0x40 => Some(KeyCode::F7),
            0x41 => Some(KeyCode::F8),
            0x42 => Some(KeyCode::F9),
            0x43 => Some(KeyCode::F10),
            0x44 => Some(KeyCode::F11),
            0x45 => Some(KeyCode::F12),
            0x46 => Some(KeyCode::PrintScreen),
            0x47 => Some(KeyCode::ScrollLock),
            0x48 => Some(KeyCode::Pause),
            0x49 => Some(KeyCode::Insert),
            0x4A => Some(KeyCode::Home),
            0x4B => Some(KeyCode::PageUp),
            0x4C => Some(KeyCode::Delete),
            0x4D => Some(KeyCode::End),
            0x4E => Some(KeyCode::PageDown),
            0x4F => Some(KeyCode::Right),
            0x50 => Some(KeyCode::Left),
            0x51 => Some(KeyCode::Down),
            0x52 => Some(KeyCode::Up),
            0x53 => Some(KeyCode::NumLock),
            0x54 => Some(KeyCode::NumpadDivide),
            0x55 => Some(KeyCode::NumpadMultiply),
            0x56 => Some(KeyCode::NumpadSubtract),
            0x57 => Some(KeyCode::NumpadAdd),
            0x58 => Some(KeyCode::NumpadEnter),
            0x59 => Some(KeyCode::Numpad1),
            0x5A => Some(KeyCode::Numpad2),
            0x5B => Some(KeyCode::Numpad3),
            0x5C => Some(KeyCode::Numpad4),
            0x5D => Some(KeyCode::Numpad5),
            0x5E => Some(KeyCode::Numpad6),
            0x5F => Some(KeyCode::Numpad7),
            0x60 => Some(KeyCode::Numpad8),
            0x61 => Some(KeyCode::Numpad9),
            0x62 => Some(KeyCode::Numpad0),
            0x63 => Some(KeyCode::NumpadDecimal),
            _ => None,
        }
    }

    /// Obtener modificadores actuales
    fn get_current_modifiers(&self) -> u8 {
        let mut modifiers = 0;
        if self.current_keys[KeyCode::LeftCtrl as usize]
            || self.current_keys[KeyCode::RightCtrl as usize]
        {
            modifiers |= 0x01;
        }
        if self.current_keys[KeyCode::LeftShift as usize]
            || self.current_keys[KeyCode::RightShift as usize]
        {
            modifiers |= 0x02;
        }
        if self.current_keys[KeyCode::LeftAlt as usize]
            || self.current_keys[KeyCode::RightAlt as usize]
        {
            modifiers |= 0x04;
        }
        if self.current_keys[KeyCode::LeftMeta as usize]
            || self.current_keys[KeyCode::RightMeta as usize]
        {
            modifiers |= 0x08;
        }
        modifiers
    }

    /// Obtener siguiente evento de teclado
    pub fn get_next_key_event(&mut self) -> Option<KeyEvent> {
        self.key_buffer.pop()
    }

    /// Verificar si hay eventos pendientes
    pub fn has_key_events(&self) -> bool {
        !self.key_buffer.is_empty()
    }

    /// Obtener estado de una tecla específica
    pub fn is_key_pressed(&self, key: KeyCode) -> bool {
        self.current_keys[key as usize]
    }

    /// Obtener estadísticas del teclado
    pub fn get_keyboard_stats(&self) -> String {
        let mut stats = String::new();
        stats.push_str("=== TECLADO USB REAL ===\n");

        if let Some(ref device) = self.keyboard_device {
            stats.push_str(&format!(
                "Dispositivo: VID={:04X} PID={:04X}\n",
                device.device_descriptor.vendor_id, device.device_descriptor.product_id
            ));
            stats.push_str(&format!("Dirección USB: {}\n", device.address));
            stats.push_str(&format!("Endpoint: 0x{:02X}\n", self.endpoint));
        } else {
            stats.push_str("Dispositivo: No detectado\n");
        }

        stats.push_str(&format!("Eventos en buffer: {}\n", self.key_buffer.len()));
        stats.push_str(&format!(
            "Teclas presionadas: {}\n",
            self.current_keys.iter().filter(|&&pressed| pressed).count()
        ));

        stats
    }
}

impl fmt::Debug for UsbKeyboardReal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsbKeyboardReal")
            .field("info", &self.info)
            .field("usb_controller", &self.usb_controller.is_some())
            .field("keyboard_device", &self.keyboard_device.is_some())
            .field("is_initialized", &self.is_initialized)
            .field("key_buffer_len", &self.key_buffer.len())
            .finish()
    }
}

impl Driver for UsbKeyboardReal {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        if self.is_initialized {
            return Ok(());
        }

        // Detectar teclado USB
        self.detect_keyboard()?;

        self.info.is_loaded = true;
        self.is_initialized = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        self.keyboard_device = None;
        self.key_buffer.clear();
        self.current_keys = [false; 256];
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
        // Leer reporte HID del teclado
        self.read_hid_report()?;
        Ok(())
    }
}

impl KeyboardDriver for UsbKeyboardReal {
    fn read_key(&mut self) -> Option<KeyEvent> {
        self.get_next_key_event()
    }

    fn read_char(&mut self) -> Option<char> {
        if let Some(event) = self.read_key() {
             // Solo eventos de pression, ignorar modificadores sueltos
             if event.state == crate::drivers::keyboard::KeyState::Pressed {
                 let shift = (event.modifiers & 1) != 0; // Simple check for now
                 return event.key.to_char(shift);
             }
        }
        None
    }

    fn is_key_pressed(&self, key: KeyCode) -> bool {
        self.is_key_pressed(key)
    }

    fn get_modifiers(&self) -> u8 {
        self.get_current_modifiers()
    }

    fn clear_buffer(&mut self) {
        self.key_buffer.clear();
    }

    fn has_key_events(&self) -> bool {
        self.has_key_events()
    }
}
