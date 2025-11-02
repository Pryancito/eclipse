//! Driver USB HID (Human Interface Device) para teclado y ratón
//! 
//! Este módulo implementa el soporte para dispositivos de entrada USB:
//! - Teclados USB (boot protocol)
//! - Ratones USB (boot protocol)
//! 
//! Integración con InputSystem para unificar entrada PS/2 y USB

use alloc::vec::Vec;

/// Protocolo USB HID Boot para teclado
const HID_PROTOCOL_KEYBOARD: u8 = 1;

/// Protocolo USB HID Boot para ratón
const HID_PROTOCOL_MOUSE: u8 = 2;

/// Tipo de dispositivo HID
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HidDeviceType {
    Keyboard,
    Mouse,
    Unknown,
}

/// Estado de un dispositivo HID
pub struct HidDevice {
    pub slot_id: u8,
    pub device_type: HidDeviceType,
    pub endpoint_in: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    // Estado del teclado
    pub keyboard_leds: u8,
    pub last_keyboard_report: [u8; 8],
    // Estado del ratón
    pub last_mouse_buttons: u8,
    pub mouse_x: i16,
    pub mouse_y: i16,
}

impl HidDevice {
    pub fn new_keyboard(slot_id: u8, endpoint_in: u8) -> Self {
        Self {
            slot_id,
            device_type: HidDeviceType::Keyboard,
            endpoint_in,
            max_packet_size: 8,
            interval: 10,
            keyboard_leds: 0,
            last_keyboard_report: [0; 8],
            last_mouse_buttons: 0,
            mouse_x: 0,
            mouse_y: 0,
        }
    }

    pub fn new_mouse(slot_id: u8, endpoint_in: u8) -> Self {
        Self {
            slot_id,
            device_type: HidDeviceType::Mouse,
            endpoint_in,
            max_packet_size: 4,
            interval: 10,
            keyboard_leds: 0,
            last_keyboard_report: [0; 8],
            last_mouse_buttons: 0,
            mouse_x: 0,
            mouse_y: 0,
        }
    }

    /// Procesa un reporte de teclado USB (formato boot protocol)
    /// 
    /// Formato del reporte (8 bytes):
    /// - Byte 0: Modificadores (Ctrl, Shift, Alt, etc.)
    /// - Byte 1: Reservado
    /// - Bytes 2-7: Códigos de teclas presionadas (hasta 6 simultáneas)
    pub fn process_keyboard_report(&mut self, data: &[u8]) {
        if data.len() < 8 {
            return;
        }

        use crate::debug::serial_write_str;

        let modifiers = data[0];
        let current_keys = &data[2..8];
        let previous_keys = &self.last_keyboard_report[2..8];

        // Detectar teclas presionadas (key down)
        for &keycode in current_keys {
            if keycode != 0 && !previous_keys.contains(&keycode) {
                let ch = usb_keycode_to_char(keycode, (modifiers & 0x22) != 0);
                if ch != '\0' {
                    serial_write_str("USB_KBD: Key pressed: ");
                    if ch.is_ascii_graphic() || ch == ' ' {
                        let buf = [ch as u8];
                        if let Ok(s) = core::str::from_utf8(&buf) {
                            serial_write_str(s);
                        }
                    }
                    serial_write_str("\n");
                    
                    // Enviar evento al InputSystem
                    let kbd_event = crate::drivers::usb_keyboard::KeyboardEvent {
                        key_code: crate::drivers::usb_keyboard::UsbKeyCode::from_hid_code(keycode),
                        pressed: true,
                        modifiers: hid_modifiers_to_state(modifiers),
                        character: Some(ch),
                        timestamp: 0, // Se actualizará en InputSystem
                    };
                    
                    let _ = crate::drivers::input_system::push_keyboard_event(kbd_event, self.slot_id as u32);
                }
            }
        }
        
        // Detectar teclas liberadas (key up)
        for &keycode in previous_keys {
            if keycode != 0 && !current_keys.contains(&keycode) {
                // Tecla liberada
                let kbd_event = crate::drivers::usb_keyboard::KeyboardEvent {
                    key_code: crate::drivers::usb_keyboard::UsbKeyCode::from_hid_code(keycode),
                    pressed: false,
                    modifiers: hid_modifiers_to_state(modifiers),
                    character: None,
                    timestamp: 0,
                };
                
                let _ = crate::drivers::input_system::push_keyboard_event(kbd_event, self.slot_id as u32);
            }
        }

        // Guardar reporte actual para la próxima comparación
        self.last_keyboard_report.copy_from_slice(data);
    }

