//! Sistema de interacciones con mouse y teclado para COSMIC Desktop Environment
//!
//! Implementa detección de eventos de entrada, gestión de clicks, movimiento del mouse,
//! procesamiento de teclas y shortcuts, con integración completa en COSMIC.

use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// Tipo de evento de mouse
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseEventType {
    Move,
    LeftClick,
    RightClick,
    MiddleClick,
    LeftPress,
    LeftRelease,
    RightPress,
    RightRelease,
    MiddlePress,
    MiddleRelease,
    WheelUp,
    WheelDown,
    Hover,
    Leave,
}

/// Tipo de evento de teclado
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyboardEventType {
    KeyPress,
    KeyRelease,
    KeyRepeat,
}

/// Evento de mouse
#[derive(Debug, Clone)]
pub struct MouseEvent {
    pub event_type: MouseEventType,
    pub x: i32,
    pub y: i32,
    pub button: Option<MouseButton>,
    pub modifiers: u32,
    pub timestamp: u64,
}

/// Evento de teclado
#[derive(Debug, Clone)]
pub struct KeyboardEvent {
    pub event_type: KeyboardEventType,
    pub key_code: u32,
    pub key_char: Option<char>,
    pub modifiers: u32,
    pub timestamp: u64,
}

/// Botón del mouse
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Extra1,
    Extra2,
}

/// Códigos de teclas especiales
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpecialKey {
    Escape,
    Enter,
    Backspace,
    Tab,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Ctrl,
    Alt,
    Shift,
    Super,
}

/// Área interactiva
#[derive(Debug, Clone)]
pub struct InteractiveArea {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub area_type: InteractiveAreaType,
    pub is_visible: bool,
    pub is_enabled: bool,
    pub hover_state: bool,
    pub click_state: bool,
}

/// Tipo de área interactiva
#[derive(Debug, Clone, PartialEq)]
pub enum InteractiveAreaType {
    Button,
    Window,
    Taskbar,
    StartMenu,
    Icon,
    Slider,
    TextField,
    Menu,
    ContextMenu,
}

/// Callback para eventos
pub type EventCallback = fn(&MouseEvent) -> bool; // true si el evento fue manejado

/// Shortcut de teclado
#[derive(Debug, Clone)]
pub struct KeyboardShortcut {
    pub key_code: u32,
    pub modifiers: u32,
    pub callback: fn() -> bool,
    pub description: String,
}

/// Estado del mouse
#[derive(Debug, Clone)]
pub struct MouseState {
    pub x: i32,
    pub y: i32,
    pub left_button: bool,
    pub right_button: bool,
    pub middle_button: bool,
    pub wheel_delta: i32,
    pub last_click_time: u64,
    pub double_click_threshold: u64, // ms
}

/// Estado del teclado
#[derive(Debug, Clone)]
pub struct KeyboardState {
    pub pressed_keys: Vec<u32>,
    pub modifiers: u32,
    pub last_key_time: u64,
    pub repeat_delay: u64, // ms
    pub repeat_rate: u64,  // ms
}

/// Gestor de entrada
pub struct InputSystem {
    mouse_state: MouseState,
    keyboard_state: KeyboardState,
    interactive_areas: BTreeMap<String, InteractiveArea>,
    mouse_callbacks: BTreeMap<String, EventCallback>,
    keyboard_shortcuts: Vec<KeyboardShortcut>,
    hovered_area: Option<String>,
    focused_area: Option<String>,
    event_queue: Vec<InputEvent>,
}

/// Evento de entrada unificado
#[derive(Debug, Clone)]
pub enum InputEvent {
    Mouse(MouseEvent),
    Keyboard(KeyboardEvent),
}

impl InputSystem {
    pub fn new() -> Self {
        Self {
            mouse_state: MouseState {
                x: 0,
                y: 0,
                left_button: false,
                right_button: false,
                middle_button: false,
                wheel_delta: 0,
                last_click_time: 0,
                double_click_threshold: 500, // 500ms
            },
            keyboard_state: KeyboardState {
                pressed_keys: Vec::new(),
                modifiers: 0,
                last_key_time: 0,
                repeat_delay: 500, // 500ms
                repeat_rate: 50,   // 50ms
            },
            interactive_areas: BTreeMap::new(),
            mouse_callbacks: BTreeMap::new(),
            keyboard_shortcuts: Vec::new(),
            hovered_area: None,
            focused_area: None,
            event_queue: Vec::new(),
        }
    }

