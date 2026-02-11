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
        // IMPORTANTE: Obtenemos la dirección del array estático en memoria baja
        let heap_ptr_low = HEAP.memory.as_mut_ptr();
        crate::serial::serial_print("[MEM] HEAP static addr: ");
        crate::serial::serial_print_hex(heap_ptr_low as u64);
        crate::serial::serial_print("\n");
        
        // Convertimos a dirección física
        let heap_phys = virt_to_phys(heap_ptr_low as u64);
        crate::serial::serial_print("[MEM] HEAP physical addr: ");
        crate::serial::serial_print_hex(heap_phys);
        crate::serial::serial_print("\n");
        
        // Convertimos a dirección Higher Half (0xFFFF8000...) de forma EXPLICITA
        let heap_start_high = PHYS_MEM_OFFSET + heap_phys;
        crate::serial::serial_print("[MEM] HEAP higher-half base: ");
        crate::serial::serial_print_hex(heap_start_high);
        crate::serial::serial_print("\n");
        
        // Get raw pointer to ALLOCATOR and dereference explicitly
        let allocator_ptr = &raw const ALLOCATOR;
        let allocator_ref = unsafe { &*allocator_ptr };
        let mut allocator = allocator_ref.lock();
        
        // Initialize allocator with Higher Half address
        allocator.init(heap_start_high as *mut u8, HEAP_SIZE);
        
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

    pub fn present(&self) -> bool {
        self.entry & 0x1 != 0
    }
    
    pub fn get_addr(&self) -> u64 {
        self.entry & 0x000F_FFFF_FFFF_F000
    }

    pub fn get_flags(&self) -> u64 {
        self.entry & 0x8000_0000_0000_0FFF
    }

    pub fn is_huge(&self) -> bool {
        self.entry & 0x80 != 0
    }
    
    pub fn writable(&self) -> bool {
        self.entry & 0x2 != 0
    }

    pub fn set_entry(&mut self, addr: u64, flags: u64) {
        self.entry = (addr & 0x000F_FFFF_FFFF_F000) | (flags & 0x8000_0000_0000_0FFF);
    }
    
    pub fn set_addr(&mut self, addr: u64, flags: u64) {
        self.set_entry(addr, flags);
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
    
    // The bootloader maps the kernel at PHYS_MEM_OFFSET -> kernel_phys_base.
    // This creates an overlap where the first 256MB of the Higher Half
    // is shifted by kernel_phys_base.
    PHYS_OFFSET.store(kernel_phys_base, Ordering::Relaxed);
    
    crate::serial::serial_print("✓ Higher half physical map verified\n");
    crate::serial::serial_print("✓ Paging enabled and working\n");
}

