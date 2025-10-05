//! Sistema de paginación con MMU para Eclipse OS
//! 
//! Este módulo implementa:
//! - Tablas de páginas de 4 niveles (PML4, PDPT, PD, PT)
//! - Gestión de páginas físicas y virtuales
//! - Protección de memoria (RWX)
//! - TLB (Translation Lookaside Buffer)
//! - Mapeo de memoria

use core::arch::asm;
use crate::debug::serial_write_str;
use alloc::format;

/// Tamaño de página estándar (4KB)
pub const PAGE_SIZE: usize = 4096;

/// Número de entradas por tabla de páginas
pub const PAGE_TABLE_ENTRIES: usize = 512;

/// Máscaras para flags de página
pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
pub const PAGE_USER: u64 = 1 << 2;
pub const PAGE_PWT: u64 = 1 << 3; // Page Write Through
pub const PAGE_PCD: u64 = 1 << 4; // Page Cache Disable
pub const PAGE_ACCESSED: u64 = 1 << 5;
pub const PAGE_DIRTY: u64 = 1 << 6;
pub const PAGE_SIZE_FLAG: u64 = 1 << 7; // Para páginas de 2MB/1GB
pub const PAGE_GLOBAL: u64 = 1 << 8;
pub const PAGE_NO_EXECUTE: u64 = 1 << 63;

/// Niveles de la jerarquía de páginas
pub const PML4_LEVEL: usize = 0;
pub const PDPT_LEVEL: usize = 1;
pub const PD_LEVEL: usize = 2;
pub const PT_LEVEL: usize = 3;

/// Estructura para una entrada de tabla de páginas
#[derive(Debug, Clone, Copy)]
pub struct PageTableEntry {
    pub value: u64,
}

impl PageTableEntry {
    /// Crear una nueva entrada de tabla de páginas
    pub fn new() -> Self {
        Self { value: 0 }
    }
    
    /// Crear una entrada con dirección física y flags
    pub fn new_with_addr(physical_addr: u64, flags: u64) -> Self {
        Self {
            value: (physical_addr & 0x000F_FFFF_FFFF_F000) | (flags & 0xFFF),
        }
    }
    
    /// Obtener la dirección física
    pub fn get_physical_addr(&self) -> u64 {
        self.value & 0x000F_FFFF_FFFF_F000
    }
    
    /// Establecer la dirección física
    pub fn set_physical_addr(&mut self, addr: u64) {
        self.value = (self.value & 0xFFF) | (addr & 0x000F_FFFF_FFFF_F000);
    }
    
    /// Verificar si la página está presente
    pub fn is_present(&self) -> bool {
        (self.value & PAGE_PRESENT) != 0
    }
    
    /// Verificar si la página es escribible
    pub fn is_writable(&self) -> bool {
        (self.value & PAGE_WRITABLE) != 0
    }
    
    /// Verificar si la página es accesible por usuario
    pub fn is_user_accessible(&self) -> bool {
        (self.value & PAGE_USER) != 0
    }
    
    /// Establecer flags
    pub fn set_flags(&mut self, flags: u64) {
        self.value = (self.value & 0x000F_FFFF_FFFF_F000) | (flags & 0xFFF);
    }
    
    /// Obtener flags
    pub fn get_flags(&self) -> u64 {
        self.value & 0xFFF
    }
    
    /// Marcar como presente
    pub fn set_present(&mut self) {
        self.value |= PAGE_PRESENT;
    }
    
    /// Marcar como no presente
    pub fn clear_present(&mut self) {
        self.value &= !PAGE_PRESENT;
    }
    
    /// Marcar como escribible
    pub fn set_writable(&mut self) {
        self.value |= PAGE_WRITABLE;
    }
    
    /// Marcar como accesible por usuario
    pub fn set_user_accessible(&mut self) {
        self.value |= PAGE_USER;
    }
    
    /// Marcar como accedida
    pub fn set_accessed(&mut self) {
        self.value |= PAGE_ACCESSED;
    }
    
    /// Marcar como modificada
    pub fn set_dirty(&mut self) {
        self.value |= PAGE_DIRTY;
    }
}

/// Estructura para una tabla de páginas
#[repr(align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; PAGE_TABLE_ENTRIES],
}

impl PageTable {
    /// Crear una nueva tabla de páginas
    pub fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); PAGE_TABLE_ENTRIES],
        }
    }
    
    /// Obtener una entrada por índice
    pub fn get_entry(&self, index: usize) -> &PageTableEntry {
        &self.entries[index]
    }
    
    /// Obtener una entrada mutable por índice
    pub fn get_entry_mut(&mut self, index: usize) -> &mut PageTableEntry {
        &mut self.entries[index]
    }
    
    /// Establecer una entrada
    pub fn set_entry(&mut self, index: usize, entry: PageTableEntry) {
        self.entries[index] = entry;
    }
    
    /// Limpiar todas las entradas
    pub fn clear(&mut self) {
        for entry in &mut self.entries {
            entry.value = 0;
        }
    }
}

