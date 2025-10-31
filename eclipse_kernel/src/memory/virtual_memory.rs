//! Gestor de memoria virtual para Eclipse OS
//! 
//! Este módulo implementa:
//! - Gestión de espacio de direcciones virtuales
//! - Mapeo de memoria en procesos
//! - Protección de memoria
//! - Gestión de regiones de memoria
//! - Estadísticas de uso de memoria virtual

use core::ptr;
use crate::debug::serial_write_str;
use alloc::format;
use crate::memory::paging::{allocate_physical_page, deallocate_physical_page, PAGE_SIZE};

/// Número máximo de regiones de memoria virtual por proceso
pub const MAX_VIRTUAL_REGIONS: usize = 256;

/// Tamaño máximo de una región de memoria virtual (1GB)
pub const MAX_VIRTUAL_REGION_SIZE: usize = 1024 * 1024 * 1024;

/// Direcciones virtuales del kernel
pub const KERNEL_BASE: u64 = 0xFFFF_8000_0000_0000;
pub const KERNEL_LIMIT: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// Direcciones virtuales del usuario
pub const USER_BASE: u64 = 0x0000_0000_0000_0000;
pub const USER_LIMIT: u64 = 0x0000_7FFF_FFFF_FFFF;

/// Direcciones virtuales para memoria compartida
pub const SHARED_BASE: u64 = 0x0000_8000_0000_0000;
pub const SHARED_LIMIT: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Tipos de región de memoria virtual
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VirtualRegionType {
    Code,        // Código ejecutable
    Data,        // Datos
    Heap,        // Heap dinámico
    Stack,       // Pila
    Shared,      // Memoria compartida
    Mapped,      // Archivo mapeado
    Anonymous,   // Memoria anónima
    Device,      // Mapeo de dispositivo
}

/// Permisos de región de memoria virtual
pub const VIRTUAL_READ: u32 = 1 << 0;
pub const VIRTUAL_WRITE: u32 = 1 << 1;
pub const VIRTUAL_EXEC: u32 = 1 << 2;
pub const VIRTUAL_USER: u32 = 1 << 3;

/// Flags de región de memoria virtual
pub const VIRTUAL_PRIVATE: u32 = 1 << 0;  // Memoria privada
pub const VIRTUAL_SHARED: u32 = 1 << 1;   // Memoria compartida
pub const VIRTUAL_FIXED: u32 = 1 << 2;    // Dirección fija
pub const VIRTUAL_GROWSUP: u32 = 1 << 3;  // Crece hacia arriba (pila)
pub const VIRTUAL_GROWSDOWN: u32 = 1 << 4; // Crece hacia abajo (heap)

/// Estructura para una región de memoria virtual
pub struct VirtualMemoryRegion {
    /// ID único de la región
    pub id: u32,
    /// Dirección virtual base
    pub virtual_base: u64,
    /// Tamaño de la región
    pub size: usize,
    /// Tipo de región
    pub region_type: VirtualRegionType,
    /// Permisos de la región
    pub permissions: u32,
    /// Flags de la región
    pub flags: u32,
    /// Si la región está activa
    pub is_active: bool,
    /// Timestamp de creación
    pub created_at: u64,
    /// Timestamp de último acceso
    pub last_access: u64,
    /// Estadísticas de uso
    pub usage_stats: VirtualRegionStats,
}

/// Estadísticas de uso de una región virtual
#[derive(Debug, Clone, Copy)]
pub struct VirtualRegionStats {
    /// Número de accesos de lectura
    pub read_accesses: u64,
    /// Número de accesos de escritura
    pub write_accesses: u64,
    /// Número de accesos de ejecución
    pub exec_accesses: u64,
    /// Número de fallos de página
    pub page_faults: u64,
    /// Número de violaciones de permisos
    pub permission_violations: u64,
    /// Número de accesos a memoria no mapeada
    pub unmapped_accesses: u64,
}