/// Debug function: Walk the page table and print entries
pub fn walk_page_table(pml4_phys: u64, vaddr: u64) {
    let pml4_virt = phys_to_virt(pml4_phys);
    let pml4 = unsafe { &*(pml4_virt as *const PageTable) };
    
    let pml4_idx = ((vaddr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((vaddr >> 30) & 0x1FF) as usize;
    let pd_idx   = ((vaddr >> 21) & 0x1FF) as usize;
    let pt_idx   = ((vaddr >> 12) & 0x1FF) as usize;
    
    crate::serial::serial_print("[Walker] Walking v=");
    crate::serial::serial_print_hex(vaddr);
    crate::serial::serial_print(" (PML4 phys: ");
    crate::serial::serial_print_hex(pml4_phys);
    crate::serial::serial_print(")\n");
    
    let pml4_entry = &pml4.entries[pml4_idx];
    crate::serial::serial_print("  PML4[");
    crate::serial::serial_print_dec(pml4_idx as u64);
    crate::serial::serial_print("]: ");
    crate::serial::serial_print_hex(pml4_entry.entry);
    crate::serial::serial_print("\n");
    
    if !pml4_entry.present() { return; }
    
    let pdpt_virt = phys_to_virt(pml4_entry.get_addr());
    let pdpt = unsafe { &*(pdpt_virt as *const PageTable) };
    let pdpt_entry = &pdpt.entries[pdpt_idx];
    
    crate::serial::serial_print("  PDPT[");
    crate::serial::serial_print_dec(pdpt_idx as u64);
    crate::serial::serial_print("]: ");
    crate::serial::serial_print_hex(pdpt_entry.entry);
    crate::serial::serial_print("\n");
    
    if !pdpt_entry.present() { return; }
    if pdpt_entry.is_huge() {
        crate::serial::serial_print("  (Is 1GB Huge Page)\n");
        return;
    }
    
    let pd_virt = phys_to_virt(pdpt_entry.get_addr());
    let pd = unsafe { &*(pd_virt as *const PageTable) };
    let pd_entry = &pd.entries[pd_idx];
    
    crate::serial::serial_print("  PD[");
    crate::serial::serial_print_dec(pd_idx as u64);
    crate::serial::serial_print("]: ");
    crate::serial::serial_print_hex(pd_entry.entry);
    crate::serial::serial_print("\n");
    
    if !pd_entry.present() { return; }
    if pd_entry.is_huge() {
        crate::serial::serial_print("  (Is 2MB Huge Page)\n");
        return;
    }
    
    let pt_virt = PHYS_MEM_OFFSET + pd_entry.get_addr();
    let pt = unsafe { &*(pt_virt as *const PageTable) };
    let pt_entry = &pt.entries[pt_idx];
    
    crate::serial::serial_print("  PT[");
    crate::serial::serial_print_dec(pt_idx as u64);
    crate::serial::serial_print("]: ");
    crate::serial::serial_print_hex(pt_entry.entry);
    crate::serial::serial_print("\n");
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

/// Remove the identity mapping (PML4[0]) from the current page table.
/// This enforces strict Higher Half only execution for the kernel.
pub fn remove_identity_mapping() {
    let cr3 = get_cr3();
    let pml4_virt = PHYS_MEM_OFFSET + cr3;
    let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
    pml4.entries[0] = PageTableEntry::new();
    
    // Flush TLB to ensure the change takes effect
    unsafe {
        core::arch::asm!(
            "mov rax, cr3",
            "mov cr3, rax",
            out("rax") _,
            options(nostack, preserves_flags)
        );
    }
}

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
        let current_pml4_phys_u64 = current_pml4_phys.start_address().as_u64();
        
        // CRITICAL: Use direct PHYS_MEM_OFFSET mapping, NOT phys_to_virt
        // phys_to_virt applies kernel_phys_base offset which is WRONG for CR3
        // CR3 points to a page table that's in the direct physical map
        let current_pml4_virt = PHYS_MEM_OFFSET + current_pml4_phys_u64;
        
        let current_pml4 = &*(current_pml4_virt as *const PageTable);

        // 1. Copy ALL mappings from the current PML4 (boot/kernel)
        // This ensures the higher half (physical map, kernel image, etc.) is identical.
        for i in 0..512 {
            pml4.entries[i] = current_pml4.entries[i].clone();
        }
        
        // 2. Clear PML4[0] to remove identity map/user space from the template
        // User space will be mapped explicitly via ELF loader.
        pml4.entries[0] = PageTableEntry::new();
        
        // 3. Setup Recursive Mapping
        // Map the LAST entry (511) to point to the NEW PML4 itself
        // This allows the kernel to access this page table structure at a known virtual address
        // when this page table is active.
        pml4.entries[511].set_addr(
             pml4_phys, 
             (x86_64::structures::paging::PageTableFlags::PRESENT | 
             x86_64::structures::paging::PageTableFlags::WRITABLE).bits()
        );
        
        pml4_phys
    }
}

/// Clone an existing process's page table (deep copy of user-space mappings)
/// Returns the physical address of the child's PML4
pub fn clone_process_paging(parent_pml4_phys: u64) -> u64 {
    // 1. Create new skeleton page table (Copies Kernel Mappings)
    let child_pml4_phys = create_process_paging();
    
    unsafe {
        let p_pml4 = &*(phys_to_virt(parent_pml4_phys) as *const PageTable);
        let c_pml4 = &mut *(phys_to_virt(child_pml4_phys) as *mut PageTable);
        
        // We focus on PML4[0] (Identity/User Map)
        // If it is present, we must Deep Copy the USER portions.
        if p_pml4.entries[0].present() {
            // Allocate NEW PDPT
            let mut new_pdpt = alloc::boxed::Box::new(PageTable::new());
            let p_pdpt_phys = p_pml4.entries[0].get_addr();
            let p_pdpt = &*(phys_to_virt(p_pdpt_phys) as *const PageTable);
            
            for i in 0..512 {
                if !p_pdpt.entries[i].present() { continue; }
                
                let flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(p_pdpt.entries[i].get_flags());
                let is_user = flags.contains(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
                
                if is_user {
                    // Allocate NEW PD
                    let mut new_pd = alloc::boxed::Box::new(PageTable::new());
                    let p_pd_phys = p_pdpt.entries[i].get_addr();
                    let p_pd = &*(phys_to_virt(p_pd_phys) as *const PageTable);
                    
                    for j in 0..512 {
                        if !p_pd.entries[j].present() { continue; }
                        
                        let pd_flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(p_pd.entries[j].get_flags());
                        let p_pd_is_user = pd_flags.contains(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
                        
                        if p_pd_is_user {
                            if pd_flags.contains(x86_64::structures::paging::PageTableFlags::HUGE_PAGE) {
                                // 2MB Huge Page Deep Copy
                                if let Some((new_frame_ptr, new_frame_phys)) = alloc_dma_buffer(2 * 1024 * 1024, 2 * 1024 * 1024) {
                                    let p_frame_phys = p_pd.entries[j].get_addr();
                                    let p_frame_virt = phys_to_virt(p_frame_phys) as *const u8;
                                    core::ptr::copy_nonoverlapping(p_frame_virt, new_frame_ptr, 2 * 1024 * 1024);
                                    new_pd.entries[j].set_addr(new_frame_phys, pd_flags.bits());
                                } else {
                                    // Fallback: Share (Dangerous)
                                    new_pd.entries[j] = p_pd.entries[j].clone();
                                }
                            } else {
                                // Standard 4KB Page Table Deep Copy
                                let mut new_pt = alloc::boxed::Box::new(PageTable::new());
                                let p_pt_phys = p_pd.entries[j].get_addr();
                                let p_pt = &*(phys_to_virt(p_pt_phys) as *const PageTable);
                                
                                for k in 0..512 {
                                    if !p_pt.entries[k].present() { continue; }
                                    let pt_flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(p_pt.entries[k].get_flags());
                                    
                                    // Deep copy 4KB frame
                                    if let Some((new_frame_ptr, new_frame_phys)) = alloc_dma_buffer(4096, 4096) {
                                        let p_frame_phys = p_pt.entries[k].get_addr();
                                        let p_frame_virt = phys_to_virt(p_frame_phys) as *const u8;
                                        core::ptr::copy_nonoverlapping(p_frame_virt, new_frame_ptr, 4096);
                                        new_pt.entries[k].set_addr(new_frame_phys, pt_flags.bits());
                                    } else {
                                        new_pt.entries[k] = p_pt.entries[k].clone();
                                    }
                                }
                                
                                let new_pt_phys = virt_to_phys(alloc::boxed::Box::into_raw(new_pt) as u64);
                                new_pd.entries[j].set_addr(new_pt_phys, pd_flags.bits());
                            }
                        } else {
                            // Kernel Page or not present - Share
                            new_pd.entries[j] = p_pd.entries[j].clone();
                        }
                    }
                    
                    let new_pd_phys = virt_to_phys(alloc::boxed::Box::into_raw(new_pd) as u64);
                    new_pdpt.entries[i].set_addr(new_pd_phys, flags.bits());
                } else {
                    // Kernel Mapping - Share
                    new_pdpt.entries[i] = p_pdpt.entries[i].clone();
                }
            }
            
            let new_pdpt_phys = virt_to_phys(alloc::boxed::Box::into_raw(new_pdpt) as u64);
            c_pml4.entries[0].set_addr(new_pdpt_phys, p_pml4.entries[0].get_flags() | 0x4); // USER bit just in case
        }
    }

    child_pml4_phys
}

/// Map a 4KB page in a process's page table
pub fn map_user_page_4kb(pml4_phys: u64, vaddr: u64, paddr: u64, flags: u64) {
    let pml4_virt = phys_to_virt(pml4_phys);
    let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
    
    /*
    crate::serial::serial_print("[Map4KB] pml4_phys=");
    crate::serial::serial_print_hex(pml4_phys);
    crate::serial::serial_print(" pml4_virt=");
    crate::serial::serial_print_hex(pml4_virt);
    crate::serial::serial_print("\n");
    */
    let pml4_idx = ((vaddr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((vaddr >> 30) & 0x1FF) as usize;
    let pd_idx   = ((vaddr >> 21) & 0x1FF) as usize;
    let pt_idx   = ((vaddr >> 12) & 0x1FF) as usize;
    
    unsafe {
        // 1. PML4 -> PDPT
        let pml4_entry = &mut pml4.entries[pml4_idx];
        if !pml4_entry.present() {
            let (pdpt_ptr, pdpt_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PDPT");
            core::ptr::write_bytes(pdpt_ptr, 0, 4096);
            pml4_entry.set_addr(pdpt_phys, (x86_64::structures::paging::PageTableFlags::PRESENT | 
                                          x86_64::structures::paging::PageTableFlags::WRITABLE | 
                                          x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits());
        }
        
        let pdpt_virt = PHYS_MEM_OFFSET + pml4_entry.get_addr();
        let pdpt = &mut *(pdpt_virt as *mut PageTable);
        
        // 2. PDPT -> PD
        let pdpt_entry = &mut pdpt.entries[pdpt_idx];
        if !pdpt_entry.present() {
            let (pd_ptr, pd_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PD");
            core::ptr::write_bytes(pd_ptr, 0, 4096);
            pdpt_entry.set_addr(pd_phys, (x86_64::structures::paging::PageTableFlags::PRESENT | 
                                         x86_64::structures::paging::PageTableFlags::WRITABLE | 
                                         x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits());
        }
        
        let pd_virt = PHYS_MEM_OFFSET + pdpt_entry.get_addr();
        let pd = &mut *(pd_virt as *mut PageTable);
        
        // 3. PD -> PT
        let pd_entry = &mut pd.entries[pd_idx];
        if !pd_entry.present() {
            let (pt_ptr, pt_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PT");
            core::ptr::write_bytes(pt_ptr, 0, 4096);
            pd_entry.set_addr(pt_phys, (x86_64::structures::paging::PageTableFlags::PRESENT | 
                                       x86_64::structures::paging::PageTableFlags::WRITABLE | 
                                       x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits());
        }
        
        let pt_virt = PHYS_MEM_OFFSET + pd_entry.get_addr();
        let pt = &mut *(pt_virt as *mut PageTable);
        
        // 4. PT -> Page
        let pt_entry = &mut pt.entries[pt_idx];
        let mut leaf_flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(flags);
        leaf_flags.insert(x86_64::structures::paging::PageTableFlags::PRESENT);
        leaf_flags.insert(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
        leaf_flags.remove(x86_64::structures::paging::PageTableFlags::NO_EXECUTE);

        pt_entry.set_addr(paddr, leaf_flags.bits());
    }
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
        } else {
             // Ensure existing entry has USER permission AND Execute permission (Clear NX)
             let mut flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(pml4_entry.get_flags());
             flags.insert(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
             flags.remove(x86_64::structures::paging::PageTableFlags::NO_EXECUTE);
             pml4_entry.set_addr(pml4_entry.get_addr(), flags.bits());
        }
        
        // Use Higher Half Direct Map explicitly
        let pdpt_virt = PHYS_MEM_OFFSET + pml4_entry.get_addr();
        let pdpt = &mut *(pdpt_virt as *mut PageTable);
        
        // 2. Walk/Create PD
        let pdpt_entry = &mut pdpt.entries[pdpt_idx];
        
        // DEBUG: Check for Huge Page in PDPT
        if x86_64::structures::paging::PageTableFlags::from_bits_truncate(pdpt_entry.get_flags())
            .contains(x86_64::structures::paging::PageTableFlags::HUGE_PAGE) 
        {
            crate::serial::serial_print("WARNING: PDPT Entry is HUGE PAGE (1GB). Splitting needed!\n");
        }
        
        if !pdpt_entry.present() {
            let (pd_ptr, pd_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PD");
            core::ptr::write_bytes(pd_ptr, 0, 4096);
            pdpt_entry.set_addr(
                pd_phys, 
                (x86_64::structures::paging::PageTableFlags::PRESENT | 
                x86_64::structures::paging::PageTableFlags::WRITABLE | 
                x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits()
            );
        } else {
             // Ensure existing entry has USER permission AND Execute permission
             let mut flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(pdpt_entry.get_flags());
             flags.insert(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
             flags.remove(x86_64::structures::paging::PageTableFlags::NO_EXECUTE);
             pdpt_entry.set_addr(pdpt_entry.get_addr(), flags.bits());
        }
        
        // Use Higher Half Direct Map explicitly
        let pd_virt = PHYS_MEM_OFFSET + pdpt_entry.get_addr();
        let pd = &mut *(pd_virt as *mut PageTable);
        
        // 3. Map Page (2MB)
        let mut leaf_flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(flags);
        leaf_flags.insert(x86_64::structures::paging::PageTableFlags::HUGE_PAGE);
        leaf_flags.insert(x86_64::structures::paging::PageTableFlags::PRESENT);
        leaf_flags.insert(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
        leaf_flags.remove(x86_64::structures::paging::PageTableFlags::NO_EXECUTE);
        
        pd.entries[pd_idx].set_addr(paddr, leaf_flags.bits());
        
        crate::serial::serial_print("[Paging] Mapped User Page: v=");
        crate::serial::serial_print_hex(vaddr);
        crate::serial::serial_print(" -> p=");
        crate::serial::serial_print_hex(paddr);
        crate::serial::serial_print(" bits=");
        crate::serial::serial_print_hex(leaf_flags.bits());
        crate::serial::serial_print(" indices: ");
        crate::serial::serial_print_dec(pml4_idx as u64);
        crate::serial::serial_print(",");
        crate::serial::serial_print_dec(pdpt_idx as u64);
        crate::serial::serial_print(",");
        crate::serial::serial_print_dec(pd_idx as u64);
        crate::serial::serial_print("\n");
        
        // RE-VERIFY immediately via memory access
        let entry_check = pd.entries[pd_idx].get_addr();
        if entry_check != paddr {
            crate::serial::serial_print("CRITICAL: Page table write failure! Expected p=");
            crate::serial::serial_print_hex(paddr);
            crate::serial::serial_print(" but read p=");
            crate::serial::serial_print_hex(entry_check);
            crate::serial::serial_print("\n");
        }
    }
    
    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(vaddr));
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
    let phys_offset = PHYS_OFFSET.load(Ordering::Relaxed);
    
    // 1. Check if the address is in the kernel/heap shifted range (first 256MB)
    if virt_addr >= PHYS_MEM_OFFSET && virt_addr < PHYS_MEM_OFFSET + KERNEL_REGION_SIZE {
        return (virt_addr - PHYS_MEM_OFFSET) + phys_offset;
    }
    
    // 2. Default higher half physical map (Virt = Base + Phys)
    if virt_addr >= PHYS_MEM_OFFSET {
        return virt_addr - PHYS_MEM_OFFSET;
    }
    
    // 3. Low memory identity map (fallback/early boot)
    virt_addr
}

/// Convert physical address to virtual address (inverse of virt_to_phys)
/// Returns the higher half virtual address for accessing physical memory
pub fn phys_to_virt(phys_addr: u64) -> u64 {
    let phys_offset = PHYS_OFFSET.load(Ordering::Relaxed);
    
    // Check if the physical address is in the range where the kernel is mapped
    if phys_addr >= phys_offset && phys_addr < phys_offset + KERNEL_REGION_SIZE {
        return (phys_addr - phys_offset) + PHYS_MEM_OFFSET;
    }
    
    // Otherwise it's standard direct mapping
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
    use x86_64::instructions::tlb::flush_all;
    use crate::serial;
    
    // For identity mapping, we'll map the framebuffer at its physical address
    let virtual_addr = fb_phys_addr;
    
    // Round size up to 2MB pages
    let num_pages = (fb_size + 0x1FFFFF) / 0x200000;
    
    serial::serial_print("MAP_FB: Identity mapping ");
    serial::serial_print_dec(num_pages);
    serial::serial_print(" pages (2MB each)\n");
    
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
        } else {
            // FORCE User access bit on existing entry
            let mut ent = pml4.entries[pml4_idx];
            let flags = (ent.get_flags() | Flags::USER_ACCESSIBLE.bits()) & !Flags::NO_EXECUTE.bits();
            ent.set_addr(ent.get_addr(), flags);
            pml4.entries[pml4_idx] = ent;
            
            if page_idx == 0 {
                serial::serial_print("  PML4[");
                serial::serial_print_dec(pml4_idx as u64);
                serial::serial_print("] setup, Entry: ");
                serial::serial_print_hex(pml4.entries[pml4_idx].entry);
                serial::serial_print("\n");
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
        } else {
            // FORCE User access bit on existing entry
            let mut ent = pdpt.entries[pdpt_idx];
            if ent.is_huge() {
                serial::serial_print("MAP_FB: ERROR - 1GB Huge Page conflict at PDPT index ");
                serial::serial_print_dec(pdpt_idx as u64);
                serial::serial_print("\n");
                return 0;
            }
            let flags = (ent.get_flags() | Flags::USER_ACCESSIBLE.bits()) & !Flags::NO_EXECUTE.bits();
            ent.set_addr(ent.get_addr(), flags);
            pdpt.entries[pdpt_idx] = ent;
        }
        
        // Get PD
        let pd_phys = pdpt.entries[pdpt_idx].get_addr();
        let pd_virt = phys_to_virt(pd_phys);
        let pd = unsafe { &mut *(pd_virt as *mut PageTable) };
        
        // Map Page (2MB) - Identity mapping for framebuffer
        // Important: Huge Page bit + User Accessible + Write Through
        let final_flags = Flags::PRESENT | Flags::WRITABLE | Flags::USER_ACCESSIBLE | Flags::HUGE_PAGE | Flags::WRITE_THROUGH;
        pd.entries[pd_idx].set_addr(page_phys, final_flags.bits());
        
        if true {
            serial::serial_print("MAP_FB: Mapped v=");
            serial::serial_print_hex(page_virt);
            serial::serial_print(" (PML4=");
            serial::serial_print_dec(pml4_idx as u64);
            serial::serial_print(", PDPT=");
            serial::serial_print_dec(pdpt_idx as u64);
            serial::serial_print(", PD=");
            serial::serial_print_dec(pd_idx as u64);
            serial::serial_print(") Entry: ");
            serial::serial_print_hex(pd.entries[pd_idx].entry);
            serial::serial_print("\n");
        }
    }
    
    // Flush TLB globally
    flush_all();
    
    // VERIFY: Walk the table for the first address
    walk_page_table(page_table_phys, virtual_addr);
    
    virtual_addr
}
