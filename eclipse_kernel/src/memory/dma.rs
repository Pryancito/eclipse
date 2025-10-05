//! Sistema DMA (Direct Memory Access) para Eclipse OS
//! 
//! Este módulo implementa:
//! - Gestión de buffers DMA
//! - Mapeo de memoria para dispositivos
//! - Transferencias DMA
//! - Coherencia de caché
//! - Estadísticas de DMA

use core::ptr;
use crate::debug::serial_write_str;
use alloc::format;
use alloc::vec::Vec;
use crate::memory::paging::{allocate_physical_page, deallocate_physical_page, PAGE_SIZE};

/// Tamaño máximo de buffer DMA (1MB)
pub const MAX_DMA_BUFFER_SIZE: usize = 1024 * 1024;

/// Número máximo de buffers DMA
pub const MAX_DMA_BUFFERS: usize = 256;

/// Flags para buffers DMA
pub const DMA_READ: u32 = 1 << 0;  // Dispositivo lee de memoria
pub const DMA_WRITE: u32 = 1 << 1; // Dispositivo escribe a memoria
pub const DMA_COHERENT: u32 = 1 << 2; // Memoria coherente con caché
pub const DMA_STREAMING: u32 = 1 << 3; // Transferencia streaming
pub const DMA_CYCLIC: u32 = 1 << 4; // Transferencia cíclica

/// Estado de un buffer DMA
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DmaBufferState {
    Free,
    Allocated,
    InUse,
    Completed,
    Error,
}

/// Estructura para un buffer DMA
pub struct DmaBuffer {
    /// ID único del buffer
    pub id: u32,
    /// Dirección física del buffer
    pub physical_addr: u64,
    /// Dirección virtual del buffer
    pub virtual_addr: *mut u8,
    /// Tamaño del buffer
    pub size: usize,
    /// Flags del buffer
    pub flags: u32,
    /// Estado del buffer
    pub state: DmaBufferState,
    /// Dispositivo que usa el buffer
    pub device_id: u32,
    /// Timestamp de creación
    pub created_at: u64,
    /// Timestamp de última transferencia
    pub last_transfer: u64,
    /// Número de transferencias completadas
    pub transfer_count: u64,
    /// Número de transferencias fallidas
    pub failed_transfers: u64,
}

impl DmaBuffer {
    /// Crear un nuevo buffer DMA
    pub fn new(id: u32, size: usize, flags: u32) -> Result<Self, &'static str> {
        if size == 0 || size > MAX_DMA_BUFFER_SIZE {
            return Err("Tamaño de buffer DMA inválido");
        }
        
        // Asignar páginas físicas para el buffer
        let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let mut physical_pages = Vec::new();
        
        for _ in 0..pages_needed {
            if let Some(physical_addr) = allocate_physical_page() {
                physical_pages.push(physical_addr);
            } else {
                // Liberar páginas ya asignadas
                for page in physical_pages {
                    let _ = deallocate_physical_page(page);
                }
                return Err("No hay suficientes páginas físicas para el buffer DMA");
            }
        }
        
        // Mapear las páginas virtualmente
        let virtual_base = 0xFFFF_8000_1000_0000; // Zona virtual para DMA
        let virtual_addr = virtual_base as *mut u8;
        
        for (i, physical_addr) in physical_pages.iter().enumerate() {
            let virtual_page = virtual_base + (i * PAGE_SIZE) as u64;
            crate::memory::paging::map_virtual_page(virtual_page, *physical_addr, 0x07)?;
        }
        