impl Default for VirtualRegionStats {
    fn default() -> Self {
        Self {
            read_accesses: 0,
            write_accesses: 0,
            exec_accesses: 0,
            page_faults: 0,
            permission_violations: 0,
            unmapped_accesses: 0,
        }
    }
}

impl VirtualMemoryRegion {
    /// Crear una nueva región de memoria virtual
    pub fn new(
        id: u32,
        virtual_base: u64,
        size: usize,
        region_type: VirtualRegionType,
        permissions: u32,
        flags: u32,
    ) -> Result<Self, &'static str> {
        if size == 0 || size > MAX_VIRTUAL_REGION_SIZE {
            return Err("Tamaño de región inválido");
        }
        
        // Verificar que la dirección virtual esté en el rango válido
        if !Self::is_valid_virtual_address(virtual_base) {
            return Err("Dirección virtual inválida");
        }
        
        // Verificar que la región no se desborde
        if virtual_base.checked_add(size as u64).is_none() {
            return Err("Región se desborda del espacio de direcciones");
        }
        
        Ok(Self {
            id,
            virtual_base,
            size,
            region_type,
            permissions,
            flags,
            is_active: false,
            created_at: get_timestamp(),
            last_access: 0,
            usage_stats: VirtualRegionStats::default(),
        })
    }
    
    /// Verificar si una dirección virtual es válida
    fn is_valid_virtual_address(addr: u64) -> bool {
        // Verificar que no esté en el rango del kernel
        if addr >= KERNEL_BASE {
            return false;
        }
        
        // Verificar que esté en el rango del usuario
        if addr < USER_BASE || addr > USER_LIMIT {
            return false;
        }
        
        true
    }
    
    /// Verificar si la región contiene una dirección
    pub fn contains_address(&self, addr: u64) -> bool {
        addr >= self.virtual_base && addr < self.virtual_base + self.size as u64
    }
    
    /// Verificar si la región se solapa con otra
    pub fn overlaps_with(&self, other: &VirtualMemoryRegion) -> bool {
        let self_end = self.virtual_base + self.size as u64;
        let other_end = other.virtual_base + other.size as u64;
        
        self.virtual_base < other_end && other.virtual_base < self_end
    }
    
    /// Verificar si la región tiene un permiso específico
    pub fn has_permission(&self, permission: u32) -> bool {
        (self.permissions & permission) != 0
    }
    
    /// Verificar si la región tiene un flag específico
    pub fn has_flag(&self, flag: u32) -> bool {
        (self.flags & flag) != 0
    }
    
    /// Activar la región
    pub fn activate(&mut self) -> Result<(), &'static str> {
        if self.is_active {
            return Err("Región ya está activa");
        }
        
        // Mapear las páginas de la región
        let pages_needed = (self.size + PAGE_SIZE - 1) / PAGE_SIZE;
        
        for i in 0..pages_needed {
            let virtual_page = self.virtual_base + (i * PAGE_SIZE) as u64;
            
            // Asignar una página física
            if let Some(physical_page) = allocate_physical_page() {
                let mut page_flags = 0x07; // Presente, escribible, usuario
                
                if !self.has_permission(VIRTUAL_READ) {
                    page_flags &= !0x01; // No presente
                }
                if !self.has_permission(VIRTUAL_WRITE) {
                    page_flags &= !0x02; // No escribible
                }
                if !self.has_permission(VIRTUAL_EXEC) {
                    page_flags |= 0x8000000000000000; // No ejecutable
                }
                
                crate::memory::paging::map_virtual_page(virtual_page, physical_page, page_flags)?;
            } else {
                return Err("No hay páginas físicas disponibles");
            }
        }
        
        self.is_active = true;
        self.last_access = get_timestamp();
        
        Ok(())
    }
    
    /// Desactivar la región
    pub fn deactivate(&mut self) -> Result<(), &'static str> {
        if !self.is_active {
            return Err("Región no está activa");
        }
        
        // Desmapear las páginas de la región
        let pages_needed = (self.size + PAGE_SIZE - 1) / PAGE_SIZE;
        
        for i in 0..pages_needed {
            let virtual_page = self.virtual_base + (i * PAGE_SIZE) as u64;
            
            // Obtener la dirección física antes de desmapear
            if let Some(physical_page) = crate::memory::paging::translate_virtual_address(virtual_page) {
                let physical_page = physical_page & !0xFFF; // Alinear a página
                crate::memory::paging::unmap_virtual_page(virtual_page)?;
                deallocate_physical_page(physical_page)?;
            }
        }
        
        self.is_active = false;
        
        Ok(())
    }
    
    /// Cambiar los permisos de la región
    pub fn change_permissions(&mut self, new_permissions: u32) -> Result<(), &'static str> {
        if !self.is_active {
            return Err("Región no está activa");
        }
        
        // Actualizar los permisos de las páginas
        let pages_needed = (self.size + PAGE_SIZE - 1) / PAGE_SIZE;
        
        for i in 0..pages_needed {
            let virtual_page = self.virtual_base + (i * PAGE_SIZE) as u64;
            
            // Obtener la dirección física actual
            if let Some(physical_page) = crate::memory::paging::translate_virtual_address(virtual_page) {
                let physical_page = physical_page & !0xFFF; // Alinear a página
                
                // Desmapear y volver a mapear con nuevos permisos
                crate::memory::paging::unmap_virtual_page(virtual_page)?;
                
                let mut page_flags = 0x07; // Presente, escribible, usuario
                
                if (new_permissions & VIRTUAL_READ) == 0 {
                    page_flags &= !0x01; // No presente
                }
                if (new_permissions & VIRTUAL_WRITE) == 0 {
                    page_flags &= !0x02; // No escribible
                }
                if (new_permissions & VIRTUAL_EXEC) == 0 {
                    page_flags |= 0x8000000000000000; // No ejecutable
                }
                
                crate::memory::paging::map_virtual_page(virtual_page, physical_page, page_flags)?;
            }
        }
        
        self.permissions = new_permissions;
        self.last_access = get_timestamp();
        
        Ok(())
    }
    
    /// Actualizar estadísticas de acceso
    pub fn update_access_stats(&mut self, access_type: u32) {
        self.last_access = get_timestamp();
        
        match access_type {
            VIRTUAL_READ => self.usage_stats.read_accesses += 1,
            VIRTUAL_WRITE => self.usage_stats.write_accesses += 1,
            VIRTUAL_EXEC => self.usage_stats.exec_accesses += 1,
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
    
    /// Registrar un acceso a memoria no mapeada
    pub fn record_unmapped_access(&mut self) {
        self.usage_stats.unmapped_accesses += 1;
    }
}

/// Gestor de memoria virtual para un proceso
pub struct VirtualMemoryManager {
    /// ID del proceso
    pub process_id: u32,
    /// Regiones de memoria virtual
    regions: [Option<VirtualMemoryRegion>; MAX_VIRTUAL_REGIONS],
    /// Contador de IDs
    next_id: u32,
    /// Dirección base del heap
    heap_base: u64,
    /// Dirección actual del heap
    heap_current: u64,
    /// Dirección base de la pila
    stack_base: u64,
    /// Dirección actual de la pila
    stack_current: u64,
    /// Estadísticas del proceso
    process_stats: VirtualMemoryStats,
}

/// Estadísticas de memoria virtual del proceso
#[derive(Debug, Clone, Copy)]
pub struct VirtualMemoryStats {
    /// Número total de regiones creadas
    pub total_regions_created: u64,
    /// Número total de regiones liberadas
    pub total_regions_freed: u64,
    /// Número de regiones activas
    pub active_regions: u32,
    /// Memoria virtual total asignada
    pub total_virtual_memory: u64,
    /// Memoria virtual actualmente en uso
    pub current_virtual_memory: u64,
    /// Número total de fallos de página
    pub total_page_faults: u64,
    /// Número total de violaciones de permisos
    pub total_permission_violations: u64,
}

impl Default for VirtualMemoryStats {
    fn default() -> Self {
        Self {
            total_regions_created: 0,
            total_regions_freed: 0,
            active_regions: 0,
            total_virtual_memory: 0,
            current_virtual_memory: 0,
            total_page_faults: 0,
            total_permission_violations: 0,
        }
    }
}

impl VirtualMemoryManager {
    /// Crear un nuevo gestor de memoria virtual para un proceso
    pub fn new(process_id: u32) -> Self {
        Self {
            process_id,
            regions: [const { None }; MAX_VIRTUAL_REGIONS],
            next_id: 1,
            heap_base: 0x1000000, // 16MB
            heap_current: 0x1000000,
            stack_base: 0x7FFF_FFFF_F000, // 128TB - 4KB
            stack_current: 0x7FFF_FFFF_F000,
            process_stats: VirtualMemoryStats::default(),
        }
    }
    
    /// Crear una nueva región de memoria virtual
    pub fn create_region(
        &mut self,
        virtual_base: u64,
        size: usize,
        region_type: VirtualRegionType,
        permissions: u32,
        flags: u32,
    ) -> Result<u32, &'static str> {
        // Verificar que no se solape con regiones existentes
        let new_region = VirtualMemoryRegion::new(
            self.next_id,
            virtual_base,
            size,
            region_type,
            permissions,
            flags,
        )?;
        
        for region in &self.regions {
            if let Some(ref r) = region {
                if r.overlaps_with(&new_region) {
                    return Err("La región se solapa con una región existente");
                }
            }
        }
        
        // Buscar un slot libre
        for i in 0..MAX_VIRTUAL_REGIONS {
            if self.regions[i].is_none() {
                self.regions[i] = Some(new_region);
                
                self.process_stats.total_regions_created += 1;
                self.process_stats.active_regions += 1;
                self.process_stats.total_virtual_memory += size as u64;
                
                let id = self.next_id;
                self.next_id += 1;
                
                return Ok(id);
            }
        }
        
        Err("No hay slots disponibles para regiones de memoria virtual")
    }
    
    /// Crear una región de heap
    pub fn create_heap_region(&mut self, size: usize) -> Result<u32, &'static str> {
        let virtual_base = self.heap_current;
        let id = self.create_region(
            virtual_base,
            size,
            VirtualRegionType::Heap,
            VIRTUAL_READ | VIRTUAL_WRITE,
            VIRTUAL_PRIVATE | VIRTUAL_GROWSDOWN,
        )?;
        
        self.heap_current += size as u64;
        Ok(id)
    }
    
    /// Crear una región de pila
    pub fn create_stack_region(&mut self, size: usize) -> Result<u32, &'static str> {
        let virtual_base = self.stack_current - size as u64;
        let id = self.create_region(
            virtual_base,
            size,
            VirtualRegionType::Stack,
            VIRTUAL_READ | VIRTUAL_WRITE,
            VIRTUAL_PRIVATE | VIRTUAL_GROWSUP,
        )?;
        
        self.stack_current = virtual_base;
        Ok(id)
    }
    
    /// Liberar una región de memoria virtual
    pub fn free_region(&mut self, id: u32) -> Result<(), &'static str> {
        for i in 0..MAX_VIRTUAL_REGIONS {
            if let Some(ref mut region) = self.regions[i] {
                if region.id == id {
                    if region.is_active {
                        region.deactivate()?;
                    }
                    
                    let size = region.size;
                    self.regions[i] = None;
                    
                    self.process_stats.total_regions_freed += 1;
                    self.process_stats.active_regions -= 1;
                    self.process_stats.total_virtual_memory -= size as u64;
                    
                    return Ok(());
                }
            }
        }
        
        Err("Región de memoria virtual no encontrada")
    }
    
    /// Obtener una región por ID
    pub fn get_region(&self, id: u32) -> Option<&VirtualMemoryRegion> {
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
    pub fn get_region_mut(&mut self, id: u32) -> Option<&mut VirtualMemoryRegion> {
        for region in &mut self.regions {
            if let Some(ref mut r) = region {
                if r.id == id {
                    return Some(r);
                }
            }
        }
        None
    }
    
    /// Encontrar la región que contiene una dirección
    pub fn find_region_containing(&self, addr: u64) -> Option<&VirtualMemoryRegion> {
        for region in &self.regions {
            if let Some(ref r) = region {
                if r.contains_address(addr) {
                    return Some(r);
                }
            }
        }
        None
    }
    
    /// Encontrar la región que contiene una dirección (mutable)
    pub fn find_region_containing_mut(&mut self, addr: u64) -> Option<&mut VirtualMemoryRegion> {
        for region in &mut self.regions {
            if let Some(ref mut r) = region {
                if r.contains_address(addr) {
                    return Some(r);
                }
            }
        }
        None
    }
    
    /// Activar una región
    pub fn activate_region(&mut self, id: u32) -> Result<(), &'static str> {
        if let Some(region) = self.get_region_mut(id) {
            region.activate()?;
            self.process_stats.current_virtual_memory += region.size as u64;
            Ok(())
        } else {
            Err("Región de memoria virtual no encontrada")
        }
    }
    
    /// Desactivar una región
    pub fn deactivate_region(&mut self, id: u32) -> Result<(), &'static str> {
        if let Some(region) = self.get_region_mut(id) {
            region.deactivate()?;
            self.process_stats.current_virtual_memory -= region.size as u64;
            Ok(())
        } else {
            Err("Región de memoria virtual no encontrada")
        }
    }
    
    /// Cambiar los permisos de una región
    pub fn change_region_permissions(&mut self, id: u32, new_permissions: u32) -> Result<(), &'static str> {
        if let Some(region) = self.get_region_mut(id) {
            region.change_permissions(new_permissions)?;
            Ok(())
        } else {
            Err("Región de memoria virtual no encontrada")
        }
    }
    
    /// Obtener estadísticas del proceso
    pub fn get_process_stats(&self) -> VirtualMemoryStats {
        self.process_stats
    }
    
    /// Verificar la integridad del gestor
    pub fn verify_integrity(&self) -> bool {
        let mut active_count = 0;
        
        for region in &self.regions {
            if let Some(ref r) = region {
                if r.is_active {
                    active_count += 1;
                }
                
                // Verificar que la región no esté corrupta
                if r.size == 0 || r.size > MAX_VIRTUAL_REGION_SIZE {
                    return false;
                }
                
                if !VirtualMemoryRegion::is_valid_virtual_address(r.virtual_base) {
                    return false;
                }
            }
        }
        
        active_count == self.process_stats.active_regions
    }
}

