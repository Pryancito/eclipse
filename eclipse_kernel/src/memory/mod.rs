//! Módulo de Gestión de Memoria para Eclipse OS
//! 
//! Este módulo proporciona todas las funcionalidades de gestión de memoria:
//! - Paginación de 4 niveles
//! - Asignación dinámica de memoria
//! - Gestión de memoria física
//! - Protección de memoria

pub mod manager;
pub mod paging;
pub mod allocator;

// Re-exportar las estructuras principales
pub use manager::{init_memory_manager, get_memory_manager};
pub use paging::{init_paging, enable_paging};

/// Constantes de memoria
pub const PAGE_SIZE: usize = 4096;
pub const PAGE_TABLE_ENTRIES: usize = 512;
pub const PAGE_LEVELS: usize = 4;

/// Flags de página
pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
pub const PAGE_USER: u64 = 1 << 2;
pub const PAGE_WRITE_THROUGH: u64 = 1 << 3;
pub const PAGE_CACHE_DISABLE: u64 = 1 << 4;
pub const PAGE_SIZE_2MB: u64 = 1 << 7;

/// Direcciones importantes
pub const KERNEL_VIRTUAL_BASE: u64 = 0xffff800000000000;
pub const KERNEL_HEAP_START: u64 = 0x2000000;
pub const KERNEL_HEAP_SIZE: usize = 0x1000000; // 16MB

/// Inicializar el sistema de memoria completo
pub fn init_memory_system(physical_base: u64, memory_size: u64) -> Result<(), &'static str> {
    // Inicializar el gestor de memoria
    init_memory_manager(physical_base, memory_size)?;
    
    // Inicializar el sistema de paginación
    let _paging = init_paging()?;
    
    // Habilitar paginación
    enable_paging();
    
    Ok(())
}

/// Obtener información del sistema de memoria
pub fn get_memory_info() -> MemoryInfo {
    if let Some(manager) = get_memory_manager() {
        MemoryInfo {
            total_memory: manager.physical_memory_size,
            free_memory: manager.physical_memory_size, // Simplificado
            used_memory: 0,
            page_size: PAGE_SIZE,
        }
    } else {
        MemoryInfo {
            total_memory: 0,
            free_memory: 0,
            used_memory: 0,
            page_size: PAGE_SIZE,
        }
    }
}

/// Información del sistema de memoria
#[derive(Debug, Clone, Copy)]
pub struct MemoryInfo {
    pub total_memory: u64,
    pub free_memory: u64,
    pub used_memory: u64,
    pub page_size: usize,
}

/// Funciones de utilidad para memoria
pub mod utils {
    use super::*;

    /// Alinear una dirección a un múltiplo de página
    pub fn align_to_page(addr: u64) -> u64 {
        (addr + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1)
    }

    /// Verificar si una dirección está alineada a página
    pub fn is_page_aligned(addr: u64) -> bool {
        (addr & (PAGE_SIZE as u64 - 1)) == 0
    }

    /// Calcular el número de páginas necesarias para un tamaño
    pub fn pages_needed(size: usize) -> usize {
        (size + PAGE_SIZE - 1) / PAGE_SIZE
    }

    /// Convertir bytes a páginas
    pub fn bytes_to_pages(bytes: usize) -> usize {
        pages_needed(bytes)
    }

    /// Convertir páginas a bytes
    pub fn pages_to_bytes(pages: usize) -> usize {
        pages * PAGE_SIZE
    }
}
