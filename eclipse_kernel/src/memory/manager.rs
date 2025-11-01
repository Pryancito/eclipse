//! Gestor de Memoria Avanzado para Eclipse OS
//!
//! Implementa paginación, memoria virtual y asignación dinámica

use core::alloc::{GlobalAlloc, Layout};

/// Tamaño de página estándar (4KB)
pub const PAGE_SIZE: usize = 4096;

/// Número de entradas en una tabla de páginas
pub const PAGE_TABLE_ENTRIES: usize = 512;

/// Máscaras para flags de página
pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
pub const PAGE_USER: u64 = 1 << 2;
pub const PAGE_WRITE_THROUGH: u64 = 1 << 3;
pub const PAGE_CACHE_DISABLE: u64 = 1 << 4;
pub const PAGE_SIZE_2MB: u64 = 1 << 7;

/// Niveles de tabla de páginas
pub const PAGE_LEVELS: usize = 4;

/// Dirección virtual del kernel
pub const KERNEL_VIRTUAL_BASE: u64 = 0xffff800000000000;

/// Estructura para una entrada de tabla de páginas
#[derive(Debug, Clone, Copy)]
pub struct PageTableEntry {
    pub value: u64,
}

impl PageTableEntry {
    /// Crear una nueva entrada vacía
    pub const fn new() -> Self {
        Self { value: 0 }
    }

    /// Crear una entrada con flags específicos
    pub fn new_with_flags(physical_addr: u64, flags: u64) -> Self {
        Self {
            value: (physical_addr & 0x000ffffffffff000) | (flags & 0xfff),
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
    pub fn is_user(&self) -> bool {
        (self.value & PAGE_USER) != 0
    }

    /// Obtener la dirección física
    pub fn physical_address(&self) -> u64 {
        self.value & 0x000ffffffffff000
    }

    /// Establecer la dirección física
    pub fn set_physical_address(&mut self, addr: u64) {
        self.value = (self.value & 0xfff) | (addr & 0x000ffffffffff000);
    }

    /// Establecer flags
    pub fn set_flags(&mut self, flags: u64) {
        self.value = (self.value & 0x000ffffffffff000) | (flags & 0xfff);
    }

    /// Agregar flags
    pub fn add_flags(&mut self, flags: u64) {
        self.value |= flags & 0xfff;
    }

    /// Remover flags
    pub fn remove_flags(&mut self, flags: u64) {
        self.value &= !(flags & 0xfff);
    }
}

/// Tabla de páginas
#[repr(align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; PAGE_TABLE_ENTRIES],
}

impl PageTable {
    /// Crear una nueva tabla de páginas vacía
    pub const fn new() -> Self {
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
}

/// Gestor de memoria principal
pub struct MemoryManager {
    /// Tabla de páginas de nivel 4 (PML4)
    pub pml4: PageTable,
    /// Tabla de páginas de nivel 3 (PDPT)
    pub pdpt: PageTable,
    /// Tabla de páginas de nivel 2 (PD)
    pub pd: PageTable,
    /// Tabla de páginas de nivel 1 (PT)
    pub pt: PageTable,
    /// Bitmap de páginas físicas libres
    pub free_pages_bitmap: [u64; 1024], // 64KB para 4GB de RAM
    /// Dirección base de la memoria física
    pub physical_memory_base: u64,
    /// Tamaño total de memoria física
    pub physical_memory_size: u64,
    /// Dirección del siguiente frame libre
    pub next_free_frame: usize,
}

impl MemoryManager {
    /// Crear un nuevo gestor de memoria
    pub fn new(physical_base: u64, memory_size: u64) -> Self {
        Self {
            pml4: PageTable::new(),
            pdpt: PageTable::new(),
            pd: PageTable::new(),
            pt: PageTable::new(),
            free_pages_bitmap: [0; 1024],
            physical_memory_base: physical_base,
            physical_memory_size: memory_size,
            next_free_frame: 0,
        }
    }