/// Gestor de páginas físicas
pub struct PhysicalPageManager {
    /// Bitmap de páginas libres
    page_bitmap: &'static mut [u64],
    /// Número total de páginas
    total_pages: u32,
    /// Número de páginas libres
    free_pages: u32,
    /// Dirección base de la memoria física
    physical_base: u64,
}

impl PhysicalPageManager {
    /// Crear un nuevo gestor de páginas físicas
    pub fn new(physical_base: u64, total_memory: u64) -> Self {
        let total_pages = (total_memory / PAGE_SIZE as u64) as u32;
        let bitmap_size = (total_pages + 63) / 64; // Redondear hacia arriba
        
        // Usar memoria física para el bitmap
        let bitmap_ptr = physical_base as *mut u64;
        
        Self {
            page_bitmap: unsafe { core::slice::from_raw_parts_mut(bitmap_ptr, bitmap_size as usize) },
            total_pages,
            free_pages: total_pages,
            physical_base,
        }
    }
    
    /// Asignar una página física
    pub fn allocate_page(&mut self) -> Option<u64> {
        if self.free_pages == 0 {
            return None;
        }
        
        for (bitmap_index, bitmap_entry) in self.page_bitmap.iter_mut().enumerate() {
            if *bitmap_entry != u64::MAX {
                // Encontrar el primer bit libre
                let bit_index = bitmap_entry.trailing_ones() as usize;
                if bit_index < 64 {
                    let page_index = bitmap_index * 64 + bit_index;
                    if page_index < self.total_pages as usize {
                        // Marcar la página como usada
                        *bitmap_entry |= 1u64 << bit_index;
                        self.free_pages -= 1;
                        
                        let physical_addr = self.physical_base + (page_index * PAGE_SIZE) as u64;
                        return Some(physical_addr);
                    }
                }
            }
        }
        
        None
    }
    
    /// Liberar una página física
    pub fn deallocate_page(&mut self, physical_addr: u64) -> Result<(), &'static str> {
        if physical_addr < self.physical_base {
            return Err("Dirección física fuera del rango válido");
        }
        
        let page_index = ((physical_addr - self.physical_base) / PAGE_SIZE as u64) as usize;
        if page_index >= self.total_pages as usize {
            return Err("Índice de página fuera del rango válido");
        }
        
        let bitmap_index = page_index / 64;
        let bit_index = page_index % 64;
        
        if bitmap_index >= self.page_bitmap.len() {
            return Err("Índice de bitmap fuera del rango válido");
        }
        
        let bitmap_entry = &mut self.page_bitmap[bitmap_index];
        let bit_mask = 1u64 << bit_index;
        
        // Verificar que la página esté marcada como usada
        if (*bitmap_entry & bit_mask) == 0 {
            return Err("Página ya está libre");
        }
        
        // Marcar la página como libre
        *bitmap_entry &= !bit_mask;
        self.free_pages += 1;
        
        Ok(())
    }
    
    /// Obtener el número de páginas libres
    pub fn get_free_pages(&self) -> u32 {
        self.free_pages
    }
    
    /// Obtener el número total de páginas
    pub fn get_total_pages(&self) -> u32 {
        self.total_pages
    }
    
    /// Verificar si una página está libre
    pub fn is_page_free(&self, physical_addr: u64) -> bool {
        if physical_addr < self.physical_base {
            return false;
        }
        
        let page_index = ((physical_addr - self.physical_base) / PAGE_SIZE as u64) as usize;
        if page_index >= self.total_pages as usize {
            return false;
        }
        
        let bitmap_index = page_index / 64;
        let bit_index = page_index % 64;
        
        if bitmap_index >= self.page_bitmap.len() {
            return false;
        }
        
        let bitmap_entry = self.page_bitmap[bitmap_index];
        let bit_mask = 1u64 << bit_index;
        
        (bitmap_entry & bit_mask) == 0
    }
}

/// Gestor de memoria virtual
pub struct VirtualMemoryManager {
    /// Tabla PML4 (Page Map Level 4)
    pml4_table: &'static mut PageTable,
    /// Gestor de páginas físicas
    physical_manager: PhysicalPageManager,
    /// Dirección base del kernel
    kernel_base: u64,
    /// Límite superior del kernel
    kernel_limit: u64,
}

