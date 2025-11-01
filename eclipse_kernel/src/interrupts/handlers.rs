//! Handlers de interrupciones avanzados para Eclipse OS
//!
//! Este módulo proporciona handlers especializados para diferentes tipos
//! de interrupciones y excepciones del sistema.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};
use crate::debug::serial_write_str;

/// Contadores de interrupciones
static TIMER_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static KEYBOARD_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static SERIAL_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static PAGE_FAULTS: AtomicU64 = AtomicU64::new(0);
static GENERAL_PROTECTION_FAULTS: AtomicU64 = AtomicU64::new(0);
static DIVISION_BY_ZERO: AtomicU64 = AtomicU64::new(0);

/// Handler de timer mejorado con context switching
#[no_mangle]
pub extern "C" fn timer_interrupt_handler() {
    TIMER_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    // Llamar al sistema de timer para manejar ticks y scheduling
    super::timer::on_timer_interrupt();
    
    // Enviar EOI al PIC
    unsafe {
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
    }
}

/// Handler de teclado mejorado
#[no_mangle]
pub extern "C" fn keyboard_interrupt_handler() {
    KEYBOARD_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    unsafe {
        let mut scancode: u8;
        asm!("in {}, 0x60", out(reg_byte) scancode, options(nostack, nomem));
        
        // Enviar EOI al PIC
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
        
        // Procesar scancode
        process_keyboard_scancode(scancode);
    }
}

/// Handler de puerto serial mejorado
#[no_mangle]
pub extern "C" fn serial_interrupt_handler() {
    SERIAL_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    unsafe {
        // Leer datos del puerto serial
        let mut data: u8;
        asm!("mov dx, 0x3F8; in al, dx", out("al") data, options(nostack, nomem));
        
        // Enviar EOI al PIC
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
        
        // Procesar datos seriales
        process_serial_data(data);
    }
}

/// Handler de page fault mejorado
#[no_mangle]
pub extern "C" fn page_fault_interrupt_handler() {
    PAGE_FAULTS.fetch_add(1, Ordering::Relaxed);
    
    unsafe {
        let mut fault_address: u64;
        asm!("mov {}, cr2", out(reg) fault_address, options(nostack, nomem));
        
        // Obtener información del error
        let mut error_code: u64;
        asm!("pop {}", out(reg) error_code, options(nostack, nomem));
        
        // Procesar page fault
        process_page_fault(fault_address, error_code);
    }
}

/// Handler de general protection fault mejorado
#[no_mangle]
pub extern "C" fn general_protection_fault_handler() {
    GENERAL_PROTECTION_FAULTS.fetch_add(1, Ordering::Relaxed);
    
    unsafe {
        let mut error_code: u64;
        asm!("pop {}", out(reg) error_code, options(nostack, nomem));
        
        // Procesar general protection fault
        process_general_protection_fault(error_code);
    }
}

/// Handler de división por cero mejorado
#[no_mangle]
pub extern "C" fn division_by_zero_handler() {
    DIVISION_BY_ZERO.fetch_add(1, Ordering::Relaxed);
    
    // Procesar división por cero
    process_division_by_zero();
}

/// Handler de double fault mejorado
#[no_mangle]
pub extern "C" fn double_fault_handler() {
    // Double fault es crítico - el sistema puede estar inestable
    serial_write_str("CRITICAL: Double fault detected - system may be unstable\n");
    
    // En una implementación completa, esto podría intentar recuperación
    // o reiniciar el sistema
    loop {
        // Halt the system
        unsafe {
            asm!("hlt", options(nostack, nomem));
        }
    }
}

/// Handler de machine check mejorado
#[no_mangle]
pub extern "C" fn machine_check_handler() {
    // Machine check es crítico - error de hardware
    serial_write_str("CRITICAL: Machine check error - hardware failure detected\n");
    
    // En una implementación completa, esto registraría el error
    // y podría intentar recuperación
    loop {
        // Halt the system
        unsafe {
            asm!("hlt", options(nostack, nomem));
        }
    }
}

/// Handler de NMI (Non-Maskable Interrupt) mejorado
#[no_mangle]
pub extern "C" fn nmi_handler() {
    // NMI - interrupción crítica del sistema
    serial_write_str("CRITICAL: NMI received - system critical error\n");
    
    // En una implementación completa, esto manejaría el NMI
    // y registraría información de depuración
}

