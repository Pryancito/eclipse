//! Driver de ratón para Eclipse OS
//!
//! Define las interfaces y tipos básicos para drivers de ratón.
//! Incluye driver PS/2 completo con soporte para movimiento, botones y rueda.

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

/// Wrapper para Port I/O del ratón
#[derive(Debug, Clone, Copy)]
pub struct MousePort {
    port: u16,
}

impl MousePort {
    pub const fn new(port: u16) -> Self {
        Self { port }
    }

    /// Leer un byte del puerto
    pub unsafe fn read(&self) -> u8 {
        let value: u8;
        core::arch::asm!("in al, dx", out("al") value, in("dx") self.port, options(nomem, nostack, preserves_flags));
        value
    }

    /// Escribir un byte al puerto
    pub unsafe fn write(&self, value: u8) {
        core::arch::asm!("out dx, al", in("dx") self.port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Puertos del controlador PS/2
const PS2_DATA_PORT: u16 = 0x60;
const PS2_STATUS_PORT: u16 = 0x64;
const PS2_COMMAND_PORT: u16 = 0x64;

/// Comandos del controlador PS/2
const PS2_CMD_ENABLE_AUX: u8 = 0xA8;
const PS2_CMD_DISABLE_AUX: u8 = 0xA7;
const PS2_CMD_WRITE_TO_MOUSE: u8 = 0xD4;

/// Comandos del ratón PS/2
const MOUSE_CMD_RESET: u8 = 0xFF;
const MOUSE_CMD_ENABLE_REPORTING: u8 = 0xF4;
const MOUSE_CMD_DISABLE_REPORTING: u8 = 0xF5;
const MOUSE_CMD_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_CMD_SET_SAMPLE_RATE: u8 = 0xF3;
const MOUSE_CMD_GET_DEVICE_ID: u8 = 0xF2;

/// Respuestas del ratón
const MOUSE_ACK: u8 = 0xFA;
const MOUSE_SELF_TEST_PASSED: u8 = 0xAA;

/// Driver PS/2 del ratón
pub struct PS2MouseDriver {
    pub info: DriverInfo,
    data_port: MousePort,
    status_port: MousePort,
    command_port: MousePort,
    
    // Estado del ratón
    x: i32,
    y: i32,
    wheel: i8,
    
    // Estado de botones
    left_button: bool,
    right_button: bool,
    middle_button: bool,
    
    // Buffer de paquetes
    packet_buffer: [u8; 4],
    packet_index: usize,
    packet_size: usize,  // 3 para ratón estándar, 4 para ratón con rueda
    
    // Flags
    is_initialized: bool,
    supports_wheel: bool,
}

impl PS2MouseDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("ps2_mouse");
        info.device_type = DeviceType::Input;
        info.version = 1;

        Self {
            info,
            data_port: MousePort::new(PS2_DATA_PORT),
            status_port: MousePort::new(PS2_STATUS_PORT),
            command_port: MousePort::new(PS2_COMMAND_PORT),
            x: 0,
            y: 0,
            wheel: 0,
            left_button: false,
            right_button: false,
            middle_button: false,
            packet_buffer: [0; 4],
            packet_index: 0,
            packet_size: 3,
            is_initialized: false,
            supports_wheel: false,
        }
    }

    /// Esperar hasta que el buffer de entrada esté lleno
    unsafe fn wait_for_input(&self) {
        for _ in 0..100000 {
            if (self.status_port.read() & 0x01) != 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Esperar hasta que el buffer de salida esté vacío
    unsafe fn wait_for_output(&self) {
        for _ in 0..100000 {
            if (self.status_port.read() & 0x02) == 0 {
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Enviar comando al ratón
    unsafe fn send_mouse_command(&self, command: u8) -> Result<u8, &'static str> {
        // Decir al controlador que queremos enviar un comando al ratón
        self.wait_for_output();
        self.command_port.write(PS2_CMD_WRITE_TO_MOUSE);
        
        // Enviar el comando
        self.wait_for_output();
        self.data_port.write(command);
        
        // Esperar respuesta
        self.wait_for_input();
        let response = self.data_port.read();
        
        if response == MOUSE_ACK {
            Ok(response)
        } else {
            Err("Mouse did not acknowledge command")
        }
    }

    /// Leer un byte del ratón
    unsafe fn read_mouse_byte(&self) -> Result<u8, &'static str> {
        self.wait_for_input();
        Ok(self.data_port.read())
    }

    /// Intentar habilitar soporte para rueda del ratón
    unsafe fn try_enable_scroll_wheel(&mut self) -> bool {
        // Secuencia mágica para habilitar la rueda: establecer sample rate a 200, 100, 80
        if self.send_mouse_command(MOUSE_CMD_SET_SAMPLE_RATE).is_err() {
            return false;
        }
        if self.send_mouse_command(200).is_err() {
            return false;
        }
        
        if self.send_mouse_command(MOUSE_CMD_SET_SAMPLE_RATE).is_err() {
            return false;
        }
        if self.send_mouse_command(100).is_err() {
            return false;
        }
        
        if self.send_mouse_command(MOUSE_CMD_SET_SAMPLE_RATE).is_err() {
            return false;
        }
        if self.send_mouse_command(80).is_err() {
            return false;
        }
        
        // Verificar el ID del dispositivo
        if self.send_mouse_command(MOUSE_CMD_GET_DEVICE_ID).is_err() {
            return false;
        }
        
        match self.read_mouse_byte() {
            Ok(id) => {
                // ID 3 = ratón con rueda, ID 4 = ratón con 5 botones
                if id == 3 || id == 4 {
                    self.packet_size = 4;
                    true
                } else {
                    self.packet_size = 3;
                    false
                }
            }
            Err(_) => {
                self.packet_size = 3;
                false
            }
        }
    }

    /// Verificar si hay datos disponibles
    fn has_data(&self) -> bool {
        unsafe { (self.status_port.read() & 0x01) != 0 }
    }

    /// Procesar un paquete completo del ratón
    fn process_packet(&mut self) -> Option<MouseEvent> {
        if self.packet_index < self.packet_size {
            return None;
        }

        // Resetear índice
        self.packet_index = 0;

        let flags = self.packet_buffer[0];
        let x_movement = self.packet_buffer[1] as i8;
        let y_movement = self.packet_buffer[2] as i8;
        
        // Verificar bit de overflow y validez del paquete
        if (flags & 0xC0) != 0 {
            // Overflow o bit reservado activado, descartar paquete
            return None;
        }

        // Actualizar estado de botones
        let new_left = (flags & 0x01) != 0;
        let new_right = (flags & 0x02) != 0;
        let new_middle = (flags & 0x04) != 0;

        // Aplicar signo extendido si es necesario
        let mut dx = x_movement as i32;
        let mut dy = -(y_movement as i32); // Invertir Y para coordenadas de pantalla

        if (flags & 0x10) != 0 {
            dx |= !0xFF; // Extender signo para X
        }
        if (flags & 0x20) != 0 {
            dy |= !0xFF; // Extender signo para Y
        }

        // Actualizar posición
        self.x += dx;
        self.y += dy;

        // Procesar rueda si está soportada
        let mut wheel_delta = 0i8;
        if self.supports_wheel && self.packet_size == 4 {
            wheel_delta = (self.packet_buffer[3] & 0x0F) as i8;
            // Convertir de unsigned a signed (4 bits)
            if wheel_delta >= 8 {
                wheel_delta -= 16;
            }
            self.wheel += wheel_delta;
        }

        // Determinar tipo de evento
        let event = if dx != 0 || dy != 0 {
            Some(MouseEvent::new(
                MouseButton::None,
                MouseState::Moved,
                self.x,
                self.y,
                wheel_delta,
            ))
        } else if wheel_delta != 0 {
            Some(MouseEvent::new(
                MouseButton::Wheel,
                if wheel_delta > 0 { MouseState::WheelUp } else { MouseState::WheelDown },
                self.x,
                self.y,
                wheel_delta,
            ))
        } else if new_left != self.left_button {
            self.left_button = new_left;
            Some(MouseEvent::new(
                MouseButton::Left,
                if new_left { MouseState::Pressed } else { MouseState::Released },
                self.x,
                self.y,
                0,
            ))
        } else if new_right != self.right_button {
            self.right_button = new_right;
            Some(MouseEvent::new(
                MouseButton::Right,
                if new_right { MouseState::Pressed } else { MouseState::Released },
                self.x,
                self.y,
                0,
            ))
        } else if new_middle != self.middle_button {
            self.middle_button = new_middle;
            Some(MouseEvent::new(
                MouseButton::Middle,
                if new_middle { MouseState::Pressed } else { MouseState::Released },
                self.x,
                self.y,
                0,
            ))
        } else {
            None
        };

        event
    }

    /// Inicializar el ratón PS/2
    pub fn init_mouse(&mut self) -> Result<(), &'static str> {
        unsafe {
            // Habilitar puerto auxiliar (ratón)
            self.wait_for_output();
            self.command_port.write(PS2_CMD_ENABLE_AUX);

            // Restablecer valores predeterminados del ratón
            self.send_mouse_command(MOUSE_CMD_SET_DEFAULTS)?;

            // Intentar habilitar rueda del ratón
            self.supports_wheel = self.try_enable_scroll_wheel();

            // Habilitar reporte de datos
            self.send_mouse_command(MOUSE_CMD_ENABLE_REPORTING)?;

            self.is_initialized = true;
            Ok(())
        }
    }
}

impl Driver for PS2MouseDriver {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        self.init_mouse()
            .map_err(|_| DriverError::IoError)?;
        self.info.is_loaded = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        unsafe {
            // Deshabilitar reporte del ratón
            let _ = self.send_mouse_command(MOUSE_CMD_DISABLE_REPORTING);
            
            // Deshabilitar puerto auxiliar
            self.wait_for_output();
            self.command_port.write(PS2_CMD_DISABLE_AUX);
        }
        
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
        // Leer byte del ratón cuando se recibe interrupción
        if self.has_data() {
            unsafe {
                let byte = self.data_port.read();
                self.packet_buffer[self.packet_index] = byte;
                self.packet_index += 1;
            }
        }
        Ok(())
    }
}

impl MouseDriver for PS2MouseDriver {
    fn read_event(&mut self) -> Option<MouseEvent> {
        // Leer bytes disponibles
        while self.has_data() && self.packet_index < self.packet_size {
            unsafe {
                let byte = self.data_port.read();
                self.packet_buffer[self.packet_index] = byte;
                self.packet_index += 1;
            }
        }

        // Procesar paquete si está completo
        self.process_packet()
    }

    fn is_button_pressed(&self, button: MouseButton) -> bool {
        match button {
            MouseButton::Left => self.left_button,
            MouseButton::Right => self.right_button,
            MouseButton::Middle => self.middle_button,
            _ => false,
        }
    }

    fn get_position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    fn get_wheel(&self) -> i8 {
        self.wheel
    }

    fn clear_buffer(&mut self) {
        while self.has_data() {
            unsafe {
                self.data_port.read();
            }
        }
        self.packet_index = 0;
    }

    fn has_events(&self) -> bool {
        self.has_data() || self.packet_index > 0
    }
}
