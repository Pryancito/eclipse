//! Sistema de gestión de memoria del microkernel
//! 
//! Implementa:
//! - Paginación básica
//! - Heap allocator
//! - Gestión de memoria física

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use spin::Mutex;

/// Tamaño del heap del kernel (2 MB)
const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Heap estático del kernel
#[repr(align(4096))]
struct KernelHeap {
    memory: [u8; HEAP_SIZE],
}

static mut HEAP: KernelHeap = KernelHeap {
    memory: [0; HEAP_SIZE],
};

/// Información de bloques libres
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

/// Allocator simple basado en lista enlazada
pub struct SimpleAllocator {
    free_list: Mutex<*mut FreeBlock>,
}

unsafe impl Send for SimpleAllocator {}
unsafe impl Sync for SimpleAllocator {}

unsafe impl GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(layout.align());
        let aligned_size = (size + 15) & !15; // Alinear a 16 bytes
        
        let mut free_list = self.free_list.lock();
        let mut current = *free_list;
        let mut prev: *mut *mut FreeBlock = &mut *free_list;
        
        // Buscar bloque libre
        while !current.is_null() {
            let block = &mut *current;
            if block.size >= aligned_size {
                // Dividir bloque si es demasiado grande
                if block.size > aligned_size + core::mem::size_of::<FreeBlock>() {
                    let new_block = (current as usize + aligned_size) as *mut FreeBlock;
                    (*new_block).size = block.size - aligned_size;
                    (*new_block).next = block.next;
                    *prev = new_block;
                } else {
                    *prev = block.next;
                }
                return current as *mut u8;
            }
            prev = &mut block.next;
            current = block.next;
        }
        
        null_mut()
    }
    
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size().max(layout.align());
        let aligned_size = (size + 15) & !15;
        
        let block = ptr as *mut FreeBlock;
        (*block).size = aligned_size;
        
        let mut free_list = self.free_list.lock();
        (*block).next = *free_list;
        *free_list = block;
    }
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator {
    free_list: Mutex::new(null_mut()),
};

/// Inicializar el sistema de memoria
pub fn init() {
    unsafe {
        // Inicializar el heap con un bloque libre grande
        let heap_start = HEAP.memory.as_mut_ptr() as *mut FreeBlock;
        (*heap_start).size = HEAP_SIZE;
        (*heap_start).next = null_mut();
        
        *ALLOCATOR.free_list.lock() = heap_start;
    }
}

/// Estructura de entrada de tabla de páginas
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PageTableEntry {
    entry: u64,
}

impl PageTableEntry {
    pub const fn new() -> Self {
        Self { entry: 0 }
    }
    
    pub fn set_addr(&mut self, addr: u64, flags: u64) {
        self.entry = (addr & 0x000F_FFFF_FFFF_F000) | flags;
    }
    
    pub fn present(&self) -> bool {
        self.entry & 0x1 != 0
    }
    
    pub fn get_addr(&self) -> u64 {
        self.entry & 0x000F_FFFF_FFFF_F000
    }
}

/// Tabla de páginas
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    pub const fn new() -> Self {
        Self {
            entries: [PageTableEntry::new(); 512],
        }
    }
}

// Flags de paginación
pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
pub const PAGE_USER: u64 = 1 << 2;
pub const PAGE_WRITE_THROUGH: u64 = 1 << 3;
pub const PAGE_CACHE_DISABLE: u64 = 1 << 4;
pub const PAGE_ACCESSED: u64 = 1 << 5;
pub const PAGE_DIRTY: u64 = 1 << 6;
pub const PAGE_HUGE: u64 = 1 << 7;
pub const PAGE_GLOBAL: u64 = 1 << 8;

/// Tablas de páginas estáticas para el kernel
static mut PML4: PageTable = PageTable::new();
static mut PDPT: PageTable = PageTable::new();
static mut PD: PageTable = PageTable::new();

/// Inicializar paginación
pub fn init_paging() {
    unsafe {
        // Configurar identity mapping para los primeros 2GB
        // PML4[0] -> PDPT
        PML4.entries[0].set_addr(
            &PDPT as *const _ as u64,
            PAGE_PRESENT | PAGE_WRITABLE
        );
        
        // PML4[511] -> PDPT (higher half)
        PML4.entries[511].set_addr(
            &PDPT as *const _ as u64,
            PAGE_PRESENT | PAGE_WRITABLE
        );
        
        // PDPT[0] -> PD
        PDPT.entries[0].set_addr(
            &PD as *const _ as u64,
            PAGE_PRESENT | PAGE_WRITABLE
        );
        
        // PD: Mapear 1GB con páginas de 2MB (huge pages)
        for i in 0..512 {
            PD.entries[i].set_addr(
                (i as u64) * 0x200000, // 2MB per entry
                PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE
            );
        }
        
        // Cargar CR3 con PML4
        let pml4_addr = &PML4 as *const _ as u64;
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) pml4_addr,
            options(nostack, preserves_flags)
        );
    }
    
    crate::serial::serial_print("Paging enabled\n");
}

/// Obtener dirección física de CR3
pub fn get_cr3() -> u64 {
    let cr3: u64;
    unsafe {
        core::arch::asm!(
            "mov {}, cr3",
            out(reg) cr3,
            options(nostack, preserves_flags)
        );
    }
    cr3
}
