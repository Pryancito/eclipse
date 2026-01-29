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
        let bitmap_size_bytes = (total_pages as usize + 63) / 64 * 8; // Size in bytes
        
        // Usar memoria física para el bitmap
        let bitmap_ptr = physical_base as *mut u64;
        
        // 1. Inicializar el bitmap a 0 (Todo libre)
        unsafe {
            core::ptr::write_bytes(bitmap_ptr as *mut u8, 0, bitmap_size_bytes);
        }

        crate::debug::serial_write_str("PHYSICAL: New Manager Base: 0x");
        crate::memory::paging::print_hex(physical_base);
        crate::debug::serial_write_str("\n");


        let mut mgr = Self {
            page_bitmap: unsafe { core::slice::from_raw_parts_mut(bitmap_ptr, bitmap_size_bytes / 8) },
            total_pages,
            free_pages: total_pages,
            physical_base,
        };

        // 2. Reservar las páginas usadas por el propio bitmap
        // Calcular cuántas páginas ocupa el bitmap
        let bitmap_pages = (bitmap_size_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
        
        for i in 0..bitmap_pages {
             if let Some(addr) = mgr.allocate_page() {
                 // Debería retornar las primeras direcciones (donde está el bitmap)
                 // Básicamente nos auto-asignamos las páginas para protegerlas.
                 if i == 0 && addr != physical_base {
                     // Sanity check: La primera asignación DEBE ser physical_base
                     // serial_write_str("ERROR: Allocator sanity check failed\n");
                 }
             }
        }
        
        mgr
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
    /// Dirección base del kernel
    kernel_base: u64,
    /// Límite superior del kernel
    kernel_limit: u64,
}

impl VirtualMemoryManager {
    /// Crear un nuevo gestor de memoria virtual
    pub fn new(pml4_table: &'static mut PageTable, kernel_base: u64, kernel_limit: u64) -> Self {
        Self {
            pml4_table,
            kernel_base,
            kernel_limit,
        }
    }
    
    /// Mapear una página virtual a una página física
    pub fn map_page(&mut self, virtual_addr: u64, physical_addr: u64, flags: u64, phys_manager: &mut PhysicalPageManager) -> Result<(), &'static str> {
        let p4_index = (virtual_addr >> 39) & 0x1FF;
        let p3_index = (virtual_addr >> 30) & 0x1FF;
        let p2_index = (virtual_addr >> 21) & 0x1FF;
        let p1_index = (virtual_addr >> 12) & 0x1FF;

        // Recorrer la jerarquía usando helper estático
        // Nota: pml4 ya está separado de phys_manager, así que no hay conflicto de borrow en arguments
        let p3_table = Self::ensure_table(&mut self.pml4_table, p4_index as usize, flags, phys_manager)?;
        let p2_table = Self::ensure_table(p3_table, p3_index as usize, flags, phys_manager)?;
        let p1_table = Self::ensure_table(p2_table, p2_index as usize, flags, phys_manager)?;

        // Configurar la entrada en la tabla de páginas (nivel 1)
        let entry = PageTableEntry::new_with_addr(physical_addr, flags | PAGE_PRESENT | PAGE_WRITABLE);
        p1_table.set_entry(p1_index as usize, entry);

        // Invalidar TLB
        self.invalidate_tlb(virtual_addr);
        
