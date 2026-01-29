//! Integración de dispositivos PS/2 con el sistema de entrada de Eclipse OS
//!
//! Este módulo proporciona la integración entre los drivers PS/2 (teclado y ratón)
//! y el sistema de entrada unificado.

use crate::drivers::{
    keyboard::{BasicKeyboardDriver, KeyboardDriver, KeyCode, KeyEvent, KeyState},
    mouse::{MouseDriver, MouseEvent, PS2MouseDriver},
    input_system::{push_input_event, InputEvent, InputEventType, SystemEvent},
    usb_keyboard::{KeyboardEvent as UsbKeyboardEvent, ModifierState, UsbKeyCode},
    usb_mouse::{MouseEvent as UsbMouseEvent, MouseButton, MouseButtonState, MousePosition},
};
use alloc::string::ToString;
use spin::Mutex;

/// Sistema global de drivers PS/2
pub static PS2_SYSTEM: Mutex<Option<PS2System>> = Mutex::new(None);

/// Sistema de drivers PS/2
pub struct PS2System {
    pub keyboard: BasicKeyboardDriver,
    pub mouse: PS2MouseDriver,
    pub keyboard_enabled: bool,
    pub mouse_enabled: bool,
}

impl PS2System {
    /// Crear nuevo sistema PS/2
    pub fn new() -> Self {
        Self {
            keyboard: BasicKeyboardDriver::new(),
            mouse: PS2MouseDriver::new(),
            keyboard_enabled: false,
            mouse_enabled: false,
        }
    }

    /// Inicializar el sistema PS/2
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        use crate::drivers::manager::Driver;
        
        // Intentar inicializar teclado
        match Driver::initialize(&mut self.keyboard) {
            Ok(_) => {
                self.keyboard_enabled = true;
            }
            Err(_) => {
                // No es crítico si falla
                self.keyboard_enabled = false;
            }
        }

        // Intentar inicializar ratón
        match Driver::initialize(&mut self.mouse) {
            Ok(_) => {
                self.mouse_enabled = true;
            }
            Err(_) => {
                // No es crítico si falla
                self.mouse_enabled = false;
            }
        }

        // Al menos uno debe funcionar
        if !self.keyboard_enabled && !self.mouse_enabled {
            return Err("No se pudo inicializar ningún dispositivo PS/2");
        }

        Ok(())
    }

    /// Procesar eventos del teclado PS/2
    pub fn process_keyboard_events(&mut self) {
        if !self.keyboard_enabled {
            return;
        }

        while let Some(key_event) = self.keyboard.read_key() {
            // Convertir evento PS/2 a evento del sistema
            if let Some(input_event) = convert_ps2_key_event_to_input_event(key_event) {
                let _ = push_input_event(input_event);
            }
        }
    }

    /// Procesar eventos del ratón PS/2
    pub fn process_mouse_events(&mut self) {
        if !self.mouse_enabled {
            return;
        }

        while let Some(mouse_event) = self.mouse.read_event() {
            // Convertir evento PS/2 a evento del sistema
            if let Some(input_event) = convert_ps2_mouse_event_to_input_event(mouse_event) {
                let _ = push_input_event(input_event);
            }
        }
    }

    /// Procesar todos los eventos PS/2
    pub fn process_all_events(&mut self) {
        self.process_keyboard_events();
        self.process_mouse_events();
    }

    /// Manejar interrupción del teclado (IRQ 1)
    pub fn handle_keyboard_interrupt(&mut self) {
        use crate::drivers::manager::Driver;
        if self.keyboard_enabled {
            let _ = Driver::handle_interrupt(&mut self.keyboard, 0);
        }
    }

    /// Manejar interrupción del ratón (IRQ 12)
    pub fn handle_mouse_interrupt(&mut self) {
        use crate::drivers::manager::Driver;
        if self.mouse_enabled {
            let _ = Driver::handle_interrupt(&mut self.mouse, 0);
        }
    }
}

