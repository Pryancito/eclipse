//! Serial Communication Module (COM1-COM4) for debugging and I/O
//!
//! Provides serial port communication for debugging output and input.
//!
//! ## Current Features
//! - COM1 support (0x3F8) - primary port
//! - Output functionality (transmit)
//! - Input functionality (receive with buffering)
//! - 38400 baud rate
//! - 8N1 configuration (8 data bits, no parity, 1 stop bit)
//! - FIFO buffers enabled
//!
//! ## Limitations
//! - No interrupt-driven I/O (uses polling)
//! - COM2-COM4 not yet implemented
//! - Fixed baud rate (38400)
//! - No hardware flow control
//!
//! ## Future Enhancements
//! - Interrupt-driven I/O for better performance
//! - COM2-COM4 support
//! - Configurable baud rates
//! - Hardware flow control (RTS/CTS)

use core::arch::asm;
use spin::Mutex;

const SERIAL_PORT: u16 = 0x3F8; // COM1

/// Estado del puerto serial
static SERIAL_INITIALIZED: Mutex<bool> = Mutex::new(false);

/// Inicializar el puerto serial
pub fn init() {
    unsafe {
        // Deshabilitar interrupciones
        outb(SERIAL_PORT + 1, 0x00);
        
        // Habilitar DLAB (Divisor Latch Access Bit)
        outb(SERIAL_PORT + 3, 0x80);
        
        // Configurar baud rate a 38400 (divisor = 3)
        outb(SERIAL_PORT + 0, 0x03); // Divisor low byte
        outb(SERIAL_PORT + 1, 0x00); // Divisor high byte
        
        // Configurar: 8 bits, sin paridad, 1 stop bit
        outb(SERIAL_PORT + 3, 0x03);
        
        // Habilitar FIFO, limpiar buffers, trigger level 14 bytes
        outb(SERIAL_PORT + 2, 0xC7);
        
        // IRQs habilitadas, RTS/DSR set
        outb(SERIAL_PORT + 4, 0x0B);
        
        // Modo loopback para test
        outb(SERIAL_PORT + 4, 0x1E);
        
        // Enviar byte de test
        outb(SERIAL_PORT + 0, 0xAE);
        
        // Verificar que el serial funciona
        if inb(SERIAL_PORT + 0) == 0xAE {
            // Serial funciona, configurar en modo normal
            outb(SERIAL_PORT + 4, 0x0F);
            *SERIAL_INITIALIZED.lock() = true;
        }
    }
}

/// Verificar si el serial está listo para transmitir
fn is_transmit_empty() -> bool {
    unsafe { inb(SERIAL_PORT + 5) & 0x20 != 0 }
}

/// Verificar si hay datos disponibles para recibir
fn is_data_available() -> bool {
    unsafe { inb(SERIAL_PORT + 5) & 0x01 != 0 }
}

/// Leer un byte del puerto serial (blocking)
/// Retorna None si no hay datos disponibles
pub fn read_byte() -> Option<u8> {
    if !*SERIAL_INITIALIZED.lock() {
        return None;
    }
    
    if is_data_available() {
        Some(unsafe { inb(SERIAL_PORT) })
    } else {
        None
    }
}

/// Leer un byte del puerto serial (blocking - espera hasta que haya datos)
pub fn read_byte_blocking() -> u8 {
    while !is_data_available() {
        core::hint::spin_loop();
    }
    unsafe { inb(SERIAL_PORT) }
}

/// Leer múltiples bytes del serial hasta llenar el buffer o timeout
/// Retorna el número de bytes leídos
pub fn read_bytes(buffer: &mut [u8], timeout_iterations: u32) -> usize {
    if !*SERIAL_INITIALIZED.lock() {
        return 0;
    }
    
    let mut count = 0;
    let mut timeout = timeout_iterations;
    
    for byte in buffer.iter_mut() {
        if timeout == 0 {
            break;
        }
        
        if let Some(b) = read_byte() {
            *byte = b;
            count += 1;
            timeout = timeout_iterations; // Reset timeout on successful read
        } else {
            timeout -= 1;
            core::hint::spin_loop();
        }
    }
    
    count
}

/// Escribir un byte al puerto serial (versión pública)
pub fn serial_print_byte(byte: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if !*SERIAL_INITIALIZED.lock() {
            return;
        }
        write_byte(byte);
    });
}

/// Escribir un caracter al puerto serial
pub fn serial_print_char(c: char) {
    serial_print_byte(c as u8);
}

/// Escribir un byte al puerto serial (interno)
fn write_byte(byte: u8) {
    // Esperar a que el buffer de transmisión esté vacío
    while !is_transmit_empty() {
        core::hint::spin_loop();
    }
    
    unsafe {
        outb(SERIAL_PORT, byte);
    }
}

/// Escribir una cadena al puerto serial
pub fn serial_print(s: &str) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if !*SERIAL_INITIALIZED.lock() {
            return;
        }
        
        for byte in s.bytes() {
            write_byte(byte);
        }
    });
}

/// Writer struct for formatted output support
pub struct SerialWriter;

impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        serial_print(s);
        Ok(())
    }
}

/// Escribir un número en hexadecimal
pub fn serial_print_hex(num: u64) {
    if !*SERIAL_INITIALIZED.lock() {
        return;
    }
    
    serial_print("0x");
    let hex_chars = b"0123456789ABCDEF";
    
    for i in (0..16).rev() {
        let nibble = ((num >> (i * 4)) & 0xF) as usize;
        write_byte(hex_chars[nibble]);
    }
}

/// Escribir un número decimal
pub fn serial_print_dec(num: u64) {
    if !*SERIAL_INITIALIZED.lock() {
        return;
    }
    
    if num == 0 {
        write_byte(b'0');
        return;
    }
    
    let mut n = num;
    let mut digits = [0u8; 20];
    let mut i = 0;
    
    while n > 0 {
        digits[i] = (b'0' + (n % 10) as u8);
        n /= 10;
        i += 1;
    }
    
    while i > 0 {
        i -= 1;
        write_byte(digits[i]);
    }
}

/// Escribir a un puerto de I/O
#[inline]
unsafe fn outb(port: u16, value: u8) {
    asm!(
        "out dx, al",
        in("dx") port,
        in("al") value,
        options(nomem, nostack, preserves_flags)
    );
}

/// Leer de un puerto de I/O
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    asm!(
        "in al, dx",
        in("dx") port,
        out("al") value,
        options(nomem, nostack, preserves_flags)
    );
    value
}
