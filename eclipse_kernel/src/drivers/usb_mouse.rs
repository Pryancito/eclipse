//! Driver USB para ratón
//! 
//! Implementa soporte completo para ratones USB con funcionalidades avanzadas.

use crate::drivers::framebuffer::{FramebufferDriver, Color};
use crate::syslog;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;

/// Botones del ratón
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Side1,
    Side2,
    WheelUp,
    WheelDown,
}

/// Posición del mouse
pub type MousePosition = (i32, i32);

/// Evento de mouse
#[derive(Debug, Clone, PartialEq)]
pub enum MouseEvent {
    ButtonPress { button: MouseButton, position: MousePosition },
    ButtonRelease { button: MouseButton, position: MousePosition },
    Move { position: MousePosition, buttons: MouseButtonState },
    Scroll { delta: i32, position: MousePosition },
}

/// Datos de mouse
#[derive(Debug, Clone, PartialEq)]
pub struct MouseData {
    pub button: Option<MouseButton>,
    pub position: MousePosition,
    pub x: i32,
    pub y: i32,
    pub pressed: bool,
    pub timestamp: u64,
}

/// Estado de los botones del ratón
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseButtonState {
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    pub side1: bool,
    pub side2: bool,
}

impl Default for MouseButtonState {
    fn default() -> Self {
        Self {
            left: false,
            right: false,
            middle: false,
            side1: false,
            side2: false,
        }
    }
}

// MouseEvent ya está definido arriba

/// Configuración del ratón
#[derive(Debug, Clone)]
pub struct MouseConfig {
    pub sensitivity: f32,
    pub acceleration: f32,
    pub double_click_time: u64,
    pub scroll_sensitivity: f32,
    pub enable_side_buttons: bool,
    pub enable_wheel: bool,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            sensitivity: 1.0,
            acceleration: 1.0,
            double_click_time: 500, // 500ms
            scroll_sensitivity: 1.0,
            enable_side_buttons: true,
            enable_wheel: true,
        }
    }
}

/// Driver USB para ratón
#[derive(Debug)]
pub struct UsbMouseDriver {
    device_id: u32,
    config: MouseConfig,
    current_position: (i32, i32),
    last_position: (i32, i32),
    button_state: MouseButtonState,
    last_click_time: u64,
    click_count: u32,
    event_queue: Vec<MouseEvent>,
    max_events: usize,
    is_initialized: bool,
}

impl UsbMouseDriver {
    pub fn new(device_id: u32) -> Self {
        Self {
            device_id,
            config: MouseConfig::default(),
            current_position: (0, 0),
            last_position: (0, 0),
            button_state: MouseButtonState::default(),
            last_click_time: 0,
            click_count: 0,
            event_queue: Vec::new(),
            max_events: 100,
            is_initialized: false,
        }
    }

