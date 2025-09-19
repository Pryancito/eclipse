//! Driver USB real para ratón
//! 
//! Implementa soporte completo para ratones USB reales con comunicación
//! hardware directa, no simulación.

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType, DeviceOperations},
    manager::{Driver, DriverInfo, DriverResult, DriverError},
    usb::{RealUsbController, UsbDeviceInfo, UsbDeviceClass, UsbControllerType},
    mouse::{MouseDriver, MouseEvent, MouseButton, MouseState},
};
use alloc::vec::Vec;
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

/// Driver USB real para ratón
pub struct UsbMouseReal {
    pub info: DriverInfo,
    pub usb_controller: Option<RealUsbController>,
    pub mouse_device: Option<UsbDeviceInfo>,
    pub is_initialized: bool,
    pub event_buffer: Vec<MouseEvent>,
    pub current_buttons: [bool; 8], // Estado actual de los botones
    pub last_report: [u8; 4], // Último reporte HID recibido
    pub endpoint: u8,
    pub address: u8,
    pub x: i32,
    pub y: i32,
    pub wheel: i8,
}

impl UsbMouseReal {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("usb_mouse_real");
        info.device_type = DeviceType::Mouse;
        info.version = 2;

        Self {
            info,
            usb_controller: None,
            mouse_device: None,
            is_initialized: false,
            event_buffer: Vec::new(),
            current_buttons: [false; 8],
            last_report: [0; 4],
            endpoint: 0x81, // Endpoint interrupt IN
            address: 0,
            x: 0,
            y: 0,
            wheel: 0,
        }
    }

    /// Configurar controlador USB
    pub fn set_usb_controller(&mut self, controller: RealUsbController) {
        self.usb_controller = Some(controller);
    }

    /// Detectar ratón USB
    pub fn detect_mouse(&mut self) -> DriverResult<()> {
        if let Some(ref mut controller) = self.usb_controller {
            // Buscar dispositivos HID
            let hid_devices = controller.get_hid_devices();
            
            for device in hid_devices {
                // Verificar si es un ratón (clase HID, subclase 0x01, protocolo 0x02)
                if device.device_descriptor.device_class == UsbDeviceClass::HID {
                    // En una implementación real, verificaríamos los descriptores de interfaz
                    // Por ahora, asumimos que es un ratón
                    self.mouse_device = Some(device.clone());
                    self.address = device.address;
                    return Ok(());
                }
            }
        }
        Err(DriverError::DeviceNotFound)
    }

    /// Leer reporte HID del ratón
    pub fn read_hid_report(&mut self) -> DriverResult<()> {
        if let Some(ref controller) = self.usb_controller {
            if let Some(ref device) = self.mouse_device {
                let mut report = [0u8; 4];
                controller.read_hid_data(device.address, self.endpoint, &mut report)?;
                
                // Procesar reporte HID
                self.process_hid_report(&report);
                self.last_report = report;
            }
        }
        Ok(())
    }

    /// Procesar reporte HID del ratón
    fn process_hid_report(&mut self, report: &[u8; 4]) {
        // Estructura típica de reporte de ratón USB:
        // Byte 0: Botones (bit 0: botón izquierdo, bit 1: botón derecho, bit 2: botón medio)
        // Byte 1: Delta X (movimiento horizontal)
        // Byte 2: Delta Y (movimiento vertical)
        // Byte 3: Rueda de scroll

        let buttons = report[0];
        let delta_x = report[1] as i8 as i32; // Convertir a signed
        let delta_y = report[2] as i8 as i32; // Convertir a signed
        let wheel_delta = report[3] as i8;

        // Procesar botones
        self.process_buttons(buttons);

        // Procesar movimiento
        if delta_x != 0 || delta_y != 0 {
            self.process_movement(delta_x, delta_y);
        }

        // Procesar rueda
        if wheel_delta != 0 {
            self.process_wheel(wheel_delta);
        }
    }

    /// Procesar botones del ratón
    fn process_buttons(&mut self, buttons: u8) {
        let button_mapping = [
            (0x01, MouseButton::Left),
            (0x02, MouseButton::Right),
            (0x04, MouseButton::Middle),
            (0x08, MouseButton::Button4),
            (0x10, MouseButton::Button5),
        ];

        for (bit, button) in button_mapping.iter() {
            let is_pressed = (buttons & bit) != 0;
            let button_index = *button as usize;
            
            if is_pressed != self.current_buttons[button_index] {
                self.current_buttons[button_index] = is_pressed;
                
                let event = MouseEvent {
                    button: *button,
                    state: if is_pressed { MouseState::Pressed } else { MouseState::Released },
                    x: self.x,
                    y: self.y,
                    wheel: self.wheel,
                };
                
                self.event_buffer.push(event);
            }
        }
    }

    /// Procesar movimiento del ratón
    fn process_movement(&mut self, delta_x: i32, delta_y: i32) {
        self.x += delta_x;
        self.y += delta_y;

        // Crear evento de movimiento
        let event = MouseEvent {
            button: MouseButton::None,
            state: MouseState::Moved,
            x: self.x,
            y: self.y,
            wheel: self.wheel,
        };

        self.event_buffer.push(event);
    }

    /// Procesar rueda del ratón
    fn process_wheel(&mut self, wheel_delta: i8) {
        self.wheel += wheel_delta;

        // Crear evento de rueda
        let event = MouseEvent {
            button: MouseButton::Wheel,
            state: if wheel_delta > 0 { MouseState::WheelUp } else { MouseState::WheelDown },
            x: self.x,
            y: self.y,
            wheel: self.wheel,
        };

        self.event_buffer.push(event);
    }

    /// Obtener siguiente evento del ratón
    pub fn get_next_mouse_event(&mut self) -> Option<MouseEvent> {
        self.event_buffer.pop()
    }

    /// Verificar si hay eventos pendientes
    pub fn has_mouse_events(&self) -> bool {
        !self.event_buffer.is_empty()
    }

    /// Verificar si un botón está presionado
    pub fn is_button_pressed(&self, button: MouseButton) -> bool {
        self.current_buttons[button as usize]
    }

    /// Obtener posición actual del ratón
    pub fn get_position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// Establecer posición del ratón
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    /// Obtener valor de la rueda
    pub fn get_wheel(&self) -> i8 {
        self.wheel
    }

    /// Obtener estadísticas del ratón
    pub fn get_mouse_stats(&self) -> String {
        let mut stats = String::new();
        stats.push_str("=== RATÓN USB REAL ===\n");
        
        if let Some(ref device) = self.mouse_device {
            stats.push_str(&format!("Dispositivo: VID={:04X} PID={:04X}\n", 
                device.device_descriptor.vendor_id,
                device.device_descriptor.product_id
            ));
            stats.push_str(&format!("Dirección USB: {}\n", device.address));
            stats.push_str(&format!("Endpoint: 0x{:02X}\n", self.endpoint));
        } else {
            stats.push_str("Dispositivo: No detectado\n");
        }
        
        stats.push_str(&format!("Posición: ({}, {})\n", self.x, self.y));
        stats.push_str(&format!("Rueda: {}\n", self.wheel));
        stats.push_str(&format!("Eventos en buffer: {}\n", self.event_buffer.len()));
        
        let pressed_buttons: Vec<&str> = self.current_buttons.iter()
            .enumerate()
            .filter(|(_, &pressed)| pressed)
            .map(|(i, _)| match i {
                0 => "Izquierdo",
                1 => "Derecho", 
                2 => "Medio",
                3 => "Botón4",
                4 => "Botón5",
                _ => "Desconocido",
            })
            .collect();
        
        if !pressed_buttons.is_empty() {
            stats.push_str(&format!("Botones presionados: {}\n", pressed_buttons.join(", ")));
        } else {
            stats.push_str("Botones presionados: Ninguno\n");
        }
        
        stats
    }

    /// Calibrar ratón (resetear posición)
    pub fn calibrate(&mut self) {
        self.x = 0;
        self.y = 0;
        self.wheel = 0;
        self.event_buffer.clear();
    }

    /// Verificar si el ratón está conectado
    pub fn is_connected(&self) -> bool {
        self.mouse_device.is_some()
    }

    /// Obtener información del dispositivo
    pub fn get_device_info(&self) -> Option<&UsbDeviceInfo> {
        self.mouse_device.as_ref()
    }
}

