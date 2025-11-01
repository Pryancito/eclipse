//! Allocador global para el kernel Eclipse
//!
//! Este módulo configura el allocador global usando `linked_list_allocator`
//! para habilitar `alloc` en el kernel.

use core::alloc::Layout;
use linked_list_allocator::LockedHeap;

/// Tamaño del heap del kernel (4MB)
/// Mantener este valor moderado evita sobrepasar los límites que el bootloader
/// reserva para la imagen ELF del kernel, pero nos da más margen que el 1MB
/// original para las nuevas estructuras.
const HEAP_SIZE: usize = 4 * 1024 * 1024;

/// Heap global del kernel
#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

/// Inicializa el allocador global
pub fn init_allocator() {
    unsafe {
        // Crear un buffer estático para el heap
        static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

        // Inicializar el heap con el buffer
        HEAP.lock().init(HEAP_MEM.as_mut_ptr(), HEAP_SIZE);
    }
}

// El manejador de errores de allocación se maneja automáticamente
// en Rust estable