/// Handler de syscall mejorado
#[no_mangle]
pub extern "C" fn syscall_handler() {
    // System call handler - procesa syscalls del userland
    unsafe {
        // Obtener registros de syscall
        let mut rax: u64; // Número de syscall
        let mut rdi: u64; // Argumento 1
        let mut rsi: u64; // Argumento 2
        let mut rdx: u64; // Argumento 3
        let mut r10: u64; // Argumento 4 (en lugar de rcx)
        let mut r8: u64;  // Argumento 5
        let mut r9: u64;  // Argumento 6
        
        asm!(
            "mov {}, rax",
            "mov {}, rdi",
            "mov {}, rsi",
            "mov {}, rdx",
            "mov {}, r10",
            "mov {}, r8",
            "mov {}, r9",
            out(reg) rax,
            out(reg) rdi,
            out(reg) rsi,
            out(reg) rdx,
            out(reg) r10,
            out(reg) r8,
            out(reg) r9,
            options(nostack, nomem)
        );
        
        // Procesar syscall
        let result = process_syscall(rax, rdi, rsi, rdx, r10, r8, r9);
        
        // Poner resultado en RAX
        asm!("mov rax, {}", in(reg) result, options(nostack, nomem));
    }
}

// ============================================================================
// FUNCIONES DE PROCESAMIENTO
// ============================================================================

/// Procesar scancode del teclado
fn process_keyboard_scancode(scancode: u8) {
    use crate::drivers::keyboard::KeyCode;
    use crate::drivers::stdin::process_key_event;
    
    // Verificar si es key press o release
    let pressed = (scancode & 0x80) == 0;
    let scancode_clean = scancode & 0x7F;
    
    // Mapeo simple de scancodes PS/2 a KeyCode
    let keycode = match scancode_clean {
        0x01 => Some(KeyCode::Escape),
        0x02 => Some(KeyCode::Key1),
        0x03 => Some(KeyCode::Key2),
        0x04 => Some(KeyCode::Key3),
        0x05 => Some(KeyCode::Key4),
        0x06 => Some(KeyCode::Key5),
        0x07 => Some(KeyCode::Key6),
        0x08 => Some(KeyCode::Key7),
        0x09 => Some(KeyCode::Key8),
        0x0A => Some(KeyCode::Key9),
        0x0B => Some(KeyCode::Key0),
        0x0E => Some(KeyCode::Backspace),
        0x0F => Some(KeyCode::Tab),
        0x10 => Some(KeyCode::Q),
        0x11 => Some(KeyCode::W),
        0x12 => Some(KeyCode::E),
        0x13 => Some(KeyCode::R),
        0x14 => Some(KeyCode::T),
        0x15 => Some(KeyCode::Y),
        0x16 => Some(KeyCode::U),
        0x17 => Some(KeyCode::I),
        0x18 => Some(KeyCode::O),
        0x19 => Some(KeyCode::P),
        0x1C => Some(KeyCode::Enter),
        0x1D => Some(KeyCode::Ctrl),
        0x1E => Some(KeyCode::A),
        0x1F => Some(KeyCode::S),
        0x20 => Some(KeyCode::D),
        0x21 => Some(KeyCode::F),
        0x22 => Some(KeyCode::G),
        0x23 => Some(KeyCode::H),
        0x24 => Some(KeyCode::J),
        0x25 => Some(KeyCode::K),
        0x26 => Some(KeyCode::L),
        0x2A => Some(KeyCode::LeftShift),
        0x2C => Some(KeyCode::Z),
        0x2D => Some(KeyCode::X),
        0x2E => Some(KeyCode::C),
        0x2F => Some(KeyCode::V),
        0x30 => Some(KeyCode::B),
        0x31 => Some(KeyCode::N),
        0x32 => Some(KeyCode::M),
        0x39 => Some(KeyCode::Space),
        _ => None,
    };
    
    // Procesar el evento si tenemos un keycode válido
    if let Some(key) = keycode {
        // TODO: Rastrear estado de Shift para mayúsculas
        let shift = false; // Por ahora, siempre minúsculas
        process_key_event(key, pressed, shift);
    }
}

