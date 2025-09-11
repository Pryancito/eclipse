//! Sistema de paginación para Eclipse OS
//! 
//! Este módulo maneja la configuración de tablas de páginas y mapeo de memoria

use core::ptr;
use core::mem;
use core::arch::asm;
use crate::main_simple::serial_write_str;
/// Tamaño de página estándar (4KB)
pub const PAGE_SIZE: u64 = 0x1000;

/// Flags de página
pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
pub const PAGE_USER: u64 = 1 << 2;
pub const PAGE_PWT: u64 = 1 << 3;  // Page Write Through
pub const PAGE_PCD: u64 = 1 << 4;  // Page Cache Disable
pub const PAGE_ACCESSED: u64 = 1 << 5;
pub const PAGE_DIRTY: u64 = 1 << 6;
pub const PAGE_SIZE_FLAG: u64 = 1 << 7;  // Page Size (2MB pages)
pub const PAGE_GLOBAL: u64 = 1 << 8;

/// Entrada de tabla de páginas
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PageTableEntry {
    pub value: u64,
}

impl PageTableEntry {
    /// Crear nueva entrada de tabla de páginas
    pub fn new() -> Self {
        Self { value: 0 }
    }

    /// Crear entrada con dirección física y flags
    pub fn new_with_flags(physical_addr: u64, flags: u64) -> Self {
        Self {
            value: (physical_addr & 0x000FFFFFFFFFF000) | (flags & 0xFFF),
        }
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

    /// Obtener dirección física
    pub fn get_physical_address(&self) -> u64 {
        self.value & 0x000FFFFFFFFFF000
    }

    /// Establecer dirección física
    pub fn set_physical_address(&mut self, addr: u64) {
        self.value = (self.value & 0xFFF) | (addr & 0x000FFFFFFFFFF000);
    }

    /// Establecer flags
    pub fn set_flags(&mut self, flags: u64) {
        self.value = (self.value & 0x000FFFFFFFFFF000) | (flags & 0xFFF);
    }

    /// Agregar flags
    pub fn add_flags(&mut self, flags: u64) {
        self.value |= flags & 0xFFF;
    }

    /// Remover flags
    pub fn remove_flags(&mut self, flags: u64) {
        self.value &= !(flags & 0xFFF);
    }
}

/// Tabla de páginas (512 entradas de 8 bytes cada una)
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    /// Crear nueva tabla de páginas vacía
    pub fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); 512],
        }
    }

    /// Obtener entrada por índice
    pub fn get_entry(&self, index: usize) -> &PageTableEntry {
        &self.entries[index]
    }

    /// Obtener entrada mutable por índice
    pub fn get_entry_mut(&mut self, index: usize) -> &mut PageTableEntry {
        &mut self.entries[index]
    }

    /// Establecer entrada
    pub fn set_entry(&mut self, index: usize, entry: PageTableEntry) {
        self.entries[index] = entry;
    }

    /// Mapear página virtual a física
    pub fn map_page(&mut self, virtual_addr: u64, physical_addr: u64, flags: u64) -> Result<(), &'static str> {
        let index = ((virtual_addr >> 12) & 0x1FF) as usize;
        
        if index >= 512 {
            return Err("Índice de tabla de páginas inválido");
        }

        let entry = PageTableEntry::new_with_flags(physical_addr, flags | PAGE_PRESENT);
        self.set_entry(index, entry);
        
        Ok(())
    }

    /// Desmapear página
    pub fn unmap_page(&mut self, virtual_addr: u64) -> Result<(), &'static str> {
        let index = ((virtual_addr >> 12) & 0x1FF) as usize;
        
        if index >= 512 {
            return Err("Índice de tabla de páginas inválido");
        }

        self.set_entry(index, PageTableEntry::new());
        
        Ok(())
    }
}

/// Directorio de páginas (PDPT)
#[repr(C, align(4096))]
pub struct PageDirectoryPointerTable {
    pub entries: [PageTableEntry; 512],
}