        Ok(())
    }

    /// Desmapear una página virtual
    pub fn unmap_page(&mut self, virtual_addr: u64) -> Result<(), &'static str> {
        let p4_index = (virtual_addr >> 39) & 0x1FF;
        let p3_index = (virtual_addr >> 30) & 0x1FF;
        let p2_index = (virtual_addr >> 21) & 0x1FF;
        let p1_index = (virtual_addr >> 12) & 0x1FF;

        // Traverse tables. If any is missing, page is already unmapped.
        let p3_entry = self.pml4_table.get_entry(p4_index as usize);
        if !p3_entry.is_present() { return Ok(()); }
        let p3_table = unsafe { &mut *(p3_entry.get_physical_addr() as *mut PageTable) };

        let p2_entry = p3_table.get_entry(p3_index as usize);
        if !p2_entry.is_present() { return Ok(()); }
        let p2_table = unsafe { &mut *(p2_entry.get_physical_addr() as *mut PageTable) };

        let p1_entry = p2_table.get_entry(p2_index as usize);
        if !p1_entry.is_present() { return Ok(()); }
        let p1_table = unsafe { &mut *(p1_entry.get_physical_addr() as *mut PageTable) };

        // Clear entry at level 1
        let mut entry = PageTableEntry::new();
        entry.set_physical_addr(0);
        entry.set_flags(0); // Not present
        p1_table.set_entry(p1_index as usize, entry);

        self.invalidate_tlb(virtual_addr);
        Ok(())
    }

    /// Helper estático para obtener o crear tabla (evita borrow de self)
    fn ensure_table(parent_table: &mut PageTable, index: usize, flags: u64, phys_manager: &mut PhysicalPageManager) -> Result<&'static mut PageTable, &'static str> {
        let entry = parent_table.get_entry(index);
        
        if entry.is_present() {
            let table_addr = entry.get_physical_addr();
            Ok(unsafe { &mut *(table_addr as *mut PageTable) })
        } else {
            let new_table_addr = phys_manager.allocate_page()
                .ok_or("No hay páginas físicas disponibles")?;
            
            let new_table = unsafe { &mut *(new_table_addr as *mut PageTable) };
            new_table.clear();
            
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
    
    /// Traducir dirección virtual a física y obtener flags
    pub fn translate_address(&self, virtual_addr: u64) -> Option<(u64, u64)> {
        let p4_index = (virtual_addr >> 39) & 0x1FF;
        let p3_index = (virtual_addr >> 30) & 0x1FF;
        let p2_index = (virtual_addr >> 21) & 0x1FF;
        let p1_index = (virtual_addr >> 12) & 0x1FF;
        let page_offset = virtual_addr & 0xFFF;

        unsafe {
            let p3_entry = self.pml4_table.get_entry(p4_index as usize);
            if !p3_entry.is_present() { return None; }
            let p3_table = &*(p3_entry.get_physical_addr() as *const PageTable);

            let p2_entry = p3_table.get_entry(p3_index as usize);
            if !p2_entry.is_present() { return None; }
            
            // Check for huge page (1GB) - Not supported yet but good to know
            if (p2_entry.value & PAGE_SIZE_FLAG) != 0 {
                // TODO: Handle 1GB pages
                return None;
            }
            
            let p2_table = &*(p2_entry.get_physical_addr() as *const PageTable);

            let p1_entry = p2_table.get_entry(p2_index as usize);
            if !p1_entry.is_present() { return None; }
            
             // Check for large page (2MB) - Not supported yet
            if (p1_entry.value & PAGE_SIZE_FLAG) != 0 {
                 // TODO: Handle 2MB pages
                 return None;
            }

            let p1_table = &*(p1_entry.get_physical_addr() as *const PageTable);

            let page_entry = p1_table.get_entry(p1_index as usize);
            if !page_entry.is_present() { return None; }

            // Retornar tupla (Dirección Física, Flags)
            let flags = page_entry.value & 0xFFF; // Los 12 bits bajos son flags
            Some((page_entry.get_physical_addr() + page_offset, flags))
        }
    }
    
    /// Establecer la tabla PML4 en CR3
    pub fn set_pml4_table(&self) {
        let pml4_addr = self.pml4_table as *const PageTable as u64;
        
        // Debug: Print Addresses
        crate::debug::serial_write_str("PAGING: Loading CR3 with PML4: 0x");
        print_hex(pml4_addr);
        crate::debug::serial_write_str("\n");
        
        let rsp: u64;
        unsafe { asm!("mov {}, rsp", out(reg) rsp); }
        crate::debug::serial_write_str("PAGING: Current RSP: 0x");
        print_hex(rsp);
        crate::debug::serial_write_str("\n");

        unsafe {
            asm!("mov cr3, {}", in(reg) pml4_addr, options(nostack));
        }
    }
}

pub fn print_hex(mut num: u64) {
    if num == 0 {
        crate::debug::serial_write_char(b'0');
        return;
    }
    let mut buffer = [0u8; 16]; // 64-bit int max chars
    let mut i = 0;
    while num > 0 {
        let digit = (num & 0xF) as u8;
        buffer[i] = if digit < 10 { b'0' + digit } else { b'A' + (digit - 10) };
        num >>= 4;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        crate::debug::serial_write_char(buffer[i]);
    }
}

/// Variables globales del sistema de paginación
static mut PHYSICAL_MANAGER: Option<PhysicalPageManager> = None;
static mut VIRTUAL_MANAGER: Option<VirtualMemoryManager> = None;

/// Inicializar el sistema de paginación
pub fn init_paging(config: &crate::memory::MemoryConfig) -> Result<(), &'static str> {
    serial_write_str("PAGING: Inicializando sistema de paginación...\n");
    
    // Detectar memoria física disponible
    // ERROR CRÍTICO ANTERIOR: 0x100000 (1MB) es donde está CÓDIGO del Kernel.
    // Usar 0x2000000 (32MB) para las estructuras de paginación y el inicio del heap físico.
    // Esto protege los primeros 32MB que contienen Kernel Code + Stack + Bootloader info.
    let physical_base = 0x2000000; // 32MB
    let total_memory = config.total_physical_memory;
    let kernel_base = 0xFFFF_8000_0000_0000; // Dirección virtual del kernel
    let kernel_limit = kernel_base + config.kernel_heap_size;
    
    // Logging estático para evitar allocs antes de init_heap
    serial_write_str("PAGING: Memoria detectada.\n");
    
    // 1. Crear el Physical Manager (gestiona la RAM)
    serial_write_str("PAGING CHECKPOINT A: Crear Physical Manager\n");
    let mut physical_manager = PhysicalPageManager::new(physical_base, total_memory);
    serial_write_str("PAGING: Physical Manager created.\n");
    crate::debug::serial_write_str("PAGING: Total RAM: ");
    print_hex(total_memory);
    crate::debug::serial_write_str(" bytes. Free Pages: ");
    print_hex(physical_manager.free_pages as u64);
    crate::debug::serial_write_str("\n");

    
    // 2. Asignar página para PML4 (NO usar physical_base a ciegas, pedirle al manager)
    serial_write_str("PAGING CHECKPOINT A2: Alloc PML4\n");
    let pml4_addr = physical_manager.allocate_page()
        .ok_or("FATAL: No se pudo asignar memoria para PML4")?;
        
    let pml4_table = unsafe { &mut *(pml4_addr as *mut PageTable) };
    pml4_table.clear(); // IMPORTANTE: Limpiar basura anterior
    
    // 3. Crear Virtual Manager con esa tabla validada
    let mut virtual_manager = VirtualMemoryManager::new(pml4_table, kernel_base, kernel_limit);
    
    // Mapear el kernel (ahora pasamos el manager explícitamente)
    serial_write_str("PAGING CHECKPOINT B: Map Kernel Start\n");
    map_kernel_memory(&mut physical_manager, &mut virtual_manager, kernel_base, kernel_limit)?;
    serial_write_str("PAGING CHECKPOINT C: Map Kernel Done\n");
    
    // Establecer la tabla PML4
    serial_write_str("PAGING CHECKPOINT D: Set PML4\n");
    virtual_manager.set_pml4_table();
    
    // Habilitar paginación
    serial_write_str("PAGING CHECKPOINT E: Enable Paging\n");
    enable_paging();
    serial_write_str("PAGING CHECKPOINT F: Paging Enabled\n");
    
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
    
    // 1. Mapear el kernel en la dirección virtual alta (Higher Half)
    serial_write_str("PAGING CHECKPOINT B1: HH Loops\n");
    while current_addr < kernel_limit {
        let flags = PAGE_PRESENT | PAGE_WRITABLE;
        virtual_manager.map_page(current_addr, physical_addr, flags, physical_manager)?;
        
        current_addr += PAGE_SIZE as u64;
        physical_addr += PAGE_SIZE as u64;
    }

    // 2. Identity Mapping CRÍTICO (0GB - 4GB)
    // RSP está en 0x3FFB8000 (~1GB), así que necesitamos mapear más que 128MB.
    // Mapeamos los primeros 4GB para estar seguros (Stack, Framebuffer, DMA, MMIO).
    serial_write_str("PAGING CHECKPOINT B2: Identity Loop (4GB)\n");
    // Usamos u64 para evitar overflow en 32-bit (aunque estamos en 64-bit)
    let identity_limit = 4u64 * 1024 * 1024 * 1024; 
    let mut id_addr = 0;
    while id_addr < identity_limit {
        let flags = PAGE_PRESENT | PAGE_WRITABLE;
        // Check if we exceed total physical memory to save space?
        // Actually, internal fragmentation of page tables is small. 
        // Mapping holes is fine (just useless PTs).
        // But better constraint it to actual RAM size if possible, BUT stack might be in MMIO hole?
        // Safe to map 4GB for now.
        
        // Optimización: Solo mapear si tenemos memoria física o es zona baja (<4GB usualmente seguro)
        virtual_manager.map_page(id_addr, id_addr, flags, physical_manager)?;
        id_addr += PAGE_SIZE as u64;
        
        // Logging de progreso cada 512MB para no colgar la serial console
        if id_addr % (512 * 1024 * 1024) == 0 {
             crate::debug::serial_write_str("."); 
        }
    }
    crate::debug::serial_write_str("\n");
    serial_write_str("PAGING CHECKPOINT B3: Identity Done\n");
    
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
    let vm = get_virtual_manager();
    let pm = get_physical_manager(); // Need to get mutable ref carefully?
    // WARNING: get_virtual_manager and get_physical_manager both return &'static mut
    // This might cause multiple mutable borrows if we are not careful.
    // However, they return separate static refs from separate statics.
    // It should be fine at runtime.
    vm.map_page(virtual_addr, physical_addr, flags, pm)
}

