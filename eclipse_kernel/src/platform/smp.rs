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
    
    // Obtener el LAPIC ID real del BSP (Bootstrap Processor) usando CPUID
    let bsp_lapic_id = crate::interrupts::apic::get_current_lapic_id();
    
    // Iterar sobre los CPUs encontrados y despertarlos (menos el BSP)
    for &apic_id in acpi.detected_apic_ids.iter() {
        // 0xFF indicates empty slot
        if apic_id != 0xFF && apic_id != bsp_lapic_id {
            crate::debug::serial_write_str(&alloc::format!("SMP: Waking up Core APIC ID {}\n", apic_id));
            
            // 1. Send INIT IPI
            unsafe {
                crate::interrupts::apic::send_ipi(apic_id, 0, crate::interrupts::apic::IpiDeliveryMode::Init);
            }
            
            // Wait 10ms (según especificación Intel)
            delay_microseconds(10000);
            
            // 2. Send SIPI (Start-up IPI) with vector 0x08 (Address 0x8000)
            unsafe {
                crate::interrupts::apic::send_ipi(apic_id, 0x08, crate::interrupts::apic::IpiDeliveryMode::StartUp);
            }
            
            // Wait 200us (según especificación Intel)
            delay_microseconds(200);

            // 3. Second SIPI (just in case)
            unsafe {
                crate::interrupts::apic::send_ipi(apic_id, 0x08, crate::interrupts::apic::IpiDeliveryMode::StartUp);
            }
            
            // Wait for it to come online
            delay_microseconds(10000);
        }
    }
    
    let online = crate::main_ap::AP_ONLINE_COUNT.load(Ordering::SeqCst);
    crate::debug::serial_write_str(&alloc::format!("SMP: Total APs Online: {}\n", online));
}

/// Delay en microsegundos usando TSC (Time Stamp Counter)
/// Nota: Esta es una aproximación basada en un estimado conservador de frecuencia de CPU
fn delay_microseconds(us: u64) {
    // Estimamos una CPU de ~2GHz como base conservadora
    // Ajustar este valor según la frecuencia real del CPU mejorará la precisión
    const ESTIMATED_CPU_MHZ: u64 = 2000;
    let cycles = us * ESTIMATED_CPU_MHZ;
    
    unsafe {
        let start = read_tsc();
        while read_tsc() - start < cycles {
            core::hint::spin_loop();
        }
    }
}

/// Leer Time Stamp Counter
unsafe fn read_tsc() -> u64 {
    let mut low: u32;
    let mut high: u32;
    core::arch::asm!(
        "rdtsc",
        out("eax") low,
        out("edx") high,
        options(nostack, nomem, preserves_flags)
    );
    ((high as u64) << 32) | (low as u64)
}

fn manual_delay(count: u64) {
    for _ in 0..count {
        core::hint::spin_loop();
    }
}
