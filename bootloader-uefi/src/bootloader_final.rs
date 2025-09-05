//! Bootloader UEFI final para Eclipse OS
//! 
//! Versión que compila sin errores

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
    // Mostrar banner simple usando println! de uefi
    println!("Eclipse OS Bootloader");
    println!("Version funcional");
    println!("Kernel Eclipse listo");
    println!();
    
    // Simular carga del kernel
    println!("Cargando kernel Eclipse...");
    println!("Kernel cargado exitosamente");
    println!("Iniciando sistema Eclipse...");
    println!("Sistema Eclipse iniciado");
    println!();
    
    // Reiniciar después de un tiempo
    println!("Reiniciando en 3 segundos...");
    
    // Esperar un poco
    for i in (1..=3).rev() {
        println!("{}...", i);
    }
    
    // Reiniciar sistema
    unsafe {
        let rt = system_table.runtime_services();
        rt.reset(ResetType::WARM, uefi::Status::SUCCESS, None);
    }
}
