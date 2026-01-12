//! Sistema de heap dinámico para Eclipse OS
//! 
//! Este módulo implementa:
//! - Allocator de heap con múltiples estrategias
//! - Gestión de fragmentación
//! - Pool de bloques de diferentes tamaños
//! - Estadísticas de uso de memoria
//! - Detección de memory leaks

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};
use crate::debug::serial_write_str;
use alloc::format;
use alloc::vec::Vec;
use crate::memory::paging::{allocate_physical_page, deallocate_physical_page, PAGE_SIZE};

/// Tamaño mínimo de bloque (16 bytes)
pub const MIN_BLOCK_SIZE: usize = 16;

/// Tamaño máximo de bloque (1MB)
pub const MAX_BLOCK_SIZE: usize = 1024 * 1024;

/// Número de pools de diferentes tamaños
pub const POOL_COUNT: usize = 16;

/// Estructura para un bloque de memoria
#[derive(Debug, Clone, Copy)]
pub struct MemoryBlock {
    /// Tamaño del bloque
    pub size: usize,
    /// Si el bloque está libre
    pub is_free: bool,
    /// Puntero al siguiente bloque
    pub next: Option<NonNull<MemoryBlock>>,
    /// Puntero al bloque anterior
    pub prev: Option<NonNull<MemoryBlock>>,
}

impl MemoryBlock {
    /// Crear un nuevo bloque de memoria
    pub fn new(size: usize) -> Self {
        Self {
            size,
            is_free: true,
            next: None,
            prev: None,
        }
    }
    
    /// Obtener el puntero a los datos del bloque
    pub fn get_data_ptr(&self) -> *mut u8 {
        unsafe {
            (self as *const MemoryBlock as *mut u8).add(core::mem::size_of::<MemoryBlock>())
        }
    }
    
    /// Obtener el tamaño total del bloque incluyendo el header
    pub fn get_total_size(&self) -> usize {
        self.size + core::mem::size_of::<MemoryBlock>()
    }
    
    /// Dividir el bloque en dos si es posible
    pub fn split(&mut self, requested_size: usize) -> Option<NonNull<MemoryBlock>> {
        let min_split_size = requested_size + core::mem::size_of::<MemoryBlock>() + MIN_BLOCK_SIZE;
        
        if self.size < min_split_size {
            return None;
        }
        
        // CORRECCIÓN CRÍTICA: Debemos restar el tamaño del header del nuevo bloque
        // self.size es el tamaño de DATOS disponibles.
        // Al dividir, usamos 'requested_size' para DATOS del primer bloque.
        // El espacio restante es (self.size - requested_size).
        // DE ESE espacio, debemos restar el tamaño del HEADER del nuevo bloque.
        let remaining_size = self.size - requested_size - core::mem::size_of::<MemoryBlock>();
        self.size = requested_size;
        
        // Crear un nuevo bloque con el espacio restante
        let new_block_ptr = unsafe {
            self.get_data_ptr().add(requested_size) as *mut MemoryBlock
        };
        
        unsafe {
            ptr::write(new_block_ptr, MemoryBlock::new(remaining_size));
            let new_block = NonNull::new_unchecked(new_block_ptr);
            
            // Conectar los bloques
            (*new_block_ptr).next = self.next;
            (*new_block_ptr).prev = Some(NonNull::new_unchecked(self as *const MemoryBlock as *mut MemoryBlock));
            
            if let Some(next) = self.next {
                (*next.as_ptr()).prev = Some(new_block);
            }
            
            self.next = Some(new_block);
            
            Some(new_block)
        }
    }
    
    /// Fusionar con el siguiente bloque si es libre
    pub fn merge_with_next(&mut self) -> bool {
        if let Some(next_ptr) = self.next {
            unsafe {
                let next_block = &mut *next_ptr.as_ptr();
                if next_block.is_free {
                    self.size += next_block.get_total_size();
                    self.next = next_block.next;
                    
                    if let Some(next_next) = next_block.next {
                        (*next_next.as_ptr()).prev = Some(NonNull::new_unchecked(self as *const MemoryBlock as *mut MemoryBlock));
                    }
                    
                    return true;
                }
            }
        }
        false
    }
    
