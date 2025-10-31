#![no_std]

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::keyboard::{KeyCode, KeyEvent, KeyState};
use crate::drivers::manager::Driver;
use crate::drivers::mouse::{
    self as mouse, MouseButton as RealMouseButton, MouseEvent as RealMouseEvent, MouseState,
};
use crate::drivers::usb_keyboard::{KeyboardEvent, ModifierState, UsbKeyCode, UsbKeyboardDriver};
use crate::drivers::usb_keyboard_real::UsbKeyboardReal;
use crate::drivers::usb_manager::UsbManager;
use crate::drivers::usb_mouse::{
    MouseButton as UsbMouseButton, MouseButtonState, MouseEvent, MousePosition, UsbMouseDriver,
};
use crate::drivers::usb_mouse_real::UsbMouseReal;

/// Sistema de entrada unificado para Eclipse OS
/// Gestiona eventos de teclado y mouse de forma centralizada

/// Tipo de evento de entrada
#[derive(Debug, Clone, PartialEq)]
pub enum InputEventType {
    Keyboard(KeyboardEvent),
    Mouse(MouseEvent),
    System(SystemEvent),
}

/// Eventos del sistema
#[derive(Debug, Clone, PartialEq)]
pub enum SystemEvent {
    DeviceConnected { device_type: String, device_id: u32 },
    DeviceDisconnected { device_type: String, device_id: u32 },
    InputError { error: String },
    BufferOverflow,
}

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
    pub keyboard_repeat_rate: u32,  // eventos por segundo
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
    pub usb_manager: Option<UsbManager>,
    pub real_keyboards: Vec<UsbKeyboardReal>,
    pub real_mice: Vec<UsbMouseReal>,
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
            usb_manager: None,
            real_keyboards: Vec::new(),
            real_mice: Vec::new(),
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
        self.real_keyboards.clear();
        self.real_mice.clear();

        // Inicializar gestor USB real
        let mut usb_manager = UsbManager::new();
        if usb_manager.initialize().is_ok() {
            self.usb_manager = Some(usb_manager);

            // Obtener drivers USB reales del gestor
            if let Some(ref mut manager) = self.usb_manager {
                // Los drivers reales se manejan internamente en el UsbManager
                // Solo actualizamos las estadísticas
                self.stats.active_keyboards = if manager.is_keyboard_connected() {
                    1
                } else {
                    0
                };
                self.stats.active_mice = if manager.is_mouse_connected() { 1 } else { 0 };
            }
        }

        // Resetear estadísticas
        self.stats = InputSystemStats {
            total_events: 0,
            keyboard_events: 0,
            mouse_events: 0,
            system_events: 0,
            events_processed: 0,
            events_dropped: 0,
            buffer_usage: 0.0,
            active_keyboards: self.stats.active_keyboards,
            active_mice: self.stats.active_mice,
        };

        self.initialized = true;
        Ok(())
    }

    /// Agregar teclado USB
    pub fn add_keyboard(&mut self, mut keyboard: UsbKeyboardDriver) -> Result<u32, &'static str> {
        keyboard
            .initialize()
            .map_err(|e| "Error initializing keyboard")?;

        let device_id = self.keyboards.len() as u32;
        self.keyboards.push(keyboard);
        self.stats.active_keyboards += 1;

        // Crear evento de dispositivo conectado
        let event = InputEvent::new(
            InputEventType::System(SystemEvent::DeviceConnected {
                device_type: "Keyboard".to_string(),
                device_id,
            }),
            device_id,
            self.current_timestamp,
        );
        self.add_event(event);

        Ok(device_id)
    }

    /// Agregar mouse USB
    pub fn add_mouse(&mut self, mut mouse: UsbMouseDriver) -> Result<u32, &'static str> {
        mouse.initialize().map_err(|e| "Error initializing mouse")?;

        let device_id = self.mice.len() as u32;
        self.mice.push(mouse);
        self.stats.active_mice += 1;

        // Crear evento de dispositivo conectado
        let event = InputEvent::new(
            InputEventType::System(SystemEvent::DeviceConnected {
                device_type: "Mouse".to_string(),
                device_id,
            }),
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
                InputEventType::Keyboard(keyboard_event),
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
                InputEventType::Mouse(mouse_event),
                device_id,
                self.current_timestamp,
            );
            self.add_event(input_event);
        }

        // Procesar drivers USB reales
        self.process_real_usb_events()?;

        // Actualizar timestamp
        self.current_timestamp += 1;

        Ok(())
    }

    /// Procesar eventos de drivers USB reales
    fn process_real_usb_events(&mut self) -> Result<(), &'static str> {
        // Recopilar eventos para evitar problemas de borrowing
        let mut keyboard_events = Vec::new();
        let mut mouse_events = Vec::new();

        if let Some(ref mut usb_manager) = self.usb_manager {
            // Procesar interrupciones USB
            usb_manager
                .handle_usb_interrupts()
                .map_err(|_| "USB interrupt error")?;

            // Recopilar eventos de teclado
            while usb_manager.has_keyboard_events() {
                if let Some(key_event) = usb_manager.get_next_key_event() {
                    keyboard_events.push(key_event);
                }
            }

            // Recopilar eventos de ratón
            while usb_manager.has_mouse_events() {
                if let Some(mouse_event) = usb_manager.get_next_mouse_event() {
                    mouse_events.push(mouse_event);
                }
            }
        }

        // Procesar eventos recopilados
        for key_event in keyboard_events {
            let keyboard_event = InputSystem::convert_real_key_event_static(key_event);
            let input_event = InputEvent::new(
                InputEventType::Keyboard(keyboard_event),
                0, // Device ID para teclado real
                self.current_timestamp,
            );
            self.add_event(input_event);
        }

        for mouse_event in mouse_events {
            let system_mouse_event = InputSystem::convert_real_mouse_event_static(mouse_event);
            let input_event = InputEvent::new(
                InputEventType::Mouse(system_mouse_event),
                0, // Device ID para ratón real
                self.current_timestamp,
            );
            self.add_event(input_event);
        }

        Ok(())
    }

    /// Convertir KeyEvent real a KeyboardEvent del sistema (método estático)
    fn convert_real_key_event_static(key_event: KeyEvent) -> KeyboardEvent {
        // Convertir KeyCode a UsbKeyCode
        let usb_key_code = match key_event.key {
            KeyCode::A => UsbKeyCode::A,
            KeyCode::B => UsbKeyCode::B,
            KeyCode::C => UsbKeyCode::C,
            KeyCode::D => UsbKeyCode::D,
            KeyCode::E => UsbKeyCode::E,
            KeyCode::F => UsbKeyCode::F,
            KeyCode::G => UsbKeyCode::G,
            KeyCode::H => UsbKeyCode::H,
            KeyCode::I => UsbKeyCode::I,
            KeyCode::J => UsbKeyCode::J,
            KeyCode::K => UsbKeyCode::K,
            KeyCode::L => UsbKeyCode::L,
            KeyCode::M => UsbKeyCode::M,
            KeyCode::N => UsbKeyCode::N,
            KeyCode::O => UsbKeyCode::O,
            KeyCode::P => UsbKeyCode::P,
            KeyCode::Q => UsbKeyCode::Q,
            KeyCode::R => UsbKeyCode::R,
            KeyCode::S => UsbKeyCode::S,
            KeyCode::T => UsbKeyCode::T,
            KeyCode::U => UsbKeyCode::U,
            KeyCode::V => UsbKeyCode::V,
            KeyCode::W => UsbKeyCode::W,
            KeyCode::X => UsbKeyCode::X,
            KeyCode::Y => UsbKeyCode::Y,
            KeyCode::Z => UsbKeyCode::Z,
            KeyCode::Enter => UsbKeyCode::Enter,
            KeyCode::Escape => UsbKeyCode::Escape,
            KeyCode::Backspace => UsbKeyCode::Backspace,
            KeyCode::Tab => UsbKeyCode::Tab,
            KeyCode::Space => UsbKeyCode::Space,
            KeyCode::LeftShift => UsbKeyCode::LeftShift,
            KeyCode::RightShift => UsbKeyCode::RightShift,
            KeyCode::LeftCtrl => UsbKeyCode::LeftCtrl,
            KeyCode::RightCtrl => UsbKeyCode::RightCtrl,
            KeyCode::LeftAlt => UsbKeyCode::LeftAlt,
            KeyCode::RightAlt => UsbKeyCode::RightAlt,
            _ => UsbKeyCode::Unknown,
        };

        // Convertir KeyState a ModifierState
        let modifier_state = ModifierState {
            left_ctrl: key_event.modifiers & 0x01 != 0,
            right_ctrl: key_event.modifiers & 0x01 != 0,
            left_shift: key_event.modifiers & 0x02 != 0,
            right_shift: key_event.modifiers & 0x02 != 0,
            left_alt: key_event.modifiers & 0x04 != 0,
            right_alt: key_event.modifiers & 0x04 != 0,
            left_meta: key_event.modifiers & 0x08 != 0,
            right_meta: key_event.modifiers & 0x08 != 0,
            num_lock: false,
            caps_lock: false,
            scroll_lock: false,
        };

        KeyboardEvent {
            key_code: usb_key_code,
            modifiers: modifier_state,
            pressed: matches!(key_event.state, KeyState::Pressed),
            character: None, // Por defecto, se puede calcular después
            timestamp: 0,    // Por defecto, se puede establecer después
        }
    }

    /// Convertir MouseEvent real a MouseEvent del sistema (método estático)
    fn convert_real_mouse_event_static(mouse_event: RealMouseEvent) -> MouseEvent {
        // Convertir MouseButton real a MouseButton del sistema
        let system_button = match mouse_event.button {
            RealMouseButton::Left => UsbMouseButton::Left,
            RealMouseButton::Right => UsbMouseButton::Right,
            RealMouseButton::Middle => UsbMouseButton::Middle,
            RealMouseButton::Button4 => UsbMouseButton::Side1,
            RealMouseButton::Button5 => UsbMouseButton::Side2,
            RealMouseButton::Wheel => UsbMouseButton::WheelUp,
            _ => UsbMouseButton::Left, // Default
        };

        let position = MousePosition::new_with_coords(mouse_event.x, mouse_event.y);

        // Convertir a enum MouseEvent del sistema
        match mouse_event.state {
            MouseState::Pressed => MouseEvent::ButtonPress {
                button: system_button,
                position,
            },
            MouseState::Released => MouseEvent::ButtonRelease {
                button: system_button,
                position,
            },
            MouseState::Moved => MouseEvent::Move {
                position,
                buttons: MouseButtonState::new(),
            },
            MouseState::WheelUp => MouseEvent::Scroll { delta: 1, position },
            MouseState::WheelDown => MouseEvent::Scroll {
                delta: -1,
                position,
            },
        }
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
        self.stats.buffer_usage =
            (self.event_buffer.len() as f32 / self.config.max_events as f32) * 100.0;
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
    pub fn process_keyboard_data(
        &mut self,
        device_id: u32,
        data: &[u8],
    ) -> Result<(), &'static str> {
        if let Some(keyboard) = self.get_keyboard(device_id) {
            keyboard
                .process_keyboard_data(data)
                .map_err(|e| "Error processing keyboard data")?;
        } else {
            return Err("Teclado no encontrado");
        }
        Ok(())
    }

    /// Procesar datos de mouse
    pub fn process_mouse_data(&mut self, device_id: u32, data: &[u8]) -> Result<(), &'static str> {
        if let Some(mouse) = self.get_mouse(device_id) {
            mouse
                .process_mouse_data(data)
                .map_err(|e| "Error processing mouse data")?;
        } else {
            return Err("Mouse no encontrado");
        }
        Ok(())
    }

    /// Obtener estado actual de todos los dispositivos
    pub fn get_device_states(&self) -> DeviceStates {
        DeviceStates {
            keyboards: self
                .keyboards
                .iter()
                .map(|k| k.get_modifier_state())
                .collect(),
            mice: self
                .mice
                .iter()
                .map(|m| (m.get_position(), m.get_button_state()))
                .collect(),
        }
    }

    /// Obtener información de drivers USB reales
    pub fn get_real_usb_info(&self) -> String {
        if let Some(ref usb_manager) = self.usb_manager {
            usb_manager.get_complete_stats()
        } else {
            "Gestor USB real no inicializado".to_string()
        }
    }

    /// Verificar si hay teclado USB real conectado
    pub fn has_real_keyboard(&self) -> bool {
        if let Some(ref usb_manager) = self.usb_manager {
            usb_manager.is_keyboard_connected()
        } else {
            false
        }
    }

    /// Verificar si hay ratón USB real conectado
    pub fn has_real_mouse(&self) -> bool {
        if let Some(ref usb_manager) = self.usb_manager {
            usb_manager.is_mouse_connected()
        } else {
            false
        }
    }

    /// Obtener número de dispositivos USB reales conectados
    pub fn get_real_usb_device_count(&self) -> u32 {
        if let Some(ref usb_manager) = self.usb_manager {
            usb_manager.get_connected_device_count()
        } else {
            0
        }
    }

    /// Reinicializar drivers USB reales
    pub fn reinitialize_real_usb(&mut self) -> Result<(), &'static str> {
        if let Some(ref mut usb_manager) = self.usb_manager {
            usb_manager
                .reinitialize_devices()
                .map_err(|_| "Error reinicializando USB")?;

            // Actualizar estadísticas
            self.stats.active_keyboards = if usb_manager.is_keyboard_connected() {
                1
            } else {
                0
            };
            self.stats.active_mice = if usb_manager.is_mouse_connected() {
                1
            } else {
                0
            };
        }
        Ok(())
    }

    /// Obtener posición del ratón USB real
    pub fn get_real_mouse_position(&self) -> Option<(i32, i32)> {
        if let Some(ref usb_manager) = self.usb_manager {
            if usb_manager.is_mouse_connected() {
                Some(usb_manager.get_mouse_position())
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Establecer posición del ratón USB real
    pub fn set_real_mouse_position(&mut self, x: i32, y: i32) -> Result<(), &'static str> {
        if let Some(ref mut usb_manager) = self.usb_manager {
            if usb_manager.is_mouse_connected() {
                usb_manager.set_mouse_position(x, y);
                Ok(())
            } else {
                Err("Ratón USB real no conectado")
            }
        } else {
            Err("Gestor USB real no inicializado")
        }
    }

    /// Verificar si una tecla está presionada en el teclado USB real
    pub fn is_real_key_pressed(&self, key: KeyCode) -> bool {
        if let Some(ref usb_manager) = self.usb_manager {
            if usb_manager.is_keyboard_connected() {
                usb_manager.is_key_pressed(key)
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Verificar si un botón del ratón USB real está presionado
    pub fn is_real_mouse_button_pressed(&self, button: RealMouseButton) -> bool {
        if let Some(ref usb_manager) = self.usb_manager {
            if usb_manager.is_mouse_connected() {
                // Convertir RealMouseButton a MouseButton del sistema
                let system_button = match button {
                    RealMouseButton::Left => mouse::MouseButton::Left,
                    RealMouseButton::Right => mouse::MouseButton::Right,
                    RealMouseButton::Middle => mouse::MouseButton::Middle,
                    RealMouseButton::Button4 => mouse::MouseButton::Button4,
                    RealMouseButton::Button5 => mouse::MouseButton::Button5,
                    _ => mouse::MouseButton::Left,
                };
                usb_manager.is_mouse_button_pressed(system_button)
            } else {
                false
            }
        } else {
            false
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
