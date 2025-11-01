/// Driver HID (Human Interface Device) para USB
/// 
/// Este driver implementa el protocolo HID para dispositivos de entrada USB:
/// - Teclados USB
/// - Ratones/Mouse USB
/// - Gamepads/Joysticks USB
///
/// Basado en la especificación HID 1.11

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::format;

use crate::drivers::manager::DriverResult;
use crate::drivers::usb_xhci_control::*;

/// Tipo de dispositivo HID
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidDeviceType {
    Keyboard,
    Mouse,
    Gamepad,
    Unknown(u8),
}

/// Subclase HID (Boot Interface)
pub const HID_SUBCLASS_BOOT: u8 = 1;

/// Protocolos HID para Boot Interface
pub const HID_PROTOCOL_KEYBOARD: u8 = 1;
pub const HID_PROTOCOL_MOUSE: u8 = 2;

/// Descriptor HID
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct HidDescriptor {
    pub length: u8,              // Tamaño del descriptor (9 bytes mínimo)
    pub descriptor_type: u8,     // Tipo (0x21 para HID)
    pub hid_version: u16,        // Versión HID (ej: 0x0111 para 1.11)
    pub country_code: u8,        // Código del país (0 = no específico)
    pub num_descriptors: u8,     // Número de descriptores de clase
    pub report_descriptor_type: u8,  // Tipo del Report Descriptor (0x22)
    pub report_descriptor_length: u16, // Longitud del Report Descriptor
}

/// Boot Protocol Keyboard Report (8 bytes)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct KeyboardReport {
    pub modifiers: u8,           // Bits: Ctrl, Shift, Alt, etc.
    pub reserved: u8,            // Reservado (debe ser 0)
    pub keys: [u8; 6],          // Hasta 6 teclas presionadas simultáneamente
}

impl KeyboardReport {
    pub fn new() -> Self {
        Self {
            modifiers: 0,
            reserved: 0,
            keys: [0; 6],
        }
    }
    
    /// Verifica si una tecla está presionada
    pub fn is_key_pressed(&self, keycode: u8) -> bool {
        self.keys.iter().any(|&k| k == keycode)
    }
    
    /// Obtiene el primer keycode presionado
    pub fn get_first_key(&self) -> Option<u8> {
        self.keys.iter().find(|&&k| k != 0).copied()
    }
    
    /// Verifica si Ctrl está presionado
    pub fn is_ctrl(&self) -> bool {
        (self.modifiers & 0x11) != 0  // Left Ctrl o Right Ctrl
    }
    
    /// Verifica si Shift está presionado
    pub fn is_shift(&self) -> bool {
        (self.modifiers & 0x22) != 0  // Left Shift o Right Shift
    }
    
    /// Verifica si Alt está presionado
    pub fn is_alt(&self) -> bool {
        (self.modifiers & 0x44) != 0  // Left Alt o Right Alt
    }
}

/// Boot Protocol Mouse Report (3-4 bytes)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct MouseReport {
    pub buttons: u8,             // Bits: Left, Right, Middle, etc.
    pub x: i8,                   // Movimiento X (relativo)
    pub y: i8,                   // Movimiento Y (relativo)
    pub wheel: i8,               // Rueda del mouse (opcional)
}

impl MouseReport {
    pub fn new() -> Self {
        Self {
            buttons: 0,
            x: 0,
            y: 0,
            wheel: 0,
        }
    }
    
    /// Verifica si el botón izquierdo está presionado
    pub fn is_left_button(&self) -> bool {
        (self.buttons & 0x01) != 0
    }
    
    /// Verifica si el botón derecho está presionado
    pub fn is_right_button(&self) -> bool {
        (self.buttons & 0x02) != 0
    }
    
    /// Verifica si el botón central está presionado
    pub fn is_middle_button(&self) -> bool {
        (self.buttons & 0x04) != 0
    }
}