impl VirtualMemoryManager {
    /// Crear un nuevo gestor de memoria virtual
    pub fn new(physical_base: u64, total_memory: u64, kernel_base: u64, kernel_limit: u64) -> Self {
        let physical_manager = PhysicalPageManager::new(physical_base, total_memory);
        
        // Usar memoria física para la tabla PML4
        let pml4_ptr = physical_base as *mut PageTable;
        
        Self {
            pml4_table: unsafe { &mut *pml4_ptr },
            physical_manager,
            kernel_base,
            kernel_limit,
        }
    }
    
    /// Mapear una página virtual a una página física (versión simplificada)
    pub fn map_page(&mut self, virtual_addr: u64, physical_addr: u64, flags: u64) -> Result<(), &'static str> {
        // Por ahora, implementación simplificada que solo registra el mapeo
        // En una implementación completa, se crearían las tablas de páginas
        
        serial_write_str(&format!("PAGING: Mapeando 0x{:x} -> 0x{:x} con flags 0x{:x}\n", 
                                 virtual_addr, physical_addr, flags));
        
        // Invalidar TLB para esta dirección
        self.invalidate_tlb(virtual_addr);
        
        Ok(())
    }
    
    /// Desmapear una página virtual (versión simplificada)
    pub fn unmap_page(&mut self, virtual_addr: u64) -> Result<(), &'static str> {
        // Por ahora, implementación simplificada que solo registra el desmapeo
        serial_write_str(&format!("PAGING: Desmapeando 0x{:x}\n", virtual_addr));
        
        // Invalidar TLB para esta dirección
        self.invalidate_tlb(virtual_addr);
        
        Ok(())
    }
    
    /// Obtener o crear una tabla de páginas
    fn get_or_create_table(&mut self, parent_table: &mut PageTable, index: usize, flags: u64) -> Result<&mut PageTable, &'static str> {
        let entry = parent_table.get_entry(index);
        
        if entry.is_present() {
            // La tabla ya existe
            let table_addr = entry.get_physical_addr();
            Ok(unsafe { &mut *(table_addr as *mut PageTable) })
        } else {
            // Crear una nueva tabla
            let new_table_addr = self.physical_manager.allocate_page()
                .ok_or("No hay páginas físicas disponibles")?;
            
            // Crear una nueva tabla
            let new_table = unsafe { &mut *(new_table_addr as *mut PageTable) };
            new_table.clear();
            
            // Establecer la entrada en la tabla padre
            let new_entry = PageTableEntry::new_with_addr(new_table_addr, flags | PAGE_PRESENT | PAGE_WRITABLE);
            parent_table.set_entry(index, new_entry);
            
            Ok(new_table)
        }
    }
    
    /// Invalidar TLB para una dirección específica
    fn invalidate_tlb(&self, virtual_addr: u64) {
        unsafe {
            asm!("invlpg [{}]", in(reg) virtual_addr, options(nostack));
        }
    }
    
    /// Invalidar todo el TLB
    pub fn invalidate_all_tlb(&self) {
        unsafe {
            asm!("mov rax, cr3; mov cr3, rax", options(nostack));
        }
    }
    
    /// Traducir dirección virtual a física (versión simplificada)
    pub fn translate_address(&self, virtual_addr: u64) -> Option<u64> {
        // Por ahora, implementación simplificada que asume mapeo directo
        // En una implementación completa, se recorrerían las tablas de páginas
        
        // Para el kernel, asumimos mapeo directo 1:1
        if virtual_addr >= self.kernel_base && virtual_addr < self.kernel_limit {
            return Some(virtual_addr - self.kernel_base + 0x100000); // Mapeo directo desde 1MB
        }
        
        None
    }
    
    /// Establecer la tabla PML4 en CR3
    pub fn set_pml4_table(&self) {
        let pml4_addr = self.pml4_table as *const PageTable as u64;
        unsafe {
            asm!("mov cr3, {}", in(reg) pml4_addr, options(nostack));
        }
    }
}

/// Variables globales del sistema de paginación
static mut PHYSICAL_MANAGER: Option<PhysicalPageManager> = None;
static mut VIRTUAL_MANAGER: Option<VirtualMemoryManager> = None;