/// Instancia global del gestor de memoria virtual
static mut VIRTUAL_MEMORY_MANAGER: Option<VirtualMemoryManager> = None;

/// Obtener timestamp actual (simulado)
fn get_timestamp() -> u64 {
    // En un sistema real, esto usaría un timer del sistema
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
}

/// Inicializar el gestor de memoria virtual
pub fn init_virtual_memory() -> Result<(), &'static str> {
    serial_write_str("VIRTUAL_MEMORY: Inicializando gestor de memoria virtual...\n");
    
    let manager = VirtualMemoryManager::new(0); // PID 0 para el kernel
    
    unsafe {
        VIRTUAL_MEMORY_MANAGER = Some(manager);
    }
    
    serial_write_str("VIRTUAL_MEMORY: Gestor de memoria virtual inicializado\n");
    Ok(())
}

/// Obtener el gestor de memoria virtual
fn get_virtual_memory_manager() -> &'static mut VirtualMemoryManager {
    unsafe {
        VIRTUAL_MEMORY_MANAGER.as_mut().expect("Gestor de memoria virtual no inicializado")
    }
}

/// Crear una región de memoria virtual
pub fn virtual_memory_create_region(
    virtual_base: u64,
    size: usize,
    region_type: VirtualRegionType,
    permissions: u32,
    flags: u32,
) -> Result<u32, &'static str> {
    let manager = get_virtual_memory_manager();
    manager.create_region(virtual_base, size, region_type, permissions, flags)
}

