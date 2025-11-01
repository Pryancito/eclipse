//! Stack Allocator para procesos de Eclipse OS
//!
//! Este módulo maneja la asignación y liberación de stacks
//! para cada proceso, permitiendo multitasking seguro.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use crate::debug::serial_write_str;

/// Tamaño de stack por proceso (64KB)
pub const PROCESS_STACK_SIZE: usize = 65536; // 64KB

/// Alineamiento del stack (16 bytes para x86-64)
pub const STACK_ALIGNMENT: usize = 16;

/// Información de un stack asignado
#[derive(Debug, Clone, Copy)]
pub struct StackInfo {
    /// Dirección base del stack (bottom)
    pub base: u64,
    /// Dirección top del stack (donde empieza RSP)
    pub top: u64,
    /// Tamaño del stack
    pub size: usize,
    /// PID del proceso dueño
    pub owner_pid: u32,
}

impl StackInfo {
    /// Crear nueva info de stack
    pub fn new(base: u64, size: usize, owner_pid: u32) -> Self {
        Self {
            base,
            top: base + size as u64,
            size,
            owner_pid,
        }
    }

    /// Verificar si una dirección está dentro del stack
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.top
    }

    /// Obtener espacio usado
    pub fn get_used_space(&self, current_rsp: u64) -> usize {
        if current_rsp >= self.base && current_rsp <= self.top {
            (self.top - current_rsp) as usize
        } else {
            0
        }
    }
}

/// Allocator de stacks
pub struct StackAllocator {
    /// Stacks asignados
    allocated_stacks: Vec<StackInfo>,
    /// Contador de stacks asignados
    total_allocated: AtomicU64,
    /// Contador de stacks liberados
    total_freed: AtomicU64,
}

impl StackAllocator {
    /// Crear nuevo allocator de stacks
    pub const fn new() -> Self {
        Self {
            allocated_stacks: Vec::new(),
            total_allocated: AtomicU64::new(0),
            total_freed: AtomicU64::new(0),
        }
    }

    /// Asignar un nuevo stack para un proceso
    pub fn allocate(&mut self, pid: u32) -> Result<StackInfo, &'static str> {
        serial_write_str(&alloc::format!(
            "STACK_ALLOC: Asignando stack de {}KB para proceso {}\n",
            PROCESS_STACK_SIZE / 1024,
            pid
        ));

        // Asignar memoria usando el kernel allocator
        let layout = core::alloc::Layout::from_size_align(
            PROCESS_STACK_SIZE,
            STACK_ALIGNMENT
        ).map_err(|_| "Failed to create layout for stack")?;

        let stack_ptr = unsafe {
            alloc::alloc::alloc_zeroed(layout)
        };

        if stack_ptr.is_null() {
            return Err("Failed to allocate stack memory");
        }

        let stack_base = stack_ptr as u64;
        let stack_info = StackInfo::new(stack_base, PROCESS_STACK_SIZE, pid);

        // Guardar info del stack
        self.allocated_stacks.push(stack_info);
        self.total_allocated.fetch_add(1, Ordering::Relaxed);

        serial_write_str(&alloc::format!(
            "STACK_ALLOC: Stack asignado en 0x{:016x} - 0x{:016x} ({} bytes)\n",
            stack_info.base,
            stack_info.top,
            stack_info.size
        ));

        Ok(stack_info)
    }

    /// Liberar un stack
    pub fn deallocate(&mut self, pid: u32) -> Result<(), &'static str> {
        // Buscar el stack del proceso
        let index = self.allocated_stacks
            .iter()
            .position(|s| s.owner_pid == pid)
            .ok_or("Stack not found for process")?;

        let stack_info = self.allocated_stacks[index];

        serial_write_str(&alloc::format!(
            "STACK_ALLOC: Liberando stack de proceso {} (0x{:016x})\n",
            pid,
            stack_info.base
        ));

        // Liberar memoria
        let layout = core::alloc::Layout::from_size_align(
            PROCESS_STACK_SIZE,
            STACK_ALIGNMENT
        ).map_err(|_| "Failed to create layout for deallocation")?;

        unsafe {
            alloc::alloc::dealloc(stack_info.base as *mut u8, layout);
        }

        // Remover de la lista
        self.allocated_stacks.remove(index);
        self.total_freed.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Obtener info del stack de un proceso
    pub fn get_stack_info(&self, pid: u32) -> Option<StackInfo> {
        self.allocated_stacks
            .iter()
            .find(|s| s.owner_pid == pid)
            .copied()
    }

    /// Verificar si un proceso tiene stack asignado
    pub fn has_stack(&self, pid: u32) -> bool {
        self.allocated_stacks.iter().any(|s| s.owner_pid == pid)
    }

    /// Obtener estadísticas
    pub fn get_stats(&self) -> StackAllocatorStats {
        StackAllocatorStats {
            total_allocated: self.total_allocated.load(Ordering::Relaxed),
            total_freed: self.total_freed.load(Ordering::Relaxed),
            currently_allocated: self.allocated_stacks.len() as u64,
            total_memory_used: (self.allocated_stacks.len() * PROCESS_STACK_SIZE) as u64,
        }
    }
}

/// Estadísticas del allocator de stacks
#[derive(Debug, Clone, Copy)]
pub struct StackAllocatorStats {
    pub total_allocated: u64,
    pub total_freed: u64,
    pub currently_allocated: u64,
    pub total_memory_used: u64,
}

/// Allocator global de stacks
static STACK_ALLOCATOR: spin::Mutex<StackAllocator> = spin::Mutex::new(StackAllocator::new());

/// Asignar stack para un proceso
pub fn allocate_process_stack(pid: u32) -> Result<StackInfo, &'static str> {
    let mut allocator = STACK_ALLOCATOR.lock();
    allocator.allocate(pid)
}

/// Liberar stack de un proceso
pub fn deallocate_process_stack(pid: u32) -> Result<(), &'static str> {
    let mut allocator = STACK_ALLOCATOR.lock();
    allocator.deallocate(pid)
}

/// Obtener info del stack de un proceso
pub fn get_process_stack_info(pid: u32) -> Option<StackInfo> {
    let allocator = STACK_ALLOCATOR.lock();
    allocator.get_stack_info(pid)
}

/// Obtener estadísticas de stacks
pub fn get_stack_stats() -> StackAllocatorStats {
    let allocator = STACK_ALLOCATOR.lock();
    allocator.get_stats()
}

