#![no_std]

use core::ptr;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::boxed::Box;

/// Driver de mouse USB para Eclipse OS
/// Implementa el protocolo HID (Human Interface Device) para mouse USB

/// Botones del mouse
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseButton {
    Left = 0x01,
    Right = 0x02,
    Middle = 0x04,
    Button4 = 0x08,
    Button5 = 0x10,
    Button6 = 0x20,
    Button7 = 0x40,
    Button8 = 0x80,
}

impl MouseButton {
    /// Obtener nombre del botón
    pub fn name(&self) -> &'static str {
        match self {
            MouseButton::Left => "Left",
            MouseButton::Right => "Right",
            MouseButton::Middle => "Middle",
            MouseButton::Button4 => "Button4",
            MouseButton::Button5 => "Button5",
            MouseButton::Button6 => "Button6",
            MouseButton::Button7 => "Button7",
            MouseButton::Button8 => "Button8",
        }
    }
}

/// Estado de los botones del mouse
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseButtonState {
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    pub button4: bool,
    pub button5: bool,
    pub button6: bool,
    pub button7: bool,
    pub button8: bool,
}

impl MouseButtonState {
    pub fn new() -> Self {
        Self {
            left: false,
            right: false,
            middle: false,
            button4: false,
            button5: false,
            button6: false,
            button7: false,
            button8: false,
        }
    }
    
    /// Verificar si un botón específico está presionado
    pub fn is_pressed(&self, button: MouseButton) -> bool {
        match button {
            MouseButton::Left => self.left,
            MouseButton::Right => self.right,
            MouseButton::Middle => self.middle,
            MouseButton::Button4 => self.button4,
            MouseButton::Button5 => self.button5,
            MouseButton::Button6 => self.button6,
            MouseButton::Button7 => self.button7,
            MouseButton::Button8 => self.button8,
        }
    }
    
    /// Actualizar estado desde byte de botones
    pub fn from_byte(&mut self, buttons: u8) {
        self.left = (buttons & MouseButton::Left as u8) != 0;
        self.right = (buttons & MouseButton::Right as u8) != 0;
        self.middle = (buttons & MouseButton::Middle as u8) != 0;
        self.button4 = (buttons & MouseButton::Button4 as u8) != 0;
        self.button5 = (buttons & MouseButton::Button5 as u8) != 0;
        self.button6 = (buttons & MouseButton::Button6 as u8) != 0;
        self.button7 = (buttons & MouseButton::Button7 as u8) != 0;
        self.button8 = (buttons & MouseButton::Button8 as u8) != 0;
    }
    
    /// Convertir a byte
    pub fn to_byte(&self) -> u8 {
        let mut result = 0;
        if self.left { result |= MouseButton::Left as u8; }
        if self.right { result |= MouseButton::Right as u8; }
        if self.middle { result |= MouseButton::Middle as u8; }
        if self.button4 { result |= MouseButton::Button4 as u8; }
        if self.button5 { result |= MouseButton::Button5 as u8; }
        if self.button6 { result |= MouseButton::Button6 as u8; }
        if self.button7 { result |= MouseButton::Button7 as u8; }
        if self.button8 { result |= MouseButton::Button8 as u8; }
        result
    }
}

/// Posición del mouse
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MousePosition {
    pub x: i32,
    pub y: i32,
}

impl MousePosition {
    pub fn new() -> Self {
        Self { x: 0, y: 0 }
    }
    
    pub fn new_xy(x: i32, y: i32) -> Self {
        Self { x, y }
    }
    
    /// Mover posición relativa
    pub fn move_relative(&mut self, dx: i32, dy: i32) {
        self.x += dx;
        self.y += dy;
    }
    
    /// Establecer posición absoluta
    pub fn set_absolute(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }
}

/// Rueda del mouse
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseWheel {
    pub vertical: i8,
    pub horizontal: i8,
}

impl MouseWheel {
    pub fn new() -> Self {
        Self { vertical: 0, horizontal: 0 }
    }
    
    /// Actualizar desde datos del mouse
    pub fn update(&mut self, vertical: i8, horizontal: i8) {
        self.vertical = vertical;
        self.horizontal = horizontal;
    }
}

