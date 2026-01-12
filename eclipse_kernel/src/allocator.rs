
/// Allocador global para el kernel Eclipse
///
/// Este módulo configura el allocador global para usar nuestro
/// sistema de memoria avanzado (src/memory/mod.rs).

use crate::memory::KernelAllocator;

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

/// Inicializa el allocador global
pub fn init_allocator() {
    // No hace nada. La inicialización real ocurre en memory::init_memory_system()
    // llamado desde main_simple.rs.
    // Mantenemos esta función para compatibilidad con código existente.
}
