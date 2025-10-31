//! Sistema de memoria compartida para IPC en Eclipse OS
//! 
//! Este módulo implementa:
//! - Memoria compartida entre procesos
//! - Gestión de regiones de memoria compartida
//! - Sincronización de acceso
//! - Mapeo de memoria en espacio de usuario
//! - Protección y permisos

use core::ptr;
use crate::debug::serial_write_str;
use alloc::format;
use alloc::vec::Vec;
use alloc::string::ToString;
use crate::memory::paging::{allocate_physical_page, deallocate_physical_page, PAGE_SIZE};

/// Número máximo de regiones de memoria compartida
pub const MAX_SHARED_REGIONS: usize = 1024;

/// Tamaño máximo de una región de memoria compartida (64MB)
pub const MAX_SHARED_REGION_SIZE: usize = 64 * 1024 * 1024;

/// Permisos para memoria compartida
pub const SHARED_READ: u32 = 1 << 0;
pub const SHARED_WRITE: u32 = 1 << 1;
pub const SHARED_EXEC: u32 = 1 << 2;

/// Flags para memoria compartida
pub const SHARED_ANONYMOUS: u32 = 1 << 0; // Memoria anónima
pub const SHARED_NAMED: u32 = 1 << 1;     // Memoria con nombre
pub const SHARED_PRIVATE: u32 = 1 << 2;   // Memoria privada (Copy-on-Write)
pub const SHARED_SHARED: u32 = 1 << 3;    // Memoria compartida

/// Estado de una región de memoria compartida
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SharedRegionState {
    Free,
    Allocated,
    Mapped,
    Unmapped,
    Error,
}

/// Estructura para una región de memoria compartida
pub struct SharedMemoryRegion {
    /// ID único de la región
    pub id: u32,
    /// Nombre de la región (opcional)
    pub name: Option<alloc::string::String>,
    /// Dirección física base
    pub physical_base: u64,
    /// Tamaño de la región
    pub size: usize,
    /// Permisos de la región
    pub permissions: u32,
    /// Flags de la región
    pub flags: u32,
    /// Estado de la región
    pub state: SharedRegionState,
    /// Número de procesos que han mapeado esta región
    pub mapping_count: u32,
    /// Timestamp de creación
    pub created_at: u64,
    /// Timestamp de último acceso
    pub last_access: u64,
    /// Estadísticas de uso
    pub usage_stats: SharedRegionStats,
}

/// Estadísticas de uso de una región
#[derive(Debug, Clone, Copy)]
pub struct SharedRegionStats {
    /// Número de accesos de lectura
    pub read_accesses: u64,
    /// Número de accesos de escritura
    pub write_accesses: u64,
    /// Número de fallos de página
    pub page_faults: u64,
    /// Número de violaciones de permisos
    pub permission_violations: u64,
}

impl Default for SharedRegionStats {
    fn default() -> Self {
        Self {
            read_accesses: 0,
            write_accesses: 0,
            page_faults: 0,
            permission_violations: 0,
        }
    }
}

impl SharedMemoryRegion {
    /// Crear una nueva región de memoria compartida
    pub fn new(id: u32, size: usize, permissions: u32, flags: u32) -> Result<Self, &'static str> {
        if size == 0 || size > MAX_SHARED_REGION_SIZE {
            return Err("Tamaño de región inválido");
        }
        
