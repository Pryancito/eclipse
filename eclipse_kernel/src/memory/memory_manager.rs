//! Gestor principal de memoria para Eclipse OS
//! 
//! Coordina todos los subsistemas de memoria

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use super::{
    MemoryInfo, MemoryStats, MemoryConfig, MemoryManager,
    heap::{HeapManager, HeapConfig, KernelHeapAllocator},
    virtual_memory::{VirtualMemoryManager, VirtualMemoryConfig},
};
use crate::paging::PagingManager;

/// Configuración completa del sistema de memoria
#[derive(Debug, Clone)]
pub struct CompleteMemoryConfig {
    pub memory_config: MemoryConfig,
    pub heap_config: HeapConfig,
    pub virtual_memory_config: VirtualMemoryConfig,
    pub enable_debugging: bool,
    pub enable_memory_protection: bool,
}

impl Default for CompleteMemoryConfig {
    fn default() -> Self {
        Self {
            memory_config: MemoryConfig::default(),
            heap_config: HeapConfig::default(),
            virtual_memory_config: VirtualMemoryConfig::default(),
            enable_debugging: true,
            enable_memory_protection: true,
        }
    }
}

/// Estadísticas completas del sistema de memoria
#[derive(Debug, Clone)]
pub struct CompleteMemoryStats {
    pub memory_info: MemoryInfo,
    pub memory_stats: MemoryStats,
    pub heap_stats: super::heap::HeapStats,
    pub virtual_memory_stats: (usize, usize, usize, usize), // allocated, free, total, swapped
    pub fragmentation: f32,
}

/// Gestor principal de memoria del sistema
pub struct SystemMemoryManager {
    memory_manager: MemoryManager,
    heap_manager: HeapManager,
    virtual_memory_manager: VirtualMemoryManager,
    config: CompleteMemoryConfig,
    initialized: bool,
}

impl SystemMemoryManager {
    pub fn new(config: CompleteMemoryConfig) -> Self {
        Self {
            memory_manager: MemoryManager::new(config.memory_config.clone()),
            heap_manager: HeapManager::new(config.heap_config.clone()),
            virtual_memory_manager: VirtualMemoryManager::new(
                config.virtual_memory_config.clone(),
                config.memory_config.heap_size,
            ),
            config,
            initialized: false,
        }
    }

    pub fn initialize(&mut self, heap_start: usize) -> Result<(), &'static str> {
        if self.initialized {
            return Err("System memory manager already initialized");
        }

        // Inicializar gestor de memoria base
        self.memory_manager.initialize()?;

        // Inicializar heap
        self.heap_manager.initialize(heap_start)?;

        // Inicializar memoria virtual
        self.virtual_memory_manager.initialize()?;

        self.initialized = true;
        Ok(())
    }

    pub fn allocate_memory(&mut self, size: usize, permission: crate::paging::PagePermission) -> Result<usize, &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        // Intentar asignar desde memoria virtual primero
        self.virtual_memory_manager.allocate_memory(size, permission)
    }

    pub fn deallocate_memory(&mut self, virtual_addr: usize) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        self.virtual_memory_manager.deallocate_memory(virtual_addr)
    }

    pub fn map_memory(&mut self, virtual_addr: usize, physical_addr: usize, size: usize, permission: crate::paging::PagePermission) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        self.virtual_memory_manager.map_memory(virtual_addr, physical_addr, size, permission)
    }

    pub fn unmap_memory(&mut self, virtual_addr: usize) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        self.virtual_memory_manager.unmap_memory(virtual_addr)
    }

    pub fn translate_address(&self, virtual_addr: usize) -> Option<usize> {
        self.virtual_memory_manager.translate_address(virtual_addr)
    }

    pub fn get_complete_stats(&self) -> CompleteMemoryStats {
        CompleteMemoryStats {
            memory_info: self.memory_manager.get_info().clone(),
            memory_stats: self.memory_manager.get_stats().clone(),
            heap_stats: self.heap_manager.get_stats().clone(),
            virtual_memory_stats: self.virtual_memory_manager.get_memory_stats(),
            fragmentation: self.heap_manager.get_fragmentation(),
        }
    }

    pub fn get_memory_usage_percentage(&self) -> f32 {
        self.memory_manager.get_memory_usage_percentage()
    }

    pub fn get_heap_usage_percentage(&self) -> f32 {
        self.heap_manager.get_usage_percentage()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn perform_garbage_collection(&mut self) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        // En un sistema real, aquí se implementaría garbage collection
        // Por ahora, solo actualizamos estadísticas
        Ok(())
    }

    pub fn defragment_memory(&mut self) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        // En un sistema real, aquí se implementaría defragmentación
        // Por ahora, solo actualizamos estadísticas
        Ok(())
    }

    pub fn enable_memory_protection(&mut self) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        if !self.config.enable_memory_protection {
            return Err("Memory protection not enabled in config");
        }

        // En un sistema real, aquí se habilitaría la protección de memoria
        Ok(())
    }

    pub fn disable_memory_protection(&mut self) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("System memory manager not initialized");
        }

        // En un sistema real, aquí se deshabilitaría la protección de memoria
        Ok(())
    }
}

/// Allocator global del sistema
pub struct SystemAllocator {
    memory_manager: SystemMemoryManager,
}

impl SystemAllocator {
    pub fn new() -> Self {
        Self {
            memory_manager: SystemMemoryManager::new(CompleteMemoryConfig::default()),
        }
    }

    pub fn initialize(&mut self, heap_start: usize) -> Result<(), &'static str> {
        self.memory_manager.initialize(heap_start)
    }

    pub fn get_stats(&self) -> CompleteMemoryStats {
        self.memory_manager.get_complete_stats()
    }
}

unsafe impl GlobalAlloc for SystemAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // En una implementación real, esto asignaría memoria real
        // Por ahora, devolvemos un puntero nulo
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // En una implementación real, esto liberaría memoria real
    }
}

// #[global_allocator]
// static SYSTEM_ALLOCATOR: SystemAllocator = SystemAllocator::new();
