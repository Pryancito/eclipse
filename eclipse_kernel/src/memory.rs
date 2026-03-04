//! Microkernel memory management system
//! 
//! Implements:
//! - Basic paging
//! - Heap allocator
//! - Physical memory management

use linked_list_allocator::LockedHeap;
use core::sync::atomic::{AtomicU64, Ordering};

pub const PHYS_MEM_OFFSET: u64 = 0xFFFF900000000000;

/// Virtual address where the kernel is mapped (Higher Half)
pub const KERNEL_OFFSET: u64 = 0xFFFF800000000000;

/// Virtual address base for MMIO mappings (PML4[500])
pub const MMIO_VADDR_BASE: u64 = 0xFFFFFA0000000000;

/// Physical offset for virtual-to-physical address translation
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Size of the kernel region with offset-based mapping (256MB)
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

use core::alloc::{GlobalAlloc, Layout};
use spin::Mutex;

/// Wrapper for the global allocator that disables interrupts during allocations.
/// This prevents deadlocks if an interrupt handler attempts to allocate memory
/// while the interrupted code already held the allocator lock.
pub struct InterruptSafeAllocator(LockedHeap);

unsafe impl GlobalAlloc for InterruptSafeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        x86_64::instructions::interrupts::without_interrupts(|| {
            self.0.alloc(layout)
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        x86_64::instructions::interrupts::without_interrupts(|| {
            self.0.dealloc(ptr, layout)
        })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        x86_64::instructions::interrupts::without_interrupts(|| {
            self.0.alloc_zeroed(layout)
        })
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        x86_64::instructions::interrupts::without_interrupts(|| {
            self.0.realloc(ptr, layout, new_size)
        })
    }
}

#[cfg(not(test))]
#[global_allocator]
static ALLOCATOR: InterruptSafeAllocator = InterruptSafeAllocator(LockedHeap::empty());

/// Global lock for page table modifications to prevent races in SMP.
/// Must always be used with interrupts disabled to avoid deadlocks.
pub static PAGING_LOCK: Mutex<()> = Mutex::new(());

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
        
        #[cfg(not(test))]
        {
            // Get raw pointer to the inner LockedHeap and initialize it
            let allocator_ptr = &raw const ALLOCATOR;
            let allocator_ref = unsafe { &*allocator_ptr };
            let mut inner = allocator_ref.0.lock();
            
            // Initialize allocator with Higher Half address
            inner.init(heap_start_high as *mut u8, HEAP_SIZE);
            
            crate::serial::serial_print("[MEM] Allocator initialized (Interrupt-safe)\n");
        }
        #[cfg(test)]
        {
            crate::serial::serial_print("[MEM] Using std allocator for tests\n");
        }
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
pub const PAGE_PAT: u64 = 1 << 7; // PAT for 4KB pages
pub const PAGE_PAT_HUGE: u64 = 1 << 12; // PAT for Huge pages

/// Initialize Page Attribute Table (PAT)
/// 
/// Default PAT:
/// PA0: WB (06), PA1: WT (04), PA2: UC- (07), PA3: UC (00)
/// PA4: WB (06), PA5: WT (04), PA6: UC- (07), PA7: UC (00)
/// 
/// Customized PAT:
/// PA1: WC (01) -> PWT=1, PCD=0, PAT=0
pub fn init_pat() {
    unsafe {
        // Read IA32_PAT MSR (0x277). rdmsr returns the 64-bit value as EDX:EAX.
        let pat_lo: u32;
        let pat_hi: u32;
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0x277u32,
            out("eax") pat_lo,
            out("edx") pat_hi,
        );

        // Combine high:low into the full 64-bit PAT value so that PA4-PA7 are preserved.
        let mut pat = (pat_hi as u64) << 32 | (pat_lo as u64);

        // Set PA1 to WC (01). PA1 is bits 8-15.
        pat &= !(0xFF << 8);
        pat |= 0x01 << 8;

        // Per Intel SDM Vol 3 §11.12.4: flush caches and TLBs before/after writing PAT
        // to avoid undefined behavior from inconsistent memory type attributes.
        // Step 1: flush all caches.
        core::arch::asm!("wbinvd");

        // Step 2: flush TLBs by reloading CR3.  Any write to CR3 invalidates
        // all non-global TLB entries; the value written is the current CR3.
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3);
        core::arch::asm!("mov cr3, {}", in(reg) cr3);

        // Step 3: write the new PAT MSR value.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0x277u32,
            in("eax") pat as u32,
            in("edx") (pat >> 32) as u32,
        );

        // Step 4: flush TLBs again after the PAT change.
        core::arch::asm!("mov cr3, {}", in(reg) cr3);
    }
    crate::serial::serial_print("[MEM] PAT initialized (PA1=WC)\n");
}

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
    
    // Diagnostic: Print PML4 entries to find physical 0 map
    unsafe {
    }
    
    // crate::serial::serial_print("✓ Paging enabled and working\n");
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
    
    let pt_phys = pd_entry.get_addr();
    let pt_virt = phys_to_virt(pt_phys);
    let pt = unsafe { &*(pt_virt as *const PageTable) };
    let pt_entry = &pt.entries[pt_idx];
    
    crate::serial::serial_print("  PT[");
    crate::serial::serial_print_dec(pt_idx as u64);
    crate::serial::serial_print("]: ");
    crate::serial::serial_print_hex(pt_entry.entry);
    crate::serial::serial_print("\n");
}

