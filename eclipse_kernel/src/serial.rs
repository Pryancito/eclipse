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
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use spin::Mutex;

const LOG_BUFFER_SIZE: usize = 64 * 1024; // 64KB
static mut LOG_BUFFER: [u8; LOG_BUFFER_SIZE] = [0u8; LOG_BUFFER_SIZE];
static LOG_BUFFER_HEAD: AtomicUsize = AtomicUsize::new(0);
static LOG_BUFFER_WRAPPED: AtomicBool = AtomicBool::new(false);

const SERIAL_PORT: u16 = 0x3F8; // COM1

/// Estado del puerto serial
static mut SERIAL_INITIALIZED: bool = false;
/// Lock para acceso al hardware del puerto serial (multicore stability)
static SERIAL_PORT_LOCK: Mutex<()> = Mutex::new(());

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
        outb(SERIAL_PORT + 4, 0x0F);
        
        SERIAL_INITIALIZED = true;
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
    unsafe {
        if !SERIAL_INITIALIZED {
            return None;
        }
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
    unsafe {
        if !SERIAL_INITIALIZED {
            return 0;
        }
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
    unsafe {
        if !SERIAL_INITIALIZED {
            return;
        }
    }
    write_byte(byte);
}

/// Escribir un caracter al puerto serial
pub fn serial_print_char(c: char) {
    serial_print_byte(c as u8);
}

/// Escribir un byte al puerto serial (interno)
fn write_byte(byte: u8) {
    // Esperar a que el buffer de transmisión esté vacío con un timeout de seguridad
    // En hardware real sin puerto serie, esto evitará que el kernel se cuelgue
    let mut timeout = 1_000_000;
    while !is_transmit_empty() && timeout > 0 {
        core::hint::spin_loop();
        timeout -= 1;
    }
    
    if timeout > 0 {
        unsafe {
            outb(SERIAL_PORT, byte);
        }
    }
}

/// Escribir una cadena al puerto serial (y al framebuffer si está inicializado)
pub fn serial_print(s: &str) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = SERIAL_PORT_LOCK.lock();
        unsafe {
            if SERIAL_INITIALIZED {
                for byte in s.bytes() {
                    write_byte(byte);
                }
            }
        }
        // Also log to screen once GS base is ready (early boot may not have it yet).
        if crate::boot::gs_base_ready() {
            crate::progress::log(s);
        }

        // Add to circular log buffer for sys_get_logs
        for byte in s.bytes() {
            let pos = LOG_BUFFER_HEAD.fetch_add(1, Ordering::Relaxed);
            let actual_pos = pos % LOG_BUFFER_SIZE;
            if pos >= LOG_BUFFER_SIZE {
                LOG_BUFFER_WRAPPED.store(true, Ordering::Relaxed);
            }
            unsafe {
                LOG_BUFFER[actual_pos] = byte;
            }
        }
    });
}

/// Writer para fmt::Write que usa el puerto serial con lock sincronizado
pub struct SerialWriter;

impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        serial_print(s);
        Ok(())
    }
}

/// Imprimir con formato de forma sincronizada y atómica
pub fn serial_printf(args: core::fmt::Arguments) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = SERIAL_PORT_LOCK.lock();
        let mut writer = RawSerialWriter;
        let _ = core::fmt::write(&mut writer, args);
    });
}

struct RawSerialWriter;
impl core::fmt::Write for RawSerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        unsafe {
            if SERIAL_INITIALIZED {
                for byte in s.bytes() {
                    write_byte_internal(byte);
                }
            }
        }
        // Also log to screen once GS base is ready (early boot may not have it yet).
        if crate::boot::gs_base_ready() {
            crate::progress::log(s);
        }

        // Add to circular log buffer
        for byte in s.bytes() {
            let pos = LOG_BUFFER_HEAD.fetch_add(1, Ordering::Relaxed);
            let actual_pos = pos % LOG_BUFFER_SIZE;
            if pos >= LOG_BUFFER_SIZE {
                LOG_BUFFER_WRAPPED.store(true, Ordering::Relaxed);
            }
            unsafe {
                LOG_BUFFER[actual_pos] = byte;
            }
        }
        Ok(())
    }
}

fn write_byte_internal(byte: u8) {
    let mut timeout = 1_000_000;
    while !is_transmit_empty() && timeout > 0 {
        core::hint::spin_loop();
        timeout -= 1;
    }
    
    if timeout > 0 {
        unsafe {
            outb(SERIAL_PORT, byte);
        }
    }
}

/// Escribir un número en hexadecimal (a serial y framebuffer)
pub fn serial_print_hex(num: u64) {
    let hex_chars = b"0123456789ABCDEF";
    let mut buf = [0u8; 20];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nibble = ((num >> ((15 - i) * 4)) & 0xF) as usize;
        buf[2 + i] = hex_chars[nibble];
    }
    serial_print(core::str::from_utf8(&buf).unwrap());
}

/// Escribir un número decimal (a serial y framebuffer)
pub fn serial_print_dec(num: u64) {
    if num == 0 {
        serial_print("0");
        return;
    }
    let mut n = num;
    let mut digits = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut buf = [0u8; 20];
    for j in 0..i {
        buf[j] = digits[i - 1 - j];
    }
    serial_print(core::str::from_utf8(&buf[..i]).unwrap());
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

/// Copiar los logs del kernel al espacio de usuario
pub fn copy_logs_to_user(buf_ptr: u64, len: u64) -> usize {
    let head_val = LOG_BUFFER_HEAD.load(Ordering::Relaxed);
    let wrapped = LOG_BUFFER_WRAPPED.load(Ordering::Relaxed);
    
    let head = head_val % LOG_BUFFER_SIZE;
    let total_available = if wrapped { LOG_BUFFER_SIZE } else { head };
    let to_copy = core::cmp::min(len as usize, total_available);
    
    if to_copy == 0 { return 0; }
    
    unsafe {
        let user_buf = core::slice::from_raw_parts_mut(buf_ptr as *mut u8, to_copy);
        if !wrapped {
            user_buf.copy_from_slice(&LOG_BUFFER[head - to_copy..head]);
        } else {
            // If wrapped, we might need two copies from the circular buffer
            if to_copy <= head {
                user_buf.copy_from_slice(&LOG_BUFFER[head - to_copy..head]);
            } else {
                let second_part_len = head;
                let first_part_len = to_copy - head;
                // First part: from the end of the buffer
                user_buf[..first_part_len].copy_from_slice(
                    &LOG_BUFFER[LOG_BUFFER_SIZE - first_part_len..]
                );
                // Second part: from the beginning up to head
                user_buf[first_part_len..].copy_from_slice(&LOG_BUFFER[..second_part_len]);
            }
        }
    }
    to_copy
}
