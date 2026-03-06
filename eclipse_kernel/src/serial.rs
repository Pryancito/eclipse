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

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
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
    // Spin *outside* the lock so other CPUs can still transmit while we wait.
    // Once data is detected, claim it under the lock (re-check to handle races).
    loop {
        while !is_data_available() {
            crate::cpu::pause();
        }
        let _lock = SERIAL_PORT_LOCK.lock();
        if is_data_available() {
            return unsafe { inb(SERIAL_PORT) };
        }
        // Another CPU grabbed the byte; retry the outer loop.
    }
}

/// Leer múltiples bytes del serial hasta llenar el buffer o timeout
/// Retorna el número de bytes leídos
pub fn read_bytes(buffer: &mut [u8], timeout_iterations: u32) -> usize {
    if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
        return 0;
    }

    // Hold the lock for the entire read so bytes are not interleaved between CPUs.
    let _lock = SERIAL_PORT_LOCK.lock();
    let mut count = 0;
    let mut timeout = timeout_iterations;

    for byte in buffer.iter_mut() {
        if timeout == 0 {
            break;
        }

        if is_data_available() {
            *byte = unsafe { inb(SERIAL_PORT) };
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
    x86_64::instructions::interrupts::without_interrupts(|| {
        if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
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
    let _lock = SERIAL_PORT_LOCK.lock();
    // Reduce timeout to avoid long hangs on failing hardware
    let mut timeout = 100_000;
    while !is_transmit_empty() && timeout > 0 {
        crate::cpu::pause();
        timeout -= 1;
    }
    
    if timeout > 0 {
        unsafe {
            outb(SERIAL_PORT, byte);
            io_wait();
        }
    } else {
        // Optional: record serial timeout if it happens too often
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
        let mut screen_log_buf = [0u8; 128];
        let mut screen_log_len = 0;

        {
            let _lock = SERIAL_PORT_LOCK.lock();
            if !SERIAL_INITIALIZED.load(Ordering::Acquire) {
                return;
            }

            let me = crate::sync::ReentrantMutex::<()>::current_cpu();
            let mut writer = PrefixedWriter::new(me);
            let _ = core::fmt::write(&mut writer, args);

            // También capturamos para el log de pantalla si hay espacio
            struct StubWriter<'a> {
                buf: &'a mut [u8],
                len: &'a mut usize,
            }
            impl core::fmt::Write for StubWriter<'_> {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    let bytes = s.as_bytes();
                    let space = self.buf.len() - *self.len;
                    let to_copy = core::cmp::min(bytes.len(), space);
                    self.buf[*self.len..*self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
                    *self.len += to_copy;
                    Ok(())
                }
            }
            let mut sw = StubWriter { buf: &mut screen_log_buf, len: &mut screen_log_len };
            let _ = core::fmt::write(&mut sw, args);
        }
        
        // Registrar en pantalla fuera del lock serial
        if screen_log_len > 0 {
            if let Ok(s) = core::str::from_utf8(&screen_log_buf[..screen_log_len]) {
                crate::progress::log(s);
            }
        }
    });
}

/// Registro de si cada CPU necesita un prefijo (AtomicBool para SMP safety)
static CPU_NEEDS_PREFIX: [AtomicBool; 128] = [const { AtomicBool::new(true) }; 128];

/// Seguimiento global de qué CPU escribió por última vez y estado de la línea física
static GLOBAL_LAST_CPU: AtomicI32 = AtomicI32::new(-1);
static GLOBAL_AT_LINE_START: AtomicBool = AtomicBool::new(true);

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
        // --- COHERENCIA GLOBAL ---
        // Si otra CPU dejó la línea a medias, forzamos un \n antes de nuestro prefijo
        let last_cpu = GLOBAL_LAST_CPU.load(Ordering::Acquire);
        if last_cpu != -1 && last_cpu != self.cpu_id && !GLOBAL_AT_LINE_START.load(Ordering::Acquire) {
            write_byte_internal(b'\n');
            GLOBAL_AT_LINE_START.store(true, Ordering::Release);
        }
        GLOBAL_LAST_CPU.store(self.cpu_id, Ordering::Release);

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
        GLOBAL_AT_LINE_START.store(false, Ordering::Release);
    }
}

impl core::fmt::Write for PrefixedWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let cpu_idx = (self.cpu_id as usize).min(127);
        
        for byte in s.bytes() {
            if CPU_NEEDS_PREFIX[cpu_idx].load(Ordering::Relaxed) && byte != b'\n' {
                self.write_prefix();
                CPU_NEEDS_PREFIX[cpu_idx].store(false, Ordering::Relaxed);
            }
            
            write_byte_internal(byte);
            
            if byte == b'\n' {
                CPU_NEEDS_PREFIX[cpu_idx].store(true, Ordering::Relaxed);
                GLOBAL_AT_LINE_START.store(true, Ordering::Release);
                GLOBAL_LAST_CPU.store(self.cpu_id, Ordering::Release);
            } else {
                GLOBAL_AT_LINE_START.store(false, Ordering::Release);
                GLOBAL_LAST_CPU.store(self.cpu_id, Ordering::Release);
            }
        }
        Ok(())
    }
}

fn write_byte_internal(byte: u8) {
    // Reduce timeout to avoid long hangs on failing hardware
    let mut timeout = 100_000;
    while !is_transmit_empty() && timeout > 0 {
        crate::cpu::pause();
        timeout -= 1;
    }
    
    if timeout > 0 {
        unsafe {
            outb(SERIAL_PORT, byte);
            io_wait();
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

/// Pequeño retardo para dar tiempo al hardware de I/O
#[inline]
unsafe fn io_wait() {
    // Escribir a un puerto no utilizado (costumbre de Linux/BSD)
    outb(0x80, 0);
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