        Ok(Self {
            id,
            physical_addr: physical_pages[0],
            virtual_addr,
            size,
            flags,
            state: DmaBufferState::Allocated,
            device_id: 0,
            created_at: get_timestamp(),
            last_transfer: 0,
            transfer_count: 0,
            failed_transfers: 0,
        })
    }
    
    /// Obtener la dirección física del buffer
    pub fn get_physical_addr(&self) -> u64 {
        self.physical_addr
    }
    
    /// Obtener la dirección virtual del buffer
    pub fn get_virtual_addr(&self) -> *mut u8 {
        self.virtual_addr
    }
    
    /// Obtener el tamaño del buffer
    pub fn get_size(&self) -> usize {
        self.size
    }
    
    /// Verificar si el buffer está libre
    pub fn is_free(&self) -> bool {
        self.state == DmaBufferState::Free
    }
    
    /// Verificar si el buffer está en uso
    pub fn is_in_use(&self) -> bool {
        self.state == DmaBufferState::InUse
    }
    
    /// Marcar el buffer como en uso
    pub fn mark_in_use(&mut self, device_id: u32) {
        self.state = DmaBufferState::InUse;
        self.device_id = device_id;
    }
    
    /// Marcar el buffer como completado
    pub fn mark_completed(&mut self) {
        self.state = DmaBufferState::Completed;
        self.last_transfer = get_timestamp();
        self.transfer_count += 1;
    }
    
    /// Marcar el buffer como error
    pub fn mark_error(&mut self) {
        self.state = DmaBufferState::Error;
        self.failed_transfers += 1;
    }
    
    /// Liberar el buffer
    pub fn free(&mut self) {
        self.state = DmaBufferState::Free;
        self.device_id = 0;
    }
    
    /// Invalidar caché para el buffer
    pub fn invalidate_cache(&self) {
        if self.flags & DMA_COHERENT == 0 {
            // Invalidar caché para memoria no coherente
            unsafe {
                core::arch::asm!(
                    "clflush [{}]",
                    in(reg) self.virtual_addr,
                    options(nostack)
                );
            }
        }
    }
    
    /// Limpiar caché para el buffer
    pub fn clean_cache(&self) {
        if self.flags & DMA_COHERENT == 0 {
            // Limpiar caché para memoria no coherente
            unsafe {
                core::arch::asm!(
                    "clflush [{}]",
                    in(reg) self.virtual_addr,
                    options(nostack)
                );
            }
        }
    }
}

/// Gestor de buffers DMA
pub struct DmaManager {
    /// Buffers DMA disponibles
    buffers: [Option<DmaBuffer>; MAX_DMA_BUFFERS],
    /// Contador de IDs
    next_id: u32,
    /// Estadísticas de DMA
    stats: DmaStats,
}

/// Estadísticas de DMA
#[derive(Debug, Clone, Copy)]
pub struct DmaStats {
    /// Número de buffers DMA activos
    pub active_buffers: u32,
    /// Memoria total usada por DMA
    pub total_dma_memory: u64,
    /// Número de transferencias completadas
    pub completed_transfers: u64,
    /// Número de transferencias fallidas
    pub failed_transfers: u64,
    /// Número total de buffers creados
    pub total_buffers_created: u64,
    /// Número total de buffers liberados
    pub total_buffers_freed: u64,
}

impl Default for DmaStats {
    fn default() -> Self {
        Self {
            active_buffers: 0,
            total_dma_memory: 0,
            completed_transfers: 0,
            failed_transfers: 0,
            total_buffers_created: 0,
            total_buffers_freed: 0,
        }
    }
}

impl DmaManager {
    /// Crear un nuevo gestor DMA
    pub fn new() -> Self {
        Self {
            buffers: [const { None }; MAX_DMA_BUFFERS],
            next_id: 1,
            stats: DmaStats::default(),
        }
    }
    
    /// Asignar un buffer DMA
    pub fn allocate_buffer(&mut self, size: usize, flags: u32) -> Result<u32, &'static str> {
        // Buscar un slot libre
        for i in 0..MAX_DMA_BUFFERS {
            if self.buffers[i].is_none() {
                let buffer = DmaBuffer::new(self.next_id, size, flags)?;
                self.buffers[i] = Some(buffer);
                
                self.stats.active_buffers += 1;
                self.stats.total_dma_memory += size as u64;
                self.stats.total_buffers_created += 1;
                
                let id = self.next_id;
                self.next_id += 1;
                
                return Ok(id);
            }
        }
        