impl PageDirectoryPointerTable {
    /// Crear nuevo PDPT vacío
    pub fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); 512],
        }
    }

    /// Mapear tabla de páginas
    pub fn map_page_table(&mut self, virtual_addr: u64, page_table: &PageTable) -> Result<(), &'static str> {
        let index = ((virtual_addr >> 21) & 0x1FF) as usize;
        
        if index >= 512 {
            return Err("Índice de PDPT inválido");
        }

        let page_table_addr = page_table as *const PageTable as u64;
        let entry = PageTableEntry::new_with_flags(page_table_addr, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        self.entries[index] = entry;
        
        Ok(())
    }
}

/// Directorio de páginas (PD)
#[repr(C, align(4096))]
pub struct PageDirectory {
    pub entries: [PageTableEntry; 512],
}

impl PageDirectory {
    /// Crear nuevo directorio de páginas vacío
    pub fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); 512],
        }
    }

    /// Mapear tabla de páginas
    pub fn map_page_table(&mut self, virtual_addr: u64, page_table: &PageTable) -> Result<(), &'static str> {
        let index = ((virtual_addr >> 21) & 0x1FF) as usize;
        
        if index >= 512 {
            return Err("Índice de directorio de páginas inválido");
        }

        let page_table_addr = page_table as *const PageTable as u64;
        let entry = PageTableEntry::new_with_flags(page_table_addr, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        self.entries[index] = entry;
        
        Ok(())
    }
}

/// Tabla de páginas de nivel 4 (PML4)
#[repr(C, align(4096))]
pub struct PageMapLevel4 {
    pub entries: [PageTableEntry; 512],
}

impl PageMapLevel4 {
    /// Crear nueva PML4 vacía
    pub fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); 512],
        }
    }

    /// Mapear PDPT
    pub fn map_pdpt(&mut self, virtual_addr: u64, pdpt: &PageDirectoryPointerTable) -> Result<(), &'static str> {
        let index = ((virtual_addr >> 39) & 0x1FF) as usize;
        
        if index >= 512 {
            return Err("Índice de PML4 inválido");
        }

        let pdpt_addr = pdpt as *const PageDirectoryPointerTable as u64;
        let entry = PageTableEntry::new_with_flags(pdpt_addr, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        self.entries[index] = entry;
        
        Ok(())
    }
}

/// Gestor de paginación
pub struct PagingManager {
    pml4: PageMapLevel4,
    pdpt: PageDirectoryPointerTable,
    pd: PageDirectory,
    pt: PageTable,
    next_physical_addr: u64,
}

impl PagingManager {
    /// Crear nuevo gestor de paginación
    pub fn new() -> Self {
        Self {
            pml4: PageMapLevel4::new(),
            pdpt: PageDirectoryPointerTable::new(),
            pd: PageDirectory::new(),
            pt: PageTable::new(),
            next_physical_addr: 0x100000,  // Empezar después de los primeros 1MB
        }
    }

    /// Configurar paginación para userland
    pub fn setup_userland_paging(&mut self) -> Result<u64, &'static str> {
        // Configurar mapeo de PML4 -> PDPT
        self.pml4.map_pdpt(0, &self.pdpt)?;
        
        // Configurar mapeo de PDPT -> PD (usar PageTable en lugar de PageDirectory)
        // self.pdpt.map_page_table(0, &self.pd)?;
        
        // Configurar mapeo de PD -> PT
        self.pd.map_page_table(0, &self.pt)?;
        
        // Mapear memoria del kernel (primeros 1MB)
        self.map_kernel_memory()?;
        
        // Mapear memoria del userland
        self.map_userland_memory()?;
        