        // Asignar páginas físicas para la región
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
                return Err("No hay suficientes páginas físicas para la región");
            }
        }
        
        Ok(Self {
            id,
            name: None,
            physical_base: physical_pages[0],
            size,
            permissions,
            flags,
            state: SharedRegionState::Allocated,
            mapping_count: 0,
            created_at: get_timestamp(),
            last_access: 0,
            usage_stats: SharedRegionStats::default(),
        })
    }
    
    /// Establecer el nombre de la región
    pub fn set_name(&mut self, name: alloc::string::String) {
        self.name = Some(name);
    }
    
    /// Obtener el nombre de la región
    pub fn get_name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    
    /// Verificar si la región tiene un permiso específico
    pub fn has_permission(&self, permission: u32) -> bool {
        (self.permissions & permission) != 0
    }
    
    /// Verificar si la región tiene un flag específico
    pub fn has_flag(&self, flag: u32) -> bool {
        (self.flags & flag) != 0
    }
    
    /// Incrementar el contador de mapeos
    pub fn increment_mapping_count(&mut self) {
        self.mapping_count += 1;
        self.last_access = get_timestamp();
    }
    
    /// Decrementar el contador de mapeos
    pub fn decrement_mapping_count(&mut self) {
        if self.mapping_count > 0 {
            self.mapping_count -= 1;
        }
    }
    
    /// Verificar si la región está mapeada
    pub fn is_mapped(&self) -> bool {
        self.mapping_count > 0
    }
    
    /// Mapear la región en el espacio de direcciones de un proceso
    pub fn map_to_process(&mut self, process_id: u32, virtual_addr: u64) -> Result<(), &'static str> {
        if self.state != SharedRegionState::Allocated {
            return Err("Región no está disponible para mapeo");
        }
        
        // Mapear las páginas físicas a la dirección virtual
        let pages_needed = (self.size + PAGE_SIZE - 1) / PAGE_SIZE;
        
        for i in 0..pages_needed {
            let virtual_page = virtual_addr + (i * PAGE_SIZE) as u64;
            let physical_page = self.physical_base + (i * PAGE_SIZE) as u64;
            
            let mut page_flags = 0x07; // Presente, escribible, usuario
            
            if !self.has_permission(SHARED_READ) {
                page_flags &= !0x01; // No presente
            }
            if !self.has_permission(SHARED_WRITE) {
                page_flags &= !0x02; // No escribible
            }
            if !self.has_permission(SHARED_EXEC) {
                page_flags |= 0x8000000000000000; // No ejecutable
            }
            
            crate::memory::paging::map_virtual_page(virtual_page, physical_page, page_flags)?;
        }
        
        self.increment_mapping_count();
        self.state = SharedRegionState::Mapped;
        
        Ok(())
    }
    
    /// Desmapear la región del espacio de direcciones de un proceso
    pub fn unmap_from_process(&mut self, process_id: u32, virtual_addr: u64) -> Result<(), &'static str> {
        if self.state != SharedRegionState::Mapped {
            return Err("Región no está mapeada");
        }
        
        // Desmapear las páginas virtuales
        let pages_needed = (self.size + PAGE_SIZE - 1) / PAGE_SIZE;
        
        for i in 0..pages_needed {
            let virtual_page = virtual_addr + (i * PAGE_SIZE) as u64;
            crate::memory::paging::unmap_virtual_page(virtual_page)?;
        }
        
        self.decrement_mapping_count();
        
        if self.mapping_count == 0 {
            self.state = SharedRegionState::Allocated;
        }
        
        Ok(())
    }
    
    /// Liberar la región
    pub fn free(&mut self) -> Result<(), &'static str> {
        if self.state == SharedRegionState::Mapped {
            return Err("No se puede liberar una región mapeada");
        }
        
        // Liberar las páginas físicas
        let pages_needed = (self.size + PAGE_SIZE - 1) / PAGE_SIZE;
        
        for i in 0..pages_needed {
            let physical_page = self.physical_base + (i * PAGE_SIZE) as u64;
            deallocate_physical_page(physical_page)?;
        }
        
        self.state = SharedRegionState::Free;
        Ok(())
    }
    
    /// Actualizar estadísticas de acceso
    pub fn update_access_stats(&mut self, access_type: u32) {
        self.last_access = get_timestamp();
        
        match access_type {
            SHARED_READ => self.usage_stats.read_accesses += 1,
            SHARED_WRITE => self.usage_stats.write_accesses += 1,
            _ => {}
        }
    }
    
    /// Registrar un fallo de página
    pub fn record_page_fault(&mut self) {
        self.usage_stats.page_faults += 1;
    }
    
    /// Registrar una violación de permisos
    pub fn record_permission_violation(&mut self) {
        self.usage_stats.permission_violations += 1;
    }
}

/// Gestor de memoria compartida
pub struct SharedMemoryManager {
    /// Regiones de memoria compartida
    regions: [Option<SharedMemoryRegion>; MAX_SHARED_REGIONS],
    /// Contador de IDs
    next_id: u32,
    /// Estadísticas globales
    global_stats: SharedMemoryStats,
}

/// Estadísticas globales de memoria compartida
#[derive(Debug, Clone, Copy)]
pub struct SharedMemoryStats {
    /// Número total de regiones creadas
    pub total_regions_created: u64,
    /// Número total de regiones liberadas
    pub total_regions_freed: u64,
    /// Número de regiones activas
    pub active_regions: u32,
    /// Memoria total asignada
    pub total_memory_allocated: u64,
    /// Número total de mapeos
    pub total_mappings: u64,
    /// Número total de desmapeos
    pub total_unmappings: u64,
}

impl Default for SharedMemoryStats {
    fn default() -> Self {
        Self {
            total_regions_created: 0,
            total_regions_freed: 0,
            active_regions: 0,
            total_memory_allocated: 0,
            total_mappings: 0,
            total_unmappings: 0,
        }
    }
}