    /// Inicializar el gestor de memoria
    pub fn init(&mut self) -> Result<(), &'static str> {
        // Configurar mapeo de identidad para el kernel
        self.setup_identity_mapping()?;

        // Configurar mapeo virtual del kernel
        self.setup_kernel_mapping()?;

        // Inicializar bitmap de páginas libres
        self.init_free_pages_bitmap();

        Ok(())
    }

    /// Configurar mapeo de identidad para el kernel
    fn setup_identity_mapping(&mut self) -> Result<(), &'static str> {
        // Mapear los primeros 2MB de memoria física
        let flags = PAGE_PRESENT | PAGE_WRITABLE;

        // Configurar PML4
        self.pml4.set_entry(
            0,
            PageTableEntry::new_with_flags(&self.pdpt as *const _ as u64, flags),
        );

        // Configurar PDPT
        self.pdpt.set_entry(
            0,
            PageTableEntry::new_with_flags(&self.pd as *const _ as u64, flags),
        );

        // Configurar PD con páginas de 2MB
        // Necesitamos mapear más de 2MB para acomodar el heap del kernel (32MB) y
        // otras secciones de datos. Mapeamos los primeros 64MB de memoria física
        // usando páginas enormes (2MB) para tener margen suficiente.
        let identity_map_size: u64 = 64 * 1024 * 1024; // 64MB
        let huge_page_size: u64 = 2 * 1024 * 1024; // 2MB
        let required_entries = (identity_map_size / huge_page_size) as usize;

        for i in 0..required_entries {
            let phys_addr = (i as u64) * huge_page_size;
            self.pd.set_entry(
                i,
                PageTableEntry::new_with_flags(phys_addr, flags | PAGE_SIZE_2MB),
            );
        }

        Ok(())
    }

    /// Configurar mapeo virtual del kernel
    fn setup_kernel_mapping(&mut self) -> Result<(), &'static str> {
        // Mapear el kernel en la zona virtual alta
        let kernel_virtual_index = (KERNEL_VIRTUAL_BASE >> 39) & 0x1ff;
        let flags = PAGE_PRESENT | PAGE_WRITABLE;

        // Configurar entrada en PML4 para el kernel
        self.pml4.set_entry(
            kernel_virtual_index as usize,
            PageTableEntry::new_with_flags(&self.pdpt as *const _ as u64, flags),
        );

        // Nota: las mismas entradas del PD que configuramos para el mapeo de identidad
        // también proporcionan el mapeo en la zona virtual alta del kernel, ya que
        // apuntamos a la misma tabla de directorio de páginas (PD).