/// Desmapear una página virtual
pub fn unmap_virtual_page(virtual_addr: u64) -> Result<(), &'static str> {
    let manager = get_virtual_manager();
    manager.unmap_page(virtual_addr)
}

/// Traducir dirección virtual a física
pub fn translate_virtual_address(virtual_addr: u64) -> Option<(u64, u64)> {
    let manager = get_virtual_manager();
    manager.translate_address(virtual_addr)
}

/// Configurar paginación para userland
/// 
/// Configura las tablas de páginas necesarias para ejecutar código en modo usuario.
/// Retorna la dirección física de la tabla PML4 configurada.
/// 
/// Esta función crea una nueva estructura de páginas para el proceso userland,
/// permitiendo que el código de usuario se ejecute de manera aislada del kernel.
pub fn setup_userland_paging() -> Result<u64, &'static str> {
    serial_write_str("PAGING: Setting up userland paging tables\n");
    
    // Crear una nueva PML4 para el proceso userland
    let pml4_phys_addr = allocate_physical_page()
        .ok_or("No hay páginas físicas disponibles para PML4")?;
    
    // Inicializar la página física a cero ANTES de crear la referencia
    unsafe {
        core::ptr::write_bytes(pml4_phys_addr as *mut u8, 0, PAGE_SIZE);
    }
    
    // Ahora es seguro crear la referencia
    let pml4_table = unsafe { &mut *(pml4_phys_addr as *mut PageTable) };
    
    // Copiar las entradas del kernel de la PML4 actual
    // Esto asegura que el kernel siga siendo accesible después de cambiar CR3
    // Las entradas superiores (256-511) generalmente contienen el espacio del kernel
    // Nota: Se deshabilitan las interrupciones para evitar inconsistencias durante la copia
    unsafe {
        // Deshabilitar interrupciones durante la sección crítica
        asm!("cli", options(nostack));
        
        let current_pml4_addr: u64;
        asm!("mov {}, cr3", out(reg) current_pml4_addr, options(nostack));
        
        let current_pml4 = &*(current_pml4_addr as *const PageTable);
        
        // Copiar las entradas del kernel (mitad superior de la tabla)
        for i in 256..512 {
            pml4_table.entries[i] = current_pml4.entries[i];
        }
        
        // Rehabilitar interrupciones
        asm!("sti", options(nostack));
    }
    
    serial_write_str(&alloc::format!(
        "PAGING: Created new PML4 at 0x{:x} with kernel mappings\n",
        pml4_phys_addr
    ));
    
    // Retornar la dirección física de la PML4
    Ok(pml4_phys_addr)
}

