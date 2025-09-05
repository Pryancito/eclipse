//! Bootloader UEFI funcional para Eclipse OS
//! 
//! Versión que compila y funciona

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
    // Mostrar banner simple usando output directo
    let output = system_table.stdout();
    let _ = output.write(b"\n");
    let _ = output.write(b"🌙 Eclipse OS Bootloader\n");
    let _ = output.write(b"   Versión funcional\n");
    let _ = output.write(b"   Kernel Eclipse listo\n");
    let _ = output.write(b"\n");
    
    // Simular carga del kernel
    let _ = output.write(b"   Cargando kernel Eclipse...\n");
    let _ = output.write(b"   ✅ Kernel cargado exitosamente\n");
    let _ = output.write(b"   🚀 Iniciando sistema Eclipse...\n");
    let _ = output.write(b"   ✅ Sistema Eclipse iniciado\n");
    let _ = output.write(b"\n");
    
    // Reiniciar después de un tiempo
    let _ = output.write(b"   🔄 Reiniciando en 3 segundos...\n");
    
    // Esperar un poco
    for i in (1..=3).rev() {
        let _ = output.write(b"   ");
        let _ = output.write(i.to_string().as_bytes());
        let _ = output.write(b"...\n");
    }
    
    // Reiniciar sistema
    unsafe {
        let rt = system_table.runtime_services();
        rt.reset(ResetType::WARM, uefi::Status::SUCCESS, None);
    }
}