    /// Fusionar con el bloque anterior si es libre
    pub fn merge_with_prev(&mut self) -> bool {
        if let Some(prev_ptr) = self.prev {
            unsafe {
                let prev_block = &mut *prev_ptr.as_ptr();
                if prev_block.is_free {
                    prev_block.size += self.get_total_size();
                    prev_block.next = self.next;
                    
                    if let Some(next) = self.next {
                        (*next.as_ptr()).prev = Some(prev_ptr);
                    }
                    
                    return true;
                }
            }
        }
        false
    }
}

/// Pool de bloques de un tamaño específico
pub struct BlockPool {
    /// Tamaño de los bloques en este pool
    pub block_size: usize,
    /// Lista de bloques libres
    pub free_blocks: Option<NonNull<MemoryBlock>>,
    /// Número de bloques en el pool
    pub block_count: usize,
    /// Número de bloques libres
    pub free_count: usize,
}

impl BlockPool {
    /// Crear un nuevo pool de bloques
    pub fn new(block_size: usize) -> Self {
        Self {
            block_size,
            free_blocks: None,
            block_count: 0,
            free_count: 0,
        }
    }
    
    /// Agregar un bloque al pool
    pub fn add_block(&mut self, block: NonNull<MemoryBlock>) {
        unsafe {
            let block_ref = &mut *block.as_ptr();
            block_ref.is_free = true;
            block_ref.next = self.free_blocks;
            block_ref.prev = None;
            
                if let Some(free_block) = self.free_blocks {
                    (*free_block.as_ptr()).prev = Some(block);
                }
            
            self.free_blocks = Some(block);
            self.free_count += 1;
        }
    }
    
    /// Obtener un bloque libre del pool
    pub fn get_free_block(&mut self) -> Option<NonNull<MemoryBlock>> {
        if let Some(block) = self.free_blocks {
            unsafe {
                let block_ref = &mut *block.as_ptr();
                self.free_blocks = block_ref.next;
                
                if let Some(next) = block_ref.next {
                    (*next.as_ptr()).prev = None;
                }
                
                block_ref.is_free = false;
                block_ref.next = None;
                block_ref.prev = None;
                
                self.free_count -= 1;
                Some(block)
            }
        } else {
            None
        }
    }
    
    /// Devolver un bloque al pool
    pub fn return_block(&mut self, block: NonNull<MemoryBlock>) {
        unsafe {
            let block_ref = &mut *block.as_ptr();
            if !block_ref.is_free {
                block_ref.is_free = true;
                block_ref.next = self.free_blocks;
                block_ref.prev = None;
                
                if let Some(free_block) = self.free_blocks {
                    (*free_block.as_ptr()).prev = Some(block);
                }
                
                self.free_blocks = Some(block);
                self.free_count += 1;
            }
        }
    }
}

/// Allocator principal del heap
pub struct HeapAllocator {
    /// Pools de bloques de diferentes tamaños
    pools: [BlockPool; POOL_COUNT],
    /// Lista de bloques grandes
    large_blocks: Option<NonNull<MemoryBlock>>,
    /// Estadísticas del heap
    stats: HeapStats,
    /// Dirección base del heap
    heap_base: *mut u8,
    /// Tamaño total del heap
    heap_size: usize,
    /// Dirección actual del heap
    heap_current: *mut u8,
}

/// Estadísticas del heap
#[derive(Debug, Clone, Copy)]
pub struct HeapStats {
    /// Número total de asignaciones
    pub total_allocations: u64,
    /// Número total de liberaciones
    pub total_deallocations: u64,
    /// Número de asignaciones activas
    pub active_allocations: u64,
    /// Memoria total asignada
    pub total_allocated: u64,
    /// Memoria total liberada
    pub total_freed: u64,
    /// Memoria actualmente en uso
    pub current_usage: u64,
    /// Número de fragmentaciones
    pub fragmentation_count: u64,
    /// Tamaño del fragmento más grande
    pub largest_free_block: usize,
}