    /// Inicializar el driver del ratón
    pub fn initialize(&mut self) -> Result<(), String> {
        if self.is_initialized {
            return Err("El driver del ratón ya está inicializado".to_string());
        }

        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_MOUSE", &alloc::format!(
            "Inicializando driver USB para ratón (ID: {})",
            self.device_id
        ));

        // Simular inicialización del dispositivo USB
        self.current_position = (400, 300); // Posición inicial en el centro
        self.last_position = self.current_position;
        self.is_initialized = true;

        syslog::log_kernel(syslog::SyslogSeverity::Info, "USB_MOUSE", "Driver USB para ratón inicializado correctamente");
        Ok(())
    }

    /// Procesar datos del ratón USB
    pub fn process_mouse_data(&mut self, data: &[u8]) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del ratón no está inicializado".to_string());
        }

        if data.len() < 3 {
            return Err("Datos del ratón insuficientes".to_string());
        }

        // Parsear datos del ratón (formato estándar USB HID)
        let button_state = data[0];
        let x_movement = data[1] as i8 as i32;
        let y_movement = data[2] as i8 as i32;

        // Actualizar estado de botones
        self.button_state.left = (button_state & 0x01) != 0;
        self.button_state.right = (button_state & 0x02) != 0;
        self.button_state.middle = (button_state & 0x04) != 0;

        // Calcular nueva posición con sensibilidad
        let sensitivity = self.config.sensitivity * self.config.acceleration;
        let new_x = self.current_position.0 + (x_movement as f32 * sensitivity) as i32;
        let new_y = self.current_position.1 + (y_movement as f32 * sensitivity) as i32;

        // Actualizar posición
        self.last_position = self.current_position;
        self.current_position = (new_x, new_y);

        // Crear evento del ratón
        let event = MouseEvent::Move { 
            position: self.current_position, 
            buttons: self.button_state 
        };

        // Agregar evento a la cola
        self.add_event(event);

        Ok(())
    }

    /// Obtener posición actual del ratón
    pub fn get_position(&self) -> (i32, i32) {
        self.current_position
    }

    /// Obtener estado de los botones
    pub fn get_button_state(&self) -> MouseButtonState {
        self.button_state
    }

    /// Verificar si un botón está presionado
    pub fn is_button_pressed(&self, button: MouseButton) -> bool {
        match button {
            MouseButton::Left => self.button_state.left,
            MouseButton::Right => self.button_state.right,
            MouseButton::Middle => self.button_state.middle,
            MouseButton::Side1 => self.button_state.side1,
            MouseButton::Side2 => self.button_state.side2,
            _ => false,
        }
    }

    /// Obtener siguiente evento del ratón
    pub fn get_next_event(&mut self) -> Option<MouseEvent> {
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

    /// Actualizar configuración del ratón
    pub fn update_config(&mut self, new_config: MouseConfig) {
        self.config = new_config;
    }

    /// Obtener configuración actual
    pub fn get_config(&self) -> &MouseConfig {
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

    /// Obtener estadísticas del ratón
    pub fn get_stats(&self) -> String {
        alloc::format!(
            "Ratón USB (ID: {}) - Posición: ({}, {}), Eventos: {}, Botones: L:{} R:{} M:{}",
            self.device_id,
            self.current_position.0,
            self.current_position.1,
            self.event_count(),
            self.button_state.left,
            self.button_state.right,
            self.button_state.middle
        )
    }

    /// Simular movimiento del ratón (para testing)
    pub fn simulate_movement(&mut self, delta_x: i32, delta_y: i32) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del ratón no está inicializado".to_string());
        }

        let sensitivity = self.config.sensitivity * self.config.acceleration;
        let new_x = self.current_position.0 + (delta_x as f32 * sensitivity) as i32;
        let new_y = self.current_position.1 + (delta_y as f32 * sensitivity) as i32;

        self.last_position = self.current_position;
        self.current_position = (new_x, new_y);

        let event = MouseEvent::Move { 
            position: self.current_position, 
            buttons: self.button_state 
        };

        self.add_event(event);
        Ok(())
    }

    /// Simular clic del ratón (para testing)
    pub fn simulate_click(&mut self, button: MouseButton) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del ratón no está inicializado".to_string());
        }

        // Actualizar estado del botón
        match button {
            MouseButton::Left => self.button_state.left = true,
            MouseButton::Right => self.button_state.right = true,
            MouseButton::Middle => self.button_state.middle = true,
            MouseButton::Side1 => self.button_state.side1 = true,
            MouseButton::Side2 => self.button_state.side2 = true,
            _ => {}
        }

        let event = MouseEvent::Move { 
            position: self.current_position, 
            buttons: self.button_state 
        };

        self.add_event(event);
        Ok(())
    }

    /// Simular liberación del botón del ratón (para testing)
    pub fn simulate_release(&mut self, button: MouseButton) -> Result<(), String> {
        if !self.is_initialized {
            return Err("El driver del ratón no está inicializado".to_string());
        }

        // Actualizar estado del botón
        match button {
            MouseButton::Left => self.button_state.left = false,
            MouseButton::Right => self.button_state.right = false,
            MouseButton::Middle => self.button_state.middle = false,
            MouseButton::Side1 => self.button_state.side1 = false,
            MouseButton::Side2 => self.button_state.side2 = false,
            _ => {}
        }

        let event = MouseEvent::Move { 
            position: self.current_position, 
            buttons: self.button_state 
        };

        self.add_event(event);
        Ok(())
    }

    // Métodos auxiliares privados

    fn add_event(&mut self, event: MouseEvent) {
        if self.event_queue.len() >= self.max_events {
            self.event_queue.remove(0); // Remover evento más antiguo
        }
        self.event_queue.push(event);
    }

    fn get_timestamp(&self) -> u64 {
        // Simular timestamp (en un sistema real usaría un reloj del sistema)
        self.last_click_time + 1
    }

    /// Establecer sensibilidad del ratón
    pub fn set_sensitivity(&mut self, sensitivity: f32) {
        self.config.sensitivity = sensitivity;
    }
}

impl MouseEvent {
    /// Convertir a MouseEvent del input_system
    pub fn to_input_system_mouse_event(&self) -> crate::drivers::input_system::MouseEvent {
        match self {
            MouseEvent::ButtonPress { button, position } => crate::drivers::input_system::MouseEvent {
                button: Some(*button),
                position: *position,
                pressed: true,
                timestamp: 0, // Se establecerá en el input_system
            },
            MouseEvent::ButtonRelease { button, position } => crate::drivers::input_system::MouseEvent {
                button: Some(*button),
                position: *position,
                pressed: false,
                timestamp: 0, // Se establecerá en el input_system
            },
            MouseEvent::Move { position, .. } => crate::drivers::input_system::MouseEvent {
                button: None,
                position: *position,
                pressed: false,
                timestamp: 0, // Se establecerá en el input_system
            },
            MouseEvent::Scroll { .. } => crate::drivers::input_system::MouseEvent {
                button: None,
                position: (0, 0),
                pressed: false,
                timestamp: 0, // Se establecerá en el input_system
            },
        }
    }
}