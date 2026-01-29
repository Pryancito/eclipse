//! Entry point for Application Processors (secondary cores)
//!
//! Este módulo contiene el punto de entrada para los Application Processors (APs),
//! es decir, todos los núcleos de CPU excepto el Bootstrap Processor (BSP).
//!
//! # Proceso de Inicialización de APs
//!
//! 1. El AP se despierta mediante la secuencia INIT-SIPI-SIPI enviada por el BSP
//! 2. Incrementa el contador global AP_ONLINE_COUNT para señalizar que está vivo
//! 3. Lee su propio LAPIC ID usando CPUID para identificarse
//! 4. Inicializa su Local APIC para poder recibir interrupciones
//! 5. Carga la IDT (Interrupt Descriptor Table)
//! 6. Habilita interrupciones con la instrucción STI
//! 7. Entra en un loop HLT esperando tareas del scheduler
//!
//! # Limitaciones Actuales
//!
//! - El AP usa un stack temporal en 0x8000 (heredado del trampoline)
//! - No hay integración completa con el scheduler
//! - Los APs permanecen en modo idle después de inicializarse
//!
//! # Mejoras Futuras
//!
//! - Asignar stack dedicado de kernel por CPU
//! - Implementar thread-local storage (TLS) por CPU
//! - Integración con scheduler para ejecutar tareas
//! - Soporte para migración de tareas entre núcleos

use core::sync::atomic::{AtomicU32, Ordering};
use crate::drivers::advanced::acpi::get_acpi_manager;

/// Counter of active APs
pub static AP_ONLINE_COUNT: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn ap_entry() -> ! {
    // 1. Incrementar contador primero para señalar al BSP que estamos vivos
    AP_ONLINE_COUNT.fetch_add(1, Ordering::SeqCst);
    
    // 2. Obtener nuestro LAPIC ID para identificarnos
    let my_apic_id = crate::interrupts::apic::get_current_lapic_id();
    
    // 3. Inicializar LAPIC local para este AP
    // El APIC debe ser inicializado en cada procesador
    if let Err(_e) = crate::interrupts::apic::initialize_apic() {
        // No podemos escribir fácilmente al puerto serial porque puede haber locks,
        // pero al menos intentamos inicializar el APIC
        loop {
            unsafe { core::arch::asm!("hlt"); }
        }
    }
    
    // 4. Cargar IDT (Interrupt Descriptor Table)
    // Cada procesador necesita su propia IDT configurada
    crate::interrupts::init_idt();
    
    // 5. Habilitar interrupciones en este núcleo
    unsafe {
        core::arch::asm!("sti");
    }
    
    // TODO: Integración con scheduler para que este núcleo pueda ejecutar tareas
    // TODO: Configurar stack dedicado por CPU (actualmente usando stack temporal)
    
    // Por ahora, el AP está completamente inicializado y entra en modo idle
    // En una implementación completa, aquí entraría al scheduler para ejecutar tareas
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}
