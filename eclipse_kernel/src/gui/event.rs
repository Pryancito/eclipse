//! Sistema de eventos para GUI
//! 
//! Maneja eventos de teclado, ratón y sistema

use crate::gui::framebuffer::Point;
use crate::gui::window::WindowId;

/// Tipos de eventos
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventType {
    /// Eventos de teclado
    KeyDown,
    KeyUp,
    KeyPress,
    
    /// Eventos de ratón
    MouseMove,
    MouseDown,
    MouseUp,
    MouseClick,
    MouseDoubleClick,
    MouseWheel,
    
    /// Eventos de ventana
    WindowCreate,
    WindowDestroy,
    WindowMove,
    WindowResize,
    WindowFocus,
    WindowUnfocus,
    WindowShow,
    WindowHide,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    WindowClose,
    
    /// Eventos de sistema
    SystemShutdown,
    SystemRestart,
    SystemSuspend,
    SystemResume,
}

/// Códigos de teclas
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u16)]
pub enum KeyCode {
    // Teclas alfanuméricas
    A = 0x1E, B = 0x30, C = 0x2E, D = 0x20, E = 0x12, F = 0x21, G = 0x22, H = 0x23,
    I = 0x17, J = 0x24, K = 0x25, L = 0x26, M = 0x32, N = 0x31, O = 0x18, P = 0x19,
    Q = 0x10, R = 0x13, S = 0x1F, T = 0x14, U = 0x16, V = 0x2F, W = 0x11, X = 0x2D,
    Y = 0x15, Z = 0x2C,
    
    // Números
    Num0 = 0x0B, Num1 = 0x02, Num2 = 0x03, Num3 = 0x04, Num4 = 0x05,
    Num5 = 0x06, Num6 = 0x07, Num7 = 0x08, Num8 = 0x09, Num9 = 0x0A,
    
    // Teclas especiales
    Escape = 0x01,
    Tab = 0x0F,
    Space = 0x39,
    Enter = 0x1C,
    Backspace = 0x0E,
    Delete = 0x53,
    Insert = 0x52,
    Home = 0x47,
    End = 0x4F,
    PageUp = 0x49,
    PageDown = 0x51,
    
    // Teclas de función
    F1 = 0x3B, F2 = 0x3C, F3 = 0x3D, F4 = 0x3E, F5 = 0x3F, F6 = 0x40,
    F7 = 0x41, F8 = 0x42, F9 = 0x43, F10 = 0x44, F11 = 0x57, F12 = 0x58,
    
    // Teclas de flecha
    ArrowUp = 0x48,
    ArrowDown = 0x50,
    ArrowLeft = 0x4B,
    ArrowRight = 0x4D,
    
    // Teclas modificadoras
    LeftShift = 0x2A,
    RightShift = 0x36,
    LeftCtrl = 0x1D,
    RightCtrl = 0xE01D,
    LeftAlt = 0x38,
    RightAlt = 0xE038,
    LeftSuper = 0xE05B, // Tecla Windows
    RightSuper = 0xE05C,
    Menu = 0xE05D,
    
    // Otras teclas
    CapsLock = 0x3A,
    NumLock = 0x45,
    ScrollLock = 0x46,
    PrintScreen = 0xE037,
    Pause = 0xE046, // Pause/Break
    
    Unknown = 0xFFFF,
}

/// Modificadores de teclado
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool, // Tecla Windows
    pub caps_lock: bool,
    pub num_lock: bool,
    pub scroll_lock: bool,
}

impl KeyModifiers {
    pub fn new() -> Self {
        Self {
            shift: false,
            ctrl: false,
            alt: false,
            super_key: false,
            caps_lock: false,
            num_lock: false,
            scroll_lock: false,
        }
    }
    
    pub fn none() -> Self {
        Self::new()
    }
}

/// Botones del ratón
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Extra1,
    Extra2,
}

/// Evento de teclado
#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub event_type: EventType,
    pub key_code: KeyCode,
    pub modifiers: KeyModifiers,
    pub character: Option<char>,
    pub timestamp: u64,
}

/// Evento de ratón
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub event_type: EventType,
    pub position: Point,
    pub button: Option<MouseButton>,
    pub wheel_delta: i32,
    pub modifiers: KeyModifiers,
    pub timestamp: u64,
}

/// Evento de ventana
#[derive(Debug, Clone, Copy)]
pub struct WindowEvent {
    pub event_type: EventType,
    pub window_id: WindowId,
    pub position: Option<Point>,
    pub size: Option<(u32, u32)>,
    pub timestamp: u64,
}