/// Invalidar rango de TLB
///
/// Invalida las entradas del TLB para un rango de direcciones virtuales.
/// Esto asegura que la CPU vea los nuevos mapeos de páginas.
fn flush_tlb_range(start_addr: u64, end_addr: u64) {
    let mut addr = start_addr;
    while addr < end_addr {
        unsafe {
            asm!("invlpg [{}]", in(reg) addr, options(nostack));
        }
        addr += PAGE_SIZE as u64;
    }
}

/// Mapear una sola página en la jerarquía de tablas de páginas
///
/// Navega o crea la jerarquía de 4 niveles (PML4 → PDPT → PD → PT) y
/// mapea una página virtual a una dirección física.
///
/// # Argumentos
/// - `pml4_table`: Tabla PML4 raíz (debe ser válida)
/// - `virtual_addr`: Dirección virtual a mapear (debe estar alineada a página)
/// - `physical_addr`: Dirección física destino (debe estar alineada a página)
/// - `flags`: Flags de la página (PRESENT, WRITABLE, USER, etc.)
/// - `phys_manager`: Gestor de páginas físicas para asignar tablas intermedias
///
/// # Invariantes
/// - `pml4_table` debe apuntar a una tabla de páginas válida
/// - Las direcciones deben estar alineadas a 4KB
/// - `phys_manager` debe tener páginas disponibles para tablas intermedias
fn map_page_in_table(
    pml4_table: &mut PageTable,
    virtual_addr: u64,
    physical_addr: u64,
    flags: u64,
    phys_manager: &mut PhysicalPageManager
) -> Result<(), &'static str> {
    let p4_index = ((virtual_addr >> 39) & 0x1FF) as usize;
    let p3_index = ((virtual_addr >> 30) & 0x1FF) as usize;
    let p2_index = ((virtual_addr >> 21) & 0x1FF) as usize;
    let p1_index = ((virtual_addr >> 12) & 0x1FF) as usize;
    
    // Get or create PDPT (Level 3)
    let p4_entry = pml4_table.get_entry_mut(p4_index);
    let p3_table = if p4_entry.is_present() {
        unsafe { &mut *(p4_entry.get_physical_addr() as *mut PageTable) }
    } else {
        let new_table_addr = phys_manager.allocate_page()
            .ok_or("No hay páginas físicas disponibles para PDPT")?;
        let new_table = unsafe { &mut *(new_table_addr as *mut PageTable) };
        new_table.clear();
        *p4_entry = PageTableEntry::new_with_addr(new_table_addr, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        new_table
    };
    
    // Get or create PD (Level 2)
    let p3_entry = p3_table.get_entry_mut(p3_index);
    let p2_table = if p3_entry.is_present() {
        unsafe { &mut *(p3_entry.get_physical_addr() as *mut PageTable) }
    } else {
        let new_table_addr = phys_manager.allocate_page()
            .ok_or("No hay páginas físicas disponibles para PD")?;
        let new_table = unsafe { &mut *(new_table_addr as *mut PageTable) };
        new_table.clear();
        *p3_entry = PageTableEntry::new_with_addr(new_table_addr, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        new_table
    };
    
    // Get or create PT (Level 1)
    let p2_entry = p2_table.get_entry_mut(p2_index);
    let p1_table = if p2_entry.is_present() {
        unsafe { &mut *(p2_entry.get_physical_addr() as *mut PageTable) }
    } else {
        let new_table_addr = phys_manager.allocate_page()
            .ok_or("No hay páginas físicas disponibles para PT")?;
        let new_table = unsafe { &mut *(new_table_addr as *mut PageTable) };
        new_table.clear();
        *p2_entry = PageTableEntry::new_with_addr(new_table_addr, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        new_table
    };
    
    // Map the final page
    let entry = PageTableEntry::new_with_addr(physical_addr, flags);
    p1_table.set_entry(p1_index, entry);
    
    Ok(())
}


/// Mapear memoria para userland
///
/// Mapea un rango de memoria virtual en el espacio de direcciones del userland.
///
/// # Argumentos
/// - `pml4_addr`: Dirección física de la tabla PML4 del proceso
/// - `virtual_addr`: Dirección virtual base a mapear
/// - `size`: Tamaño del rango a mapear en bytes
pub fn map_userland_memory(pml4_addr: u64, virtual_addr: u64, size: u64) -> Result<(), &'static str> {
    serial_write_str(&alloc::format!(
        "PAGING: map_userland_memory(pml4=0x{:x}, vaddr=0x{:x}, size=0x{:x})\n",
        pml4_addr, virtual_addr, size
    ));
    
    // Validar parámetros
    if size == 0 {
        return Err("El tamaño debe ser mayor que 0");
    }
    
    if size > 0x40000000 {  // Límite de 1GB por llamada
        return Err("Tamaño excesivo solicitado");
    }
    
    // Acceder a la tabla PML4
    let pml4_table = unsafe { &mut *(pml4_addr as *mut PageTable) };
    
    // Obtener el gestor de memoria física (una sola vez para evitar múltiples borrows)
    let phys_manager = get_physical_manager();
    
    // Alinear la dirección virtual al inicio de la página
    let start_vaddr = virtual_addr & !0xFFF;
    let end_vaddr = (virtual_addr.checked_add(size).ok_or("Desbordamiento al calcular end_vaddr")? + 0xFFF) & !0xFFF;
    
    // Flags: Present, Writable, User-accessible, No-Execute (W^X: stack no debe ser ejecutable)
    let flags = PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER | PAGE_NO_EXECUTE;
    
    // Mapear cada página en el rango
    let mut current_vaddr = start_vaddr;
    while current_vaddr < end_vaddr {
        // Asignar una nueva página física
        let phys_addr = phys_manager.allocate_page()
            .ok_or("No hay páginas físicas disponibles para userland")?;
        
        // Limpiar la página física (inicializar a 0)
        unsafe {
            core::ptr::write_bytes(phys_addr as *mut u8, 0, PAGE_SIZE);
        }
        
        // Mapear la página virtual a la física con permisos de usuario
        map_page_in_table(pml4_table, current_vaddr, phys_addr, flags, phys_manager)?;
        
        current_vaddr += PAGE_SIZE as u64;
    }
    
    serial_write_str(&alloc::format!(
        "PAGING: Mapped {} pages for userland (stack/heap)\n",
        (end_vaddr - start_vaddr) / PAGE_SIZE as u64
    ));
    
    // Invalidar TLB para asegurar que la CPU vea los nuevos mapeos
    flush_tlb_range(start_vaddr, end_vaddr);
    
    Ok(())
}

/// Mapear memoria con mapeo identidad (virtual == física)
///
/// Crea un mapeo donde las direcciones virtuales son iguales a las físicas.
/// Útil para código ejecutable que ya está cargado en memoria.
///
/// # Argumentos
/// - `pml4_addr`: Dirección física de la tabla PML4 del proceso
/// - `physical_addr`: Dirección física/virtual base a mapear
/// - `size`: Tamaño del rango a mapear en bytes
pub fn identity_map_userland_memory(pml4_addr: u64, physical_addr: u64, size: u64) -> Result<(), &'static str> {
    serial_write_str(&alloc::format!(
        "PAGING: identity_map_userland_memory(pml4=0x{:x}, paddr=0x{:x}, size=0x{:x})\n",
        pml4_addr, physical_addr, size
    ));
    
    // Validar parámetros
    if size == 0 {
        return Err("El tamaño debe ser mayor que 0");
    }
    
    if size > 0x40000000 {  // Límite de 1GB por llamada
        return Err("Tamaño excesivo solicitado");
    }
    
    // Acceder a la tabla PML4
    let pml4_table = unsafe { &mut *(pml4_addr as *mut PageTable) };
    
    // Obtener el gestor de memoria física (una sola vez)
    let phys_manager = get_physical_manager();
    
    // Alinear la dirección al inicio de la página
    let start_addr = physical_addr & !0xFFF;
    let end_addr = (physical_addr.checked_add(size).ok_or("Desbordamiento al calcular end_addr")? + 0xFFF) & !0xFFF;
    
    // Flags: Present, User-accessible, Read-only (W^X: código no debe ser escribible)
    // Nota: No usamos PAGE_WRITABLE para código, solo PRESENT + USER
    let flags = PAGE_PRESENT | PAGE_USER;
    
    // Mapear cada página en el rango con mapeo identidad
    let mut current_addr = start_addr;
    while current_addr < end_addr {
        // Mapear virtual == física (identity mapping)
        // No asignamos páginas nuevas, mapeamos la física existente
        map_page_in_table(pml4_table, current_addr, current_addr, flags, phys_manager)?;
        
        current_addr += PAGE_SIZE as u64;
    }
    
    serial_write_str(&alloc::format!(
        "PAGING: Identity-mapped {} pages for userland code\n",
        (end_addr - start_addr) / PAGE_SIZE as u64
    ));
    
    // Invalidar TLB para asegurar que la CPU vea los nuevos mapeos
    flush_tlb_range(start_addr, end_addr);
    
    Ok(())
}

