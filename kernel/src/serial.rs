//! Módulo de comunicación serial (COM1) para debugging

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

/// Escribir un byte al puerto serial
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
    if !*SERIAL_INITIALIZED.lock() {
        return;
    }
    
    for byte in s.bytes() {
        write_byte(byte);
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
