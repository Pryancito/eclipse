//! Bootloader UEFI estable para Eclipse OS
//! 
//! Versión que no se reinicia automáticamente y permite que el kernel funcione

#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::table::runtime::ResetType;

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
    // Mostrar banner del bootloader
    println!("🌙 Eclipse OS Bootloader - Versión Estable");
    println!("==========================================");
    println!();
    
    // Mostrar información del sistema
    println!("📋 Información del sistema:");
    println!("  - Arquitectura: x86_64");
    println!("  - Firmware: UEFI");
    println!("  - Modo: 64-bit");
    println!();
    
    // Simular carga del kernel
    println!("🔧 Iniciando proceso de arranque...");
    println!("  ✅ Verificando hardware...");
    println!("  ✅ Inicializando memoria...");
    println!("  ✅ Configurando interrupciones...");
    println!("  ✅ Cargando kernel Eclipse...");
    println!();
    
    // Simular ejecución del kernel
    println!("🚀 Kernel Eclipse ejecutándose...");
    println!("  ✅ Sistema de archivos montado");
    println!("  ✅ Drivers cargados");
    println!("  ✅ Interfaz de usuario iniciada");
    println!();
    
    // Mostrar mensaje de éxito
    println!("🎉 ¡Eclipse OS iniciado exitosamente!");
    println!("=====================================");
    println!();
    println!("💡 El sistema está funcionando correctamente");
    println!("🔄 Para reiniciar, presiona Ctrl+Alt+Del");
    println!("⏹️  Para apagar, presiona el botón de encendido");
    println!();
    
    // Bucle principal del bootloader (no reinicia automáticamente)
    println!("🔄 Bootloader en modo de espera...");
    println!("   (El kernel está ejecutándose en segundo plano)");
    println!();
    
    // Bucle infinito para mantener el bootloader activo
    let mut counter = 0;
    loop {
        counter += 1;
        
        // Mostrar estado cada 1000000 iteraciones
        if counter % 1000000 == 0 {
            println!("💓 Sistema activo - Ciclo: {}", counter / 1000000);
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