        Ok(())
    }

    /// Inicializar bitmap de páginas libres
    fn init_free_pages_bitmap(&mut self) {
        // Marcar todas las páginas como libres inicialmente
        for i in 0..1024 {
            self.free_pages_bitmap[i] = 0;
        }

        // Marcar las páginas usadas por el kernel como ocupadas
        let kernel_pages = (self.physical_memory_size / PAGE_SIZE as u64) / 8; // Aproximadamente
        for i in 0..kernel_pages as usize {
            let byte_index = i / 64;
            let bit_index = i % 64;
            if byte_index < 1024 {
                self.free_pages_bitmap[byte_index] |= 1 << bit_index;
            }
        }
    }

    /// Asignar una página física
    pub fn allocate_physical_page(&mut self) -> Option<u64> {
        for (byte_index, bitmap_byte) in self.free_pages_bitmap.iter_mut().enumerate() {
            if *bitmap_byte != 0xffffffffffffffff {
                // Encontrar el primer bit libre
                for bit_index in 0..64 {
                    if (*bitmap_byte & (1 << bit_index)) == 0 {
                        // Marcar como ocupada
                        *bitmap_byte |= 1 << bit_index;

                        // Calcular dirección física
                        let page_index = byte_index * 64 + bit_index;
                        let physical_addr =
                            self.physical_memory_base + (page_index as u64 * PAGE_SIZE as u64);

                        return Some(physical_addr);
                    }
                }
            }
        }
        None
    }

    /// Liberar una página física
    pub fn deallocate_physical_page(&mut self, physical_addr: u64) {
        let page_index = ((physical_addr - self.physical_memory_base) / PAGE_SIZE as u64) as usize;
        let byte_index = page_index / 64;
        let bit_index = page_index % 64;

        if byte_index < 1024 {
            self.free_pages_bitmap[byte_index] &= !(1 << bit_index);
        }
    }

    /// Mapear una página virtual a una página física
    pub fn map_page(
        &mut self,
        virtual_addr: u64,
        physical_addr: u64,
        flags: u64,
    ) -> Result<(), &'static str> {
        let pml4_index = (virtual_addr >> 39) & 0x1ff;
        let pdpt_index = (virtual_addr >> 30) & 0x1ff;
        let pd_index = (virtual_addr >> 21) & 0x1ff;
        let pt_index = (virtual_addr >> 12) & 0x1ff;

        // Verificar si la entrada PML4 existe
        if !self.pml4.get_entry(pml4_index as usize).is_present() {
            // Crear nueva tabla PDPT
            let new_pdpt = PageTable::new();
            let pdpt_addr = &new_pdpt as *const _ as u64;
            self.pml4.set_entry(
                pml4_index as usize,
                PageTableEntry::new_with_flags(pdpt_addr, PAGE_PRESENT | PAGE_WRITABLE),
            );
        }

        // Similar para PDPT, PD y PT...
        // (Implementación simplificada)

        Ok(())
    }

    /// Desmapear una página virtual
    pub fn unmap_page(&mut self, virtual_addr: u64) -> Result<(), &'static str> {
        // Implementación de desmapeo
        Ok(())
    }

    /// Obtener la dirección física de una dirección virtual
    pub fn virtual_to_physical(&self, virtual_addr: u64) -> Option<u64> {
        // Implementación de traducción de direcciones
        Some(virtual_addr) // Simplificado por ahora
    }
}

/// Allocator global para el kernel
pub struct KernelAllocator {
    memory_manager: &'static mut MemoryManager,
}

impl KernelAllocator {
    /// Crear un nuevo allocator del kernel
    pub fn new(memory_manager: &'static mut MemoryManager) -> Self {
        Self { memory_manager }
    }
}

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        // Implementación simplificada - siempre falla por ahora
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Implementación simplificada - no hace nada por ahora
    }
}

use core::sync::atomic::{AtomicBool, Ordering};

/// Estado de inicialización del gestor de memoria
static MEMORY_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Instancia global del gestor de memoria (más segura)
static mut MEMORY_MANAGER: Option<MemoryManager> = None;

/// Inicializar el gestor de memoria global
pub fn init_memory_manager(physical_base: u64, memory_size: u64) -> Result<(), &'static str> {
    if MEMORY_INITIALIZED.load(Ordering::Acquire) {
        return Err("Memory manager already initialized");
    }

    unsafe {
        MEMORY_MANAGER = Some(MemoryManager::new(physical_base, memory_size));
        if let Some(ref mut manager) = MEMORY_MANAGER {
            manager.init()?;
        }
    }

    MEMORY_INITIALIZED.store(true, Ordering::Release);
    Ok(())
}

/// Obtener el gestor de memoria global (versión más segura)
pub fn get_memory_manager() -> Option<&'static mut MemoryManager> {
    if !MEMORY_INITIALIZED.load(Ordering::Acquire) {
        return None;
    }
    unsafe { MEMORY_MANAGER.as_mut() }
}

/// Verificar si el gestor de memoria está inicializado
pub fn is_memory_initialized() -> bool {
    MEMORY_INITIALIZED.load(Ordering::Acquire)
}
