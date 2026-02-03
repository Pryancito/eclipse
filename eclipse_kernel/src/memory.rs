//! Sistema de gestión de memoria del microkernel
//! 
//! Implementa:
//! - Paginación básica
//! - Heap allocator
//! - Gestión de memoria física

use linked_list_allocator::LockedHeap;
use core::sync::atomic::{AtomicU64, Ordering};

/// Physical offset for virtual-to-physical address translation
/// Written once during init_paging(), then read-only
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Size of the kernel region with offset-based mapping (128MB = 64 * 2MB pages)
const KERNEL_REGION_SIZE: u64 = 0x8000000;

/// Tamaño del heap del kernel (128 MB)
const HEAP_SIZE: usize = 128 * 1024 * 1024;

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
        crate::serial::serial_print("[MEM] Heap start: ");
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
        crate::serial::serial_print("[MEM] Allocator addr: ");
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

    pub fn get_flags(&self) -> u64 {
        self.entry & 0xFFF
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

    pub fn entries_mut(&mut self) -> &mut [PageTableEntry; 512] {
        &mut self.entries
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
    
    // Store phys_offset for later use by virt_to_phys
    // Using Relaxed ordering is safe here because this runs during single-threaded init
    PHYS_OFFSET.store(phys_offset, Ordering::Relaxed);
    
    // DEBUG
    unsafe {
        crate::serial::serial_print("Init Paging. Phys Base: ");
        crate::serial::serial_print_hex(kernel_phys_base);
        crate::serial::serial_print("\nOffset: ");
        crate::serial::serial_print_hex(phys_offset);
        crate::serial::serial_print("\n");
    }

    unsafe {
        // Calcular direcciones físicas de las tablas (que están en BSS virtual)
        let pdpt_phys = (&PDPT as *const _ as u64).wrapping_add(phys_offset);
        let pd_phys = (&PD as *const _ as u64).wrapping_add(phys_offset);
        let pml4_phys = (&PML4 as *const _ as u64).wrapping_add(phys_offset);

        crate::serial::serial_print("PML4 Phys: ");
        crate::serial::serial_print_hex(pml4_phys);
        crate::serial::serial_print("\n");

        // Configurar identity mapping SHIFTED para los primeros 2GB
        // Esto mapea Virtual X -> Physical X + Offset
        
        // PML4[0] -> PDPT
        PML4.entries[0].set_addr(
            pdpt_phys,
            PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER
        );
        
        // PML4[511] -> PDPT (higher half)
        PML4.entries[511].set_addr(
            pdpt_phys,
            PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER
        );
        
        // PDPT[0] -> PD
        PDPT.entries[0].set_addr(
            pd_phys,
            PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER
        );
        
        // PD: Mapear 1GB con páginas de 2MB (huge pages)
        // Aplicando el offset SOLO al rango del kernel (64MB aprox)
        // El resto (Stack, Hardware) se mantiene Identity 1:1
        for i in 0..512 {
            let virt_addr = (i as u64) * 0x200000;
            
            // Map page 0 with identity mapping and cache-disabled for MMIO access
            // This enables access to memory-mapped I/O regions in low memory (0x0-0x200000)
            // such as PCI device BARs, VGA memory, and other hardware MMIO regions
            if i == 0 {
                PD.entries[i].set_addr(
                    0,
                    PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE | PAGE_CACHE_DISABLE
                );
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

/// Create a new isolated page table for a process
/// Returns the physical address of the PML4
/// Create a new isolated page table for a process
/// Returns the physical address of the PML4
pub fn create_process_paging() -> u64 {
    unsafe {
        // Use alloc_dma_buffer to avoid stack overflow with Box::new(PageTable::new())
        let (pml4_ptr, pml4_phys) = alloc_dma_buffer(4096, 4096).expect("Failed to allocate PML4");
        let (pdpt_ptr, pdpt_phys) = alloc_dma_buffer(4096, 4096).expect("Failed to allocate PDPT");
        let (pd_ptr, pd_phys) = alloc_dma_buffer(4096, 4096).expect("Failed to allocate PD");
        
        let pml4 = &mut *(pml4_ptr as *mut PageTable);
        let pdpt = &mut *(pdpt_ptr as *mut PageTable);
        let pd   = &mut *(pd_ptr as *mut PageTable);
        
        // Zero out the new tables
        core::ptr::write_bytes(pml4_ptr, 0, 4096);
        core::ptr::write_bytes(pdpt_ptr, 0, 4096);
        core::ptr::write_bytes(pd_ptr, 0, 4096);
        
        // Clone kernel mappings (entire first 1GB provided by PD[0])
        // This includes kernel code, heap, boot stack (at i=511), and MMIO (at i=0)
        for i in 0..512 {
            pd.entries[i] = PD.entries[i];
        }
        
        pdpt.entries[0].set_addr(pd_phys, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        pml4.entries[0].set_addr(pdpt_phys, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        
        // Also map kernel into higher half
        pml4.entries[511].set_addr(pdpt_phys, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        
        pml4_phys
    }
}

/// Clone an existing process's page table (deep copy of user-space mappings)
/// Returns the physical address of the child's PML4
pub fn clone_process_paging(parent_pml4_phys: u64) -> u64 {
    let phys_offset = PHYS_OFFSET.load(Ordering::Relaxed);
    
    // Parent tables (virtual access)
    let p_pml4_virt = parent_pml4_phys.wrapping_sub(phys_offset);
    let p_pml4 = unsafe { &*(p_pml4_virt as *const PageTable) };

    // Parent PDPT and PD
    let p_pdpt_phys = p_pml4.entries[0].get_addr();
    let p_pdpt = unsafe { &*(p_pdpt_phys.wrapping_sub(phys_offset) as *const PageTable) };
    let p_pd_phys = p_pdpt.entries[0].get_addr();
    let p_pd = unsafe { &*(p_pd_phys.wrapping_sub(phys_offset) as *const PageTable) };

    // Create a new table structure for the child
    // This clones the kernel mappings automatically
    let c_pml4_phys = create_process_paging();
    let c_pml4_virt = c_pml4_phys.wrapping_sub(phys_offset);
    let c_pml4 = unsafe { &mut *(c_pml4_virt as *mut PageTable) };

    // Child PDPT and PD
    let c_pdpt_phys = c_pml4.entries[0].get_addr();
    let c_pdpt = unsafe { &mut *(c_pdpt_phys.wrapping_sub(phys_offset) as *mut PageTable) };
    let c_pd_phys = c_pdpt.entries[0].get_addr();
    let c_pd = unsafe { &mut *(c_pd_phys.wrapping_sub(phys_offset) as *mut PageTable) };

    // Deep copy user regions (>= 128MB, entries 64-511 in PD)
    for i in 64..512 {
        let entry = p_pd.entries[i];
        if !entry.present() {
            continue;
        }

        let p_paddr = entry.get_addr();
        let vaddr = (i as u64) * 0x200000;

        // Clone if it's a "custom" mapping (phys != virt-identity)
        // or just clone everything user-mode for safety.
        // In our system, if it's a process mapping, it's a Huge page in user space.
        if entry.get_flags() & PAGE_USER != 0 {
            // Check if it's an identity mapping (Hardware/Identity region)
            // If it's not identity, it's an ELF segment or stack
            if p_paddr != vaddr {
                if let Some((child_kptr, child_paddr)) = alloc_dma_buffer(0x200000, 0x200000) {
                    let parent_kptr = p_paddr.wrapping_sub(phys_offset) as *const u8;
                    unsafe {
                        core::ptr::copy_nonoverlapping(parent_kptr, child_kptr, 0x200000);
                    }
                    c_pd.entries[i].set_addr(child_paddr, entry.get_flags());
                }
            } else {
                // Identity mapping, keep as is
                c_pd.entries[i] = entry;
            }
        }
    }

    c_pml4_phys
}

/// Map a 2MB page in a process's page table
/// This is a simplified version using 2MB pages for everything for now
pub fn map_user_page_2mb(pml4_phys: u64, vaddr: u64, paddr: u64, flags: u64) {
    let phys_offset = PHYS_OFFSET.load(Ordering::Relaxed);
    
    // Convert PML4 phys to virt to access it
    let pml4_virt = pml4_phys.wrapping_sub(phys_offset);
    let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
    
    let pml4_idx = ((vaddr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((vaddr >> 30) & 0x1FF) as usize;
    let pd_idx   = ((vaddr >> 21) & 0x1FF) as usize;
    
    // We assume the PDPT and PD already exist for the process (created in create_process_paging)
    // but only for the first entry. If mapping beyond 1GB, we'd need to allocate more.
    // For init and log service, they are well within the first 1GB (0x10000000 is 256MB).
    
    let pdpt_phys = pml4.entries[pml4_idx].get_addr();
    let pdpt_virt = pdpt_phys.wrapping_sub(phys_offset);
    let pdpt = unsafe { &mut *(pdpt_virt as *mut PageTable) };
    
    let pd_phys = pdpt.entries[pdpt_idx].get_addr();
    let pd_virt = pd_phys.wrapping_sub(phys_offset);
    let pd = unsafe { &mut *(pd_virt as *mut PageTable) };
    
    pd.entries[pd_idx].set_addr(paddr, flags | PAGE_HUGE | PAGE_PRESENT | PAGE_USER);
    
    // Flush TLB (expensive, but safe)
    unsafe {
        core::arch::asm!("mov rax, cr3", "mov cr3, rax", out("rax") _);
    }
}

/// Translate virtual address to physical address
/// 
/// The kernel uses two different virtual-to-physical mapping schemes:
/// 
/// 1. **Kernel region (0x0 - 0x8000000 / 128MB)**: Offset-based mapping
///    - Contains: .text, .rodata, .data, .bss (including heap), page tables
///    - Mapping: `physical = virtual + phys_offset`
///    - The phys_offset is determined during boot based on where bootloader loaded the kernel
///    - Example: If kernel loaded at physical 0x200000, offset = 0
///
/// 2. **Higher memory (>= 128MB)**: Identity mapping  
///    - Contains: Stack, user space, MMIO regions
///    - Mapping: `physical = virtual`
///
/// This dual scheme allows the kernel to be position-independent while keeping
/// higher memory simple for userspace and hardware access.
pub fn virt_to_phys(virt_addr: u64) -> u64 {
    // Using Relaxed ordering is safe because PHYS_OFFSET is written once during init
    let phys_offset = PHYS_OFFSET.load(Ordering::Relaxed);
    
    // Check if address is in the kernel region (first 128MB = 64 * 2MB pages)
    // These are mapped with phys_offset
    if virt_addr < KERNEL_REGION_SIZE {
        // Kernel region: virt + phys_offset = phys
        virt_addr.wrapping_add(phys_offset)
    } else {
        // Higher memory: identity mapped (virt = phys)
        virt_addr
    }
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