impl Default for HeapStats {
    fn default() -> Self {
        Self {
            total_allocations: 0,
            total_deallocations: 0,
            active_allocations: 0,
            total_allocated: 0,
            total_freed: 0,
            current_usage: 0,
            fragmentation_count: 0,
            largest_free_block: 0,
        }
    }
}

impl HeapAllocator {
    /// Crear un nuevo allocator de heap
    pub fn new(heap_base: *mut u8, heap_size: usize) -> Self {
        let mut pools = [
            BlockPool::new(16),   // 16 bytes
            BlockPool::new(32),   // 32 bytes
            BlockPool::new(64),   // 64 bytes
            BlockPool::new(128),  // 128 bytes
            BlockPool::new(256),  // 256 bytes
            BlockPool::new(512),  // 512 bytes
            BlockPool::new(1024), // 1KB
            BlockPool::new(2048), // 2KB
            BlockPool::new(4096), // 4KB
            BlockPool::new(8192), // 8KB
            BlockPool::new(16384), // 16KB
            BlockPool::new(32768), // 32KB
            BlockPool::new(65536), // 64KB
            BlockPool::new(131072), // 128KB
            BlockPool::new(262144), // 256KB
            BlockPool::new(524288), // 512KB
        ];
        
        // Inicializar el primer bloque del heap
        let first_block = unsafe {
            ptr::write(heap_base as *mut MemoryBlock, MemoryBlock::new(heap_size - core::mem::size_of::<MemoryBlock>()));
            NonNull::new_unchecked(heap_base as *mut MemoryBlock)
        };
        
        // Agregar el bloque inicial al pool apropiado
        let block_size = unsafe { first_block.as_ref().size };
        let pool_index = Self::get_pool_index(block_size);
        if pool_index < POOL_COUNT {
            pools[pool_index].add_block(first_block);
        } else {
            // Bloque grande
            unsafe {
                let block_ref = &mut *first_block.as_ptr();
                block_ref.next = None;
                block_ref.prev = None;
            }
        }
        
        Self {
            pools,
            large_blocks: if pool_index >= POOL_COUNT { Some(first_block) } else { None },
            stats: HeapStats::default(),
            heap_base,
            heap_size,
            heap_current: unsafe { heap_base.add(core::mem::size_of::<MemoryBlock>()) },
        }
    }
    
    /// Obtener el índice del pool para un tamaño dado
    fn get_pool_index(size: usize) -> usize {
        // CORRECCIÓN: Deshabilitamos los pools para evitar romper la cadena física de bloques.
        // Al obligar a usar "Bloques Grandes" (POOL_COUNT), mantenemos todos los bloques
        // en una única lista enlazada física (large_blocks), permitiendo que split/merge
        // funcionen correctamente sin perder referencias al resto del heap.
        POOL_COUNT 
    }
    
    /// Asignar memoria
    pub fn allocate(&mut self, size: usize, align: usize) -> *mut u8 {
        if size == 0 {
            crate::debug::serial_write_str("HEAP: Request for 0 bytes\n");
            return ptr::null_mut();
        }
        
        // Alinear el tamaño al alineamiento del bloque para garantizar
        // que el siguiente bloque (si hay split) comience en una dirección alineada.
        let block_align = core::mem::align_of::<MemoryBlock>();
        let global_align = align.max(block_align);
        let aligned_size = (size + global_align - 1) & !(global_align - 1);

        // crate::debug::serial_write_str(" [H] ");
        if size > 100000 {
             crate::debug::serial_write_str("HEAP: Large Alloc > 100KB\n");
        }

        // Buscar en bloques grandes (que ahora son TODOS los bloques)
        // Note: find_large_block uses aligned_size for splitting
        if let Some(block) = self.find_large_block(aligned_size) {
            unsafe {
                let block_ref = &mut *block.as_ptr();
                let data_ptr = block_ref.get_data_ptr();
                
                self.stats.total_allocations += 1;
                self.stats.active_allocations += 1;
                self.stats.total_allocated += aligned_size as u64;
                self.stats.current_usage += aligned_size as u64;
                
                if aligned_size > 100000 {
                     crate::debug::serial_write_str(&alloc::format!("HEAP: Alloc success, ptr=0x{:p}\n", data_ptr));
                }

                return data_ptr;
            }
        }
        
        // Si no hay bloques disponibles, intentar expandir el heap
        if let Some(new_block) = self.expand_heap(aligned_size) {
            unsafe {
                let block_ref = &mut *new_block.as_ptr();
                let data_ptr = block_ref.get_data_ptr();
                
                self.stats.total_allocations += 1;
                self.stats.active_allocations += 1;
                self.stats.total_allocated += aligned_size as u64;
                self.stats.current_usage += aligned_size as u64;
                
                return data_ptr;
            }
        }
        
        // No hay memoria disponible
        crate::debug::serial_write_str("HEAP PANIC: OOM detected (Static Log)\n");
        ptr::null_mut()
    }
    
