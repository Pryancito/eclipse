//! Driver de ratón para Eclipse OS
//!
//! Define las interfaces y tipos básicos para drivers de ratón.

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType},
    manager::{Driver, DriverError, DriverInfo, DriverResult},
};

/// Botones del ratón
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Button4,
    Button5,
    None,
    Wheel,
}

/// Estado del ratón
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseState {
    Pressed,
    Released,
    Moved,
    WheelUp,
    WheelDown,
}

/// Evento de ratón
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
    pub button: MouseButton,
    pub state: MouseState,
    pub x: i32,
    pub y: i32,
    pub wheel: i8,
}

impl MouseEvent {
    pub fn new(button: MouseButton, state: MouseState, x: i32, y: i32, wheel: i8) -> Self {
        Self {
            button,
            state,
            x,
            y,
            wheel,
        }
    }
}

/// Driver de ratón base
pub trait MouseDriver {
    /// Leer siguiente evento del ratón
    fn read_event(&mut self) -> Option<MouseEvent>;

    /// Verificar si un botón está presionado
    fn is_button_pressed(&self, button: MouseButton) -> bool;

    /// Obtener posición actual del ratón
    fn get_position(&self) -> (i32, i32);

    /// Establecer posición del ratón
    fn set_position(&mut self, x: i32, y: i32);

    /// Obtener valor de la rueda
    fn get_wheel(&self) -> i8;

    /// Limpiar buffer de eventos
    fn clear_buffer(&mut self);

    /// Verificar si hay eventos pendientes
    fn has_events(&self) -> bool;
}

/// Driver de ratón básico
pub struct BasicMouseDriver {
    pub info: DriverInfo,
    pub is_initialized: bool,
    pub x: i32,
    pub y: i32,
}

impl BasicMouseDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("basic_mouse");
        info.device_type = DeviceType::Input;
        info.version = 1;

        Self {
            info,
            is_initialized: false,
            x: 0,
            y: 0,
        }
    }
}

impl Driver for BasicMouseDriver {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        self.is_initialized = true;
        self.info.is_loaded = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        self.is_initialized = false;
        self.info.is_loaded = false;
        Ok(())
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        device_info.device_type == DeviceType::Input
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        device.driver_id = Some(self.info.id);
        Ok(())
    }

    fn detach_device(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }

    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }
}

impl MouseDriver for BasicMouseDriver {
    fn read_event(&mut self) -> Option<MouseEvent> {
        // Implementación básica - no hay eventos por defecto
        None
    }

    fn is_button_pressed(&self, _button: MouseButton) -> bool {
        false
    }

    fn get_position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn get_wheel(&self) -> i8 {
        0
    }

    fn clear_buffer(&mut self) {
        // No hay buffer que limpiar
    }

    fn has_events(&self) -> bool {
        false
    }
}