    /// Procesa un reporte de ratón USB (formato boot protocol)
    /// 
    /// Formato del reporte (3-4 bytes):
    /// - Byte 0: Botones (bit 0=izq, bit 1=der, bit 2=medio)
    /// - Byte 1: Movimiento X (signed)
    /// - Byte 2: Movimiento Y (signed)
    /// - Byte 3: Rueda (opcional, signed)
    pub fn process_mouse_report(&mut self, data: &[u8]) {
        if data.len() < 3 {
            return;
        }

        use crate::debug::serial_write_str;

        let buttons = data[0];
        let dx = data[1] as i8 as i16;
        let dy = data[2] as i8 as i16;

        // Actualizar posición acumulada
        self.mouse_x = self.mouse_x.saturating_add(dx);
        self.mouse_y = self.mouse_y.saturating_add(dy);

        // Detectar cambios en botones o movimiento
        let buttons_changed = buttons != self.last_mouse_buttons;
        let movement = dx != 0 || dy != 0;

        if buttons_changed || movement {
            serial_write_str(&alloc::format!("USB_MOUSE: pos=({},{}), buttons={:03b}\n", 
                self.mouse_x, self.mouse_y, buttons));
            
            // Enviar evento al InputSystem
            // Crear el MouseEvent usando el enum correcto
            use crate::drivers::usb_mouse;
            
            let position = usb_mouse::MousePosition {
                x: self.mouse_x as i32,
                y: self.mouse_y as i32,
            };
            
            let button_state = usb_mouse::MouseButtonState {
                left: (buttons & 0x01) != 0,
                right: (buttons & 0x02) != 0,
                middle: (buttons & 0x04) != 0,
                side1: false,
                side2: false,
            };
            
            // MouseEvent es un enum, crear el variant correcto
            if movement {
                let event = usb_mouse::MouseEvent::Move { 
                    position,
                    buttons: button_state.clone(),
                };
                let _ = crate::drivers::input_system::push_mouse_event(event, self.slot_id as u32);
            }
            
            if buttons_changed {
                // Detectar qué botón cambió
                let prev_left = (self.last_mouse_buttons & 0x01) != 0;
                let prev_right = (self.last_mouse_buttons & 0x02) != 0;
                let prev_middle = (self.last_mouse_buttons & 0x04) != 0;
                
                if ((buttons & 0x01) != 0) != prev_left {
                    let button = usb_mouse::MouseButton::Left;
                    let event = if (buttons & 0x01) != 0 {
                        usb_mouse::MouseEvent::ButtonPress { button, position }
                    } else {
                        usb_mouse::MouseEvent::ButtonRelease { button, position }
                    };
                    let _ = crate::drivers::input_system::push_mouse_event(event, self.slot_id as u32);
                }
                
                if ((buttons & 0x02) != 0) != prev_right {
                    let button = usb_mouse::MouseButton::Right;
                    let event = if (buttons & 0x02) != 0 {
                        usb_mouse::MouseEvent::ButtonPress { button, position }
                    } else {
                        usb_mouse::MouseEvent::ButtonRelease { button, position }
                    };
                    let _ = crate::drivers::input_system::push_mouse_event(event, self.slot_id as u32);
                }
                
                if ((buttons & 0x04) != 0) != prev_middle {
                    let button = usb_mouse::MouseButton::Middle;
                    let event = if (buttons & 0x04) != 0 {
                        usb_mouse::MouseEvent::ButtonPress { button, position }
                    } else {
                        usb_mouse::MouseEvent::ButtonRelease { button, position }
                    };
                    let _ = crate::drivers::input_system::push_mouse_event(event, self.slot_id as u32);
                }
            }
            
            self.last_mouse_buttons = buttons;
        }
    }
}

