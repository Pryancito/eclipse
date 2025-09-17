#![no_std]

use alloc::vec::Vec;
use alloc::string::String;
use alloc::string::ToString;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::boxed::Box;

use crate::drivers::usb_keyboard::{UsbKeyboardDriver, UsbKeyCode};
use crate::drivers::usb_mouse::{UsbMouseDriver, MouseButton};

/// Sistema de entrada unificado para Eclipse OS
/// Gestiona eventos de teclado y mouse de forma centralizada

/// Evento de teclado
#[derive(Debug, Clone, PartialEq)]
pub struct KeyboardEvent {
    pub key_code: UsbKeyCode,
    pub pressed: bool,
    pub timestamp: u64,
}

/// Evento de mouse
#[derive(Debug, Clone, PartialEq)]
pub struct MouseEvent {
    pub button: Option<MouseButton>,
    pub position: (i32, i32),
    pub pressed: bool,
    pub timestamp: u64,
}

/// Evento del sistema
#[derive(Debug, Clone, PartialEq)]
pub enum SystemEvent {
    DeviceConnected,
    DeviceDisconnected,
}

/// Estado de modificadores del teclado
#[derive(Debug, Clone, PartialEq)]
pub struct ModifierState {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// Posición del mouse
pub type MousePosition = (i32, i32);

/// Estado de botones del mouse
pub type MouseButtonState = crate::drivers::usb_mouse::MouseButtonState;

/// Tipo de evento de entrada
#[derive(Debug, Clone, PartialEq)]
pub enum InputEventType {
    Keyboard(KeyboardEvent),
    Mouse(MouseEvent),
    System(SystemEvent),
}

// SystemEvent ya está definido arriba

/// Evento de entrada unificado
#[derive(Debug, Clone, PartialEq)]
pub struct InputEvent {
    pub event_type: InputEventType,
    pub timestamp: u64, // Timestamp en milisegundos
    pub device_id: u32,
    pub processed: bool,
}

impl InputEvent {
    pub fn new(event_type: InputEventType, device_id: u32, timestamp: u64) -> Self {
        Self {
            event_type,
            timestamp,
            device_id,
            processed: false,
        }
    }
    
    /// Marcar evento como procesado
    pub fn mark_processed(&mut self) {
        self.processed = true;
    }
    
    /// Verificar si es evento de teclado
    pub fn is_keyboard(&self) -> bool {
        matches!(self.event_type, InputEventType::Keyboard(_))
    }
    
    /// Verificar si es evento de mouse
    pub fn is_mouse(&self) -> bool {
        matches!(self.event_type, InputEventType::Mouse(_))
    }
    
    /// Verificar si es evento del sistema
    pub fn is_system(&self) -> bool {
        matches!(self.event_type, InputEventType::System(_))
    }
}

/// Configuración del sistema de entrada
#[derive(Debug, Clone)]
pub struct InputSystemConfig {
    pub max_events: usize,
    pub keyboard_repeat_delay: u32, // ms
    pub keyboard_repeat_rate: u32, // eventos por segundo
    pub mouse_sensitivity: f32,
    pub enable_keyboard_repeat: bool,
    pub enable_mouse_acceleration: bool,
}

impl Default for InputSystemConfig {
    fn default() -> Self {
        Self {
            max_events: 1000,
            keyboard_repeat_delay: 500,
            keyboard_repeat_rate: 30,
            mouse_sensitivity: 1.0,
            enable_keyboard_repeat: true,
            enable_mouse_acceleration: true,
        }
    }
}

/// Estadísticas del sistema de entrada
#[derive(Debug, Clone)]
pub struct InputSystemStats {
    pub total_events: u64,
    pub keyboard_events: u64,
    pub mouse_events: u64,
    pub system_events: u64,
    pub events_processed: u64,
    pub events_dropped: u64,
    pub buffer_usage: f32, // Porcentaje de uso del buffer
    pub active_keyboards: u32,
    pub active_mice: u32,
}

/// Sistema de entrada unificado
#[derive(Debug)]
pub struct InputSystem {
    pub config: InputSystemConfig,
    pub event_buffer: VecDeque<InputEvent>,
    pub keyboards: Vec<UsbKeyboardDriver>,
    pub mice: Vec<UsbMouseDriver>,
    pub stats: InputSystemStats,
    pub initialized: bool,
    pub current_timestamp: u64,
}

impl InputSystem {
    /// Crear nuevo sistema de entrada
    pub fn new(config: InputSystemConfig) -> Self {
        Self {
            config,
            event_buffer: VecDeque::new(),
            keyboards: Vec::new(),
            mice: Vec::new(),
            stats: InputSystemStats {
                total_events: 0,
                keyboard_events: 0,
                mouse_events: 0,
                system_events: 0,
                events_processed: 0,
                events_dropped: 0,
                buffer_usage: 0.0,
                active_keyboards: 0,
                active_mice: 0,
            },
            initialized: false,
            current_timestamp: 0,
        }
    }
    