pub fn walk_current(vaddr: u64) {
    walk_page_table(get_cr3(), vaddr);
}

/// Obtener dirección física de CR3
pub fn get_cr3() -> u64 {
    #[cfg(not(test))]
    {
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
    #[cfg(test)]
    {
        0x1000 // Mock value
    }
}

/// Establecer CR3 (para cambiar espacio de direcciones)
pub unsafe fn set_cr3(_cr3: u64) {
    #[cfg(not(test))]
    {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) _cr3,
            options(nostack, preserves_flags)
        );
    }
}

/// CR3 del kernel (higher-half); se guarda antes de ejecutar el primer proceso.
/// Usado en exec() para leer el binario desde punteros devueltos por get_service_binary.
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// Guardar el CR3 actual como "kernel CR3". Llamar una sola vez al arranque, antes del scheduler.
pub fn save_kernel_cr3() {
    KERNEL_CR3.store(get_cr3(), Ordering::SeqCst);
}

/// Obtener el CR3 del kernel (0 si no se ha llamado save_kernel_cr3).
pub fn get_kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::SeqCst)
}

use x86_64::registers::control::Cr3;
// use x86_64::structures::paging::PageTable; // Import removed to avoid conflict with local definition
use x86_64::PhysAddr;

/// Remove the identity mapping (PML4[0]) from the current page table.
/// This enforces strict Higher Half only execution for the kernel.
pub fn remove_identity_mapping() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = PAGING_LOCK.lock();
        let pml4_phys = get_cr3();
        let pml4_virt = phys_to_virt(pml4_phys);
        let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
        
        // Use recursive mapping if possible or higher half direct map
        pml4.entries[0].set_entry(0, 0); // No Present, No Read/Write
        
        x86_64::instructions::tlb::flush_all();
        
        crate::serial::serial_print("[MEM] PML4[0] (identity map) removed\n");
    });
}

/// Force physical address 0 to be mapped at virtual address 0 (Identity)
pub fn map_physical_low_memory() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = PAGING_LOCK.lock();
        let pml4_phys = get_cr3();
        let pml4_virt = phys_to_virt(pml4_phys);
        let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
        
        // PDPT address from existing PML4[288] (higher half physical map)
        let phys_map_idx = ((PHYS_MEM_OFFSET >> 39) & 0x1FF) as usize; // 288
        let pml4_phys_map = pml4.entries[phys_map_idx].get_addr();
        pml4.entries[0].set_addr(pml4_phys_map, (x86_64::structures::paging::PageTableFlags::PRESENT | 
                                            x86_64::structures::paging::PageTableFlags::WRITABLE).bits());
        
        x86_64::instructions::tlb::flush_all();
        crate::serial::serial_print("[MEM] PML4[0] (identity map) restored\n");
    });
}

/// Temporarily restore or remove identity mapping (PML4[0])
/// This is used during AP startup to allow cores to transition to long mode.
pub fn set_identity_map(enabled: bool) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = PAGING_LOCK.lock();
        let pml4_phys = get_cr3();
        let pml4 = unsafe { &mut *(phys_to_virt(pml4_phys) as *mut PageTable) };

        if enabled {
            // Restore identity mapping from physical map (index 288)
            let phys_map_idx = ((PHYS_MEM_OFFSET >> 39) & 0x1FF) as usize; // 288
            let pml4_phys_map = pml4.entries[phys_map_idx].get_addr();
            pml4.entries[0].set_addr(
                pml4_phys_map,
                (x86_64::structures::paging::PageTableFlags::PRESENT
                    | x86_64::structures::paging::PageTableFlags::WRITABLE)
                    .bits(),
            );
        } else {
            pml4.entries[0].set_addr(0, 0);
        }
        x86_64::instructions::tlb::flush_all();
    });
}