/// Crear una región de heap
pub fn virtual_memory_create_heap(size: usize) -> Result<u32, &'static str> {
    let manager = get_virtual_memory_manager();
    manager.create_heap_region(size)
}

/// Crear una región de pila
pub fn virtual_memory_create_stack(size: usize) -> Result<u32, &'static str> {
    let manager = get_virtual_memory_manager();
    manager.create_stack_region(size)
}

/// Liberar una región de memoria virtual
pub fn virtual_memory_free_region(id: u32) -> Result<(), &'static str> {
    let manager = get_virtual_memory_manager();
    manager.free_region(id)
}

/// Obtener una región de memoria virtual
pub fn virtual_memory_get_region(id: u32) -> Option<&'static VirtualMemoryRegion> {
    let manager = get_virtual_memory_manager();
    manager.get_region(id)
}

/// Encontrar la región que contiene una dirección
pub fn virtual_memory_find_region_containing(addr: u64) -> Option<&'static VirtualMemoryRegion> {
    let manager = get_virtual_memory_manager();
    manager.find_region_containing(addr)
}

/// Activar una región
pub fn virtual_memory_activate_region(id: u32) -> Result<(), &'static str> {
    let manager = get_virtual_memory_manager();
    manager.activate_region(id)
}

/// Desactivar una región
pub fn virtual_memory_deactivate_region(id: u32) -> Result<(), &'static str> {
    let manager = get_virtual_memory_manager();
    manager.deactivate_region(id)
}