/// Convertir evento de teclado PS/2 a evento del sistema de entrada
fn convert_ps2_key_event_to_input_event(key_event: KeyEvent) -> Option<InputEvent> {
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
        KeyCode::Key0 => UsbKeyCode::Num0,
        KeyCode::Key1 => UsbKeyCode::Num1,
        KeyCode::Key2 => UsbKeyCode::Num2,
        KeyCode::Key3 => UsbKeyCode::Num3,
        KeyCode::Key4 => UsbKeyCode::Num4,
        KeyCode::Key5 => UsbKeyCode::Num5,
        KeyCode::Key6 => UsbKeyCode::Num6,
        KeyCode::Key7 => UsbKeyCode::Num7,
        KeyCode::Key8 => UsbKeyCode::Num8,
        KeyCode::Key9 => UsbKeyCode::Num9,
        KeyCode::Enter => UsbKeyCode::Enter,
        KeyCode::Escape => UsbKeyCode::Escape,
        KeyCode::Backspace => UsbKeyCode::Backspace,
        KeyCode::Tab => UsbKeyCode::Tab,
        KeyCode::Space => UsbKeyCode::Space,
        KeyCode::Minus => UsbKeyCode::Minus,
        KeyCode::Equal => UsbKeyCode::Equal,
        KeyCode::LeftBracket => UsbKeyCode::LeftBracket,
        KeyCode::RightBracket => UsbKeyCode::RightBracket,
        KeyCode::Backslash => UsbKeyCode::Backslash,
        KeyCode::Semicolon => UsbKeyCode::Semicolon,
        KeyCode::Quote | KeyCode::Apostrophe => UsbKeyCode::Quote,
        KeyCode::Grave => UsbKeyCode::Grave,
        KeyCode::Comma => UsbKeyCode::Comma,
        KeyCode::Period => UsbKeyCode::Period,
        KeyCode::Slash => UsbKeyCode::Slash,
        KeyCode::CapsLock => UsbKeyCode::CapsLock,
        KeyCode::F1 => UsbKeyCode::F1,
        KeyCode::F2 => UsbKeyCode::F2,
        KeyCode::F3 => UsbKeyCode::F3,
        KeyCode::F4 => UsbKeyCode::F4,
        KeyCode::F5 => UsbKeyCode::F5,
        KeyCode::F6 => UsbKeyCode::F6,
        KeyCode::F7 => UsbKeyCode::F7,
        KeyCode::F8 => UsbKeyCode::F8,
        KeyCode::F9 => UsbKeyCode::F9,
        KeyCode::F10 => UsbKeyCode::F10,
        KeyCode::F11 => UsbKeyCode::F11,
        KeyCode::F12 => UsbKeyCode::F12,
        KeyCode::Insert => UsbKeyCode::Insert,
        KeyCode::Home => UsbKeyCode::Home,
        KeyCode::PageUp => UsbKeyCode::PageUp,
        KeyCode::Delete => UsbKeyCode::Delete,
        KeyCode::End => UsbKeyCode::End,
        KeyCode::PageDown => UsbKeyCode::PageDown,
        KeyCode::Right => UsbKeyCode::Right,
        KeyCode::Left => UsbKeyCode::Left,
        KeyCode::Down => UsbKeyCode::Down,
        KeyCode::Up => UsbKeyCode::Up,
        KeyCode::LeftShift | KeyCode::Shift => UsbKeyCode::LeftShift,
        KeyCode::RightShift => UsbKeyCode::RightShift,
        KeyCode::LeftCtrl | KeyCode::Ctrl => UsbKeyCode::LeftCtrl,
        KeyCode::RightCtrl => UsbKeyCode::RightCtrl,
        KeyCode::LeftAlt | KeyCode::Alt => UsbKeyCode::LeftAlt,
        KeyCode::RightAlt => UsbKeyCode::RightAlt,
        _ => UsbKeyCode::Unknown,
    };

    // Convertir modificadores
    let modifier_state = ModifierState {
        left_ctrl: key_event.modifiers & 0x01 != 0,
        right_ctrl: false,
        left_shift: key_event.modifiers & 0x02 != 0,
        right_shift: false,
        left_alt: key_event.modifiers & 0x04 != 0,
        right_alt: false,
        left_meta: false,
        right_meta: false,
        num_lock: false,
        caps_lock: false,
        scroll_lock: false,
    };

    let keyboard_event = UsbKeyboardEvent {
        key_code: usb_key_code,
        modifiers: modifier_state,
        pressed: matches!(key_event.state, KeyState::Pressed),
        character: None,
        timestamp: 0,
    };

    Some(InputEvent::new(
        InputEventType::Keyboard(keyboard_event),
        1, // Device ID para teclado PS/2
        0, // Timestamp
    ))
}

