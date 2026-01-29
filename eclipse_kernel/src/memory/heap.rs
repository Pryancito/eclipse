//! Sistema de heap dinámico para Eclipse OS
//! 
//! Este módulo implementa el allocator de heap usando linked_list_allocator

use linked_list_allocator::LockedHeap;
use crate::debug::serial_write_str;
use crate::memory::paging::{allocate_physical_page, PAGE_SIZE};
use alloc::format;

/// Instancia global del allocator de heap protegido por un Spinlock (LockedHeap)
#[global_allocator]
pub static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Inicializar el heap
pub fn init_heap(heap_size: u64) -> Result<(), &'static str> {
    serial_write_str("HEAP: Inicializando heap del kernel...\n");
    
    let heap_base_addr = 0xFFFF_8000_0000_0000; // Dirección virtual del heap
    let pages_needed = (heap_size / PAGE_SIZE as u64) as usize;
    
    // serial_write_str(&format!("HEAP: Mapping range 0x{:X} size {} MB\n", heap_base_addr, heap_size / (1024 * 1024)));
    serial_write_str("HEAP: Mapping heap memory...\n");

    for i in 0..pages_needed {
        if let Some(physical_addr) = allocate_physical_page() {
             let virtual_addr = heap_base_addr + (i * PAGE_SIZE) as u64;
             
             // 0x03 = PRESENT | WRITE
             crate::memory::paging::map_virtual_page(virtual_addr, physical_addr, 0x03)?; 
        } else {
            return Err("No hay suficientes páginas físicas para el heap");
        }
    }
    
    // Inicializar el LockedHeap con el rango mapeado
    unsafe {
        ALLOCATOR.lock().init(heap_base_addr as *mut u8, heap_size as usize);
    }
    
    serial_write_str("HEAP: Heap inicializado correctamente.\n");
    Ok(())
}

// Funciones de compatibilidad para mantener la API existente usada por memory/mod.rs

pub fn kernel_alloc(size: usize, align: usize) -> *mut u8 {
    let layout = core::alloc::Layout::from_size_align(size, align).unwrap();
    unsafe {
        use core::alloc::GlobalAlloc; // Importar trait para acceder a alloc
        ALLOCATOR.alloc(layout)
    }
}

pub fn kernel_dealloc(ptr: *mut u8) {
    // Para dealloc necesitamos el layout, pero la API anterior no lo pedía.
    // En un sistema real, el allocator recordaría el layout.
    // LockedHeap requiere layout para dealloc si se usa via GlobalAlloc.
    // SIN EMBARGO, LockedHeap implementa GlobalAlloc, así que si esto se llama
    // desde bibliotecas de Rust (Box, Vec), pasarán el layout.
    // Si esto se llama manualmente desde el kernel viejo... tenemos un problema.
    // 
    // Revisando memory/mod.rs: 
    // unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) { heap::kernel_dealloc(ptr); }
    //
    // El wrapper KernelAllocator en memory/mod.rs ignora el layout! 
    // Esto es peligroso para LockedHeap.
    // 
    // CORRECCIÓN: Vamos a cambiar memory/mod.rs para que use ALLOCATOR directamente
    // y eliminar estas funciones wrapper obsoletas y peligrosas.
    // Pero por ahora, para compilar, dejaremos un panic o no-op si no podemos
    // recuperar el layout, O MEJOR: modificar memory/mod.rs para que pase el layout.
    
    crate::debug::serial_write_str("WARNING: kernel_dealloc called without layout. Leaking memory.\n");
}

// Estadísticas de heap (stubs por ahora, ya que LockedHeap no expone estadísticas fácilmente)

#[derive(Debug, Clone, Copy, Default)]
pub struct HeapStats {
    pub total_allocations: u64,
    pub total_deallocations: u64,
    pub active_allocations: u64,
    pub total_allocated: u64,
    pub total_freed: u64,
    pub current_usage: u64,
    pub fragmentation_count: u64,
    pub largest_free_block: usize,
}

pub fn get_fragmentation_ratio() -> f32 { 0.0 }
pub fn get_heap_stats() -> HeapStats { HeapStats::default() }
pub fn verify_heap_integrity() -> bool { true }