/// Evento del mouse
#[derive(Debug, Clone, PartialEq)]
pub enum MouseEvent {
    Move { position: MousePosition, buttons: MouseButtonState },
    ButtonPress { button: MouseButton, position: MousePosition, buttons: MouseButtonState },
    ButtonRelease { button: MouseButton, position: MousePosition, buttons: MouseButtonState },
    Wheel { wheel: MouseWheel, position: MousePosition, buttons: MouseButtonState },
}

/// Información del mouse USB
#[derive(Debug, Clone)]
pub struct UsbMouseInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: String,
    pub product: String,
    pub version: u16,
    pub max_packet_size: u8,
    pub polling_interval: u8,
    pub num_buttons: u8,
    pub resolution: (u16, u16), // DPI horizontal y vertical
    pub max_speed: u16, // Píxeles por segundo
}

/// Driver de mouse USB
#[derive(Debug)]
pub struct UsbMouseDriver {
    pub info: UsbMouseInfo,
    pub device_address: u8,
    pub endpoint_address: u8,
    pub position: MousePosition,
    pub button_state: MouseButtonState,
    pub wheel: MouseWheel,
    pub event_buffer: VecDeque<MouseEvent>,
    pub initialized: bool,
    pub error_count: u32,
    pub sensitivity: f32, // Multiplicador de sensibilidad
}

impl UsbMouseDriver {
    /// Crear nuevo driver de mouse USB
    pub fn new(vendor_id: u16, product_id: u16, device_address: u8, endpoint_address: u8) -> Self {
        Self {
            info: UsbMouseInfo {
                vendor_id,
                product_id,
                manufacturer: String::new(),
                product: String::new(),
                version: 0,
                max_packet_size: 4, // Típico para mouse básico
                polling_interval: 8, // 125 Hz
                num_buttons: 3,
                resolution: (800, 800), // DPI por defecto
                max_speed: 1000,
            },
            device_address,
            endpoint_address,
            position: MousePosition::new(),
            button_state: MouseButtonState::new(),
            wheel: MouseWheel::new(),
            event_buffer: VecDeque::new(),
            initialized: false,
            error_count: 0,
            sensitivity: 1.0,
        }
    }
    
    /// Inicializar el mouse USB
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Configurar endpoint de interrupción
        self.configure_endpoint()?;
        
        // Configurar resolución y sensibilidad
        self.configure_resolution()?;
        
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
    
