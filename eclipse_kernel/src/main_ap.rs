//! Entry point for Application Processors (secondary cores)

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
