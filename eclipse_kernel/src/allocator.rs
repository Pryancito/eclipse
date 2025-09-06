//! Allocador global para el kernel Eclipse
//! 
//! Este módulo configura el allocador global usando `linked_list_allocator`
//! para habilitar `alloc` en el kernel.

use linked_list_allocator::LockedHeap;
use core::alloc::Layout;

/// Tamaño del heap del kernel (1MB)
const HEAP_SIZE: usize = 1024 * 1024;

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
