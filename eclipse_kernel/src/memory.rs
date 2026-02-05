//! Microkernel memory management system
//! 
//! Implements:
//! - Basic paging
//! - Heap allocator
//! - Physical memory management

use linked_list_allocator::LockedHeap;
use core::sync::atomic::{AtomicU64, Ordering};

/// Higher Half offset for physical memory mapping
/// All physical RAM is mapped at this virtual address
/// Physical address X is accessible at (PHYS_MEM_OFFSET + X)
pub const PHYS_MEM_OFFSET: u64 = 0xFFFF800000000000;

/// Physical offset for virtual-to-physical address translation (legacy)
/// This is now set to 0 since we use higher half mapping
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Size of the kernel region with offset-based mapping (256MB = 128 * 2MB pages)
const KERNEL_REGION_SIZE: u64 = 0x10000000;

/// Kernel heap size (64 MB)
/// Reduced from 128MB to avoid physical memory conflict with the 3GB PCI hole
/// (Kernel starts at ~2.87GB, so a 128MB heap would cross the 3GB limit).
const HEAP_SIZE: usize = 64 * 1024 * 1024;

/// Static kernel heap
#[repr(align(4096))]
struct KernelHeap {
    memory: [u8; HEAP_SIZE],
}

/// Tablas de páginas estáticas para el kernel
/// Definidas ANTES del heap para asegurar que estén en memoria física más baja
#[repr(align(4096))]
struct PagingTable {
    table: PageTable,
}

#[link_section = ".page_tables"]
static mut PML4: PageTable = PageTable::new();
#[link_section = ".page_tables"]
static mut PDPT: PageTable = PageTable::new();
#[link_section = ".page_tables"]
static mut PD: [PageTable; 4] = [PageTable::new(), PageTable::new(), PageTable::new(), PageTable::new()];

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
        // Clear bit 63 (NX) from address to ensure pages are executable by default
        // The NX bit will only be set if explicitly included in flags
        self.entry = (addr & 0x000F_FFFF_FFFF_F000) | (flags & 0x8000_0000_0000_0FFF);
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

// Las tablas ahora están arriba para mayor seguridad