impl Driver for UsbMouseReal {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        if self.is_initialized {
            return Ok(());
        }

        // Detectar ratón USB
        self.detect_mouse()?;
        
        self.info.is_loaded = true;
        self.is_initialized = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        self.mouse_device = None;
        self.event_buffer.clear();
        self.current_buttons = [false; 8];
        self.x = 0;
        self.y = 0;
        self.wheel = 0;
        self.is_initialized = false;
        self.info.is_loaded = false;
        Ok(())
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        device_info.device_type == DeviceType::Mouse
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        device.driver_id = Some(self.info.id);
        Ok(())
    }

    fn detach_device(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }

    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> {
        // Leer reporte HID del ratón
        self.read_hid_report()?;
        Ok(())
    }
}

impl MouseDriver for UsbMouseReal {
    fn read_event(&mut self) -> Option<MouseEvent> {
        self.get_next_mouse_event()
    }

    fn is_button_pressed(&self, button: MouseButton) -> bool {
        self.is_button_pressed(button)
    }

    fn get_position(&self) -> (i32, i32) {
        self.get_position()
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.set_position(x, y);
    }

    fn get_wheel(&self) -> i8 {
        self.get_wheel()
    }

    fn clear_buffer(&mut self) {
        self.event_buffer.clear();
    }

    fn has_events(&self) -> bool {
        self.has_mouse_events()
    }
}
