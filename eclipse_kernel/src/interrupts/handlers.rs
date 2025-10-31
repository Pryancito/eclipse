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

/// Handler de timer mejorado
#[no_mangle]
pub extern "C" fn timer_interrupt_handler() {
    TIMER_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    // Enviar EOI al PIC
    unsafe {
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
    }
    
    // En una implementación completa, esto actualizaría el scheduler
    // y manejaría el time slicing
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
    // System call handler
    // En una implementación completa, esto procesaría syscalls del userland
    unsafe {
        // Obtener número de syscall desde RAX
        let mut syscall_num: u64;
        asm!("mov {}, rax", out(reg) syscall_num, options(nostack, nomem));
        
        // Procesar syscall
        process_syscall(syscall_num);
    }
}

// ============================================================================
// FUNCIONES DE PROCESAMIENTO
// ============================================================================

/// Procesar scancode del teclado
fn process_keyboard_scancode(scancode: u8) {
    // En una implementación completa, esto procesaría el scancode
    // y lo convertiría a caracteres o comandos
    if scancode & 0x80 == 0 {
        // Tecla presionada
        // Procesar tecla presionada
    } else {
        // Tecla liberada
        // Procesar tecla liberada
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
fn process_syscall(syscall_num: u64) {
    // En una implementación completa, esto procesaría el syscall
    // y llamaría a la función correspondiente
    match syscall_num {
        0 => {
            // Syscall de ejemplo
            serial_write_str("INFO: Example syscall received\n");
        }
        _ => {
            serial_write_str("ERROR: Unknown syscall number\n");
        }
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