/// Convierte byte de modifiers HID a ModifierState
fn hid_modifiers_to_state(hid_mods: u8) -> crate::drivers::usb_keyboard::ModifierState {
    crate::drivers::usb_keyboard::ModifierState {
        left_shift: (hid_mods & 0x02) != 0,
        right_shift: (hid_mods & 0x20) != 0,
        left_ctrl: (hid_mods & 0x01) != 0,
        right_ctrl: (hid_mods & 0x10) != 0,
        left_alt: (hid_mods & 0x04) != 0,
        right_alt: (hid_mods & 0x40) != 0,
        left_meta: (hid_mods & 0x08) != 0,
        right_meta: (hid_mods & 0x80) != 0,
        caps_lock: false, // No viene en el modifier byte
        num_lock: false,
        scroll_lock: false,
    }
}

/// Convierte un USB HID keycode a carácter ASCII
fn usb_keycode_to_char(usb_code: u8, shift: bool) -> char {
    match usb_code {
        0x04..=0x1D if !shift => (b'a' + (usb_code - 0x04)) as char,
        0x04..=0x1D if shift => (b'A' + (usb_code - 0x04)) as char,
        
        0x1E => if shift { '!' } else { '1' },
        0x1F => if shift { '@' } else { '2' },
        0x20 => if shift { '#' } else { '3' },
        0x21 => if shift { '$' } else { '4' },
        0x22 => if shift { '%' } else { '5' },
        0x23 => if shift { '^' } else { '6' },
        0x24 => if shift { '&' } else { '7' },
        0x25 => if shift { '*' } else { '8' },
        0x26 => if shift { '(' } else { '9' },
        0x27 => if shift { ')' } else { '0' },
        
        0x28 => '\n',  // Enter
        0x29 => '\x1b', // Escape
        0x2A => '\x08', // Backspace
        0x2B => '\t',   // Tab
        0x2C => ' ',    // Space
        
        0x2D => if shift { '_' } else { '-' },
        0x2E => if shift { '+' } else { '=' },
        0x2F => if shift { '{' } else { '[' },
        0x30 => if shift { '}' } else { ']' },
        0x31 => if shift { '|' } else { '\\' },
        0x33 => if shift { ':' } else { ';' },
        0x34 => if shift { '"' } else { '\'' },
        0x35 => if shift { '~' } else { '`' },
        0x36 => if shift { '<' } else { ',' },
        0x37 => if shift { '>' } else { '.' },
        0x38 => if shift { '?' } else { '/' },
        
        _ => '\0',
    }
}

/// Manager global de dispositivos HID
static mut HID_DEVICES: Option<Vec<HidDevice>> = None;

/// Inicializa el subsistema USB HID
pub fn init_usb_hid() -> Result<(), &'static str> {
    unsafe {
        HID_DEVICES = Some(Vec::new());
    }
    Ok(())
}

/// Registra un nuevo dispositivo HID (teclado o ratón)
pub fn register_hid_device(device: HidDevice) -> Result<(), &'static str> {
    unsafe {
        if let Some(devices) = &mut HID_DEVICES {
            devices.push(device);
            Ok(())
        } else {
            Err("USB HID not initialized")
        }
    }
}

/// Procesa datos recibidos de un dispositivo HID
pub fn process_hid_data(slot_id: u8, endpoint: u8, data: &[u8]) {
    unsafe {
        if let Some(devices) = &mut HID_DEVICES {
            for device in devices.iter_mut() {
                if device.slot_id == slot_id && device.endpoint_in == endpoint {
                    match device.device_type {
                        HidDeviceType::Keyboard => device.process_keyboard_report(data),
                        HidDeviceType::Mouse => device.process_mouse_report(data),
                        HidDeviceType::Unknown => {},
                    }
                    break;
                }
            }
        }
    }
}

