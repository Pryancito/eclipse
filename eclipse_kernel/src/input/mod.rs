//! Sistema de entrada unificado para Eclipse OS
//! 
//! Integra eventos de teclado, ratón y otros dispositivos HID

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use crate::drivers::usb_hid::{HidEvent, get_hid_event_count, pop_hid_event};

/// Evento de teclado procesado
#[derive(Debug, Clone, Copy)]
pub struct KeyboardEvent {
    pub key_code: u8,
    pub scancode: u8,
    pub pressed: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Evento de ratón procesado
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub x: i32,
    pub y: i32,
    pub delta_x: i32,
    pub delta_y: i32,
    pub left_button: bool,
    pub right_button: bool,
    pub middle_button: bool,
    pub wheel_delta: i8,
}

/// Evento de entrada unificado
#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    Keyboard(KeyboardEvent),
    Mouse(MouseEvent),
}

/// Estado del ratón
pub struct MouseState {
    pub x: i32,
    pub y: i32,
    pub left_button: bool,
    pub right_button: bool,
    pub middle_button: bool,
}

impl MouseState {
    pub fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            left_button: false,
            right_button: false,
            middle_button: false,
        }
    }
}

/// Manager de entrada unificado
pub struct InputManager {
    event_queue: VecDeque<InputEvent>,
    mouse_state: MouseState,
    max_events: usize,
}

impl InputManager {
    pub fn new() -> Self {
        Self {
            event_queue: VecDeque::with_capacity(128),
            mouse_state: MouseState::new(),
            max_events: 128,
        }
    }

    /// Procesar eventos HID y convertirlos en eventos de entrada
    pub fn poll_events(&mut self) {
        while get_hid_event_count() > 0 {
            if let Some(hid_event) = pop_hid_event() {
                match hid_event {
                    HidEvent::Keyboard { modifiers, keys } => {
                        self.process_keyboard_event(modifiers, &keys);
                    }
                    HidEvent::Mouse { buttons, x, y, wheel } => {
                        self.process_mouse_event(buttons, x, y, wheel);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Procesar evento de teclado
    fn process_keyboard_event(&mut self, modifiers: u8, keys: &[u8; 6]) {
        let shift = (modifiers & 0x02) != 0 || (modifiers & 0x20) != 0;
        let ctrl = (modifiers & 0x01) != 0 || (modifiers & 0x10) != 0;
        let alt = (modifiers & 0x04) != 0 || (modifiers & 0x40) != 0;
        let meta = (modifiers & 0x08) != 0 || (modifiers & 0x80) != 0;

        for &key in keys.iter() {
            if key != 0 {
                let event = InputEvent::Keyboard(KeyboardEvent {
                    key_code: key,
                    scancode: key,
                    pressed: true,
                    shift,
                    ctrl,
                    alt,
                    meta,
                });

                self.push_event(event);
            }
        }
    }

    /// Procesar evento de ratón
    fn process_mouse_event(&mut self, buttons: u8, delta_x: i8, delta_y: i8, wheel: i8) {
        // Actualizar posición del ratón
        self.mouse_state.x += delta_x as i32;
        self.mouse_state.y += delta_y as i32;

        // Limitar coordenadas a rango válido (esto debería venir del framebuffer)
        self.mouse_state.x = self.mouse_state.x.max(0).min(1920);
        self.mouse_state.y = self.mouse_state.y.max(0).min(1080);

        // Actualizar estado de botones
        self.mouse_state.left_button = (buttons & 0x01) != 0;
        self.mouse_state.right_button = (buttons & 0x02) != 0;
        self.mouse_state.middle_button = (buttons & 0x04) != 0;

        let event = InputEvent::Mouse(MouseEvent {
            x: self.mouse_state.x,
            y: self.mouse_state.y,
            delta_x: delta_x as i32,
            delta_y: delta_y as i32,
            left_button: self.mouse_state.left_button,
            right_button: self.mouse_state.right_button,
            middle_button: self.mouse_state.middle_button,
            wheel_delta: wheel,
        });

        self.push_event(event);
    }

    /// Agregar evento a la cola
    fn push_event(&mut self, event: InputEvent) {
        if self.event_queue.len() >= self.max_events {
            self.event_queue.pop_front();
        }
        self.event_queue.push_back(event);
    }

    /// Obtener siguiente evento
    pub fn pop_event(&mut self) -> Option<InputEvent> {
        self.event_queue.pop_front()
    }

    /// Obtener evento sin removerlo
    pub fn peek_event(&self) -> Option<InputEvent> {
        self.event_queue.front().copied()
    }

    /// Obtener estado del ratón
    pub fn get_mouse_state(&self) -> &MouseState {
        &self.mouse_state
    }

    /// Obtener número de eventos en cola
    pub fn event_count(&self) -> usize {
        self.event_queue.len()
    }

    /// Limpiar todos los eventos
    pub fn clear_events(&mut self) {
        self.event_queue.clear();
    }
}

/// Manager global de entrada
static mut INPUT_MANAGER: Option<InputManager> = None;

/// Inicializar el manager de entrada
pub fn init_input_manager() {
    unsafe {
        INPUT_MANAGER = Some(InputManager::new());
    }
}

/// Obtener el manager de entrada
pub fn get_input_manager() -> Option<&'static mut InputManager> {
    unsafe { INPUT_MANAGER.as_mut() }
}

/// Poll de eventos de entrada
pub fn poll_input_events() {
    if let Some(manager) = get_input_manager() {
        manager.poll_events();
    }
}

/// Obtener siguiente evento de entrada
pub fn get_next_input_event() -> Option<InputEvent> {
    if let Some(manager) = get_input_manager() {
        manager.pop_event()
    } else {
        None
    }
}

/// Obtener estado actual del ratón
pub fn get_mouse_state() -> Option<(i32, i32, bool, bool, bool)> {
    if let Some(manager) = get_input_manager() {
        let state = manager.get_mouse_state();
        Some((
            state.x,
            state.y,
            state.left_button,
            state.right_button,
            state.middle_button,
        ))
    } else {
        None
    }
}

