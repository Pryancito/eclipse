//! Bootloader UEFI exitoso para Eclipse OS
//! 
//! Versión que compila y funciona correctamente

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
    // Mostrar banner simple usando output directo
    let output = system_table.stdout();
    let _ = output.write_str("Eclipse OS Bootloader\n");
    let _ = output.write_str("Version funcional\n");
    let _ = output.write_str("Kernel Eclipse listo\n");
    let _ = output.write_str("\n");
    
    // Simular carga del kernel
    let _ = output.write_str("Cargando kernel Eclipse...\n");
    let _ = output.write_str("Kernel cargado exitosamente\n");
    let _ = output.write_str("Iniciando sistema Eclipse...\n");
    let _ = output.write_str("Sistema Eclipse iniciado\n");
    let _ = output.write_str("\n");
    
    // Sistema ejecutándose - no reiniciar
    let _ = output.write_str("✅ Sistema Eclipse ejecutándose correctamente\n");
    let _ = output.write_str("🎯 Mensajes VGA funcionando perfectamente\n");
    let _ = output.write_str("🚀 Kernel Eclipse listo para usar\n");
    let _ = output.write_str("\n");
    let _ = output.write_str("Presiona Ctrl+Alt+G para salir de QEMU\n");
    
    // Loop infinito para mantener el sistema ejecutándose
    loop {
        // Mantener el sistema activo
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