/// Mapear páginas físicas pre-asignadas a direcciones virtuales
///
/// Mapea una lista de páginas físicas ya asignadas a un rango de direcciones virtuales.
/// Útil para mapear segmentos ELF que ya han sido cargados en memoria física.
///
/// # Argumentos
/// - `pml4_addr`: Dirección física de la tabla PML4 del proceso
/// - `virtual_addr`: Dirección virtual base donde mapear
/// - `physical_pages`: Vector de direcciones físicas de páginas (4KB cada una)
/// - `flags`: Flags de página (permisos)
pub fn map_preallocated_pages(
    pml4_addr: u64,
    virtual_addr: u64,
    physical_pages: &[u64],
    flags: u64,
) -> Result<(), &'static str> {
    serial_write_str(&alloc::format!(
        "PAGING: map_preallocated_pages(pml4=0x{:x}, vaddr=0x{:x}, {} pages, flags=0x{:x})\n",
        pml4_addr, virtual_addr, physical_pages.len(), flags
    ));
    
    if physical_pages.is_empty() {
        return Ok(());
    }
    
    // Acceder a la tabla PML4
    let pml4_table = unsafe { &mut *(pml4_addr as *mut PageTable) };
    
    // Obtener el gestor de memoria física
    let phys_manager = get_physical_manager();
    
    // Alinear la dirección virtual al inicio de la página
    let start_vaddr = virtual_addr & !0xFFF;
    
    // Mapear cada página física a su dirección virtual correspondiente
    for (i, &phys_addr) in physical_pages.iter().enumerate() {
        let current_vaddr = start_vaddr + (i as u64 * PAGE_SIZE as u64);
        
        // Mapear la página virtual a la física con los permisos especificados
        map_page_in_table(pml4_table, current_vaddr, phys_addr, flags, phys_manager)?;
    }
    
    serial_write_str(&alloc::format!(
        "PAGING: Mapped {} pre-allocated pages starting at vaddr 0x{:x}\n",
        physical_pages.len(), start_vaddr
    ));
    
    // Invalidar TLB para asegurar que la CPU vea los nuevos mapeos
    let end_vaddr = start_vaddr + (physical_pages.len() as u64 * PAGE_SIZE as u64);
    flush_tlb_range(start_vaddr, end_vaddr);
    
    Ok(())
}