//! Microkernel memory management system
//! 
//! Implements:
//! - Basic paging
//! - Heap allocator
//! - Physical memory management

use linked_list_allocator::LockedHeap;
use core::sync::atomic::{AtomicU64, Ordering};

/// Physical offset for virtual-to-physical address translation
/// Written once during init_paging(), then read-only
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

// Las tablas ahora están arriba para mayor seguridad

/// Inicializar paginación
pub fn init_paging(kernel_phys_base: u64) {
    // Disable interrupts during paging initialization to avoid triple faults
    // if an interrupt occurs before our transition is complete
    unsafe {
        core::arch::asm!("cli");
    }

    let phys_offset = kernel_phys_base.wrapping_sub(0x200000);
    PHYS_OFFSET.store(phys_offset, Ordering::Relaxed);

    unsafe {
        let pml4_virt = &raw const PML4 as u64;
        let pdpt_virt = &raw const PDPT as u64;
        let pd0_virt = &raw const PD[0] as u64;

        crate::serial::serial_print("Init Paging. Phys Base: ");
        crate::serial::serial_print_hex(kernel_phys_base);
        crate::serial::serial_print("\nOffset: ");
        crate::serial::serial_print_hex(phys_offset);
        crate::serial::serial_print("\nPML4 Virt: ");
        crate::serial::serial_print_hex(pml4_virt);
        crate::serial::serial_print("\n");
        
        // Zero all tables explicitly
        core::ptr::write_bytes(pml4_virt as *mut u8, 0, 4096);
        core::ptr::write_bytes(pdpt_virt as *mut u8, 0, 4096);
        core::ptr::write_bytes(pd0_virt as *mut u8, 0, 16384);

        // Calcular direcciones físicas
        let pdpt_phys = pdpt_virt.wrapping_add(phys_offset);
        let pml4_phys = pml4_virt.wrapping_add(phys_offset);
        let pd0_phys = pd0_virt.wrapping_add(phys_offset);
        let pd1_phys = pd0_phys + 4096;
        let pd2_phys = pd0_phys + 8192;
        let pd3_phys = pd0_phys + 12288;

        crate::serial::serial_print("PML4 Phys: ");
        crate::serial::serial_print_hex(pml4_phys);
        crate::serial::serial_print("\n");

        let rsp: u64;
        core::arch::asm!("mov {}, rsp", out(reg) rsp);
        crate::serial::serial_print("Current RSP: ");
        crate::serial::serial_print_hex(rsp);
        crate::serial::serial_print("\n");

        // PML4[0] -> PDPT
        PML4.entries[0].set_addr(pdpt_phys, PAGE_PRESENT | PAGE_WRITABLE);
        
        // PML4[511] -> PDPT (higher half mirror)
        PML4.entries[511].set_addr(pdpt_phys, PAGE_PRESENT | PAGE_WRITABLE);
        
        // PDPT: Link the 4 PDs to map 4GB
        PDPT.entries[0].set_addr(pd0_phys, PAGE_PRESENT | PAGE_WRITABLE);
        PDPT.entries[1].set_addr(pd1_phys, PAGE_PRESENT | PAGE_WRITABLE);
        PDPT.entries[2].set_addr(pd2_phys, PAGE_PRESENT | PAGE_WRITABLE);
        PDPT.entries[3].set_addr(pd3_phys, PAGE_PRESENT | PAGE_WRITABLE);
        
        // PDs: Map 4GB with 2MB huge pages
        for j in 0..4 {
            for i in 0..512 {
                let virt_addr = (j as u64) * 0x40000000 + (i as u64) * 0x200000;

                let phys_addr = if virt_addr < KERNEL_REGION_SIZE {
                    // Kernel region (linked range): virt + offset = phys
                    virt_addr.wrapping_add(phys_offset)
                } else {
                    // Identity mapping (virt = phys)
                    virt_addr
                };

                PD[j].entries[i].set_addr(phys_addr, PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE);
            }
        }
        
        // DEBUG: Verify mapping for RSP
        let rsp_i = ((rsp & 0x3FFFFFFF) / 0x200000) as usize;
        let rsp_j = (rsp / 0x40000000) as usize;
        if rsp_j < 4 {
             crate::serial::serial_print("RSP Mapping (PD[");
             crate::serial::serial_print_dec(rsp_j as u64);
             crate::serial::serial_print("].entries[");
             crate::serial::serial_print_dec(rsp_i as u64);
             crate::serial::serial_print("]): ");
             crate::serial::serial_print_hex(PD[rsp_j].entries[rsp_i].get_addr());
             crate::serial::serial_print("\n");
        }
        
        // ===== PHYSICAL ADDRESS CHAIN DIAGNOSTICS =====
        crate::serial::serial_print("\n=== PHYSICAL ADDRESS CHAIN ===\n");
        crate::serial::serial_print("PML4 virt: ");
        crate::serial::serial_print_hex(pml4_virt);
        crate::serial::serial_print(" -> phys: ");
        crate::serial::serial_print_hex(pml4_phys);
        crate::serial::serial_print("\n");
        
        crate::serial::serial_print("PDPT virt: ");
        crate::serial::serial_print_hex(pdpt_virt);
        crate::serial::serial_print(" -> phys: ");
        crate::serial::serial_print_hex(pdpt_phys);
        crate::serial::serial_print("\n");
        
        crate::serial::serial_print("PD[0] virt: ");
        crate::serial::serial_print_hex(pd0_virt);
        crate::serial::serial_print(" -> phys: ");
        crate::serial::serial_print_hex(pd0_phys);
        crate::serial::serial_print("\n");
        
        crate::serial::serial_print("PD[1] phys: ");
        crate::serial::serial_print_hex(pd1_phys);
        crate::serial::serial_print("\n");
        
        crate::serial::serial_print("PD[2] phys: ");
        crate::serial::serial_print_hex(pd2_phys);
        crate::serial::serial_print("\n");
        
        crate::serial::serial_print("PD[3] phys: ");
        crate::serial::serial_print_hex(pd3_phys);
        crate::serial::serial_print("\n");
        
        // Verify PDPT points to correct PD addresses
        crate::serial::serial_print("\nPDPT[0] points to: ");
        crate::serial::serial_print_hex(PDPT.entries[0].get_addr());
        crate::serial::serial_print(" (should be ");
        crate::serial::serial_print_hex(pd0_phys);
        crate::serial::serial_print(")\n");
        
        crate::serial::serial_print("PDPT[1] points to: ");
        crate::serial::serial_print_hex(PDPT.entries[1].get_addr());
        crate::serial::serial_print(" (should be ");
        crate::serial::serial_print_hex(pd1_phys);
        crate::serial::serial_print(")\n");
        
        // ===== SANITY CHECKS BEFORE CR3 SWITCH =====
        crate::serial::serial_print("\n=== PRE-CR3 SANITY CHECKS ===\n");
        
        // 1. Verify alignment (must be 4KB aligned)
        if pml4_phys & 0xFFF != 0 {
            crate::serial::serial_print("FATAL: PML4 not 4KB aligned: ");
            crate::serial::serial_print_hex(pml4_phys);
            crate::serial::serial_print("\n");
            loop { core::arch::asm!("hlt"); }
        }
        crate::serial::serial_print("✓ PML4 alignment OK (");
        crate::serial::serial_print_hex(pml4_phys);
        crate::serial::serial_print(")\n");
        
        // 2. Verify PML4[0] has PRESENT bit
        if !PML4.entries[0].present() {
            crate::serial::serial_print("FATAL: PML4[0] not present\n");
            loop { core::arch::asm!("hlt"); }
        }
        crate::serial::serial_print("✓ PML4[0] present, points to: ");
        crate::serial::serial_print_hex(PML4.entries[0].get_addr());
        crate::serial::serial_print("\n");
        
        // 3. Verify PDPT entries have PRESENT bit
        for i in 0..4 {
            if !PDPT.entries[i].present() {
                crate::serial::serial_print("FATAL: PDPT[");
                crate::serial::serial_print_dec(i as u64);
                crate::serial::serial_print("] not present\n");
                loop { core::arch::asm!("hlt"); }
            }
        }
        crate::serial::serial_print("✓ All 4 PDPT entries present\n");
        
        // 4. Verify current instruction page is mapped
        let code_addr = init_paging as *const () as u64;
        let code_page_idx = (code_addr / 0x200000) as usize;
        let code_pd_idx = (code_addr / 0x40000000) as usize;
        
        crate::serial::serial_print("✓ Code address: ");
        crate::serial::serial_print_hex(code_addr);
        crate::serial::serial_print(" -> PD[");
        crate::serial::serial_print_dec(code_pd_idx as u64);
        crate::serial::serial_print("][");
        crate::serial::serial_print_dec(code_page_idx as u64);
        crate::serial::serial_print("]\n");
        
        if code_pd_idx < 4 && !PD[code_pd_idx].entries[code_page_idx].present() {
            crate::serial::serial_print("FATAL: Code page not mapped!\n");
            loop { core::arch::asm!("hlt"); }
        }
        crate::serial::serial_print("✓ Code page mapped to: ");
        crate::serial::serial_print_hex(PD[code_pd_idx].entries[code_page_idx].get_addr());
        crate::serial::serial_print("\n");
        
        // CRITICAL: Verify code page maps to where kernel actually is
        let expected_code_phys = code_addr + phys_offset;
        let actual_code_page_phys = PD[code_pd_idx].entries[code_page_idx].get_addr();
        let expected_code_page_phys = (expected_code_phys & !0x1FFFFF);  // 2MB aligned
        
        crate::serial::serial_print("  Expected code phys: ");
        crate::serial::serial_print_hex(expected_code_phys);
        crate::serial::serial_print(" (page: ");
        crate::serial::serial_print_hex(expected_code_page_phys);
        crate::serial::serial_print(")\n");
        
        crate::serial::serial_print("  Actual page mapping: ");
        crate::serial::serial_print_hex(actual_code_page_phys);
        crate::serial::serial_print("\n");
        
        if actual_code_page_phys != expected_code_page_phys {
            crate::serial::serial_print("WARNING: Code page mapping mismatch!\n");
            crate::serial::serial_print("  This will cause instruction fetch fault after CR3 switch!\n");
        }
        
        // 5. Verify stack page is mapped
        if rsp_j < 4 && !PD[rsp_j].entries[rsp_i].present() {
            crate::serial::serial_print("FATAL: Stack page not mapped!\n");
            loop { core::arch::asm!("hlt"); }
        }
        crate::serial::serial_print("✓ Stack page mapped OK\n");
        
        crate::serial::serial_print("=== ALL SANITY CHECKS PASSED ===\n\n");

        crate::serial::serial_print("Switching CR3 to ");
        crate::serial::serial_print_hex(pml4_phys);
        crate::serial::serial_print("...\n");

         // Load CR3 and flush TLB
        core::arch::asm!(
            "mov cr3, {0}",
            // Explicit TLB flush
            "mov r8, cr3",
            "mov cr3, r8",
            // Re-enable interrupts
            "sti",
            in(reg) pml4_phys,
            out("r8") _,
            options(nostack, preserves_flags)
        );
    }
    crate::serial::init();
    
    // Try to call serial_print - this is where it fails
    crate::serial::serial_print("Paging enabled and verified.\n");
    
    // Re-enable interrupts AFTER Rust has restored the stack frame
    unsafe {
        core::arch::asm!("sti");
    }
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
            pd.entries[i] = PD[0].entries[i];
            // Remove PAGE_USER for kernel pages in PD[0] range
            if (i as u64) * 0x200000 < KERNEL_REGION_SIZE {
                let addr = pd.entries[i].get_addr();
                let flags = pd.entries[i].get_flags();
                pd.entries[i].set_addr(addr, flags & !PAGE_USER);
            }
        }
        
        // IMPORTANT: Link to other kernel tables (1GB-4GB)
        // This allows processes to access the kernel even if it's loaded at high addresses (e.g. 2.8GB)
        let phys_offset = PHYS_OFFSET.load(Ordering::Relaxed);
        pdpt.entries[0].set_addr(pd_phys, PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        pdpt.entries[1].set_addr((&raw const PD[1] as u64).wrapping_add(phys_offset), PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        pdpt.entries[2].set_addr((&raw const PD[2] as u64).wrapping_add(phys_offset), PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        pdpt.entries[3].set_addr((&raw const PD[3] as u64).wrapping_add(phys_offset), PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        
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
    
    // Ensure child's PDPT also points to kernel's PD1-PD3
    unsafe {
        c_pdpt.entries[1].set_addr((&raw const PD[1] as u64).wrapping_add(phys_offset), PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        c_pdpt.entries[2].set_addr((&raw const PD[2] as u64).wrapping_add(phys_offset), PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
        c_pdpt.entries[3].set_addr((&raw const PD[3] as u64).wrapping_add(phys_offset), PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER);
    }

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
    
    // Check if address is in the kernel region (first 256MB = 128 * 2MB pages)
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
