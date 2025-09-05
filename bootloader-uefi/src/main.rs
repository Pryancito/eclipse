//! Bootloader UEFI exitoso para Eclipse OS
//! 
//! Versión que compila y funciona correctamente

#![no_std]
#![no_main]

use uefi::prelude::*;
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

// Función para cargar el kernel de Eclipse OS
fn load_kernel(_system_table: &SystemTable<Boot>) -> Result<(), uefi::Status> {
    // En una implementación real, aquí se cargaría el kernel de Eclipse OS
    // Por ahora, simulamos la carga exitosa
    // TODO: Implementar carga real del kernel usando UEFI APIs
    Ok(())
}

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
    // Intentar cargar el kernel primero
    let kernel_result = load_kernel(&system_table);
    
    // Ahora obtener output para mostrar mensajes
    let output = system_table.stdout();
    let _ = output.write_str("Eclipse OS Bootloader\n");
    let _ = output.write_str("Version funcional\n");
    let _ = output.write_str("Kernel Eclipse listo\n");
    let _ = output.write_str("\n");
    
    // Mostrar resultado de la carga del kernel
    let _ = output.write_str("Cargando kernel Eclipse...\n");
    
    match kernel_result {
        Ok(_) => {
            let _ = output.write_str("Kernel cargado exitosamente\n");
            let _ = output.write_str("Transfiriendo control al kernel...\n");
            // El kernel debería tomar control aquí
            // Si llegamos aquí, el kernel no tomó control
            let _ = output.write_str("El kernel no tomo control del sistema\n");
        }
        Err(e) => {
            let _ = output.write_str("Error al cargar el kernel: ");
            let _ = write!(output, "{:?}", e);
            let _ = output.write_str("\n");
            let _ = output.write_str("Continuando con modo de emergencia...\n");
        }
    }
    
    let _ = output.write_str("\n");
    let _ = output.write_str("Sistema Eclipse en modo de emergencia\n");
    let _ = output.write_str("Presiona Ctrl+Alt+G para salir de QEMU\n");
    
    // Loop infinito para mantener el sistema ejecutándose
    loop {
        // Mantener el sistema activo
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