impl SharedMemoryManager {
    /// Crear un nuevo gestor de memoria compartida
    pub fn new() -> Self {
        Self {
            regions: [const { None }; MAX_SHARED_REGIONS],
            next_id: 1,
            global_stats: SharedMemoryStats::default(),
        }
    }
    
    /// Crear una nueva región de memoria compartida
    pub fn create_region(&mut self, size: usize, permissions: u32, flags: u32) -> Result<u32, &'static str> {
        // Buscar un slot libre
        for i in 0..MAX_SHARED_REGIONS {
            if self.regions[i].is_none() {
                let region = SharedMemoryRegion::new(self.next_id, size, permissions, flags)?;
                self.regions[i] = Some(region);
                
                self.global_stats.total_regions_created += 1;
                self.global_stats.active_regions += 1;
                self.global_stats.total_memory_allocated += size as u64;
                
                let id = self.next_id;
                self.next_id += 1;
                
                return Ok(id);
            }
        }
        
        Err("No hay slots disponibles para regiones de memoria compartida")
    }
    
    /// Crear una región con nombre
    pub fn create_named_region(&mut self, name: &str, size: usize, permissions: u32, flags: u32) -> Result<u32, &'static str> {
        let id = self.create_region(size, permissions, flags)?;
        
        if let Some(region) = self.get_region_mut(id) {
            region.set_name(name.to_string());
        }
        
        Ok(id)
    }
    
    /// Liberar una región de memoria compartida
    pub fn free_region(&mut self, id: u32) -> Result<(), &'static str> {
        for i in 0..MAX_SHARED_REGIONS {
            if let Some(ref mut region) = self.regions[i] {
                if region.id == id {
                    let size = region.size;
                    region.free()?;
                    self.regions[i] = None;
                    
                    self.global_stats.total_regions_freed += 1;
                    self.global_stats.active_regions -= 1;
                    self.global_stats.total_memory_allocated -= size as u64;
                    
                    return Ok(());
                }
            }
        }
        
        Err("Región de memoria compartida no encontrada")
    }
    
    /// Obtener una región por ID
    pub fn get_region(&self, id: u32) -> Option<&SharedMemoryRegion> {
        for region in &self.regions {
            if let Some(ref r) = region {
                if r.id == id {
                    return Some(r);
                }
            }
        }
        None
    }
    
    /// Obtener una región mutable por ID
    pub fn get_region_mut(&mut self, id: u32) -> Option<&mut SharedMemoryRegion> {
        for region in &mut self.regions {
            if let Some(ref mut r) = region {
                if r.id == id {
                    return Some(r);
                }
            }
        }
        None
    }
    
    /// Buscar una región por nombre
    pub fn find_region_by_name(&self, name: &str) -> Option<&SharedMemoryRegion> {
        for region in &self.regions {
            if let Some(ref r) = region {
                if let Some(region_name) = r.get_name() {
                    if region_name == name {
                        return Some(r);
                    }
                }
            }
        }
        None
    }
    
    /// Mapear una región en un proceso
    pub fn map_region(&mut self, id: u32, process_id: u32, virtual_addr: u64) -> Result<(), &'static str> {
        if let Some(region) = self.get_region_mut(id) {
            region.map_to_process(process_id, virtual_addr)?;
            self.global_stats.total_mappings += 1;
            Ok(())
        } else {
            Err("Región de memoria compartida no encontrada")
        }
    }
    
    /// Desmapear una región de un proceso
    pub fn unmap_region(&mut self, id: u32, process_id: u32, virtual_addr: u64) -> Result<(), &'static str> {
        if let Some(region) = self.get_region_mut(id) {
            region.unmap_from_process(process_id, virtual_addr)?;
            self.global_stats.total_unmappings += 1;
            Ok(())
        } else {
            Err("Región de memoria compartida no encontrada")
        }
    }
    
    /// Obtener estadísticas globales
    pub fn get_global_stats(&self) -> SharedMemoryStats {
        self.global_stats
    }
    
    /// Limpiar regiones inactivas
    pub fn cleanup_inactive_regions(&mut self) {
        for i in 0..MAX_SHARED_REGIONS {
            if let Some(ref mut region) = self.regions[i] {
                if region.state == SharedRegionState::Allocated && !region.is_mapped() {
                    let current_time = get_timestamp();
                    // Liberar regiones no mapeadas hace más de 10 segundos
                    if current_time - region.last_access > 10000 {
                        let region_size = region.size;
                        if let Ok(_) = region.free() {
                            self.regions[i] = None;
                            self.global_stats.total_regions_freed += 1;
                            self.global_stats.active_regions -= 1;
                            self.global_stats.total_memory_allocated -= region_size as u64;
                        }
                    }
                }
            }
        }
    }
    
    /// Verificar la integridad del sistema
    pub fn verify_integrity(&self) -> bool {
        let mut active_count = 0;
        
        for region in &self.regions {
            if let Some(ref r) = region {
                if r.state != SharedRegionState::Free {
                    active_count += 1;
                }
                
                // Verificar que la región no esté corrupta
                if r.size == 0 || r.size > MAX_SHARED_REGION_SIZE {
                    return false;
                }
                
                if r.physical_base == 0 {
                    return false;
                }
            }
        }
        
        active_count == self.global_stats.active_regions
    }
}