/// Evento de sistema
#[derive(Debug, Clone, Copy)]
pub struct SystemEvent {
    pub event_type: EventType,
    pub timestamp: u64,
}

/// Evento unificado
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Window(WindowEvent),
    System(SystemEvent),
}

impl Event {
    /// Obtener el tipo de evento
    pub fn event_type(&self) -> EventType {
        match self {
            Event::Key(e) => e.event_type,
            Event::Mouse(e) => e.event_type,
            Event::Window(e) => e.event_type,
            Event::System(e) => e.event_type,
        }
    }
    
    /// Obtener el timestamp del evento
    pub fn timestamp(&self) -> u64 {
        match self {
            Event::Key(e) => e.timestamp,
            Event::Mouse(e) => e.timestamp,
            Event::Window(e) => e.timestamp,
            Event::System(e) => e.timestamp,
        }
    }
}

/// Cola de eventos
pub struct EventQueue {
    pub events: [Option<Event>; 256],
    pub head: usize,
    pub tail: usize,
    pub count: usize,
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            events: [None; 256],
            head: 0,
            tail: 0,
            count: 0,
        }
    }
    
    /// Agregar evento a la cola
    pub fn push(&mut self, event: Event) -> bool {
        if self.count >= self.events.len() {
            return false; // Cola llena
        }
        
        self.events[self.tail] = Some(event);
        self.tail = (self.tail + 1) % self.events.len();
        self.count += 1;
        true
    }
    
    /// Obtener siguiente evento de la cola
    pub fn pop(&mut self) -> Option<Event> {
        if self.count == 0 {
            return None;
        }
        
        let event = self.events[self.head].take();
        self.head = (self.head + 1) % self.events.len();
        self.count -= 1;
        event
    }
    
    /// Verificar si la cola está vacía
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    
    /// Obtener número de eventos en la cola
    pub fn len(&self) -> usize {
        self.count
    }
    
    /// Limpiar la cola
    pub fn clear(&mut self) {
        self.events = [None; 256];
        self.head = 0;
        self.tail = 0;
        self.count = 0;
    }
}

/// Manejador de eventos
pub type EventHandler = fn(event: Event);

/// Sistema de eventos
pub struct EventSystem {
    pub event_queue: EventQueue,
    pub key_modifiers: KeyModifiers,
    pub mouse_position: Point,
    pub mouse_buttons: [bool; 5], // Para los 5 botones del ratón
    pub focused_window: Option<WindowId>,
    pub event_handlers: [Option<EventHandler>; 64],
    pub handler_count: usize,
}

impl EventSystem {
    pub fn new() -> Self {
        Self {
            event_queue: EventQueue::new(),
            key_modifiers: KeyModifiers::new(),
            mouse_position: Point::new(0, 0),
            mouse_buttons: [false; 5],
            focused_window: None,
            event_handlers: [None; 64],
            handler_count: 0,
        }
    }
    
    /// Agregar manejador de eventos
    pub fn add_handler(&mut self, handler: EventHandler) -> bool {
        if self.handler_count >= self.event_handlers.len() {
            return false;
        }
        
        self.event_handlers[self.handler_count] = Some(handler);
        self.handler_count += 1;
        true
    }
    
    /// Procesar eventos pendientes
    pub fn process_events(&mut self) {
        while let Some(event) = self.event_queue.pop() {
            // Actualizar estado interno
            self.update_state(&event);
            
            // Llamar a los manejadores de eventos
            for i in 0..self.handler_count {
                if let Some(handler) = self.event_handlers[i] {
                    handler(event);
                }
            }
        }
    }
    