    /// Configurar resolución del mouse
    fn configure_resolution(&mut self) -> Result<(), &'static str> {
        // En una implementación real, aquí se configuraría la resolución DPI
        // Por ahora usamos valores por defecto
        Ok(())
    }
    
    /// Iniciar polling del mouse
    fn start_polling(&mut self) -> Result<(), &'static str> {
        // En una implementación real, aquí se configuraría el polling USB
        // Por ahora simulamos el inicio del polling
        Ok(())
    }
    
    /// Procesar datos recibidos del mouse
    pub fn process_mouse_data(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() < 4 {
            return Err("Datos insuficientes");
        }
        
        // Parsear datos HID del mouse
        let buttons = data[0];
        let x_delta = data[1] as i8 as i32;
        let y_delta = data[2] as i8 as i32;
        let wheel_delta = data[3] as i8;
        
        // Actualizar estado de botones
        let previous_buttons = self.button_state;
        self.button_state.from_byte(buttons);
        
        // Detectar cambios en botones
        self.detect_button_changes(previous_buttons);
        
        // Procesar movimiento
        if x_delta != 0 || y_delta != 0 {
            self.process_movement(x_delta, y_delta);
        }
        
        // Procesar rueda
        if wheel_delta != 0 {
            self.process_wheel(wheel_delta);
        }
        
        Ok(())
    }
    
    /// Detectar cambios en botones
    fn detect_button_changes(&mut self, previous_buttons: MouseButtonState) {
        let buttons = [
            (MouseButton::Left, previous_buttons.left, self.button_state.left),
            (MouseButton::Right, previous_buttons.right, self.button_state.right),
            (MouseButton::Middle, previous_buttons.middle, self.button_state.middle),
            (MouseButton::Button4, previous_buttons.button4, self.button_state.button4),
            (MouseButton::Button5, previous_buttons.button5, self.button_state.button5),
            (MouseButton::Button6, previous_buttons.button6, self.button_state.button6),
            (MouseButton::Button7, previous_buttons.button7, self.button_state.button7),
            (MouseButton::Button8, previous_buttons.button8, self.button_state.button8),
        ];
        
        for (button, was_pressed, is_pressed) in buttons {
            if !was_pressed && is_pressed {
                // Botón presionado
                let event = MouseEvent::ButtonPress {
                    button,
                    position: self.position,
                    buttons: self.button_state,
                };
                self.event_buffer.push_back(event);
            } else if was_pressed && !is_pressed {
                // Botón liberado
                let event = MouseEvent::ButtonRelease {
                    button,
                    position: self.position,
                    buttons: self.button_state,
                };
                self.event_buffer.push_back(event);
            }
        }
    }
    
    /// Procesar movimiento del mouse
    fn process_movement(&mut self, x_delta: i32, y_delta: i32) {
        // Aplicar sensibilidad
        let adjusted_x = (x_delta as f32 * self.sensitivity) as i32;
        let adjusted_y = (y_delta as f32 * self.sensitivity) as i32;
        
        // Actualizar posición
        self.position.move_relative(adjusted_x, adjusted_y);
        
        // Crear evento de movimiento
        let event = MouseEvent::Move {
            position: self.position,
            buttons: self.button_state,
        };
        self.event_buffer.push_back(event);
    }
    
    /// Procesar rueda del mouse
    fn process_wheel(&mut self, wheel_delta: i8) {
        self.wheel.update(wheel_delta, 0); // Solo rueda vertical por ahora
        
        let event = MouseEvent::Wheel {
            wheel: self.wheel,
            position: self.position,
            buttons: self.button_state,
        };
        self.event_buffer.push_back(event);
    }
    
    /// Obtener siguiente evento del buffer
    pub fn get_next_event(&mut self) -> Option<MouseEvent> {
        self.event_buffer.pop_front()
    }
    
    /// Verificar si hay eventos pendientes
    pub fn has_events(&self) -> bool {
        !self.event_buffer.is_empty()
    }
    
    /// Obtener posición actual del mouse
    pub fn get_position(&self) -> MousePosition {
        self.position
    }
    
    /// Establecer posición del mouse
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.position.set_absolute(x, y);
    }
    
    /// Obtener estado actual de los botones
    pub fn get_button_state(&self) -> MouseButtonState {
        self.button_state
    }
    
    /// Verificar si un botón específico está presionado
    pub fn is_button_pressed(&self, button: MouseButton) -> bool {
        self.button_state.is_pressed(button)
    }
    
    /// Establecer sensibilidad del mouse
    pub fn set_sensitivity(&mut self, sensitivity: f32) {
        self.sensitivity = sensitivity.max(0.1).min(10.0); // Limitar entre 0.1 y 10.0
    }
    
    /// Obtener sensibilidad actual
    pub fn get_sensitivity(&self) -> f32 {
        self.sensitivity
    }
    
    /// Verificar si el mouse está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    
    /// Obtener información del mouse
    pub fn get_info(&self) -> &UsbMouseInfo {
        &self.info
    }
    
    /// Limpiar buffer de eventos
    pub fn clear_buffer(&mut self) {
        self.event_buffer.clear();
    }
    
    /// Obtener número de eventos en el buffer
    pub fn event_count(&self) -> usize {
        self.event_buffer.len()
    }
    
    /// Obtener estado de la rueda
    pub fn get_wheel_state(&self) -> MouseWheel {
        self.wheel
    }
    
    /// Resetear posición del mouse
    pub fn reset_position(&mut self) {
        self.position = MousePosition::new();
    }
    
    /// Obtener estadísticas del mouse
    pub fn get_stats(&self) -> MouseStats {
        MouseStats {
            total_events: self.event_buffer.len(),
            error_count: self.error_count,
            position: self.position,
            button_state: self.button_state,
            sensitivity: self.sensitivity,
        }
    }
}

/// Estadísticas del mouse
#[derive(Debug, Clone)]
pub struct MouseStats {
    pub total_events: usize,
    pub error_count: u32,
    pub position: MousePosition,
    pub button_state: MouseButtonState,
    pub sensitivity: f32,
}

/// Función de conveniencia para crear un driver de mouse USB
pub fn create_usb_mouse_driver(vendor_id: u16, product_id: u16, device_address: u8, endpoint_address: u8) -> UsbMouseDriver {
    UsbMouseDriver::new(vendor_id, product_id, device_address, endpoint_address)
}
