//! Controlador del Puerto Serie para Eclipse OS Kernel
//!
//! Este módulo implementa un controlador robusto para el puerto serie COM1
//! usando el chip 16550 UART. Proporciona funcionalidades para inicialización,
//! escritura y verificación del estado del puerto.

use core::fmt;

/// Estructura que representa el puerto serie COM1
pub struct SerialPort {
    /// Dirección base del puerto serie COM1
    base_address: u16,
}

impl SerialPort {
    /// Crear una nueva instancia del puerto serie COM1
    pub const fn new() -> Self {
        Self {
            base_address: 0x3F8,  // COM1 base address
        }
    }

    /// Inicializar el puerto serie con configuración estándar
    ///
    /// Configura:
    /// - Velocidad de baudios: 115200
    /// - Formato: 8 bits datos, sin paridad, 1 bit de parada
    /// - Interrupciones: deshabilitadas
    /// - FIFO: habilitada con trigger de 14 bytes
    pub fn init(&mut self) {
        unsafe {
            // Deshabilitar interrupciones (leer registro de interrupción)
            self.read_port(2); // Limpiar cualquier interrupción pendiente

            // Configurar el divisor de baudios (DLAB = 1)
            self.write_port(3, 0x80);

            // Establecer velocidad de baudios a 115200
            // Divisor = 115200 / velocidad_deseada
            // Para 115200: divisor = 1
            self.write_port(0, 0x01);    // LSB del divisor
            self.write_port(1, 0x00);    // MSB del divisor

            // Configurar formato de datos: 8 bits, sin paridad, 1 bit de parada (DLAB = 0)
            self.write_port(3, 0x03);

            // Habilitar FIFO con trigger de 14 bytes
            self.write_port(2, 0xC7);

            // Configurar control de módem: RTS y DTR habilitados
            self.write_port(4, 0x0B);

            // Limpiar cualquier dato pendiente
            let _ = self.read_port(0);
        }
    }

    /// Función auxiliar para escribir en un puerto I/O
    unsafe fn write_port(&mut self, offset: u16, value: u8) {
        let port = self.base_address + offset;
        core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack));
    }

    /// Función auxiliar para leer de un puerto I/O
    unsafe fn read_port(&mut self, offset: u16) -> u8 {
        let port = self.base_address + offset;
        let mut value: u8;
        core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack));
        value
    }

    /// Escribir un byte al puerto serie
    ///
    /// Espera hasta que el buffer de transmisión esté vacío
    pub fn write_byte(&mut self, byte: u8) {
        unsafe {
            // Esperar hasta que el buffer de transmisión esté vacío
            while !self.is_transmit_empty() {}

            // Escribir el byte
            self.write_port(0, byte);
        }
    }

    /// Escribir una cadena al puerto serie
    pub fn write_str(&mut self, s: &str) {
        if !s.is_ascii() {
            return;
        }
        for byte in s.bytes() {
            self.write_byte(byte);
        }
    }

    /// Escribir un número en formato hexadecimal
    pub fn write_hex(&mut self, value: u64) {
        self.write_str("0x");

        // Convertir a hexadecimal
        let mut temp = value;
        let mut started = false;

        // Procesar de MSB a LSB
        for i in (0..16).rev() {
            let nibble = ((temp >> (i * 4)) & 0xF) as u8;

            if nibble != 0 || started || i == 0 {
                started = true;
                let hex_char = if nibble < 10 {
                    b'0' + nibble
                } else {
                    b'A' + (nibble - 10)
                };
                self.write_byte(hex_char);
            }
        }

        // Si el valor es 0, escribir al menos un 0
        if !started {
            self.write_byte(b'0');
        }
    }

    /// Verificar si el buffer de transmisión está vacío
    fn is_transmit_empty(&mut self) -> bool {
        unsafe {
            (self.read_port(5) & 0x20) != 0
        }
    }

    /// Verificar si hay datos disponibles para leer
    pub fn is_data_available(&mut self) -> bool {
        unsafe {
            (self.read_port(5) & 0x01) != 0
        }
    }

    /// Leer un byte del puerto serie (si está disponible)
    pub fn read_byte(&mut self) -> Option<u8> {
        if self.is_data_available() {
            unsafe { Some(self.read_port(0)) }
        } else {
            None
        }
    }

    /// Obtener el estado actual del puerto serie
    pub fn get_status(&mut self) -> SerialStatus {
        unsafe {
            let line_status = self.read_port(5);
            let modem_status = self.read_port(6);

            SerialStatus {
                data_ready: (line_status & 0x01) != 0,
                overrun_error: (line_status & 0x02) != 0,
                parity_error: (line_status & 0x04) != 0,
                framing_error: (line_status & 0x08) != 0,
                break_interrupt: (line_status & 0x10) != 0,
                transmit_empty: (line_status & 0x20) != 0,
                transmit_holding_empty: (line_status & 0x40) != 0,
                dcd: (modem_status & 0x80) != 0,
                ri: (modem_status & 0x40) != 0,
                dsr: (modem_status & 0x20) != 0,
                cts: (modem_status & 0x10) != 0,
            }
        }
    }
}

impl core::fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_str(s);
        Ok(())
    }
}

/// Estado del puerto serie
#[derive(Debug, Clone, Copy)]
pub struct SerialStatus {
    pub data_ready: bool,
    pub overrun_error: bool,
    pub parity_error: bool,
    pub framing_error: bool,
    pub break_interrupt: bool,
    pub transmit_empty: bool,
    pub transmit_holding_empty: bool,
    pub dcd: bool,  // Data Carrier Detect
    pub ri: bool,   // Ring Indicator
    pub dsr: bool,  // Data Set Ready
    pub cts: bool,  // Clear To Send
}

impl fmt::Display for SerialStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Serial Status: ")?;
        if self.data_ready { write!(f, "DR ")?; }
        if self.overrun_error { write!(f, "OE ")?; }
        if self.parity_error { write!(f, "PE ")?; }
        if self.framing_error { write!(f, "FE ")?; }
        if self.break_interrupt { write!(f, "BI ")?; }
        if self.transmit_empty { write!(f, "THRE ")?; }
        if self.transmit_holding_empty { write!(f, "TEMT ")?; }
        if self.dcd { write!(f, "DCD ")?; }
        if self.ri { write!(f, "RI ")?; }
        if self.dsr { write!(f, "DSR ")?; }
        if self.cts { write!(f, "CTS ")?; }
        Ok(())
    }
}

/// Instancia global del puerto serie
static mut SERIAL_PORT: Option<SerialPort> = None;

/// Obtener referencia al puerto serie global
pub fn get_serial_port() -> &'static mut SerialPort {
    unsafe {
        if SERIAL_PORT.is_none() {
            SERIAL_PORT = Some(SerialPort::new());
        }
        SERIAL_PORT.as_mut().unwrap()
    }
}

/// Inicializar el puerto serie global
pub fn init() {
    let port = get_serial_port();
    port.init();
}

/// Función de escritura global para compatibilidad
pub fn write_str(s: &str) {
    get_serial_port().write_str(s);
}

/// Función de escritura de byte global para compatibilidad
pub fn write_byte(byte: u8) {
    get_serial_port().write_byte(byte);
}

/// Función de escritura hexadecimal global para compatibilidad
pub fn write_hex(value: u64) {
    get_serial_port().write_hex(value);
}