    /// Inicializar el sistema de entrada
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Limpiar buffers
        self.event_buffer.clear();
        self.keyboards.clear();
        self.mice.clear();
        
        // Resetear estadísticas
        self.stats = InputSystemStats {
            total_events: 0,
            keyboard_events: 0,
            mouse_events: 0,
            system_events: 0,
            events_processed: 0,
            events_dropped: 0,
            buffer_usage: 0.0,
            active_keyboards: 0,
            active_mice: 0,
        };
        
        self.initialized = true;
        Ok(())
    }
    
    /// Agregar teclado USB
    pub fn add_keyboard(&mut self, mut keyboard: UsbKeyboardDriver) -> Result<u32, &'static str> {
        keyboard.initialize().map_err(|_| "Failed to initialize keyboard")?;
        
        let device_id = self.keyboards.len() as u32;
        self.keyboards.push(keyboard);
        self.stats.active_keyboards += 1;
        
        // Crear evento de dispositivo conectado
        let event = InputEvent::new(
            InputEventType::System(SystemEvent::DeviceConnected),
            device_id,
            self.current_timestamp,
        );
        self.add_event(event);
        
        Ok(device_id)
    }
    
    /// Agregar mouse USB
    pub fn add_mouse(&mut self, mut mouse: UsbMouseDriver) -> Result<u32, &'static str> {
        mouse.initialize().map_err(|_| "Failed to initialize mouse")?;
        
        let device_id = self.mice.len() as u32;
        self.mice.push(mouse);
        self.stats.active_mice += 1;
        
        // Crear evento de dispositivo conectado
        let event = InputEvent::new(
            InputEventType::System(SystemEvent::DeviceConnected),
            device_id,
            self.current_timestamp,
        );
        self.add_event(event);
        
        Ok(device_id)
    }
    
    /// Procesar eventos de todos los dispositivos
    pub fn process_events(&mut self) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Sistema de entrada no inicializado");
        }
        
        // Procesar teclados
        let mut keyboard_events = Vec::new();
        for (i, keyboard) in self.keyboards.iter_mut().enumerate() {
            while keyboard.has_events() {
                if let Some(keyboard_event) = keyboard.get_next_event() {
                    keyboard_events.push((i as u32, keyboard_event));
                }
            }
        }
        
        // Agregar eventos de teclado
        for (device_id, keyboard_event) in keyboard_events {
            let input_event = InputEvent::new(
                InputEventType::Keyboard(keyboard_event.to_input_system_keyboard_event()),
                device_id,
                self.current_timestamp,
            );
            self.add_event(input_event);
        }
        
        // Procesar mouse
        let mut mouse_events = Vec::new();
        for (i, mouse) in self.mice.iter_mut().enumerate() {
            while mouse.has_events() {
                if let Some(mouse_event) = mouse.get_next_event() {
                    mouse_events.push((i as u32, mouse_event));
                }
            }
        }
        
        // Agregar eventos de mouse
        for (device_id, mouse_event) in mouse_events {
            let input_event = InputEvent::new(
                InputEventType::Mouse(mouse_event.to_input_system_mouse_event()),
                device_id,
                self.current_timestamp,
            );
            self.add_event(input_event);
        }
        
        // Actualizar timestamp
        self.current_timestamp += 1;
        
        Ok(())
    }
    
    /// Agregar evento al buffer
    fn add_event(&mut self, event: InputEvent) {
        // Verificar si hay espacio en el buffer
        if self.event_buffer.len() >= self.config.max_events {
            // Buffer lleno, eliminar evento más antiguo
            self.event_buffer.pop_front();
            self.stats.events_dropped += 1;
        }
        
        // Agregar evento
        self.event_buffer.push_back(event);
        self.stats.total_events += 1;
        
        // Actualizar estadísticas por tipo
        match self.event_buffer.back().unwrap().event_type {
            InputEventType::Keyboard(_) => self.stats.keyboard_events += 1,
            InputEventType::Mouse(_) => self.stats.mouse_events += 1,
            InputEventType::System(_) => self.stats.system_events += 1,
        }
        
        // Actualizar uso del buffer
        self.stats.buffer_usage = (self.event_buffer.len() as f32 / self.config.max_events as f32) * 100.0;
    }
    
    /// Obtener siguiente evento
    pub fn get_next_event(&mut self) -> Option<InputEvent> {
        self.event_buffer.pop_front()
    }
    
    /// Verificar si hay eventos pendientes
    pub fn has_events(&self) -> bool {
        !self.event_buffer.is_empty()
    }
    
    /// Obtener número de eventos pendientes
    pub fn event_count(&self) -> usize {
        self.event_buffer.len()
    }
    
    /// Limpiar buffer de eventos
    pub fn clear_events(&mut self) {
        self.event_buffer.clear();
    }
    
    /// Obtener estadísticas del sistema
    pub fn get_stats(&self) -> &InputSystemStats {
        &self.stats
    }
    
    /// Obtener teclado por ID
    pub fn get_keyboard(&mut self, device_id: u32) -> Option<&mut UsbKeyboardDriver> {
        self.keyboards.get_mut(device_id as usize)
    }
    
    /// Obtener mouse por ID
    pub fn get_mouse(&mut self, device_id: u32) -> Option<&mut UsbMouseDriver> {
        self.mice.get_mut(device_id as usize)
    }
    
    /// Obtener número de teclados activos
    pub fn keyboard_count(&self) -> usize {
        self.keyboards.len()
    }
    
    /// Obtener número de mouse activos
    pub fn mouse_count(&self) -> usize {
        self.mice.len()
    }
    
    /// Verificar si el sistema está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    
    /// Actualizar configuración
    pub fn update_config(&mut self, config: InputSystemConfig) {
        self.config = config;
        
        // Aplicar nueva sensibilidad a todos los mouse
        for mouse in &mut self.mice {
            mouse.set_sensitivity(self.config.mouse_sensitivity);
        }
    }
    
    /// Obtener configuración actual
    pub fn get_config(&self) -> &InputSystemConfig {
        &self.config
    }
    
    /// Procesar datos de teclado
    pub fn process_keyboard_data(&mut self, device_id: u32, data: &[u8]) -> Result<(), &'static str> {
        if let Some(keyboard) = self.get_keyboard(device_id) {
            keyboard.process_keyboard_data(data).map_err(|_| "Failed to process keyboard data")?;
        } else {
            return Err("Teclado no encontrado");
        }
        Ok(())
    }
    
    /// Procesar datos de mouse
    pub fn process_mouse_data(&mut self, device_id: u32, data: &[u8]) -> Result<(), &'static str> {
        if let Some(mouse) = self.get_mouse(device_id) {
            mouse.process_mouse_data(data).map_err(|_| "Failed to process mouse data")?;
        } else {
            return Err("Mouse no encontrado");
        }
        Ok(())
    }
    
    /// Obtener estado actual de todos los dispositivos
    pub fn get_device_states(&self) -> DeviceStates {
        DeviceStates {
            keyboards: self.keyboards.iter().map(|k| k.get_modifier_state().to_input_system_modifier_state()).collect(),
            mice: self.mice.iter().map(|m| (m.get_position(), m.get_button_state())).collect(),
        }
    }
}

/// Estados de todos los dispositivos
#[derive(Debug, Clone)]
pub struct DeviceStates {
    pub keyboards: Vec<ModifierState>,
    pub mice: Vec<(MousePosition, MouseButtonState)>,
}

/// Función de conveniencia para crear el sistema de entrada
pub fn create_input_system(config: InputSystemConfig) -> InputSystem {
    InputSystem::new(config)
}

/// Función de conveniencia para crear el sistema de entrada con configuración por defecto
pub fn create_default_input_system() -> InputSystem {
    InputSystem::new(InputSystemConfig::default())
}
