//! Sistema de gestión de memoria avanzado para Eclipse OS
//! 
//! Este módulo implementa:
//! - Sistema de paginación con MMU
//! - Heap dinámico con allocator
//! - DMA para dispositivos
//! - Memoria compartida para IPC
//! - Gestión de memoria virtual

pub mod paging;
pub mod heap;
pub mod dma;
pub mod shared_memory;
pub mod virtual_memory;
pub mod memory_manager;
pub mod physical;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use crate::debug::serial_write_str;
use alloc::format;

/// Configuración del sistema de memoria
pub struct MemoryConfig {
    /// Tamaño total de memoria física disponible
    pub total_physical_memory: u64,
    /// Tamaño del heap del kernel
    pub kernel_heap_size: u64,
    /// Tamaño de la pila del kernel
    pub kernel_stack_size: u64,
    /// Tamaño mínimo de página
    pub page_size: u64,
    /// Número máximo de páginas
    pub max_pages: u32,
    /// Habilitar DMA
    pub enable_dma: bool,
    /// Habilitar memoria compartida
    pub enable_shared_memory: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            total_physical_memory: 4 * 1024 * 1024 * 1024, // 4GB por defecto
            kernel_heap_size: 64 * 1024 * 1024, // 64MB
            kernel_stack_size: 8 * 1024 * 1024, // 8MB
            page_size: 4096, // 4KB
            max_pages: 1024 * 1024, // 1M páginas
            enable_dma: true,
            enable_shared_memory: true,
        }
    }
}

/// Estado del sistema de memoria
#[derive(Clone, Copy)]
pub struct MemoryState {
    /// Memoria física total
    pub total_physical: u64,
    /// Memoria física usada
    pub used_physical: u64,
    /// Memoria virtual total
    pub total_virtual: u64,
    /// Memoria virtual usada
    pub used_virtual: u64,
    /// Número de páginas asignadas
    pub allocated_pages: u32,
    /// Número de páginas libres
    pub free_pages: u32,
    /// Fragmentación del heap
    pub heap_fragmentation: f32,
    /// Estadísticas de DMA
    pub dma_stats: DmaStats,
}

/// Estadísticas de DMA
#[derive(Clone, Copy)]
pub struct DmaStats {
    /// Número de buffers DMA activos
    pub active_buffers: u32,
    /// Memoria total usada por DMA
    pub total_dma_memory: u64,
    /// Número de transferencias completadas
    pub completed_transfers: u64,
    /// Número de transferencias fallidas
    pub failed_transfers: u64,
}

impl Default for DmaStats {
    fn default() -> Self {
        Self {
            active_buffers: 0,
            total_dma_memory: 0,
            completed_transfers: 0,
            failed_transfers: 0,
        }
    }
}

/// Inicializa el sistema de memoria
pub fn init_memory_system(config: MemoryConfig) -> Result<(), &'static str> {
    serial_write_str("MEMORY: Inicializando sistema de memoria...\n");
    
    // Inicializar paginación
    paging::init_paging(&config)?;
    serial_write_str("MEMORY: Sistema de paginación inicializado\n");
    
    // Inicializar heap
    heap::init_heap(config.kernel_heap_size)?;
    serial_write_str("MEMORY: Heap del kernel inicializado\n");
    
    // Inicializar DMA si está habilitado
    if config.enable_dma {
        dma::init_dma()?;
        serial_write_str("MEMORY: Sistema DMA inicializado\n");
    }
    
    // Inicializar memoria compartida si está habilitada
    if config.enable_shared_memory {
        shared_memory::init_shared_memory()?;
        serial_write_str("MEMORY: Sistema de memoria compartida inicializado\n");
    }
    
    // Inicializar gestor de memoria virtual
    virtual_memory::init_virtual_memory()?;
    serial_write_str("MEMORY: Gestor de memoria virtual inicializado\n");
    
    serial_write_str("MEMORY: Sistema de memoria inicializado completamente\n");
    Ok(())
}

/// Obtiene el estado actual del sistema de memoria
pub fn get_memory_state() -> MemoryState {
    MemoryState {
        total_physical: paging::get_total_physical_memory(),
        used_physical: paging::get_used_physical_memory(),
        total_virtual: virtual_memory::get_total_virtual_memory(),
        used_virtual: virtual_memory::get_used_virtual_memory(),
        allocated_pages: paging::get_allocated_pages(),
        free_pages: paging::get_free_pages(),
        heap_fragmentation: heap::get_fragmentation_ratio(),
        dma_stats: {
            let dma_stats = dma::get_dma_stats();
            DmaStats {
                active_buffers: dma_stats.active_buffers,
                total_dma_memory: dma_stats.total_dma_memory,
                completed_transfers: dma_stats.completed_transfers,
                failed_transfers: dma_stats.failed_transfers,
            }
        },
    }
}