    /// Actualizar estado interno basado en el evento
    fn update_state(&mut self, event: &Event) {
        match event {
            Event::Key(key_event) => {
                self.update_key_modifiers(key_event);
            }
            Event::Mouse(mouse_event) => {
                self.mouse_position = mouse_event.position;
                
                if let Some(button) = mouse_event.button {
                    let button_index = match button {
                        MouseButton::Left => 0,
                        MouseButton::Right => 1,
                        MouseButton::Middle => 2,
                        MouseButton::Extra1 => 3,
                        MouseButton::Extra2 => 4,
                    };
                    
                    match mouse_event.event_type {
                        EventType::MouseDown => {
                            self.mouse_buttons[button_index] = true;
                        }
                        EventType::MouseUp => {
                            self.mouse_buttons[button_index] = false;
                        }
                        _ => {}
                    }
                }
            }
            Event::Window(window_event) => {
                match window_event.event_type {
                    EventType::WindowFocus => {
                        self.focused_window = Some(window_event.window_id);
                    }
                    EventType::WindowUnfocus => {
                        if self.focused_window == Some(window_event.window_id) {
                            self.focused_window = None;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    
    /// Actualizar modificadores de teclado
    fn update_key_modifiers(&mut self, key_event: &KeyEvent) {
        match key_event.key_code {
            KeyCode::LeftShift | KeyCode::RightShift => {
                self.key_modifiers.shift = key_event.event_type == EventType::KeyDown;
            }
            KeyCode::LeftCtrl | KeyCode::RightCtrl => {
                self.key_modifiers.ctrl = key_event.event_type == EventType::KeyDown;
            }
            KeyCode::LeftAlt | KeyCode::RightAlt => {
                self.key_modifiers.alt = key_event.event_type == EventType::KeyDown;
            }
            KeyCode::LeftSuper | KeyCode::RightSuper => {
                self.key_modifiers.super_key = key_event.event_type == EventType::KeyDown;
            }
            KeyCode::CapsLock => {
                if key_event.event_type == EventType::KeyDown {
                    self.key_modifiers.caps_lock = !self.key_modifiers.caps_lock;
                }
            }
            KeyCode::NumLock => {
                if key_event.event_type == EventType::KeyDown {
                    self.key_modifiers.num_lock = !self.key_modifiers.num_lock;
                }
            }
            KeyCode::ScrollLock => {
                if key_event.event_type == EventType::KeyDown {
                    self.key_modifiers.scroll_lock = !self.key_modifiers.scroll_lock;
                }
            }
            _ => {}
        }
    }
    
    /// Enviar evento de teclado
    pub fn send_key_event(&mut self, key_code: KeyCode, event_type: EventType, character: Option<char>) {
        let event = KeyEvent {
            event_type,
            key_code,
            modifiers: self.key_modifiers,
            character,
            timestamp: 0, // TODO: Obtener timestamp actual
        };
        
        self.event_queue.push(Event::Key(event));
    }
    
    /// Enviar evento de ratón
    pub fn send_mouse_event(&mut self, event_type: EventType, position: Point, button: Option<MouseButton>, wheel_delta: i32) {
        let event = MouseEvent {
            event_type,
            position,
            button,
            wheel_delta,
            modifiers: self.key_modifiers,
            timestamp: 0, // TODO: Obtener timestamp actual
        };
        
        self.event_queue.push(Event::Mouse(event));
    }
    
    /// Enviar evento de ventana
    pub fn send_window_event(&mut self, event_type: EventType, window_id: WindowId, position: Option<Point>, size: Option<(u32, u32)>) {
        let event = WindowEvent {
            event_type,
            window_id,
            position,
            size,
            timestamp: 0, // TODO: Obtener timestamp actual
        };
        
        self.event_queue.push(Event::Window(event));
    }
    
    /// Obtener posición actual del ratón
    pub fn get_mouse_position(&self) -> Point {
        self.mouse_position
    }
    
    /// Verificar si un botón del ratón está presionado
    pub fn is_mouse_button_pressed(&self, button: MouseButton) -> bool {
        let button_index = match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::Extra1 => 3,
            MouseButton::Extra2 => 4,
        };
        self.mouse_buttons[button_index]
    }
    
    /// Obtener modificadores actuales
    pub fn get_key_modifiers(&self) -> KeyModifiers {
        self.key_modifiers
    }
}

/// Sistema de eventos global
static mut EVENT_SYSTEM: Option<EventSystem> = None;

/// Inicializar el sistema de eventos
pub fn init_event_system() {
    let system = EventSystem::new();
    unsafe {
        EVENT_SYSTEM = Some(system);
    }
}

/// Obtener referencia al sistema de eventos
pub fn get_event_system() -> Option<&'static mut EventSystem> {
    unsafe {
        EVENT_SYSTEM.as_mut()
    }
}

/// Procesar eventos pendientes
pub fn process_pending_events() {
    if let Some(system) = get_event_system() {
        system.process_events();
    }
}

/// Agregar manejador de eventos
pub fn add_event_handler(handler: EventHandler) -> bool {
    get_event_system().map_or(false, |system| system.add_handler(handler))
}

/// Enviar evento de teclado
pub fn send_key_event(key_code: KeyCode, event_type: EventType, character: Option<char>) {
    if let Some(system) = get_event_system() {
        system.send_key_event(key_code, event_type, character);
    }
}

/// Enviar evento de ratón
pub fn send_mouse_event(event_type: EventType, position: Point, button: Option<MouseButton>, wheel_delta: i32) {
    if let Some(system) = get_event_system() {
        system.send_mouse_event(event_type, position, button, wheel_delta);
    }
}