fn flush_tlb() {
    #[cfg(not(test))]
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
        if p_pml4.entries[0].present() {
            // Allocate NEW PDPT
            let (new_pdpt_ptr, new_pdpt_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PDPT");
            core::ptr::write_bytes(new_pdpt_ptr, 0, 4096);
            let new_pdpt = &mut *(new_pdpt_ptr as *mut PageTable);
            
            let p_pdpt_phys = p_pml4.entries[0].get_addr();
            let p_pdpt = &*(phys_to_virt(p_pdpt_phys) as *const PageTable);
            
            for i in 0..512 {
                if !p_pdpt.entries[i].present() { continue; }
                
                let flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(p_pdpt.entries[i].get_flags());
                let is_user = flags.contains(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
                
                if is_user {
                    // Allocate NEW PD
                    let (new_pd_ptr, new_pd_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PD");
                    core::ptr::write_bytes(new_pd_ptr, 0, 4096);
                    let new_pd = &mut *(new_pd_ptr as *mut PageTable);
                    
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
                                    new_pd.entries[j] = p_pd.entries[j].clone();
                                }
                            } else {
                                // Standard 4KB Page Table Deep Copy
                                let (new_pt_ptr, new_pt_phys) = alloc_dma_buffer(4096, 4096).expect("Failed alloc PT");
                                core::ptr::write_bytes(new_pt_ptr, 0, 4096);
                                let new_pt = &mut *(new_pt_ptr as *mut PageTable);
                                
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
                                        new_pt.entries[k].set_addr(new_frame_phys, (pt_flags | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits());
                                    } else {
                                        new_pt.entries[k] = p_pt.entries[k].clone();
                                    }
                                }
                                
                                new_pd.entries[j].set_addr(new_pt_phys, pd_flags.bits());
                            }
                        } else {
                            new_pd.entries[j] = p_pd.entries[j].clone();
                        }
                    }
                    
                    new_pdpt.entries[i].set_addr(new_pd_phys, flags.bits());
                } else {
                    new_pdpt.entries[i] = p_pdpt.entries[i].clone();
                }
            }
            
            c_pml4.entries[0].set_addr(new_pdpt_phys, (x86_64::structures::paging::PageTableFlags::from_bits_truncate(p_pml4.entries[0].get_flags()) | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits());
        }
    }

    child_pml4_phys
}

/// Map a 4KB page in a process's page table
pub fn map_user_page_4kb(pml4_phys: u64, vaddr: u64, paddr: u64, flags: u64) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = PAGING_LOCK.lock();
        let pml4_virt = phys_to_virt(pml4_phys);
        let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
        
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
            if pdpt_entry.present() && pdpt_entry.is_huge() {
                return;
            }
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
            if pd_entry.present() && pd_entry.is_huge() {
                return;
            }
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
            // NOTE: NO_EXECUTE is intentionally NOT removed here.
            // Callers that want non-executable pages (e.g. sys_mmap with PROT_READ|PROT_WRITE
            // but no PROT_EXEC) pass PageTableFlags::NO_EXECUTE in `flags`. Stripping it would
            // make every user page executable regardless of the requested protection, breaking
            // the W^X contract and making heap/stack pages exploitable as shellcode targets.
            // Callers that want executable pages simply do not set NO_EXECUTE in `flags`.

            pt_entry.set_addr(paddr, leaf_flags.bits());

            if pml4_phys == get_cr3() {
                core::arch::asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags));
            }
        }
    });
}