/// Dispositivo HID
pub struct HidDevice {
    slot_id: u8,
    device_type: HidDeviceType,
    interface_number: u8,
    endpoint_in: u8,             // Endpoint IN (para recibir datos del dispositivo)
    max_packet_size: u16,
    poll_interval: u8,           // Intervalo de polling (en ms)
    boot_protocol: bool,         // true si usa Boot Protocol
}

impl HidDevice {
    /// Crea un nuevo dispositivo HID
    pub fn new(slot_id: u8, device_type: HidDeviceType, interface_number: u8) -> Self {
        Self {
            slot_id,
            device_type,
            interface_number,
            endpoint_in: 0x81,  // Por defecto EP1 IN
            max_packet_size: 8,
            poll_interval: 10,  // 10ms por defecto
            boot_protocol: false,
        }
    }
    
    /// Detecta el tipo de dispositivo HID
    pub fn detect_from_protocol(protocol: u8) -> HidDeviceType {
        match protocol {
            HID_PROTOCOL_KEYBOARD => HidDeviceType::Keyboard,
            HID_PROTOCOL_MOUSE => HidDeviceType::Mouse,
            _ => HidDeviceType::Unknown(protocol),
        }
    }
    
    /// Configura el endpoint IN
    pub fn set_endpoint(&mut self, endpoint: u8, max_packet: u16, interval: u8) {
        self.endpoint_in = endpoint;
        self.max_packet_size = max_packet;
        self.poll_interval = interval;
    }
    
    /// Habilita Boot Protocol
    pub fn enable_boot_protocol(&mut self) -> DriverResult<()> {
        crate::debug::serial_write_str(&format!(
            "USB_HID: Habilitando Boot Protocol para {:?}\n",
            self.device_type
        ));
        
        self.boot_protocol = true;
        Ok(())
    }
    
    /// Obtiene el slot ID
    pub fn slot_id(&self) -> u8 {
        self.slot_id
    }
    
    /// Obtiene el tipo de dispositivo
    pub fn device_type(&self) -> HidDeviceType {
        self.device_type
    }
    
    /// Obtiene el endpoint IN
    pub fn endpoint_in(&self) -> u8 {
        self.endpoint_in
    }
}

/// Manager de dispositivos HID
pub struct HidManager {
    devices: Vec<HidDevice>,
}

impl HidManager {
    /// Crea un nuevo manager
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }
    
    /// Registra un dispositivo HID
    pub fn register_device(&mut self, device: HidDevice) {
        crate::debug::serial_write_str(&format!(
            "USB_HID: Registrando dispositivo {:?} (slot={})\n",
            device.device_type, device.slot_id
        ));
        
        self.devices.push(device);
    }
    
    /// Obtiene todos los teclados
    pub fn get_keyboards(&self) -> Vec<&HidDevice> {
        self.devices.iter()
            .filter(|d| d.device_type == HidDeviceType::Keyboard)
            .collect()
    }
    
    /// Obtiene todos los ratones
    pub fn get_mice(&self) -> Vec<&HidDevice> {
        self.devices.iter()
            .filter(|d| d.device_type == HidDeviceType::Mouse)
            .collect()
    }
    
    /// Obtiene el número de dispositivos
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }
}

/// Comandos HID específicos
pub mod hid_commands {
    use super::*;
    
    /// SET_PROTOCOL request (HID 1.11, sección 7.2.6)
    /// protocol: 0 = Boot Protocol, 1 = Report Protocol
    pub fn set_protocol(interface: u8, protocol: u8) -> SetupPacket {
        SetupPacket::new(
            0x21,  // Class, Interface
            0x0B,  // SET_PROTOCOL
            protocol as u16,
            interface as u16,
            0,
        )
    }
    
    /// GET_REPORT request (HID 1.11, sección 7.2.1)
    pub fn get_report(interface: u8, report_type: u8, report_id: u8, length: u16) -> SetupPacket {
        SetupPacket::new(
            0xA1,  // Class, Interface, Device to Host
            0x01,  // GET_REPORT
            ((report_type as u16) << 8) | (report_id as u16),
            interface as u16,
            length,
        )
    }
    