/// Inicializar el sistema de paginación
pub fn init_paging(config: &crate::memory::MemoryConfig) -> Result<(), &'static str> {
    serial_write_str("PAGING: Inicializando sistema de paginación...\n");
    
    // Detectar memoria física disponible
    let physical_base = 0x100000; // 1MB (después del área de BIOS)
    let total_memory = config.total_physical_memory;
    let kernel_base = 0xFFFF_8000_0000_0000; // Dirección virtual del kernel
    let kernel_limit = kernel_base + config.kernel_heap_size;
    
    serial_write_str(&format!("PAGING: Memoria física base: 0x{:x}\n", physical_base));
    serial_write_str(&format!("PAGING: Memoria total: {} MB\n", total_memory / (1024 * 1024)));
    serial_write_str(&format!("PAGING: Kernel base: 0x{:x}\n", kernel_base));
    
    // Crear gestores de memoria
    let mut physical_manager = PhysicalPageManager::new(physical_base, total_memory);
    let mut virtual_manager = VirtualMemoryManager::new(physical_base, total_memory, kernel_base, kernel_limit);
    
    // Mapear el kernel
    map_kernel_memory(&mut physical_manager, &mut virtual_manager, kernel_base, kernel_limit)?;
    
    // Establecer la tabla PML4
    virtual_manager.set_pml4_table();
    
    // Habilitar paginación
    enable_paging();
    
    // Guardar los gestores globalmente
    unsafe {
        PHYSICAL_MANAGER = Some(physical_manager);
        VIRTUAL_MANAGER = Some(virtual_manager);
    }
    
    serial_write_str("PAGING: Sistema de paginación inicializado\n");
    Ok(())
}

/// Mapear la memoria del kernel
fn map_kernel_memory(
    physical_manager: &mut PhysicalPageManager,
    virtual_manager: &mut VirtualMemoryManager,
    kernel_base: u64,
    kernel_limit: u64,
) -> Result<(), &'static str> {
    let mut current_addr = kernel_base;
    let mut physical_addr = 0x100000; // 1MB
    
    while current_addr < kernel_limit {
        let flags = PAGE_PRESENT | PAGE_WRITABLE;
        virtual_manager.map_page(current_addr, physical_addr, flags)?;
        
        current_addr += PAGE_SIZE as u64;
        physical_addr += PAGE_SIZE as u64;
    }
    
    Ok(())
}

/// Habilitar paginación
fn enable_paging() {
    unsafe {
        asm!(
            "mov rax, cr0",
            "mov rbx, 0x80000000",
            "or rax, rbx", // CR0.PG = 1 (bit 31)
            "mov cr0, rax",
            options(nostack)
        );
    }
}

/// Obtener el gestor de páginas físicas
pub fn get_physical_manager() -> &'static mut PhysicalPageManager {
    unsafe {
        PHYSICAL_MANAGER.as_mut().expect("Sistema de paginación no inicializado")
    }
}

/// Obtener el gestor de memoria virtual
pub fn get_virtual_manager() -> &'static mut VirtualMemoryManager {
    unsafe {
        VIRTUAL_MANAGER.as_mut().expect("Sistema de paginación no inicializado")
    }
}

/// Obtener el número total de páginas físicas
pub fn get_total_physical_memory() -> u64 {
    let manager = get_physical_manager();
    (manager.get_total_pages() * PAGE_SIZE as u32) as u64
}

/// Obtener el número de páginas físicas usadas
pub fn get_used_physical_memory() -> u64 {
    let manager = get_physical_manager();
    let used_pages = manager.get_total_pages() - manager.get_free_pages();
    (used_pages * PAGE_SIZE as u32) as u64
}

/// Obtener el número de páginas asignadas
pub fn get_allocated_pages() -> u32 {
    let manager = get_physical_manager();
    manager.get_total_pages() - manager.get_free_pages()
}

/// Obtener el número de páginas libres
pub fn get_free_pages() -> u32 {
    let manager = get_physical_manager();
    manager.get_free_pages()
}

/// Asignar una página física
pub fn allocate_physical_page() -> Option<u64> {
    let manager = get_physical_manager();
    manager.allocate_page()
}

/// Liberar una página física
pub fn deallocate_physical_page(addr: u64) -> Result<(), &'static str> {
    let manager = get_physical_manager();
    manager.deallocate_page(addr)
}

/// Mapear una página virtual
pub fn map_virtual_page(virtual_addr: u64, physical_addr: u64, flags: u64) -> Result<(), &'static str> {
    let manager = get_virtual_manager();
    manager.map_page(virtual_addr, physical_addr, flags)
}

/// Desmapear una página virtual
pub fn unmap_virtual_page(virtual_addr: u64) -> Result<(), &'static str> {
    let manager = get_virtual_manager();
    manager.unmap_page(virtual_addr)
}

/// Traducir dirección virtual a física
pub fn translate_virtual_address(virtual_addr: u64) -> Option<u64> {
    let manager = get_virtual_manager();
    manager.translate_address(virtual_addr)
}