        Err("No hay slots disponibles para buffers DMA")
    }
    
    /// Liberar un buffer DMA
    pub fn free_buffer(&mut self, id: u32) -> Result<(), &'static str> {
        for i in 0..MAX_DMA_BUFFERS {
            if let Some(buffer) = &mut self.buffers[i] {
                if buffer.id == id {
                    let size = buffer.size;
                    self.buffers[i] = None;
                    
                    self.stats.active_buffers -= 1;
                    self.stats.total_dma_memory -= size as u64;
                    self.stats.total_buffers_freed += 1;
                    
                    return Ok(());
                }
            }
        }
        
        Err("Buffer DMA no encontrado")
    }
    
    /// Obtener un buffer DMA por ID
    pub fn get_buffer(&mut self, id: u32) -> Option<&mut DmaBuffer> {
        for buffer in &mut self.buffers {
            if let Some(ref mut b) = buffer {
                if b.id == id {
                    return Some(b);
                }
            }
        }
        None
    }
    
    /// Obtener la dirección física de un buffer
    pub fn get_buffer_physical_addr(&self, id: u32) -> Option<u64> {
        for buffer in &self.buffers {
            if let Some(ref b) = buffer {
                if b.id == id {
                    return Some(b.physical_addr);
                }
            }
        }
        None
    }
    
    /// Obtener la dirección virtual de un buffer
    pub fn get_buffer_virtual_addr(&self, id: u32) -> Option<*mut u8> {
        for buffer in &self.buffers {
            if let Some(ref b) = buffer {
                if b.id == id {
                    return Some(b.virtual_addr);
                }
            }
        }
        None
    }
    
    /// Iniciar una transferencia DMA
    pub fn start_transfer(&mut self, id: u32, device_id: u32) -> Result<(), &'static str> {
        if let Some(buffer) = self.get_buffer(id) {
            if buffer.state == DmaBufferState::Allocated || buffer.state == DmaBufferState::Completed {
                buffer.mark_in_use(device_id);
                
                // Invalidar caché si es necesario
                if buffer.flags & DMA_READ != 0 {
                    buffer.invalidate_cache();
                }
                
                Ok(())
            } else {
                Err("Buffer no está disponible para transferencia")
            }
        } else {
            Err("Buffer DMA no encontrado")
        }
    }
    
    /// Completar una transferencia DMA
    pub fn complete_transfer(&mut self, id: u32) -> Result<(), &'static str> {
        if let Some(buffer) = self.get_buffer(id) {
            if buffer.state == DmaBufferState::InUse {
                buffer.mark_completed();
                
                // Limpiar caché si es necesario
                if buffer.flags & DMA_WRITE != 0 {
                    buffer.clean_cache();
                }
                
                self.stats.completed_transfers += 1;
                Ok(())
            } else {
                Err("Buffer no está en uso")
            }
        } else {
            Err("Buffer DMA no encontrado")
        }
    }
    
    /// Marcar una transferencia DMA como fallida
    pub fn fail_transfer(&mut self, id: u32) -> Result<(), &'static str> {
        if let Some(buffer) = self.get_buffer(id) {
            if buffer.state == DmaBufferState::InUse {
                buffer.mark_error();
                self.stats.failed_transfers += 1;
                Ok(())
            } else {
                Err("Buffer no está en uso")
            }
        } else {
            Err("Buffer DMA no encontrado")
        }
    }
    
    /// Obtener estadísticas de DMA
    pub fn get_stats(&self) -> DmaStats {
        self.stats
    }
    
    /// Limpiar buffers inactivos
    pub fn cleanup_inactive_buffers(&mut self) {
        for i in 0..MAX_DMA_BUFFERS {
            if let Some(buffer) = &mut self.buffers[i] {
                if buffer.state == DmaBufferState::Completed {
                    let current_time = get_timestamp();
                    // Liberar buffers completados hace más de 1 segundo
                    if current_time - buffer.last_transfer > 1000 {
                        buffer.free();
                    }
                }
            }
        }
    }
    
    /// Verificar la integridad de los buffers
    pub fn verify_integrity(&self) -> bool {
        let mut active_count = 0;
        
        for buffer in &self.buffers {
            if let Some(ref b) = buffer {
                if b.state != DmaBufferState::Free {
                    active_count += 1;
                }
                
                // Verificar que el buffer no esté corrupto
                if b.size == 0 || b.size > MAX_DMA_BUFFER_SIZE {
                    return false;
                }
                
                if b.virtual_addr.is_null() {
                    return false;
                }
            }
        }
        
        active_count == self.stats.active_buffers
    }
}

/// Instancia global del gestor DMA
static mut DMA_MANAGER: Option<DmaManager> = None;

/// Obtener timestamp actual (simulado)
fn get_timestamp() -> u64 {
    // En un sistema real, esto usaría un timer del sistema
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
}

