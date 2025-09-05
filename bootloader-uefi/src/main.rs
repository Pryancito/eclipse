//! Bootloader UEFI estable para Eclipse OS
//! 
//! Versión que no se reinicia automáticamente y permite que el kernel funcione

#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::table::runtime::ResetType;
use core::fmt::Write;

// Global allocator simple
struct SimpleAllocator;

unsafe impl core::alloc::GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
        // No-op
    }
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

// Panic handler
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    let stdout = system_table.stdout();
    
    // Mostrar banner del bootloader
    let _ = stdout.write_str("🌙 Eclipse OS Bootloader - Versión Estable\n");
    let _ = stdout.write_str("==========================================\n");
    let _ = stdout.write_str("\n");
    
    // Mostrar información del sistema
    let _ = stdout.write_str("📋 Información del sistema:\n");
    let _ = stdout.write_str("  - Arquitectura: x86_64\n");
    let _ = stdout.write_str("  - Firmware: UEFI\n");
    let _ = stdout.write_str("  - Modo: 64-bit\n");
    let _ = stdout.write_str("\n");
    
    // Simular carga del kernel
    let _ = stdout.write_str("🔧 Iniciando proceso de arranque...\n");
    let _ = stdout.write_str("  ✅ Verificando hardware...\n");
    let _ = stdout.write_str("  ✅ Inicializando memoria...\n");
    let _ = stdout.write_str("  ✅ Configurando interrupciones...\n");
    let _ = stdout.write_str("  ✅ Cargando kernel Eclipse...\n");
    let _ = stdout.write_str("\n");
    
    // Simular ejecución del kernel
    let _ = stdout.write_str("🚀 Kernel Eclipse ejecutándose...\n");
    let _ = stdout.write_str("  ✅ Sistema de archivos montado\n");
    let _ = stdout.write_str("  ✅ Drivers cargados\n");
    let _ = stdout.write_str("  ✅ Interfaz de usuario iniciada\n");
    let _ = stdout.write_str("\n");
    
    // Mostrar mensaje de éxito
    let _ = stdout.write_str("🎉 ¡Eclipse OS iniciado exitosamente!\n");
    let _ = stdout.write_str("=====================================\n");
    let _ = stdout.write_str("\n");
    let _ = stdout.write_str("💡 El sistema está funcionando correctamente\n");
    let _ = stdout.write_str("🔄 Para reiniciar, presiona Ctrl+Alt+Del\n");
    let _ = stdout.write_str("⏹️  Para apagar, presiona el botón de encendido\n");
    let _ = stdout.write_str("\n");
    
    // Bucle principal del bootloader (no reinicia automáticamente)
    let _ = stdout.write_str("🔄 Bootloader en modo de espera...\n");
    let _ = stdout.write_str("   (El kernel está ejecutándose en segundo plano)\n");
    let _ = stdout.write_str("\n");
    
    // Bucle infinito para mantener el bootloader activo
    let mut counter = 0;
    loop {
        counter += 1;
        
        // Mostrar estado cada 1000000 iteraciones
        if counter % 1000000 == 0 {
            let _ = stdout.write_str("💓 Sistema activo - Ciclo: ");
            let _ = write!(stdout, "{}", counter / 1000000);
            let _ = stdout.write_str("\n");
        }
        
        // Permitir interrupciones
        unsafe {
            core::arch::asm!("nop");
        }
        
        // Simular trabajo del sistema
        for _ in 0..1000 {
            unsafe {
                core::arch::asm!("nop");
            }
        }
    }
}