/// Map a 2MB page in a process's page table
pub fn map_user_page_2mb(pml4_phys: u64, vaddr: u64, paddr: u64, flags: u64) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = PAGING_LOCK.lock();
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
                 // Ensure existing entry has USER permission.
                 // Do NOT remove NO_EXECUTE from intermediate entries — doing so would silently
                 // widen the executable region to the entire 512 GB PML4 subtree, undermining
                 // any future defensive hardening that sets NX on intermediate entries.
                 let mut flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(pml4_entry.get_flags());
                 flags.insert(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
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
                 // Ensure existing entry has USER permission.
                 // Do NOT remove NO_EXECUTE from intermediate entries.
                 let mut flags = x86_64::structures::paging::PageTableFlags::from_bits_truncate(pdpt_entry.get_flags());
                 flags.insert(x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE);
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
            // NOTE: NO_EXECUTE is intentionally NOT removed for the leaf PDE.
            // See map_user_page_4kb() for the rationale.  Callers that want
            // non-executable 2MB pages (rare but valid) pass NO_EXECUTE in flags.
            
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
    });
    
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
    
    // 0. MMIO region (0xFFFFFA00...)
    if virt_addr >= MMIO_VADDR_BASE {
        return virt_addr - MMIO_VADDR_BASE;
    }
    
    // 1. Physical memory map (0xFFFF9000...)
    if virt_addr >= PHYS_MEM_OFFSET {
        return virt_addr - PHYS_MEM_OFFSET;
    }
    
    // 2. Kernel region (0xFFFF8000...)
    if virt_addr >= KERNEL_OFFSET && virt_addr < KERNEL_OFFSET + KERNEL_REGION_SIZE {
        return (virt_addr - KERNEL_OFFSET) + phys_offset;
    }
    
    // 3. Fallback (Identity map)
    virt_addr
}

