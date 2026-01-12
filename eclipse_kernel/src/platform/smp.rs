//! Soporte de Multi-Procesamiento Simétrico (SMP)
//! 
//! Este módulo maneja el inicio de los Application Processors (APs).

use core::sync::atomic::{AtomicU32, Ordering};
use crate::drivers::advanced::acpi::get_acpi_manager;
use crate::interrupts::apic::get_apic_base;

// Incluir el ensamblador del trampoline
// global_asm!(include_str!("trampoline.S"));

extern "C" {
    fn trampoline_start();
    fn trampoline_end();
}

/// Dirección física donde se copiará el trampoline (0x8000)
const TRAMPOLINE_ADDR: u64 = 0x8000;

/// Contadores de CPUs
static AP_COUNT: AtomicU32 = AtomicU32::new(0);

/// Inicializar SMP
pub fn init_smp() -> Result<(), &'static str> {
    crate::debug::serial_write_str("SMP: Inicializando Multicore support...\n");
    
    // Obtener información de ACPI
    let acpi = get_acpi_manager().ok_or("ACPI no inicializado")?;
    
    // Preparar el trampoline
    // 1. Copiar código a 0x8000
    let trampoline_len = trampoline_end as usize - trampoline_start as usize;
    crate::debug::serial_write_str("SMP: Copiando trampoline...\n");
    
    unsafe {
        // Mapear identidad de 0x8000 ya está hecho en paging init
        let src = trampoline_start as *const u8;
        let dst = TRAMPOLINE_ADDR as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, trampoline_len);
    }
    
    // 2. Parchear variables en el trampoline (PML4 y Entry Point)
    // Las variables están al final del archivo asm.
    // Necesitamos offsets exactos.
    // Hack: Buscamos 0x00000000 al final del bloque copiado? 
    // No, mejor exportar símbolos o calcular offsets si es fijo.
    // En trampoline.S pusimos pml4_ptr y ap_entry_ptr al final.
    // Asumiremos que están en (End - 16) y (End - 8).
    
    unsafe {
        let pml4_offset = trampoline_len - 16;
        let entry_offset = trampoline_len - 8;
        
        let pml4_ptr = (TRAMPOLINE_ADDR + pml4_offset as u64) as *mut u64;
        let entry_ptr = (TRAMPOLINE_ADDR + entry_offset as u64) as *mut u64;
        
        // Obtener CR3 actual
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3);
        
        // Escribir CR3 en trampoline
        *pml4_ptr = cr3;
        
        // Escribir dirección de ap_entry
        *entry_ptr = crate::main_ap::ap_entry as u64; 
    }
    
    // 3. Iterar CPUs y enviarlas INIT-SIPI-SIPI
    send_init_sipi();
    
    Ok(())
}

fn send_init_sipi() {
    let acpi = get_acpi_manager().unwrap(); // Already checked
    let bsp_lapic_id = 0; // TODO: Get real BSP ID from CPUID or MADT
    
    // Iterar sobre los CPUs encontrados y despertarlos (menos el BSP)
    for &apic_id in acpi.detected_apic_ids.iter() {
        // 0xFF indicates empty slot
        if apic_id != 0xFF && apic_id != bsp_lapic_id as u8 {
            crate::debug::serial_write_str(&alloc::format!("SMP: Waking up Core APIC ID {}\n", apic_id));
            
            // 1. Send INIT IPI
            unsafe {
                crate::interrupts::apic::send_ipi(apic_id, 0, crate::interrupts::apic::IpiDeliveryMode::Init);
            }
            
            // Wait 10ms
             manual_delay(10000);
            
            // 2. Send SIPI (Start-up IPI) with vector 0x08 (Address 0x8000)
            unsafe {
                crate::interrupts::apic::send_ipi(apic_id, 0x08, crate::interrupts::apic::IpiDeliveryMode::StartUp);
            }
            
             // Wait 200us
             manual_delay(200);

            // 3. Second SIPI (just in case)
            unsafe {
                crate::interrupts::apic::send_ipi(apic_id, 0x08, crate::interrupts::apic::IpiDeliveryMode::StartUp);
            }
            
            // Wait for it to confirm knowing the secret handshake (incrementing AP_ONLINE_COUNT)
            manual_delay(10000);
        }
    }
    
    let online = crate::main_ap::AP_ONLINE_COUNT.load(Ordering::SeqCst);
    crate::debug::serial_write_str(&alloc::format!("SMP: Total APs Online: {}\n", online));
}

fn manual_delay(count: u64) {
    for _ in 0..count {
        core::hint::spin_loop();
    }
}
