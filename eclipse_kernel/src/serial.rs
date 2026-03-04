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

use core::sync::atomic::{AtomicBool, Ordering};
use core::arch::asm;
use core::fmt::Write;

const SERIAL_PORT: u16 = 0x3F8; // COM1

/// Estado del puerto serial (Atomic para SMP safety)
static SERIAL_INITIALIZED: AtomicBool = AtomicBool::new(false);
/// Lock para acceso al hardware del puerto serial (multicore stability)
static SERIAL_PORT_LOCK: crate::sync::ReentrantMutex<()> = crate::sync::ReentrantMutex::new(());

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
        
        SERIAL_INITIALIZED.store(true, Ordering::Release);
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
    if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
        return None;
    }
    
    // LOCK OBLIGATORIO en lectura para SMP
    let _lock = SERIAL_PORT_LOCK.lock();
    if is_data_available() {
        Some(unsafe { inb(SERIAL_PORT) })
    } else {
        None
    }
}

/// Leer un byte del puerto serial (blocking - espera hasta que haya datos)
pub fn read_byte_blocking() -> u8 {
    while !is_data_available() {
        crate::cpu::pause();
    }
    unsafe { inb(SERIAL_PORT) }
}

/// Leer múltiples bytes del serial hasta llenar el buffer o timeout
/// Retorna el número de bytes leídos
pub fn read_bytes(buffer: &mut [u8], timeout_iterations: u32) -> usize {
    if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
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
            crate::cpu::pause();
        }
    }
    
    count
}

/// Escribir un byte al puerto serial (versión pública)
pub fn serial_print_byte(byte: u8) {
    if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
        return;
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
    let mut timeout = 10_000;
    while !is_transmit_empty() && timeout > 0 {
        crate::cpu::pause();
        timeout -= 1;
    }
    
    if timeout > 0 {
        unsafe {
            outb(SERIAL_PORT, byte);
        }
    }
}

/// Escribir una cadena al puerto serial (con prefijos por línea y lock único)
pub fn serial_print(s: &str) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        {
            let _lock = SERIAL_PORT_LOCK.lock();
            if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
                return;
            }

            let me = crate::sync::ReentrantMutex::<()>::current_cpu();
            let mut writer = PrefixedWriter::new(me);
            let _ = writer.write_str(s);
        }
        // Registrar en pantalla fuera del lock serial para evitar contención y recursión
        crate::progress::log(s);
    });
}

/// Imprimir con formato de forma sincronizada y atómica
pub fn serial_printf(args: core::fmt::Arguments) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        {
            let _lock = SERIAL_PORT_LOCK.lock();
            if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
                return;
            }

            let me = crate::sync::ReentrantMutex::<()>::current_cpu();
            let mut writer = PrefixedWriter::new(me);
            let _ = core::fmt::write(&mut writer, args);
        }
        
        // Registrar en pantalla fuera del lock serial
        // Nota: el Formatted message puede ser complejo, pero para el log de pantalla
        // buscamos solo la cadena resultante simplificada.
        if let Some(s) = args.as_str() {
            crate::progress::log(s);
        } else {
             // Si tiene argumentos variables, progress-log no los verá a menos que formateemos a buffer.
             // Pero los logs críticos suelen ser constantes o pasar por serial_print.
        }
    });
}

/// Registro de si cada CPU está al inicio de una línea (AtomicBool para SMP safety)
static CPU_AT_LINE_START: [AtomicBool; 128] = [const { AtomicBool::new(true) }; 128];

/// Writer que añade prefijo [Cn] al inicio de cada línea y delega al hardware.
/// Se usa dentro de un lock ya adquirido.
struct PrefixedWriter {
    cpu_id: i32,
}

impl PrefixedWriter {
    fn new(cpu_id: i32) -> Self {
        Self { cpu_id }
    }

    fn write_prefix(&mut self) {
        write_byte_internal(b'[');
        write_byte_internal(b'C');
        // Simple 0-9 for now (common in QEMU -smp 4/8)
        let id_byte = if self.cpu_id >= 0 && self.cpu_id <= 9 {
            (self.cpu_id as u8) + b'0'
        } else {
            b'?'
        };
        write_byte_internal(id_byte);
        write_byte_internal(b']');
        write_byte_internal(b' ');
    }
}

impl core::fmt::Write for PrefixedWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let cpu_idx = (self.cpu_id as usize).min(127);
        
        for byte in s.bytes() {
            if CPU_AT_LINE_START[cpu_idx].load(Ordering::Relaxed) && byte != b'\n' {
                self.write_prefix();
                CPU_AT_LINE_START[cpu_idx].store(false, Ordering::Relaxed);
            }
            
            write_byte_internal(byte);
            
            if byte == b'\n' {
                CPU_AT_LINE_START[cpu_idx].store(true, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

fn write_byte_internal(byte: u8) {
    let mut timeout = 10_000;
    while !is_transmit_empty() && timeout > 0 {
        crate::cpu::pause();
        timeout -= 1;
    }
    
    if timeout > 0 {
        unsafe {
            outb(SERIAL_PORT, byte);
        }
    }
}

/// Forcedly unlock the serial port mutex.
/// Danger: should ONLY be used in fork_child_setup to clear inherited locks.
pub unsafe fn force_unlock_serial() {
    SERIAL_PORT_LOCK.force_unlock();
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