/// Inicializar paginación
/// 
/// With Higher Half Kernel, the bootloader has already set up page tables:
/// - PML4[0]: Identity mapping (0-4GB) for bootloader compatibility
/// - PML4[256]: Higher half physical map (0xFFFF800000000000+)
/// - PML4[511]: Recursive mapping for page table access
/// 
/// The kernel just needs to acknowledge this setup and continue using it.
/// No need to create new page tables or switch CR3!
pub fn init_paging(kernel_phys_base: u64) {
    crate::serial::serial_print("Init Paging (Higher Half mode)\n");
    crate::serial::serial_print("Kernel phys base: ");
    crate::serial::serial_print_hex(kernel_phys_base);
    crate::serial::serial_print("\n");
    
    // The bootloader has already set up page tables and loaded CR3
    // We don't need to do anything here except verify it's working
    
    let cr3 = get_cr3();
    crate::serial::serial_print("Current CR3: ");
    crate::serial::serial_print_hex(cr3);
    crate::serial::serial_print("\n");
    
    // Verify we can access physical memory via higher half
    // Try to read from physical address 0 via higher half mapping
    let test_virt = PHYS_MEM_OFFSET;
    crate::serial::serial_print("Testing higher half access at: ");
    crate::serial::serial_print_hex(test_virt);
    crate::serial::serial_print("\n");
    
    // If we can read this without faulting, higher half mapping works
    let _test_read = unsafe { core::ptr::read_volatile(test_virt as *const u8) };
    
    crate::serial::serial_print("✓ Higher half physical map verified\n");
    crate::serial::serial_print("✓ Paging enabled and working\n");
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

use x86_64::registers::control::Cr3;
// use x86_64::structures::paging::PageTable; // Import removed to avoid conflict with local definition
use x86_64::PhysAddr;

/// Create a new isolated page table for a process
/// Returns the physical address of the PML4
pub fn create_process_paging() -> u64 {
    unsafe {
        // Use alloc_dma_buffer to avoid stack overflow with Box::new(PageTable::new())
        let (pml4_ptr, pml4_phys) = alloc_dma_buffer(4096, 4096).expect("Failed to allocate PML4");
        
        // Zero out the new PML4
        core::ptr::write_bytes(pml4_ptr, 0, 4096);
        let pml4 = &mut *(pml4_ptr as *mut PageTable);
        
        // Get current PML4 to copy kernel mappings
        let (current_pml4_phys, _) = Cr3::read();
        let current_pml4_virt = phys_to_virt(current_pml4_phys.start_address().as_u64());
        let current_pml4 = &*(current_pml4_virt as *const PageTable);
        
        // 1. Copy Higher Half Physical Map (PML4[256])
        // This is CRITICAL for the kernel to access physical memory (allocator, etc.)
        pml4.entries[256] = current_pml4.entries[256].clone();
        
        // 2. Map Kernel Code / Identity (PML4[0])
        // For now, we share the 0-16GB identity map with the kernel.
        // This includes the kernel code loaded at 0x200000.
        // WARNING: This means processes share the lower 512GB (PML4[0])!
        // This is necessary because the kernel code is currently linked at low addresses.
        pml4.entries[0] = current_pml4.entries[0].clone();
        
        // 3. Setup Recursive Mapping
        // Map the LAST entry (511) to point to the NEW PML4 itself
        // This allows the kernel to access this page table structure at a known virtual address
        // when this page table is active.
        pml4.entries[511].set_addr(
             pml4_phys, 
             (x86_64::structures::paging::PageTableFlags::PRESENT | 
             x86_64::structures::paging::PageTableFlags::WRITABLE).bits()
        );
        
        // Note: usage of PDPT and PD generic buffers is removed as we reuse the upper level tables
        // from the kernel for now. User space will be allocated dynamically later.
        
        pml4_phys
    }
}

/// Clone an existing process's page table (deep copy of user-space mappings)
/// Returns the physical address of the child's PML4
pub fn clone_process_paging(parent_pml4_phys: u64) -> u64 {
    // 1. Create new skeleton page table
    let child_pml4_phys = create_process_paging();
    
    unsafe {
        // Access parent and child PML4s
        let p_pml4 = &*(phys_to_virt(parent_pml4_phys) as *const PageTable);
        let c_pml4 = &mut *(phys_to_virt(child_pml4_phys) as *mut PageTable);
        
        // We need to iterate over USER space (0 to 0x0000_7FFF_FFFF_FFFF)
        // For efficiency in this simplistic kernel, we only scan the first PDP (512GB)
        // because our processes (init) are small and live in 0-1GB range.
        
        // Iterate PML4 entries (Low half only)
        for i in 0..256 {
            let p_pml4_entry = &p_pml4.entries[i];
            if !p_pml4_entry.present() || !x86_64::structures::paging::PageTableFlags::from_bits_truncate(p_pml4_entry.get_flags()).contains(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE) {
                continue;
            }
            
            // If we are sharing the first entry with kernel mappings (identity map),
            // we need to be careful.
            // If i == 0, we already copied the Identity map in create_process_paging.
            // But we might have user mappings mixed in there (e.g. at 256MB).
            // We'll trust create_process_paging() did the initial copy.
            // But for fork(), we want DEEP COPY of user pages.
            // TODO: Implement proper deep copy for fork(). 
            // For now, this is a placeholder that assumes shared kernel mappings are fine
            // and doesn't fully implement CoW or deep copy for user pages mixed in identity map.
            // This is enough to get services running (create_process) but NOT for fork() yet.
        }
    }

    child_pml4_phys
}

/// Map a 2MB page in a process's page table
pub fn map_user_page_2mb(pml4_phys: u64, vaddr: u64, paddr: u64, flags: u64) {
    let pml4_virt = phys_to_virt(pml4_phys);
    let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
    
    let pml4_idx = ((vaddr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((vaddr >> 30) & 0x1FF) as usize;
    let pd_idx   = ((vaddr >> 21) & 0x1FF) as usize;
    
    unsafe {
        // 1. Walk/Create PDPT
        let pml4_entry = &mut pml4.entries[pml4_idx];
        if !pml4_entry.present() {
            let (pdpt_ptr, pdpt_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PDPT");
            core::ptr::write_bytes(pdpt_ptr, 0, 4096);
            pml4_entry.set_addr(
                pdpt_phys, 
                (x86_64::structures::paging::PageTableFlags::PRESENT | 
                x86_64::structures::paging::PageTableFlags::WRITABLE | 
                x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits()
            );
        }
        
        let pdpt_virt = phys_to_virt(pml4_entry.get_addr());
        let pdpt = &mut *(pdpt_virt as *mut PageTable);
        
        // 2. Walk/Create PD
        let pdpt_entry = &mut pdpt.entries[pdpt_idx];
        if !pdpt_entry.present() {
            let (pd_ptr, pd_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PD");
            core::ptr::write_bytes(pd_ptr, 0, 4096);
            pdpt_entry.set_addr(
                pd_phys, 
                (x86_64::structures::paging::PageTableFlags::PRESENT | 
                x86_64::structures::paging::PageTableFlags::WRITABLE | 
                x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits()
            );
        }
        
        let pd_virt = phys_to_virt(pdpt_entry.get_addr());
        let pd = &mut *(pd_virt as *mut PageTable);
        
        // 3. Map Page (2MB)
        pd.entries[pd_idx].set_addr(
            paddr, 
            (x86_64::structures::paging::PageTableFlags::from_bits_truncate(flags) | 
            x86_64::structures::paging::PageTableFlags::HUGE_PAGE |
            x86_64::structures::paging::PageTableFlags::PRESENT | 
            x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits()
        );
    }
    
    // TLB Flush would be needed if this page table is active
    // But usually we map before switching, or we should invalid entry.
}


/// Translate virtual address to physical address
/// 
/// With Higher Half Kernel mapping:
/// 
/// 1. **Higher Half Physical Map (0xFFFF800000000000+)**: Direct mapping
///    - All physical RAM is mapped here
///    - Mapping: `physical = virtual - PHYS_MEM_OFFSET`
///    - Example: Virtual 0xFFFF800000001000 -> Physical 0x1000
///
/// 2. **Low memory (< 4GB)**: Identity mapping (for compatibility)
///    - Used during boot and for some legacy code
///    - Mapping: `physical = virtual`
///
/// 3. **Kernel Higher Half (0xFFFF880000000000+)**: Not yet implemented
///    - Will be used for kernel code/data in future
///
/// This is much simpler than the old offset-based approach!
pub fn virt_to_phys(virt_addr: u64) -> u64 {
    // Check if address is in higher half physical memory map
    if virt_addr >= PHYS_MEM_OFFSET {
        // Higher half physical map: subtract offset to get physical address
        virt_addr - PHYS_MEM_OFFSET
    } else {
        // Low memory: identity mapped (virt = phys)
        // This includes bootloader code, stack, and early kernel structures
        virt_addr
    }
}

/// Convert physical address to virtual address (inverse of virt_to_phys)
/// Returns the higher half virtual address for accessing physical memory
pub fn phys_to_virt(phys_addr: u64) -> u64 {
    PHYS_MEM_OFFSET + phys_addr
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

/// Map framebuffer physical memory into process page tables
/// Returns virtual address where framebuffer is mapped, or 0 on failure
pub fn map_framebuffer_for_process(page_table_phys: u64, fb_phys_addr: u64, fb_size: u64) -> u64 {
    use x86_64::structures::paging::PageTableFlags as Flags;
    
    // For identity mapping, we'll map the framebuffer at its physical address
    let virtual_addr = fb_phys_addr;
    
    // Round size up to 2MB pages
    let num_pages = (fb_size + 0x1FFFFF) / 0x200000;
    
    crate::serial::serial_print("MAP_FB: Identity mapping ");
    crate::serial::serial_print_dec(num_pages);
    crate::serial::serial_print(" pages\n");
    
    // Access the process's PML4
    let pml4_virt = phys_to_virt(page_table_phys);
    let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
    
    // For each 2MB page of the framebuffer
    for page_idx in 0..num_pages {
        let page_phys = fb_phys_addr + (page_idx * 0x200000);
        let page_virt = page_phys; // Identity mapping
        
        // Calculate indices for page table walk
        let pml4_idx = ((page_virt >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((page_virt >> 30) & 0x1FF) as usize;
        let pd_idx = ((page_virt >> 21) & 0x1FF) as usize;
        
        // Get or create PDPT
        if !pml4.entries[pml4_idx].present() {
            if let Some((pdpt_ptr, pdpt_phys)) = alloc_dma_buffer(4096, 4096) {
                unsafe { core::ptr::write_bytes(pdpt_ptr, 0, 4096); }
                pml4.entries[pml4_idx].set_addr(
                    pdpt_phys,
                    (Flags::PRESENT | Flags::WRITABLE | Flags::USER_ACCESSIBLE).bits()
                );
            } else {
                return 0;
            }
        }
        
        // Get PDPT
        let pdpt_phys = pml4.entries[pml4_idx].get_addr();
        let pdpt_virt = phys_to_virt(pdpt_phys);
        let pdpt = unsafe { &mut *(pdpt_virt as *mut PageTable) };
        
        // Get or create PD
        if !pdpt.entries[pdpt_idx].present() {
            if let Some((pd_ptr, pd_phys)) = alloc_dma_buffer(4096, 4096) {
                unsafe { core::ptr::write_bytes(pd_ptr, 0, 4096); }
                pdpt.entries[pdpt_idx].set_addr(
                    pd_phys,
                    (Flags::PRESENT | Flags::WRITABLE | Flags::USER_ACCESSIBLE).bits()
                );
            } else {
                return 0;
            }
        }
        
        // Get PD
        let pd_phys = pdpt.entries[pdpt_idx].get_addr();
        let pd_virt = phys_to_virt(pd_phys);
        let pd = unsafe { &mut *(pd_virt as *mut PageTable) };
        
        // Map Page (2MB) - Identity mapping for framebuffer
        // Important: Huge Page bit + User Accessible + Write Through (for FB maybe?)
        // Usually FB needs Write Combining but Write Through is safer than Write Back.
        // For now, just standard flags.
        pd.entries[pd_idx].set_addr(
            page_phys,
            (Flags::PRESENT | Flags::WRITABLE | Flags::USER_ACCESSIBLE | Flags::HUGE_PAGE).bits()
        );
    }
    
    virtual_addr
}