    /// Liberar memoria
    pub fn deallocate(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        
        // Encontrar el bloque que contiene este puntero
        if let Some(block) = self.find_block_containing(ptr) {
            unsafe {
                let block_ref = &mut *block.as_ptr();
                if !block_ref.is_free {
                    block_ref.is_free = true;
                    
                    self.stats.total_deallocations += 1;
                    self.stats.active_allocations -= 1;
                    self.stats.total_freed += block_ref.size as u64;
                    self.stats.current_usage -= block_ref.size as u64;
                    
                    // CORRECCIÓN: Intentar fusionar.
                    // Si se fusiona con el anterior, 'block' deja de ser válido.
                    // Si se fusiona con el siguiente, 'block' crece.
                    // En NINGÚN CASO debemos mover 'block' a listas de Pools o reinsertarlo
                    // en large_blocks, porque YA ESTÁ en la cadena física correctamente.
                    self.merge_blocks(block);
                }
            }
        }
    }
    
    /// Buscar un bloque grande que pueda satisfacer la solicitud
    fn find_large_block(&mut self, size: usize) -> Option<NonNull<MemoryBlock>> {
        let mut current = self.large_blocks;
        let debug = size > 100000;
        
        while let Some(block) = current {
            unsafe {
                let block_ref = &mut *block.as_ptr();
                if debug {
                     // crate::debug::serial_write_str("HEAP: Check block... ");
                }
                
                if block_ref.is_free && block_ref.size >= size {
                    if debug {
                        crate::debug::serial_write_str("HEAP: Found candidate block. Splitting...\n");
                    }

                    // Dividir el bloque si es necesario
                    if let Some(new_block) = block_ref.split(size) {
                        if debug {
                             crate::debug::serial_write_str("HEAP: Split OK.\n");
                        }
                        // El bloque original ahora tiene el tamaño solicitado
                        block_ref.is_free = false;
                        return Some(block);
                    } else {
                        if debug {
                             crate::debug::serial_write_str("HEAP: Exact fit / No split.\n");
                        }
                        // El bloque es del tamaño exacto
                        block_ref.is_free = false;
                        return Some(block);
                    }
                }
                current = block_ref.next;
            }
        }
        
        if debug {
            crate::debug::serial_write_str("HEAP: No large block found.\n");
        }
        None
    }
    
    /// Encontrar el bloque que contiene un puntero
    fn find_block_containing(&self, ptr: *mut u8) -> Option<NonNull<MemoryBlock>> {
        // Buscar en todos los pools
        for pool in &self.pools {
            let mut current = pool.free_blocks;
            while let Some(block) = current {
                unsafe {
                    let block_ref = &*block.as_ptr();
                    let block_start = block_ref.get_data_ptr();
                    let block_end = block_start.add(block_ref.size);
                    
                    if ptr >= block_start && ptr < block_end {
                        return Some(block);
                    }
                    
                    current = block_ref.next;
                }
            }
        }
        
        // Buscar en bloques grandes
        let mut current = self.large_blocks;
        while let Some(block) = current {
            unsafe {
                let block_ref = &*block.as_ptr();
                let block_start = block_ref.get_data_ptr();
                let block_end = block_start.add(block_ref.size);
                
                if ptr >= block_start && ptr < block_end {
                    return Some(block);
                }
                
                current = block_ref.next;
            }
        }
        
        None
    }
    