/// Cambiar los permisos de una región
pub fn virtual_memory_change_region_permissions(id: u32, new_permissions: u32) -> Result<(), &'static str> {
    let manager = get_virtual_memory_manager();
    manager.change_region_permissions(id, new_permissions)
}

/// Obtener estadísticas de memoria virtual
pub fn get_virtual_memory_stats() -> VirtualMemoryStats {
    let manager = get_virtual_memory_manager();
    manager.get_process_stats()
}

/// Obtener el total de memoria virtual
pub fn get_total_virtual_memory() -> u64 {
    let stats = get_virtual_memory_stats();
    stats.total_virtual_memory
}

/// Obtener el uso actual de memoria virtual
pub fn get_used_virtual_memory() -> u64 {
    let stats = get_virtual_memory_stats();
    stats.current_virtual_memory
}

/// Verificar la integridad del gestor de memoria virtual
pub fn virtual_memory_verify_integrity() -> bool {
    let manager = get_virtual_memory_manager();
    manager.verify_integrity()
}

/// Imprimir estadísticas de memoria virtual
pub fn print_virtual_memory_stats() {
    let stats = get_virtual_memory_stats();
    
    serial_write_str("=== ESTADÍSTICAS DE MEMORIA VIRTUAL ===\n");
    serial_write_str(&format!("Regiones creadas: {}\n", stats.total_regions_created));
    serial_write_str(&format!("Regiones liberadas: {}\n", stats.total_regions_freed));
    serial_write_str(&format!("Regiones activas: {}\n", stats.active_regions));
    serial_write_str(&format!("Memoria virtual total: {} KB\n", stats.total_virtual_memory / 1024));
    serial_write_str(&format!("Memoria virtual actual: {} KB\n", stats.current_virtual_memory / 1024));
    serial_write_str(&format!("Fallos de página: {}\n", stats.total_page_faults));
    serial_write_str(&format!("Violaciones de permisos: {}\n", stats.total_permission_violations));
    serial_write_str("=======================================\n");
}