/// Procesar datos del puerto serial
fn process_serial_data(data: u8) {
    // En una implementación completa, esto procesaría los datos seriales
    // y los enviaría al driver correspondiente
}

/// Procesar page fault
fn process_page_fault(fault_address: u64, error_code: u64) {
    let present = (error_code & 1) != 0;
    let write = (error_code & 2) != 0;
    let user = (error_code & 4) != 0;
    let reserved = (error_code & 8) != 0;
    let instruction = (error_code & 16) != 0;
    
    if present {
        // Page fault en página presente - error de protección
        serial_write_str("ERROR: Page fault - protection violation\n");
    } else {
        // Page fault en página no presente - cargar página
        // En una implementación completa, esto cargaría la página desde disco
        serial_write_str("INFO: Page fault - loading page from disk\n");
    }
}

/// Procesar general protection fault
fn process_general_protection_fault(error_code: u64) {
    let external = (error_code & 1) != 0;
    let descriptor = (error_code & 2) != 0;
    let table = (error_code & 4) != 0;
    let selector = (error_code >> 3) & 0x1FFF;
    
    if external {
        serial_write_str("ERROR: General protection fault - external event\n");
    } else if descriptor {
        serial_write_str("ERROR: General protection fault - descriptor error\n");
    } else if table {
        serial_write_str("ERROR: General protection fault - table error\n");
    } else {
        serial_write_str("ERROR: General protection fault - selector error\n");
    }
}

/// Procesar división por cero
fn process_division_by_zero() {
    serial_write_str("ERROR: Division by zero exception\n");
    
    // En una implementación completa, esto mataría el proceso
    // o enviaría una señal al proceso
}

/// Procesar syscall
fn process_syscall(rax: u64, rdi: u64, rsi: u64, rdx: u64, r10: u64, r8: u64, r9: u64) -> u64 {
    use crate::syscalls::{get_syscall_registry, SyscallArgs};
    
    let syscall_num = rax;
    
    // Crear argumentos de syscall
    let args = SyscallArgs::from_registers(rdi, rsi, rdx, r10, r8, r9);
    
    // Obtener el registro de syscalls
    let registry_guard = get_syscall_registry().lock();
    
    if let Some(registry) = registry_guard.as_ref() {
        // Ejecutar la syscall
        let result = registry.execute(syscall_num as usize, &args);
        
        // Convertir resultado a u64
        match result {
            crate::syscalls::SyscallResult::Success(value) => value,
            crate::syscalls::SyscallResult::Error(error) => error.to_errno() as u64,
        }
    } else {
        serial_write_str("ERROR: Syscall registry not initialized\n");
        (-1i64) as u64 // Error: syscall registry no inicializado
    }
}

// ============================================================================
// FUNCIONES DE UTILIDAD
// ============================================================================

/// Obtener estadísticas de interrupciones
pub fn get_interrupt_stats() -> InterruptStats {
    InterruptStats {
        timer_interrupts: TIMER_INTERRUPTS.load(Ordering::Relaxed),
        keyboard_interrupts: KEYBOARD_INTERRUPTS.load(Ordering::Relaxed),
        serial_interrupts: SERIAL_INTERRUPTS.load(Ordering::Relaxed),
        page_faults: PAGE_FAULTS.load(Ordering::Relaxed),
        general_protection_faults: GENERAL_PROTECTION_FAULTS.load(Ordering::Relaxed),
        division_by_zero: DIVISION_BY_ZERO.load(Ordering::Relaxed),
    }
}

/// Estructura de estadísticas de interrupciones
#[derive(Debug, Clone, Copy)]
pub struct InterruptStats {
    pub timer_interrupts: u64,
    pub keyboard_interrupts: u64,
    pub serial_interrupts: u64,
    pub page_faults: u64,
    pub general_protection_faults: u64,
    pub division_by_zero: u64,
}

impl Default for InterruptStats {
    fn default() -> Self {
        Self {
            timer_interrupts: 0,
            keyboard_interrupts: 0,
            serial_interrupts: 0,
            page_faults: 0,
            general_protection_faults: 0,
            division_by_zero: 0,
        }
    }
}