/// Instancia global del gestor de memoria compartida
static mut SHARED_MEMORY_MANAGER: Option<SharedMemoryManager> = None;

/// Obtener timestamp actual (simulado)
fn get_timestamp() -> u64 {
    // En un sistema real, esto usaría un timer del sistema
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
}

/// Inicializar el sistema de memoria compartida
pub fn init_shared_memory() -> Result<(), &'static str> {
    serial_write_str("SHARED_MEMORY: Inicializando sistema de memoria compartida...\n");
    
    let manager = SharedMemoryManager::new();
    
    unsafe {
        SHARED_MEMORY_MANAGER = Some(manager);
    }
    
    serial_write_str("SHARED_MEMORY: Sistema de memoria compartida inicializado\n");
    Ok(())
}

/// Obtener el gestor de memoria compartida
fn get_shared_memory_manager() -> &'static mut SharedMemoryManager {
    unsafe {
        SHARED_MEMORY_MANAGER.as_mut().expect("Sistema de memoria compartida no inicializado")
    }
}

/// Crear una región de memoria compartida
pub fn shared_memory_create(size: usize, permissions: u32, flags: u32) -> Result<u32, &'static str> {
    let manager = get_shared_memory_manager();
    manager.create_region(size, permissions, flags)
}

/// Crear una región de memoria compartida con nombre
pub fn shared_memory_create_named(name: &str, size: usize, permissions: u32, flags: u32) -> Result<u32, &'static str> {
    let manager = get_shared_memory_manager();
    manager.create_named_region(name, size, permissions, flags)
}

/// Liberar una región de memoria compartida
pub fn shared_memory_free(id: u32) -> Result<(), &'static str> {
    let manager = get_shared_memory_manager();
    manager.free_region(id)
}

/// Obtener una región de memoria compartida
pub fn shared_memory_get(id: u32) -> Option<&'static SharedMemoryRegion> {
    let manager = get_shared_memory_manager();
    manager.get_region(id)
}

/// Buscar una región por nombre
pub fn shared_memory_find_by_name(name: &str) -> Option<&'static SharedMemoryRegion> {
    let manager = get_shared_memory_manager();
    manager.find_region_by_name(name)
}

/// Mapear una región en un proceso
pub fn shared_memory_map(id: u32, process_id: u32, virtual_addr: u64) -> Result<(), &'static str> {
    let manager = get_shared_memory_manager();
    manager.map_region(id, process_id, virtual_addr)
}

/// Desmapear una región de un proceso
pub fn shared_memory_unmap(id: u32, process_id: u32, virtual_addr: u64) -> Result<(), &'static str> {
    let manager = get_shared_memory_manager();
    manager.unmap_region(id, process_id, virtual_addr)
}

/// Obtener estadísticas globales de memoria compartida
pub fn get_shared_memory_stats() -> SharedMemoryStats {
    let manager = get_shared_memory_manager();
    manager.get_global_stats()
}

/// Limpiar regiones inactivas
pub fn shared_memory_cleanup() {
    let manager = get_shared_memory_manager();
    manager.cleanup_inactive_regions();
}

/// Verificar la integridad del sistema de memoria compartida
pub fn shared_memory_verify_integrity() -> bool {
    let manager = get_shared_memory_manager();
    manager.verify_integrity()
}

/// Imprimir estadísticas de memoria compartida
pub fn print_shared_memory_stats() {
    let stats = get_shared_memory_stats();
    
    serial_write_str("=== ESTADÍSTICAS DE MEMORIA COMPARTIDA ===\n");
    serial_write_str(&format!("Regiones creadas: {}\n", stats.total_regions_created));
    serial_write_str(&format!("Regiones liberadas: {}\n", stats.total_regions_freed));
    serial_write_str(&format!("Regiones activas: {}\n", stats.active_regions));
    serial_write_str(&format!("Memoria total asignada: {} KB\n", stats.total_memory_allocated / 1024));
    serial_write_str(&format!("Mapeos totales: {}\n", stats.total_mappings));
    serial_write_str(&format!("Desmapeos totales: {}\n", stats.total_unmappings));
    serial_write_str("==========================================\n");
}