/// Inicializar el sistema DMA
pub fn init_dma() -> Result<(), &'static str> {
    serial_write_str("DMA: Inicializando sistema DMA...\n");
    
    let manager = DmaManager::new();
    
    unsafe {
        DMA_MANAGER = Some(manager);
    }
    
    serial_write_str("DMA: Sistema DMA inicializado\n");
    Ok(())
}

/// Obtener el gestor DMA
fn get_dma_manager() -> &'static mut DmaManager {
    unsafe {
        DMA_MANAGER.as_mut().expect("Sistema DMA no inicializado")
    }
}

/// Asignar un buffer DMA
pub fn dma_allocate_buffer(size: usize, flags: u32) -> Result<u32, &'static str> {
    let manager = get_dma_manager();
    manager.allocate_buffer(size, flags)
}

/// Liberar un buffer DMA
pub fn dma_free_buffer(id: u32) -> Result<(), &'static str> {
    let manager = get_dma_manager();
    manager.free_buffer(id)
}

/// Obtener la dirección física de un buffer DMA
pub fn dma_get_physical_addr(id: u32) -> Option<u64> {
    let manager = get_dma_manager();
    manager.get_buffer_physical_addr(id)
}

/// Obtener la dirección virtual de un buffer DMA
pub fn dma_get_virtual_addr(id: u32) -> Option<*mut u8> {
    let manager = get_dma_manager();
    manager.get_buffer_virtual_addr(id)
}

/// Iniciar una transferencia DMA
pub fn dma_start_transfer(id: u32, device_id: u32) -> Result<(), &'static str> {
    let manager = get_dma_manager();
    manager.start_transfer(id, device_id)
}

/// Completar una transferencia DMA
pub fn dma_complete_transfer(id: u32) -> Result<(), &'static str> {
    let manager = get_dma_manager();
    manager.complete_transfer(id)
}

/// Marcar una transferencia DMA como fallida
pub fn dma_fail_transfer(id: u32) -> Result<(), &'static str> {
    let manager = get_dma_manager();
    manager.fail_transfer(id)
}

/// Obtener estadísticas de DMA
pub fn get_dma_stats() -> DmaStats {
    let manager = get_dma_manager();
    manager.get_stats()
}

/// Limpiar buffers DMA inactivos
pub fn dma_cleanup() {
    let manager = get_dma_manager();
    manager.cleanup_inactive_buffers();
}

/// Verificar la integridad del sistema DMA
pub fn dma_verify_integrity() -> bool {
    let manager = get_dma_manager();
    manager.verify_integrity()
}

/// Imprimir estadísticas de DMA
pub fn print_dma_stats() {
    let stats = get_dma_stats();
    
    serial_write_str("=== ESTADÍSTICAS DE DMA ===\n");
    serial_write_str(&format!("Buffers DMA activos: {}\n", stats.active_buffers));
    serial_write_str(&format!("Memoria DMA total: {} KB\n", stats.total_dma_memory / 1024));
    serial_write_str(&format!("Transferencias completadas: {}\n", stats.completed_transfers));
    serial_write_str(&format!("Transferencias fallidas: {}\n", stats.failed_transfers));
    serial_write_str(&format!("Buffers creados: {}\n", stats.total_buffers_created));
    serial_write_str(&format!("Buffers liberados: {}\n", stats.total_buffers_freed));
    serial_write_str("============================\n");
}

/// Función de utilidad para copiar datos usando DMA
pub fn dma_copy_data(src_id: u32, dst_id: u32, size: usize) -> Result<(), &'static str> {
    let src_addr = dma_get_virtual_addr(src_id).ok_or("Buffer fuente no encontrado")?;
    let dst_addr = dma_get_virtual_addr(dst_id).ok_or("Buffer destino no encontrado")?;
    
    if src_addr.is_null() || dst_addr.is_null() {
        return Err("Direcciones de buffer inválidas");
    }
    
    unsafe {
        core::ptr::copy_nonoverlapping(src_addr, dst_addr, size);
    }
    
    Ok(())
}

/// Función de utilidad para llenar un buffer DMA
pub fn dma_fill_buffer(id: u32, value: u8, size: usize) -> Result<(), &'static str> {
    let addr = dma_get_virtual_addr(id).ok_or("Buffer no encontrado")?;
    
    if addr.is_null() {
        return Err("Dirección de buffer inválida");
    }
    
    unsafe {
        core::ptr::write_bytes(addr, value, size);
    }
    
    Ok(())
}