    /// SET_IDLE request (HID 1.11, sección 7.2.4)
    /// duration: duración en múltiplos de 4ms (0 = infinito)
    pub fn set_idle(interface: u8, duration: u8, report_id: u8) -> SetupPacket {
        SetupPacket::new(
            0x21,  // Class, Interface
            0x0A,  // SET_IDLE
            ((duration as u16) << 8) | (report_id as u16),
            interface as u16,
            0,
        )
    }
    
    /// GET_DESCRIPTOR request para HID Report Descriptor
    pub fn get_hid_report_descriptor(interface: u8, length: u16) -> SetupPacket {
        SetupPacket::new(
            0x81,  // Standard, Interface, Device to Host
            0x06,  // GET_DESCRIPTOR
            0x2200,  // Report Descriptor
            interface as u16,
            length,
        )
    }
}

/// Mapeo de USB HID keycodes a caracteres ASCII
pub fn keycode_to_char(keycode: u8, shift: bool) -> Option<char> {
    match keycode {
        0x04..=0x1D => {  // A-Z
            let base = if shift { b'A' } else { b'a' };
            Some((base + (keycode - 0x04)) as char)
        }
        0x1E..=0x26 => {  // 1-9
            if shift {
                match keycode {
                    0x1E => Some('!'),
                    0x1F => Some('@'),
                    0x20 => Some('#'),
                    0x21 => Some('$'),
                    0x22 => Some('%'),
                    0x23 => Some('^'),
                    0x24 => Some('&'),
                    0x25 => Some('*'),
                    0x26 => Some('('),
                    _ => None,
                }
            } else {
                Some(((keycode - 0x1E) as u8 + b'1') as char)
            }
        }
        0x27 => Some(if shift { ')' } else { '0' }),  // 0
        0x28 => Some('\n'),  // Enter
        0x29 => Some('\x1B'),  // Escape
        0x2A => Some('\x08'),  // Backspace
        0x2B => Some('\t'),  // Tab
        0x2C => Some(' '),  // Space
        0x2D => Some(if shift { '_' } else { '-' }),
        0x2E => Some(if shift { '+' } else { '=' }),
        0x2F => Some(if shift { '{' } else { '[' }),
        0x30 => Some(if shift { '}' } else { ']' }),
        0x31 => Some(if shift { '|' } else { '\\' }),
        0x33 => Some(if shift { ':' } else { ';' }),
        0x34 => Some(if shift { '"' } else { '\'' }),
        0x35 => Some(if shift { '~' } else { '`' }),
        0x36 => Some(if shift { '<' } else { ',' }),
        0x37 => Some(if shift { '>' } else { '.' }),
        0x38 => Some(if shift { '?' } else { '/' }),
        _ => None,
    }
}

/// Convierte keycode a nombre de tecla
pub fn keycode_to_name(keycode: u8) -> &'static str {
    match keycode {
        0x00 => "None",
        0x04..=0x1D => {
            const LETTERS: &[&str] = &[
                "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M",
                "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z"
            ];
            LETTERS[(keycode - 0x04) as usize]
        }
        0x1E => "1", 0x1F => "2", 0x20 => "3", 0x21 => "4", 0x22 => "5",
        0x23 => "6", 0x24 => "7", 0x25 => "8", 0x26 => "9", 0x27 => "0",
        0x28 => "Enter",
        0x29 => "Escape",
        0x2A => "Backspace",
        0x2B => "Tab",
        0x2C => "Space",
        0x3A => "F1", 0x3B => "F2", 0x3C => "F3", 0x3D => "F4",
        0x3E => "F5", 0x3F => "F6", 0x40 => "F7", 0x41 => "F8",
        0x42 => "F9", 0x43 => "F10", 0x44 => "F11", 0x45 => "F12",
        0x4F => "Right", 0x50 => "Left", 0x51 => "Down", 0x52 => "Up",
        _ => "Unknown",
    }
}