/// Imprime estadísticas del sistema de memoria
pub fn print_memory_stats() {
    let state = get_memory_state();
    
    serial_write_str("=== ESTADÍSTICAS DE MEMORIA ===\n");
    serial_write_str(&format!("Memoria física total: {} MB\n", state.total_physical / (1024 * 1024)));
    serial_write_str(&format!("Memoria física usada: {} MB\n", state.used_physical / (1024 * 1024)));
    serial_write_str(&format!("Memoria virtual total: {} MB\n", state.total_virtual / (1024 * 1024)));
    serial_write_str(&format!("Memoria virtual usada: {} MB\n", state.used_virtual / (1024 * 1024)));
    serial_write_str(&format!("Páginas asignadas: {}\n", state.allocated_pages));
    serial_write_str(&format!("Páginas libres: {}\n", state.free_pages));
    serial_write_str(&format!("Fragmentación del heap: {:.2}%\n", state.heap_fragmentation * 100.0));
    serial_write_str(&format!("Buffers DMA activos: {}\n", state.dma_stats.active_buffers));
    serial_write_str(&format!("Memoria DMA total: {} KB\n", state.dma_stats.total_dma_memory / 1024));
    serial_write_str(&format!("Transferencias DMA completadas: {}\n", state.dma_stats.completed_transfers));
    serial_write_str(&format!("Transferencias DMA fallidas: {}\n", state.dma_stats.failed_transfers));
    serial_write_str("================================\n");
}

/// Allocator global para el kernel
pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // crate::debug::serial_write_str(" [G] ");
        heap::kernel_alloc(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        heap::kernel_dealloc(ptr);
    }
}

/// Macro para asignar memoria alineada
#[macro_export]
macro_rules! aligned_alloc {
    ($size:expr, $align:expr) => {
        unsafe {
            let layout = core::alloc::Layout::from_size_align($size, $align).unwrap();
            crate::memory::KernelAllocator.alloc(layout)
        }
    };
}

/// Macro para liberar memoria alineada
#[macro_export]
macro_rules! aligned_dealloc {
    ($ptr:expr, $size:expr, $align:expr) => {
        unsafe {
            let layout = core::alloc::Layout::from_size_align($size, $align).unwrap();
            crate::memory::KernelAllocator.dealloc($ptr, layout);
        }
    };
}

/// Función para copiar memoria de forma segura
pub fn safe_memcpy(dst: *mut u8, src: *const u8, len: usize) -> Result<(), &'static str> {
    if dst.is_null() || src.is_null() {
        return Err("Punteros nulos");
    }
    
    if len == 0 {
        return Ok(());
    }
    
    // Verificar que las regiones no se solapen
    let dst_start = dst as usize;
    let dst_end = dst_start + len;
    let src_start = src as usize;
    let src_end = src_start + len;
    
    if (dst_start >= src_start && dst_start < src_end) || 
       (dst_end > src_start && dst_end <= src_end) {
        return Err("Regiones de memoria solapadas");
    }
    
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, len);
    }
    
    Ok(())
}

/// Función para llenar memoria con un valor
pub fn safe_memset(dst: *mut u8, value: u8, len: usize) -> Result<(), &'static str> {
    if dst.is_null() {
        return Err("Puntero nulo");
    }
    
    if len == 0 {
        return Ok(());
    }
    
    unsafe {
        core::ptr::write_bytes(dst, value, len);
    }
    
    Ok(())
}

/// Función para comparar memoria
pub fn safe_memcmp(ptr1: *const u8, ptr2: *const u8, len: usize) -> Result<i32, &'static str> {
    if ptr1.is_null() || ptr2.is_null() {
        return Err("Punteros nulos");
    }
    
    if len == 0 {
        return Ok(0);
    }
    
    unsafe {
        for i in 0..len {
            let byte1 = *ptr1.add(i);
            let byte2 = *ptr2.add(i);
            if byte1 != byte2 {
                return Ok(if byte1 < byte2 { -1 } else { 1 });
            }
        }
    }
    
    Ok(0)
}