        // Retornar dirección física de PML4
        Ok(&self.pml4 as *const PageMapLevel4 as u64)
    }

    /// Mapear memoria del kernel
    fn map_kernel_memory(&mut self) -> Result<(), &'static str> {
        // Mapear los primeros 1MB con permisos de kernel
        for i in 0..256 {  // 256 páginas = 1MB
            let virtual_addr = i * PAGE_SIZE;
            let physical_addr = virtual_addr;  // Mapeo 1:1 para el kernel
            let flags = PAGE_PRESENT | PAGE_WRITABLE;  // Solo kernel puede escribir
            
            self.pt.map_page(virtual_addr, physical_addr, flags)?;
        }
        
        Ok(())
    }

    /// Mapear memoria del userland
    fn map_userland_memory(&mut self) -> Result<(), &'static str> {
        // Mapear espacio de userland (0x400000 - 0x7FFFFFFFFFFF)
        let userland_start = 0x400000;
        let userland_end = 0x800000000000;  // 48 bits de espacio virtual
        
        let mut current_addr = userland_start;
        while current_addr < userland_end {
            let physical_addr = self.allocate_physical_page()?;
            let flags = PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
            
            self.pt.map_page(current_addr, physical_addr, flags)?;
            
            current_addr += PAGE_SIZE;
        }
        
        Ok(())
    }

    /// Asignar página física
    fn allocate_physical_page(&mut self) -> Result<u64, &'static str> {
        let addr = self.next_physical_addr;
        self.next_physical_addr += PAGE_SIZE;
        
        if self.next_physical_addr > 0x100000000 {  // Límite de 4GB
            return Err("Memoria física agotada");
        }
        
        Ok(addr)
    }

    /// Mapear página específica
    pub fn map_page(&mut self, virtual_addr: u64, physical_addr: u64, flags: u64) -> Result<(), &'static str> {
        // Verificar que la dirección virtual esté en el rango mapeado
        if virtual_addr < 0x400000 || virtual_addr >= 0x800000000000 {
            return Err("Dirección virtual fuera del rango de userland");
        }

        // Mapear en la tabla de páginas
        self.pt.map_page(virtual_addr, physical_addr, flags)?;
        
        Ok(())
    }

    /// Desmapear página
    pub fn unmap_page(&mut self, virtual_addr: u64) -> Result<(), &'static str> {
        self.pt.unmap_page(virtual_addr)
    }

    /// Obtener dirección física de PML4
    pub fn get_pml4_address(&self) -> u64 {
        &self.pml4 as *const PageMapLevel4 as u64
    }

    /// Invalidar TLB completa (SIMULACIÓN ULTRA-SEGURA)
    pub fn invalidate_tlb(&self) {
        // TEMPORALMENTE DESHABILITADO: Instrucciones CR3 causan opcode inválido

        unsafe {
            serial_write_str("[PAGING] Invalidación TLB SIMULADA (CR3 deshabilitado)\r\n");
            serial_write_str("[PAGING] ERROR: Opcode inválido en RIP 000000000009F0AD - CR3 problemático\r\n");
        }
    }

    /// Invalidar una página específica en la TLB (SIMULACIÓN ULTRA-SEGURA)
    pub fn invalidate_page(&self, _virtual_address: u64) {
        // TEMPORALMENTE DESHABILITADO: Instrucción INVLPG causa opcode inválido

        unsafe {
            serial_write_str("[PAGING] Invalidación de página SIMULADA (INVLPG deshabilitado)\r\n");
        }
    }

    /// Cambiar a nueva tabla de páginas (SIMULACIÓN ULTRA-SEGURA)
    pub fn switch_to_pml4(&self) {
        // TEMPORALMENTE DESHABILITADO: Instrucciones CR3 causan opcode inválido

        unsafe {
            serial_write_str("[PAGING] Cambio de tabla de páginas SIMULADO (CR3 deshabilitado)\r\n");
            serial_write_str("[PAGING] ERROR: Opcode inválido en RIP 000000000009F0AD - CR3 problemático\r\n");
        }
    }
}

impl Default for PagingManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Función de utilidad para configurar paginación
pub fn setup_userland_paging() -> Result<u64, &'static str> {
    let mut paging_manager = PagingManager::new();
    paging_manager.setup_userland_paging()
}