    /// Fusionar bloques adyacentes
    fn merge_blocks(&mut self, block: NonNull<MemoryBlock>) {
        unsafe {
            let block_ref = &mut *block.as_ptr();
            
            // Fusionar con el siguiente
            if block_ref.merge_with_next() {
                self.stats.fragmentation_count += 1;
            }
            
            // Fusionar con el anterior
            if block_ref.merge_with_prev() {
                self.stats.fragmentation_count += 1;
            }
        }
    }
    
    /// Expandir el heap
    fn expand_heap(&mut self, size: usize) -> Option<NonNull<MemoryBlock>> {
        // Intentar asignar una nueva página física
        if let Some(physical_addr) = allocate_physical_page() {
            let virtual_addr = self.heap_current as u64;
            
            // Mapear la nueva página
            if crate::memory::paging::map_virtual_page(virtual_addr, physical_addr, 0x07).is_ok() {
                let new_block = unsafe {
                    ptr::write(self.heap_current as *mut MemoryBlock, MemoryBlock::new(PAGE_SIZE - core::mem::size_of::<MemoryBlock>()));
                    NonNull::new_unchecked(self.heap_current as *mut MemoryBlock)
                };
                
                self.heap_current = unsafe { self.heap_current.add(PAGE_SIZE) };
                
                return Some(new_block);
            }
        }
        
        None
    }
    
    /// Obtener estadísticas del heap
    pub fn get_stats(&self) -> HeapStats {
        self.stats
    }
    
    /// Obtener el ratio de fragmentación
    pub fn get_fragmentation_ratio(&self) -> f32 {
        if self.stats.current_usage == 0 {
            return 0.0;
        }
        
        let mut total_free = 0;
        let mut largest_free = 0;
        
        // Contar bloques libres en pools
        for pool in &self.pools {
            total_free += pool.free_count * pool.block_size;
            if pool.free_count > 0 {
                largest_free = largest_free.max(pool.block_size);
            }
        }
        
        // Contar bloques libres grandes
        let mut current = self.large_blocks;
        while let Some(block) = current {
            unsafe {
                let block_ref = &*block.as_ptr();
                if block_ref.is_free {
                    total_free += block_ref.size;
                    largest_free = largest_free.max(block_ref.size);
                }
                current = block_ref.next;
            }
        }
        
        if total_free == 0 {
            return 0.0;
        }
        
        (total_free - largest_free) as f32 / total_free as f32
    }
    
    /// Verificar la integridad del heap
    pub fn verify_integrity(&self) -> bool {
        // Verificar que todos los bloques estén correctamente enlazados
        for pool in &self.pools {
            let mut current = pool.free_blocks;
            while let Some(block) = current {
                unsafe {
                    let block_ref = &*block.as_ptr();
                    if let Some(next) = block_ref.next {
                        if next.as_ref().prev != Some(block) {
                            return false;
                        }
                    }
                    current = block_ref.next;
                }
            }
        }
        
        true
    }
}

/// Instancia global del allocator de heap
static mut HEAP_ALLOCATOR: Option<HeapAllocator> = None;