/// Convertir evento de ratón PS/2 a evento del sistema de entrada
fn convert_ps2_mouse_event_to_input_event(mouse_event: MouseEvent) -> Option<InputEvent> {
    use crate::drivers::mouse::{MouseButton as PS2MouseButton, MouseState};

    let position = MousePosition::new_with_coords(mouse_event.x, mouse_event.y);

    let usb_mouse_event = match mouse_event.state {
        MouseState::Pressed => {
            let button = match mouse_event.button {
                PS2MouseButton::Left => MouseButton::Left,
                PS2MouseButton::Right => MouseButton::Right,
                PS2MouseButton::Middle => MouseButton::Middle,
                _ => MouseButton::Left,
            };
            UsbMouseEvent::ButtonPress { button, position }
        }
        MouseState::Released => {
            let button = match mouse_event.button {
                PS2MouseButton::Left => MouseButton::Left,
                PS2MouseButton::Right => MouseButton::Right,
                PS2MouseButton::Middle => MouseButton::Middle,
                _ => MouseButton::Left,
            };
            UsbMouseEvent::ButtonRelease { button, position }
        }
        MouseState::Moved => UsbMouseEvent::Move {
            position,
            buttons: MouseButtonState::new(),
        },
        MouseState::WheelUp => UsbMouseEvent::Scroll {
            delta: mouse_event.wheel as i32,
            position,
        },
        MouseState::WheelDown => UsbMouseEvent::Scroll {
            delta: mouse_event.wheel as i32,
            position,
        },
    };

    Some(InputEvent::new(
        InputEventType::Mouse(usb_mouse_event),
        2, // Device ID para ratón PS/2
        0, // Timestamp
    ))
}

/// Inicializar el sistema PS/2 global
pub fn init_ps2_system() -> Result<(), &'static str> {
    let mut system = PS2System::new();
    system.initialize()?;

    // Habilitar IRQs del PIC para dispositivos PS/2
    // IRQ 1 = Teclado PS/2
    // IRQ 12 = Ratón PS/2
    if let Err(e) = enable_ps2_irqs() {
        // Registrar el error pero no es crítico - el sistema puede funcionar sin interrupciones
        // usando polling como fallback
        #[cfg(feature = "logging")]
        crate::logging::log_error!("PS/2", "No se pudieron habilitar IRQs: {}", e);
    }

    *PS2_SYSTEM.lock() = Some(system);
    Ok(())
}

/// Habilitar las IRQs del PIC para dispositivos PS/2
fn enable_ps2_irqs() -> Result<(), &'static str> {
    use crate::interrupts::pic::PicManager;
    
    // Nota: El PIC debe ser inicializado solo una vez globalmente.
    // Aquí solo habilitamos las IRQs específicas.
    let pic = PicManager::new();
    
    // Habilitar IRQ 1 (teclado) - está en el PIC primario
    if let Err(e) = pic.enable_irq(1) {
        return Err("No se pudo habilitar IRQ 1 del teclado");
    }
    
    // Habilitar IRQ 12 (ratón) - está en el PIC secundario
    if let Err(e) = pic.enable_irq(12) {
        return Err("No se pudo habilitar IRQ 12 del ratón");
    }
    
    Ok(())
}

/// Procesar eventos PS/2 desde el sistema global
pub fn process_ps2_events() {
    if let Some(ref mut system) = *PS2_SYSTEM.lock() {
        system.process_all_events();
    }
}

/// Manejar interrupción del teclado PS/2 (IRQ 1)
pub fn handle_ps2_keyboard_interrupt() {
    if let Some(ref mut system) = *PS2_SYSTEM.lock() {
        system.handle_keyboard_interrupt();
    }
}

/// Manejar interrupción del ratón PS/2 (IRQ 12)
pub fn handle_ps2_mouse_interrupt() {
    if let Some(ref mut system) = *PS2_SYSTEM.lock() {
        system.handle_mouse_interrupt();
    }
}

/// Verificar si el teclado PS/2 está habilitado
pub fn is_ps2_keyboard_enabled() -> bool {
    PS2_SYSTEM
        .lock()
        .as_ref()
        .map(|s| s.keyboard_enabled)
        .unwrap_or(false)
}

/// Verificar si el ratón PS/2 está habilitado
pub fn is_ps2_mouse_enabled() -> bool {
    PS2_SYSTEM
        .lock()
        .as_ref()
        .map(|s| s.mouse_enabled)
        .unwrap_or(false)
}
