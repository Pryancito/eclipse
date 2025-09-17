//! Gestor de heap para Eclipse OS
//! 
//! Implementa un heap dinámico con diferentes estrategias de asignación

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use super::allocator::{BlockAllocator, AllocationStrategy};

/// Configuración del heap
#[derive(Debug, Clone)]
pub struct HeapConfig {
    pub initial_size: usize,
    pub max_size: usize,
    pub growth_factor: f32,
    pub allocation_strategy: AllocationStrategy,
    pub enable_compaction: bool,
}

impl Default for HeapConfig {
    fn default() -> Self {
        Self {
            initial_size: 1024 * 1024, // 1MB
            max_size: 64 * 1024 * 1024, // 64MB
            growth_factor: 1.5,
            allocation_strategy: AllocationStrategy::FirstFit,
            enable_compaction: true,
        }
    }
}

/// Estadísticas del heap
#[derive(Debug, Clone)]
pub struct HeapStats {
    pub total_allocations: u64,
    pub total_deallocations: u64,
    pub current_allocations: u64,
    pub peak_usage: usize,
    pub current_usage: usize,
    pub fragmentation: f32,
    pub compaction_count: u64,
}

/// Gestor de heap dinámico
pub struct HeapManager {
    config: HeapConfig,
    allocator: BlockAllocator,
    heap_start: usize,
    current_size: usize,
    stats: HeapStats,
    initialized: bool,
}

impl HeapManager {
    pub fn new(config: HeapConfig) -> Self {
        let allocation_strategy = config.allocation_strategy;
        Self {
            config,
            allocator: BlockAllocator::new(allocation_strategy),
            heap_start: 0,
            current_size: 0,
            stats: HeapStats {
                total_allocations: 0,
                total_deallocations: 0,
                current_allocations: 0,
                peak_usage: 0,
                current_usage: 0,
                fragmentation: 0.0,
                compaction_count: 0,
            },
            initialized: false,
        }
    }

    pub fn initialize(&mut self, heap_start: usize) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Heap already initialized");
        }

        self.heap_start = heap_start;
        self.current_size = self.config.initial_size;
        
        // Recrear el allocator con la dirección correcta
        self.allocator = BlockAllocator::new(self.config.allocation_strategy);

        self.initialized = true;
        Ok(())
    }

    pub fn allocate(&mut self, layout: Layout) -> Result<*mut u8, &'static str> {
        if !self.initialized {
            return Err("Heap not initialized");
        }

        // Intentar asignar memoria
        if let Some(ptr) = self.allocator.allocate(layout) {
            self.stats.total_allocations += 1;
            self.stats.current_allocations += 1;
            self.stats.current_usage += layout.size();
            
            if self.stats.current_usage > self.stats.peak_usage {
                self.stats.peak_usage = self.stats.current_usage;
            }

            Ok(ptr.as_ptr())
        } else {
            // Si falla, intentar expandir el heap
            if self.try_expand_heap() {
                // Reintentar la asignación
                if let Some(ptr) = self.allocator.allocate(layout) {
                    self.stats.total_allocations += 1;
                    self.stats.current_allocations += 1;
                    self.stats.current_usage += layout.size();
                    
                    if self.stats.current_usage > self.stats.peak_usage {
                        self.stats.peak_usage = self.stats.current_usage;
                    }

                    Ok(ptr.as_ptr())
                } else {
                    Err("Failed to allocate memory even after heap expansion")
                }
            } else {
                Err("Failed to allocate memory and cannot expand heap")
            }
        }
    }

    pub fn deallocate(&mut self, ptr: *mut u8, layout: Layout) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Heap not initialized");
        }

        if ptr.is_null() {
            return Ok(());
        }

        use core::ptr::NonNull;
        if let Some(non_null_ptr) = NonNull::new(ptr) {
            self.allocator.deallocate(non_null_ptr, layout);
            self.stats.total_deallocations += 1;
            self.stats.current_allocations -= 1;
            self.stats.current_usage -= layout.size();
        }

        // Si está habilitada la compactación, intentar compactar periódicamente
        if self.config.enable_compaction && 
           self.stats.current_allocations > 0 && 
           self.stats.current_allocations % 100 == 0 {
            self.compact_heap();
        }

        Ok(())
    }

    fn try_expand_heap(&mut self) -> bool {
        let new_size = (self.current_size as f32 * self.config.growth_factor) as usize;
        
        if new_size > self.config.max_size {
            return false;
        }

        // En un sistema real, aquí se expandiría el heap físicamente
        // Por ahora, solo simulamos la expansión
        self.current_size = new_size;
        
        // Recrear el allocator con el nuevo tamaño
        self.allocator = BlockAllocator::new(self.config.allocation_strategy);

        true
    }

    fn compact_heap(&mut self) {
        // Implementación simplificada de compactación
        // En un sistema real, esto movería los bloques asignados
        // para reducir la fragmentación
        self.stats.compaction_count += 1;
    }

    pub fn get_stats(&self) -> &HeapStats {
        &self.stats
    }

    pub fn get_usage_percentage(&self) -> f32 {
        if self.current_size == 0 {
            return 0.0;
        }
        (self.stats.current_usage as f32 / self.current_size as f32) * 100.0
    }

    pub fn get_fragmentation(&self) -> f32 {
        0.0 // Fragmentation no disponible en MemoryStats
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

/// Allocator global del kernel
pub struct KernelHeapAllocator {
    heap_manager: HeapManager,
}

impl KernelHeapAllocator {
    pub fn new() -> Self {
        Self {
            heap_manager: HeapManager::new(HeapConfig::default()),
        }
    }

    pub fn initialize(&mut self, heap_start: usize) -> Result<(), &'static str> {
        self.heap_manager.initialize(heap_start)
    }

    pub fn get_stats(&self) -> &HeapStats {
        self.heap_manager.get_stats()
    }
}

unsafe impl GlobalAlloc for KernelHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // En una implementación real, esto necesitaría acceso mutable
        // Por ahora, devolvemos un puntero nulo
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // En una implementación real, esto liberaría la memoria
    }
}