/// Inicializar el heap
pub fn init_heap(heap_size: u64) -> Result<(), &'static str> {
    serial_write_str("HEAP: Inicializando heap del kernel...\n");
    
    // Asignar páginas físicas para el heap
    // Asignar y mapear páginas físicas para el heap
    // IMPORTANTE: No usar Vec ni ninguna estructura que asigne memoria aquí,
    // porque el allocator aún no está listo.
    
    let heap_base_addr = 0xFFFF_8000_0000_0000; // Dirección virtual del heap
    let pages_needed = (heap_size / PAGE_SIZE as u64) as usize;
    
    // serial_write_str(&alloc::format!("HEAP: Mapping range 0x{:016X} - 0x{:016X}\n", heap_base_addr, heap_base_addr + heap_size));
    serial_write_str("HEAP: Mapping range 0xFFFF800000000000 - +64MB\n");

    for i in 0..pages_needed {
        if let Some(physical_addr) = allocate_physical_page() {
             let virtual_addr = heap_base_addr + (i * PAGE_SIZE) as u64;
             
             // Debug log for the area where the crash happens (approx offset 0x4EE80 / 4096 = 78)
             if i == 78 {
                 crate::debug::serial_write_str("HEAP: Mapping Index 78 (Crash Zone). Phys Addr: ");
                 crate::memory::paging::print_hex(physical_addr);
                 crate::debug::serial_write_str("\n");
             }

             // 0x07 = PRESENT | WRITABLE | USER (aunque sea kernel heap, user es harmless aqui, WRITABLE es critico)
             crate::memory::paging::map_virtual_page(virtual_addr, physical_addr, 0x03)?; // 0x03 = PRESENT | WRITE
        } else {
            return Err("No hay suficientes páginas físicas para el heap");
        }
    }
    
    // Crear el allocator de heap
    let allocator = HeapAllocator::new(heap_base_addr as *mut u8, heap_size as usize);
    
    // Guardar globalmente
    unsafe {
        HEAP_ALLOCATOR = Some(allocator);
    }
    
    serial_write_str(&format!("HEAP: Heap inicializado con {} MB\n", heap_size / (1024 * 1024)));
    Ok(())
}

/// Obtener el allocator de heap
fn get_heap_allocator() -> &'static mut HeapAllocator {
    unsafe {
        HEAP_ALLOCATOR.as_mut().expect("Heap no inicializado")
    }
}

/// Asignar memoria del kernel
pub fn kernel_alloc(size: usize, align: usize) -> *mut u8 {
    // crate::debug::serial_write_str(" [K] ");
    let allocator = get_heap_allocator();
    allocator.allocate(size, align)
}

/// Liberar memoria del kernel
pub fn kernel_dealloc(ptr: *mut u8) {
    let allocator = get_heap_allocator();
    allocator.deallocate(ptr);
}

/// Obtener estadísticas del heap
pub fn get_heap_stats() -> HeapStats {
    let allocator = get_heap_allocator();
    allocator.get_stats()
}

/// Obtener el ratio de fragmentación
pub fn get_fragmentation_ratio() -> f32 {
    let allocator = get_heap_allocator();
    allocator.get_fragmentation_ratio()
}

/// Verificar la integridad del heap
pub fn verify_heap_integrity() -> bool {
    let allocator = get_heap_allocator();
    allocator.verify_integrity()
}

/// Imprimir estadísticas del heap
pub fn print_heap_stats() {
    let stats = get_heap_stats();
    let fragmentation = get_fragmentation_ratio();
    
    serial_write_str("=== ESTADÍSTICAS DEL HEAP ===\n");
    serial_write_str(&format!("Asignaciones totales: {}\n", stats.total_allocations));
    serial_write_str(&format!("Liberaciones totales: {}\n", stats.total_deallocations));
    serial_write_str(&format!("Asignaciones activas: {}\n", stats.active_allocations));
    serial_write_str(&format!("Memoria total asignada: {} KB\n", stats.total_allocated / 1024));
    serial_write_str(&format!("Memoria total liberada: {} KB\n", stats.total_freed / 1024));
    serial_write_str(&format!("Memoria actualmente en uso: {} KB\n", stats.current_usage / 1024));
    serial_write_str(&format!("Fragmentaciones: {}\n", stats.fragmentation_count));
    serial_write_str(&format!("Fragmentación: {:.2}%\n", fragmentation * 100.0));
    serial_write_str(&format!("Bloque libre más grande: {} bytes\n", stats.largest_free_block));
    serial_write_str("=============================\n");
}
