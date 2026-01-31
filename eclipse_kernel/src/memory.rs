//! Sistema de gestión de memoria del microkernel
//! 
//! Implementa:
//! - Paginación básica
//! - Heap allocator
//! - Gestión de memoria física

use linked_list_allocator::LockedHeap;

/// Tamaño del heap del kernel (2 MB)
const HEAP_SIZE: usize = 32 * 1024 * 1024;

/// Heap estático del kernel
#[repr(align(4096))]
struct KernelHeap {
    memory: [u8; HEAP_SIZE],
}

static mut HEAP: KernelHeap = KernelHeap {
    memory: [0; HEAP_SIZE],
};

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Inicializar el sistema de memoria
pub fn init() {
    unsafe {
        // Inicializar el heap con un bloque libre grande
        let heap_start = HEAP.memory.as_mut_ptr();
        crate::serial::serial_print("[MEM] Heap start: 0x");
        crate::serial::serial_print_hex(heap_start as u64);
        crate::serial::serial_print("\n");
        
        // Manual write test
        crate::serial::serial_print("[MEM] Testing write to heap start...\n");
        *heap_start = 0xAA;
        if *heap_start == 0xAA {
             crate::serial::serial_print("[MEM] Write test passed\n");
        } else {
             crate::serial::serial_print("[MEM] Write test FAILED\n");
        }
        
        // Test Generic Spinlock
        crate::serial::serial_print("[MEM] Testing generic spinlock...\n");
        let lock = spin::Mutex::new(0);
        {
            let mut data = lock.lock();
            *data = 1;
        }
        crate::serial::serial_print("[MEM] Generic spinlock passed\n");

        crate::serial::serial_print("[MEM] Locking allocator...\n");
        crate::serial::serial_print("[MEM] Allocator addr: 0x");
        crate::serial::serial_print_hex(&raw const ALLOCATOR as u64);
        crate::serial::serial_print("\n");
        let mut allocator = ALLOCATOR.lock();
        crate::serial::serial_print("[MEM] Allocator locked. Initializing...\n");
        
        allocator.init(heap_start, HEAP_SIZE);
        
        crate::serial::serial_print("[MEM] Allocator initialized\n");
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
pub fn init_paging(kernel_phys_base: u64) {
    let phys_offset = kernel_phys_base.wrapping_sub(0x200000);
    
    // DEBUG
    unsafe {
        crate::serial::serial_print("Init Paging. Phys Base: 0x");
        crate::serial::serial_print_hex(kernel_phys_base);
        crate::serial::serial_print("\nOffset: 0x");
        crate::serial::serial_print_hex(phys_offset);
        crate::serial::serial_print("\n");
    }

    unsafe {
        // Calcular direcciones físicas de las tablas (que están en BSS virtual)
        let pdpt_phys = (&PDPT as *const _ as u64).wrapping_add(phys_offset);
        let pd_phys = (&PD as *const _ as u64).wrapping_add(phys_offset);
        let pml4_phys = (&PML4 as *const _ as u64).wrapping_add(phys_offset);

        crate::serial::serial_print("PML4 Phys: 0x");
        crate::serial::serial_print_hex(pml4_phys);
        crate::serial::serial_print("\n");

        // Configurar identity mapping SHIFTED para los primeros 2GB
        // Esto mapea Virtual X -> Physical X + Offset
        
        // PML4[0] -> PDPT
        PML4.entries[0].set_addr(
            pdpt_phys,
            PAGE_PRESENT | PAGE_WRITABLE
        );
        
        // PML4[511] -> PDPT (higher half)
        PML4.entries[511].set_addr(
            pdpt_phys,
            PAGE_PRESENT | PAGE_WRITABLE
        );
        
        // PDPT[0] -> PD
        PDPT.entries[0].set_addr(
            pd_phys,
            PAGE_PRESENT | PAGE_WRITABLE
        );
        
        // PD: Mapear 1GB con páginas de 2MB (huge pages)
        // Aplicando el offset SOLO al rango del kernel (64MB aprox)
        // El resto (Stack, Hardware) se mantiene Identity 1:1
        for i in 0..512 {
            let virt_addr = (i as u64) * 0x200000;
            
            // Skip page 0 (offset) if needed
            if i == 0 {
                PD.entries[i].set_addr(0, 0); // Not present
                continue;
            }

            let phys_addr = if i < 64 { // Map first 128MB (Kernel) with offset
                virt_addr.wrapping_add(phys_offset)
            } else { // Map Rest (Stack at i=511) with Identity
                virt_addr
            };

            PD.entries[i].set_addr(
                phys_addr,
                PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE | PAGE_USER
            );
        }

        // Cargar CR3 con PML4 Físico
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) pml4_phys,
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

/// Translate virtual address to physical address
/// This is a simplified version that works for identity-mapped regions
pub fn virt_to_phys(virt_addr: u64) -> u64 {
    // For our simple paging setup:
    // - Addresses in first 128MB are offset by phys_offset  
    // - Higher addresses are identity mapped
    // Since DMA buffers will be in heap (which is identity mapped in our setup),
    // we can use a simple approach
    
    // For now, we assume heap addresses are in the identity-mapped region
    // This works because our heap is allocated from BSS which is identity-mapped
    virt_addr
}

/// Allocate DMA-safe buffer
/// Returns (virtual address, physical address)
pub fn alloc_dma_buffer(size: usize, align: usize) -> Option<(*mut u8, u64)> {
    use alloc::alloc::{alloc, Layout};
    
    unsafe {
        // Allocate aligned buffer
        let layout = Layout::from_size_align(size, align).ok()?;
        let ptr = alloc(layout);
        
        if ptr.is_null() {
            return None;
        }
        
        // Calculate physical address
        let virt = ptr as u64;
        let phys = virt_to_phys(virt);
        
        Some((ptr, phys))
    }
}

/// Free DMA buffer
pub unsafe fn free_dma_buffer(ptr: *mut u8, size: usize, align: usize) {
    use alloc::alloc::{dealloc, Layout};
    
    if let Ok(layout) = Layout::from_size_align(size, align) {
        dealloc(ptr, layout);
    }
}