    /// Procesar evento de mouse
    pub fn handle_mouse_event(&mut self, event: MouseEvent) -> Result<(), String> {
        self.mouse_state.x = event.x;
        self.mouse_state.y = event.y;

        match event.event_type {
            MouseEventType::Move => {
                self.handle_mouse_move(&event)?;
            }
            MouseEventType::LeftPress => {
                self.mouse_state.left_button = true;
                self.handle_mouse_press(&event)?;
            }
            MouseEventType::LeftRelease => {
                self.mouse_state.left_button = false;
                self.handle_mouse_release(&event)?;
            }
            MouseEventType::RightPress => {
                self.mouse_state.right_button = true;
                self.handle_mouse_press(&event)?;
            }
            MouseEventType::RightRelease => {
                self.mouse_state.right_button = false;
                self.handle_mouse_release(&event)?;
            }
            MouseEventType::LeftClick => {
                self.handle_mouse_click(&event)?;
            }
            MouseEventType::RightClick => {
                self.handle_mouse_click(&event)?;
            }
            MouseEventType::WheelUp => {
                self.mouse_state.wheel_delta = 1;
                self.handle_mouse_wheel(&event)?;
            }
            MouseEventType::WheelDown => {
                self.mouse_state.wheel_delta = -1;
                self.handle_mouse_wheel(&event)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Procesar evento de teclado
    pub fn handle_keyboard_event(&mut self, event: KeyboardEvent) -> Result<(), String> {
        match event.event_type {
            KeyboardEventType::KeyPress => {
                if !self.keyboard_state.pressed_keys.contains(&event.key_code) {
                    self.keyboard_state.pressed_keys.push(event.key_code);
                }
                self.keyboard_state.last_key_time = event.timestamp;
                self.handle_key_press(&event)?;
            }
            KeyboardEventType::KeyRelease => {
                self.keyboard_state
                    .pressed_keys
                    .retain(|&k| k != event.key_code);
                self.handle_key_release(&event)?;
            }
            KeyboardEventType::KeyRepeat => {
                self.handle_key_repeat(&event)?;
            }
        }

        Ok(())
    }

    /// Manejar movimiento del mouse
    fn handle_mouse_move(&mut self, event: &MouseEvent) -> Result<(), String> {
        // Verificar si el mouse está sobre alguna área interactiva
        let current_area = self.get_area_at_position(event.x, event.y);

        if let Some(area_id) = &current_area {
            if let Some(area) = self.interactive_areas.get_mut(area_id) {
                if !area.hover_state {
                    area.hover_state = true;
                    // Trigger hover event
                    let hover_event = MouseEvent {
                        event_type: MouseEventType::Hover,
                        x: event.x,
                        y: event.y,
                        button: None,
                        modifiers: event.modifiers,
                        timestamp: event.timestamp,
                    };
                    self.process_mouse_event(&hover_event)?;
                }
            }
        }

        // Verificar si salimos de un área
        if let Some(hovered_id) = &self.hovered_area {
            if current_area.as_ref() != Some(hovered_id) {
                if let Some(area) = self.interactive_areas.get_mut(hovered_id) {
                    area.hover_state = false;
                    // Trigger leave event
                    let leave_event = MouseEvent {
                        event_type: MouseEventType::Leave,
                        x: event.x,
                        y: event.y,
                        button: None,
                        modifiers: event.modifiers,
                        timestamp: event.timestamp,
                    };
                    self.process_mouse_event(&leave_event)?;
                }
            }
        }

        self.hovered_area = current_area;
        Ok(())
    }

    /// Manejar presión del mouse
    fn handle_mouse_press(&mut self, event: &MouseEvent) -> Result<(), String> {
        if let Some(area_id) = &self.hovered_area {
            if let Some(area) = self.interactive_areas.get_mut(area_id) {
                area.click_state = true;
                self.focused_area = Some(area_id.clone());
            }
        }
        Ok(())
    }

    /// Manejar liberación del mouse
    fn handle_mouse_release(&mut self, event: &MouseEvent) -> Result<(), String> {
        if let Some(area_id) = &self.hovered_area {
            if let Some(area) = self.interactive_areas.get_mut(area_id) {
                area.click_state = false;

                // Verificar si es un click válido
                if area.click_state {
                    self.process_mouse_event(event)?;
                }
            }
        }
        Ok(())
    }

    /// Manejar click del mouse
    fn handle_mouse_click(&mut self, event: &MouseEvent) -> Result<(), String> {
        let current_time = event.timestamp;

        // Verificar double click
        let is_double_click = if let Some(button) = event.button {
            current_time - self.mouse_state.last_click_time
                < self.mouse_state.double_click_threshold
        } else {
            false
        };

        self.mouse_state.last_click_time = current_time;

        if let Some(area_id) = &self.hovered_area {
            if let Some(area) = self.interactive_areas.get_mut(area_id) {
                if area.is_enabled {
                    // Procesar el evento en el área
                    self.process_mouse_event(event)?;
                }
            }
        }

        Ok(())
    }

    /// Manejar rueda del mouse
    fn handle_mouse_wheel(&mut self, event: &MouseEvent) -> Result<(), String> {
        if let Some(area_id) = &self.hovered_area {
            if let Some(area) = self.interactive_areas.get_mut(area_id) {
                if area.is_enabled {
                    // Procesar scroll en el área
                    self.process_mouse_event(event)?;
                }
            }
        }
        Ok(())
    }

    /// Manejar presión de tecla
    fn handle_key_press(&mut self, event: &KeyboardEvent) -> Result<(), String> {
        // Verificar shortcuts
        for shortcut in &self.keyboard_shortcuts {
            if shortcut.key_code == event.key_code
                && (shortcut.modifiers & event.modifiers) == shortcut.modifiers
            {
                if (shortcut.callback)() {
                    return Ok(());
                }
            }
        }

        // Enviar evento a área enfocada
        if let Some(area_id) = &self.focused_area {
            // Procesar evento de teclado en el área enfocada
            self.process_keyboard_event(event)?;
        }

        Ok(())
    }

    /// Manejar liberación de tecla
    fn handle_key_release(&mut self, event: &KeyboardEvent) -> Result<(), String> {
        // Procesar liberación de tecla si hay área enfocada
        if let Some(_area_id) = &self.focused_area {
            self.process_keyboard_event(event)?;
        }
        Ok(())
    }

    /// Manejar repetición de tecla
    fn handle_key_repeat(&mut self, event: &KeyboardEvent) -> Result<(), String> {
        // Procesar repetición de tecla si hay área enfocada
        if let Some(_area_id) = &self.focused_area {
            self.process_keyboard_event(event)?;
        }
        Ok(())
    }

    /// Procesar evento de mouse
    fn process_mouse_event(&self, event: &MouseEvent) -> Result<(), String> {
        if let Some(area_id) = &self.hovered_area {
            if let Some(callback) = self.mouse_callbacks.get(area_id) {
                if callback(event) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Procesar evento de teclado
    fn process_keyboard_event(&self, event: &KeyboardEvent) -> Result<(), String> {
        // En una implementación real, aquí se procesarían los eventos de teclado
        // en las áreas enfocadas
        Ok(())
    }

    /// Obtener área en posición específica
    fn get_area_at_position(&self, x: i32, y: i32) -> Option<String> {
        for (id, area) in &self.interactive_areas {
            if area.is_visible && area.is_enabled {
                if x >= area.x
                    && x < (area.x + area.width as i32)
                    && y >= area.y
                    && y < (area.y + area.height as i32)
                {
                    return Some(id.clone());
                }
            }
        }
        None
    }

    /// Registrar área interactiva
    pub fn register_area(&mut self, area: InteractiveArea) {
        self.interactive_areas.insert(area.id.clone(), area);
    }

    /// Desregistrar área interactiva
    pub fn unregister_area(&mut self, area_id: &str) {
        self.interactive_areas.remove(area_id);
        if self.hovered_area.as_ref() == Some(&String::from(area_id)) {
            self.hovered_area = None;
        }
        if self.focused_area.as_ref() == Some(&String::from(area_id)) {
            self.focused_area = None;
        }
    }

    /// Registrar callback de mouse
    pub fn register_mouse_callback(&mut self, area_id: String, callback: EventCallback) {
        self.mouse_callbacks.insert(area_id, callback);
    }

    /// Registrar shortcut de teclado
    pub fn register_keyboard_shortcut(&mut self, shortcut: KeyboardShortcut) {
        self.keyboard_shortcuts.push(shortcut);
    }

    /// Obtener estado del mouse
    pub fn get_mouse_state(&self) -> &MouseState {
        &self.mouse_state
    }

    /// Obtener estado del teclado
    pub fn get_keyboard_state(&self) -> &KeyboardState {
        &self.keyboard_state
    }

    /// Obtener área enfocada
    pub fn get_focused_area(&self) -> Option<&String> {
        self.focused_area.as_ref()
    }

    /// Obtener área con hover
    pub fn get_hovered_area(&self) -> Option<&String> {
        self.hovered_area.as_ref()
    }

    /// Obtener estadísticas del sistema
    pub fn get_stats(&self) -> (usize, usize, usize) {
        (
            self.interactive_areas.len(),
            self.mouse_callbacks.len(),
            self.keyboard_shortcuts.len(),
        )
    }
}