pub fn phys_to_virt(phys_addr: u64) -> u64 {
    // Standard direct mapping via HHDM at 0xFFFF900000000000
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

/// Allocate a single 4KB physical frame for anonymous mmap (userspace).
/// Uses a bump allocator from a fixed physical region - NO kernel heap.
/// This avoids heap/stack collision (alloc_dma_buffer uses heap).
/// Returns Some(phys_addr) or None if exhausted.
/// Physical address is accessible at phys_to_virt(phys_addr).
static ANON_MMAP_NEXT: AtomicU64 = AtomicU64::new(0);

const ANON_MMAP_PHYS_START: u64 = 0x4000_0000;  // 1GB - leaves 0.25-1GB for GPU/DMA pools
const ANON_MMAP_PHYS_END: u64 = 0xB000_0000;    // Stop before 2.75GB (PCI hole ~3GB)

/// Dedicated physical memory region for GPU Firmware (Phase 3)
pub const GPU_FW_PHYS_BASE: u64 = 0x2000_0000;  // 512MB
pub const GPU_FW_MAX_SIZE: u64 = 32 * 1024 * 1024; // 32MB

/// Dedicated physical memory region for GSP RPC Queues (Phase 6)
pub const GPU_RPC_PHYS_BASE: u64 = 0x2200_0000; // 544MB
pub const GPU_RPC_MAX_SIZE: u64 = 1 * 1024 * 1024; // 1MB for queues

pub fn alloc_phys_frame_for_anon_mmap() -> Option<u64> {
    let next = ANON_MMAP_NEXT.fetch_add(4096, Ordering::SeqCst);
    let frame_phys = ANON_MMAP_PHYS_START + next;
    if frame_phys >= ANON_MMAP_PHYS_END {
        return None;
    }
    Some(frame_phys)
}

/// Returns (total_frames, used_frames) for the userspace physical pool.
pub fn get_memory_stats() -> (u64, u64) {
    let total = (ANON_MMAP_PHYS_END - ANON_MMAP_PHYS_START) / 4096;
    // Cap at total so the counter never reports used > total after the pool is exhausted.
    let used = (ANON_MMAP_NEXT.load(Ordering::Relaxed) / 4096).min(total);
    (total, used)
}

/// Fixed virtual address for GPU framebuffer (avoids identity-mapping page faults)
/// 8GB - above typical heap/stack/mmap, in canonical user range
const GPU_FB_VADDR_BASE: u64 = 0x0000_0002_0000_0000;

/// Map framebuffer physical memory into process page tables
/// Uses fixed vaddr + 4KB pages (same path as mmap) to avoid Page Fault 14 on identity mapping
/// Returns virtual address where framebuffer is mapped, or 0 on failure
pub fn map_framebuffer_for_process(page_table_phys: u64, fb_phys_addr: u64, fb_size: u64) -> u64 {
    use x86_64::structures::paging::PageTableFlags as Flags;
    use crate::serial;
    
    if fb_phys_addr == 0 || fb_phys_addr >= PHYS_MEM_OFFSET {
        serial::serial_print("MAP_FB: ERROR - Invalid framebuffer physical address\n");
        return 0;
    }
    
    // Align size to 4KB
    let aligned_size = (fb_size + 0xFFF) & !0xFFF;
    
    let virt_addr = GPU_FB_VADDR_BASE;
    // WC flags: PWT=1, PCD=0 (maps to PAT Index 1 which we set to WC)
    let pt_flags = (Flags::PRESENT | Flags::WRITABLE | Flags::USER_ACCESSIBLE | Flags::WRITE_THROUGH).bits();
    
    serial::serial_print("MAP_FB: Mapping ");
    serial::serial_print_dec((aligned_size / 4096) as u64);
    serial::serial_print("x4KB pages at vaddr=");
    serial::serial_print_hex(virt_addr);
    serial::serial_print(" (same path as mmap)\n");
    
    map_physical_range(page_table_phys, fb_phys_addr, aligned_size, virt_addr, pt_flags);
    
    serial::serial_print("MAP_FB: Done v=");
    serial::serial_print_hex(virt_addr);
    serial::serial_print("\n");
    
    virt_addr
}
/// Map a physical memory range into a process's page table using 4KB pages
pub fn map_physical_range(page_table_phys: u64, paddr: u64, length: u64, vaddr: u64, flags: u64) {
    let num_pages = (length + 0xFFF) / 0x1000;
    for i in 0..num_pages {
        let page_offset = i * 0x1000;
        map_user_page_4kb(page_table_phys, vaddr + page_offset, paddr + page_offset, flags);
    }
}

/// Unmap a virtual address range in a process's page table by zeroing the PTEs
/// and flushing the TLB for each page.  This enforces POSIX munmap() semantics:
/// any access to the range after this call generates a #PF.
///
/// Physical frames are NOT freed here (the bump allocator has no free-list);
/// they are reclaimed only when the whole process exits and its page tables are
/// torn down.  This is acceptable because sys_brk only ever grows the heap.
pub fn unmap_user_range(pml4_phys: u64, vaddr: u64, length: u64) {
    if length == 0 { return; }
    let aligned_start = vaddr & !0xFFF;
    let aligned_end   = (vaddr + length + 0xFFF) & !0xFFF;

    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = PAGING_LOCK.lock();
        let mut page = aligned_start;
        while page < aligned_end {
            let pml4_idx = ((page >> 39) & 0x1FF) as usize;
            let pdpt_idx = ((page >> 30) & 0x1FF) as usize;
            let pd_idx   = ((page >> 21) & 0x1FF) as usize;
            let pt_idx   = ((page >> 12) & 0x1FF) as usize;

            unsafe {
                let pml4 = &mut *(phys_to_virt(pml4_phys) as *mut PageTable);
                if !pml4.entries[pml4_idx].present() { page += 4096; continue; }

                let pdpt = &mut *(phys_to_virt(pml4.entries[pml4_idx].get_addr()) as *mut PageTable);
                if !pdpt.entries[pdpt_idx].present() { page += 4096; continue; }

                let pd = &mut *(phys_to_virt(pdpt.entries[pdpt_idx].get_addr()) as *mut PageTable);
                if !pd.entries[pd_idx].present() { page += 4096; continue; }

                if pd.entries[pd_idx].is_huge() {
                    // 2MB huge page: zero the PD entry and skip the whole 2MB region.
                    pd.entries[pd_idx].set_entry(0, 0);
                    x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(page));
                    page += 2 * 1024 * 1024;
                    continue;
                }

                let pt = &mut *(phys_to_virt(pd.entries[pd_idx].get_addr()) as *mut PageTable);
                pt.entries[pt_idx].set_entry(0, 0);
                x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(page));
            }

            page += 4096;
        }
    });
}