/// Obtiene estadísticas de dispositivos HID
pub fn get_hid_stats() -> (usize, usize, usize) {
    unsafe {
        if let Some(devices) = &HID_DEVICES {
            let keyboards = devices.iter().filter(|d| d.device_type == HidDeviceType::Keyboard).count();
            let mice = devices.iter().filter(|d| d.device_type == HidDeviceType::Mouse).count();
            (devices.len(), keyboards, mice)
        } else {
            (0, 0, 0)
        }
    }
}

/// Polling de dispositivos USB HID (sin interrupciones)
/// Lee datos de los dispositivos USB registrados y procesa los reportes.
/// Esta función es segura para llamar desde el main loop sin causar deadlocks.
pub fn poll_usb_hid_devices() -> usize {
    use crate::drivers::usb_hid_reader::process_completed_transfers;
    let mut events_processed = 0;
    
    // Procesar transferencias completadas desde el Event Ring de XHCI
    let completed = process_completed_transfers();
    
    unsafe {
        if let Some(devices) = &mut HID_DEVICES {
            for (slot_id, endpoint, data) in completed {
                // Buscar el dispositivo correspondiente
                for device in devices.iter_mut() {
                    if device.slot_id == slot_id && device.endpoint_in == endpoint {
                        // Procesar datos según el tipo de dispositivo
                        match device.device_type {
                            HidDeviceType::Keyboard if data.len() >= 8 => {
                                device.process_keyboard_report(&data);
                                events_processed += 1;
                            }
                            HidDeviceType::Mouse if data.len() >= 3 => {
                                device.process_mouse_report(&data);
                                events_processed += 1;
                            }
                            _ => {}
                        }
                        break;
                    }
                }
            }
        }
    }
    
    events_processed
}

/// Detecta y registra automáticamente dispositivos HID desde XHCI
/// Esta función debe llamarse después de que XHCI enumere dispositivos
pub fn detect_and_register_hid_devices() -> Result<usize, &'static str> {
    use crate::debug::serial_write_str;
    use crate::drivers::usb_hid_reader::{configure_hid_endpoint, start_periodic_in_transfers};
    
    // Por ahora, registrar dispositivos simulados basados en lo que QEMU proporciona
    // QEMU emula: usb-kbd en puerto 1, usb-mouse en puerto 2
    
    serial_write_str("USB_HID: Detectando dispositivos HID...\n");
    
    // Registrar teclado USB simulado (puerto 1, slot 1, endpoint 1)
    let keyboard = HidDevice::new_keyboard(1, 1);
    
    // Configurar endpoint del teclado
    if let Err(e) = configure_hid_endpoint(1, 1, 8, 10) {
        serial_write_str(&alloc::format!("USB_HID: Error configurando kbd endpoint: {}\n", e));
    } else {
        // Iniciar transferencias periódicas para el teclado
        if let Err(e) = start_periodic_in_transfers(1, 1) {
            serial_write_str(&alloc::format!("USB_HID: Error iniciando kbd transfers: {}\n", e));
        }
    }
    
    register_hid_device(keyboard)?;
    serial_write_str("USB_HID: Teclado USB registrado (slot 1, ep 1)\n");
    
    // Registrar ratón USB simulado (puerto 2, slot 2, endpoint 1)
    let mouse = HidDevice::new_mouse(2, 1);
    
    // Configurar endpoint del ratón
    if let Err(e) = configure_hid_endpoint(2, 1, 4, 10) {
        serial_write_str(&alloc::format!("USB_HID: Error configurando mouse endpoint: {}\n", e));
    } else {
        // Iniciar transferencias periódicas para el ratón
        if let Err(e) = start_periodic_in_transfers(2, 1) {
            serial_write_str(&alloc::format!("USB_HID: Error iniciando mouse transfers: {}\n", e));
        }
    }
    
    register_hid_device(mouse)?;
    serial_write_str("USB_HID: Ratón USB registrado (slot 2, ep 1)\n");
    
    Ok(2) // 2 dispositivos registrados
}