/// Map a physical MMIO range into the kernel's virtual address space.
///
/// Virtual address = MMIO_VADDR_BASE + paddr (unique per physical address).
/// Flags: Present + Writable + PWT + PCD  =>  UC (Uncacheable) for MMIO.
///
/// IMPORTANT: We always use the KERNEL CR3 (saved at boot before any
/// scheduler switch).  If the scheduler is already running and a user
/// process CR3 is active, `get_cr3()` would return the wrong table and
/// the MMIO mapping would silently disappear from the kernel's view on
/// the next context switch.  On real hardware this reliably prevents the
/// AHCI controller from ever being accessible.
pub fn map_mmio_range(paddr: u64, length: usize) -> u64 {
    let virt_addr = MMIO_VADDR_BASE + paddr;

    // UC flags: PWT + PCD guarantee Uncacheable on all x86_64 CPUs.
    let flags = PAGE_PRESENT | PAGE_WRITABLE | PAGE_WRITE_THROUGH | PAGE_CACHE_DISABLE;

    // Use the saved kernel CR3. Falls back to the current CR3 only if
    // save_kernel_cr3() has not been called yet (early boot).
    let kernel_cr3 = {
        let k = KERNEL_CR3.load(core::sync::atomic::Ordering::Relaxed);
        if k == 0 { get_cr3() } else { k }
    };

    // Walk/create 4-level page table entries with kernel-only flags.
    // We cannot use map_physical_range / map_user_page_4kb here because
    // those helpers forcibly add USER_ACCESSIBLE and remove NX, which is
    // wrong for kernel MMIO mappings.
    mmio_map_kernel_range(kernel_cr3, paddr, length as u64, virt_addr, flags);

    // Flush the entire TLB so the new mapping is visible on this CPU.
    flush_tlb();

    virt_addr
}

/// Walk (and create if missing) page-table levels to map a physical MMIO
/// range at a kernel virtual address using 4KB pages.
///
/// Unlike `map_user_page_4kb` this function:
///   - Never sets USER_ACCESSIBLE on any level.
///   - Does NOT strip the NX bit.
///   - Uses the caller-supplied `flags` verbatim for the leaf PTEs.
fn mmio_map_kernel_range(cr3: u64, paddr: u64, length: u64, vaddr: u64, flags: u64) {
    let num_pages = (length + 0xFFF) / 0x1000;
    for i in 0..num_pages {
        let off = i * 0x1000;
        mmio_map_kernel_page(cr3, vaddr + off, paddr + off, flags);
    }
}

/// Map a single 4KB kernel MMIO page (no USER_ACCESSIBLE on any level).
fn mmio_map_kernel_page(pml4_phys: u64, vaddr: u64, paddr: u64, flags: u64) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _lock = PAGING_LOCK.lock();
        // Intermediate entries: Present + Writable (no User, no huge).
        const INTER: u64 = PAGE_PRESENT | PAGE_WRITABLE;

        let pml4_idx = ((vaddr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((vaddr >> 30) & 0x1FF) as usize;
        let pd_idx   = ((vaddr >> 21) & 0x1FF) as usize;
        let pt_idx   = ((vaddr >> 12) & 0x1FF) as usize;

        unsafe {
            // PML4
            let pml4 = &mut *(phys_to_virt(pml4_phys) as *mut PageTable);
            let pml4_e = &mut pml4.entries[pml4_idx];
            if !pml4_e.present() {
                if let Some((ptr, phys)) = alloc_dma_buffer(4096, 4096) {
                    core::ptr::write_bytes(ptr, 0, 4096);
                    pml4_e.set_entry(phys, INTER);
                } else { return; }
            }
            let pdpt_phys = pml4_e.get_addr();

            // PDPT
            let pdpt = &mut *(phys_to_virt(pdpt_phys) as *mut PageTable);
            let pdpt_e = &mut pdpt.entries[pdpt_idx];
            if pdpt_e.is_huge() { return; }
            if !pdpt_e.present() {
                if let Some((ptr, phys)) = alloc_dma_buffer(4096, 4096) {
                    core::ptr::write_bytes(ptr, 0, 4096);
                    pdpt_e.set_entry(phys, INTER);
                } else { return; }
            }
            let pd_phys = pdpt_e.get_addr();

            // PD
            let pd = &mut *(phys_to_virt(pd_phys) as *mut PageTable);
            let pd_e = &mut pd.entries[pd_idx];
            if pd_e.is_huge() { return; }
            if !pd_e.present() {
                if let Some((ptr, phys)) = alloc_dma_buffer(4096, 4096) {
                    core::ptr::write_bytes(ptr, 0, 4096);
                    pd_e.set_entry(phys, INTER);
                } else { return; }
            }
            let pt_phys = pd_e.get_addr();

            // PT (leaf)
            let pt = &mut *(phys_to_virt(pt_phys) as *mut PageTable);
            pt.entries[pt_idx].set_entry(paddr, flags);
        }
    });
}
