//! ELF Loader para cargar binarios en userspace

use crate::process::{create_process, ProcessId};
use crate::memory;
use crate::serial;
use core::arch::asm;

/// ELF Header (64-bit)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// Program Header (64-bit)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64ProgramHeader {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

const PT_LOAD: u32 = 1;
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const USER_ADDR_MAX: u64 = 0x0000_7FFF_FFFF_FFFF;

/// Cargar binario ELF en memoria y crear proceso
pub fn load_elf(elf_data: &[u8]) -> Option<ProcessId> {
    // Verificar header ELF
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        serial::serial_print("ELF: File too small\n");
        return None;
    }
    
    let header = unsafe {
        &*(elf_data.as_ptr() as *const Elf64Header)
    };
    
    // Verificar magic number
    if &header.e_ident[0..4] != &ELF_MAGIC {
        serial::serial_print("ELF: Invalid magic number\n");
        return None;
    }
    
    // Verificar que sea 64-bit
    if header.e_ident[4] != 2 {
        serial::serial_print("ELF: Not 64-bit\n");
        return None;
    }
    
    serial::serial_print("ELF: Valid header found\n");
    serial::serial_print("ELF: Entry point: ");
    serial::serial_print_hex(header.e_entry);
    serial::serial_print("\n");

    // Validate Entry Point
    if header.e_entry > USER_ADDR_MAX {
         serial::serial_print("ELF: Entry point in kernel space (Security Violation)\n");
         return None;
    }
    
    // Iterate over program headers and load segments
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    
    if elf_data.len() < ph_offset + (ph_count * ph_size) {
        serial::serial_print("ELF: Program headers out of bounds\n");
        return None;
    }
    
    // Check segments for validity BEFORE creating process
    for i in 0..ph_count {
        let offset = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[offset..].as_ptr() as *const Elf64ProgramHeader) };
        
        if ph.p_type == PT_LOAD {
            if ph.p_vaddr > USER_ADDR_MAX || (ph.p_vaddr + ph.p_memsz) > USER_ADDR_MAX {
                serial::serial_print("ELF: Segment overlaps kernel space (Security Violation)\n");
                return None;
            }
        }
    }
    
    // Default user stack at 512MB
    let stack_base = 0x20000000; // 512MB
    let stack_size = 0x40000;  // 256KB
    
    let pid = create_process(header.e_entry, stack_base, stack_size)?;
    crate::fd::fd_init_stdio(pid); // Initialize stdio (log:)
    
    // Get the process to access its page table
    let page_table_phys = {
        let table = crate::process::PROCESS_TABLE.lock();
        let p = table[pid as usize].as_ref().unwrap();
        p.page_table_phys
    };
    
    // Allocate and map user stack
    if let Some((_ptr, phys)) = crate::memory::alloc_dma_buffer(stack_size, 0x200000) {
        serial::serial_print("ELF: Mapping stack at ");
        serial::serial_print_hex(stack_base);
        serial::serial_print("\n");
        // We map the 2MB block using 4KB pages for consistency and safety
        // CRITICAL: Must include PAGE_USER flag so Ring 3 can access the stack
        for i in 0..512 {
            let offset = (i as u64) * 0x1000;
            crate::memory::map_user_page_4kb(
                page_table_phys, 
                stack_base + offset, 
                phys + offset, 
                crate::memory::PAGE_WRITABLE | crate::memory::PAGE_USER
            );
        }
        
        crate::memory::walk_page_table(page_table_phys, stack_base);
    }

    // Keep track of mapped 2MB regions to handle segments sharing the same page
    #[derive(Clone, Copy)]
    struct MappedPage {
        vaddr_base: u64,
        kernel_ptr: *mut u8,
        phys_addr: u64,
    }
    let mut mapped_pages: [Option<MappedPage>; 8] = [None; 8];
    let mut mapped_count = 0;

    // Iterate over program headers and load segments
    for i in 0..ph_count {
        let offset = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[offset..].as_ptr() as *const Elf64ProgramHeader) };
        
        if ph.p_type == PT_LOAD {
            let vaddr_start = ph.p_vaddr;
            let vaddr_page_base = vaddr_start & !0x1FFFFF;
            
            // Find or create mapped page
            let mut current_page: Option<&MappedPage> = None;
            for j in 0..mapped_count {
                if let Some(ref mp) = mapped_pages[j] {
                    if mp.vaddr_base == vaddr_page_base {
                        current_page = Some(mp);
                        break;
                    }
                }
            }
            
            let target_kernel_ptr = if let Some(mp) = current_page {
                mp.kernel_ptr
            } else {
                serial::serial_print("ELF: Mapping page at ");
                serial::serial_print_hex(vaddr_page_base);
                serial::serial_print("\n");
                
                // Allocate new 2MB block
                if let Some((kptr, phys)) = crate::memory::alloc_dma_buffer(0x200000, 0x200000) {
                    // Zero the block
                    unsafe { core::ptr::write_bytes(kptr, 0, 0x200000); }
                    
                    let mp = MappedPage {
                        vaddr_base: vaddr_page_base,
                        kernel_ptr: kptr,
                        phys_addr: phys,
                    };
                    mapped_pages[mapped_count] = Some(mp);
                    mapped_count += 1;
                    
                    // Map it (CRITICAL: must be done for the segment to be accessible in user space)
                    // We map the 2MB block using 512 4KB pages to be absolutely safe and avoid PSE issues
                    // CRITICAL: Must include PAGE_USER flag so Ring 3 can access these pages
                    for i in 0..512 {
                        let offset = (i as u64) * 0x1000;
                        crate::memory::map_user_page_4kb(
                            page_table_phys, 
                            vaddr_page_base + offset, 
                            phys + offset, 
                            crate::memory::PAGE_WRITABLE | crate::memory::PAGE_USER
                        );
                    }
                    
                    // Diagnostic walk for the entry point specifically
                    // crate::memory::walk_page_table(page_table_phys, vaddr_start);
                    
                    kptr
                } else {
                    return None;
                }
            };

            // Copy segment data
            if ph.p_filesz > 0 {
                serial::serial_print("ELF: Copying segment to ");
                serial::serial_print_hex(vaddr_start);
                serial::serial_print("\n");
                
                let file_offset = ph.p_offset as usize;
                let in_page_offset = (vaddr_start - vaddr_page_base) as usize;
                unsafe {
                    let src = elf_data.as_ptr().add(file_offset);
                    let dst = target_kernel_ptr.add(in_page_offset);
                    core::ptr::copy_nonoverlapping(src, dst, ph.p_filesz as usize);
                }
            }
            
            // BSS is already zeroed because we zeroed the whole 2MB block
        }
    }
    
    Some(pid)
}

/// Inicializar ELF loader
pub fn init() {
    serial::serial_print("ELF loader initialized\n");
}

/// Replace current process image with ELF binary (for exec())
/// Returns entry point if successful
pub fn replace_process_image(elf_data: &[u8]) -> Option<u64> {
    // Verify ELF header
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        serial::serial_print("ELF: File too small for exec\n");
        return None;
    }
    
    let header = unsafe {
        &*(elf_data.as_ptr() as *const Elf64Header)
    };
    
    // Verify magic number
    if &header.e_ident[0..4] != &ELF_MAGIC {
        serial::serial_print("ELF: Invalid magic number for exec\n");
        return None;
    }
    
    // Verify 64-bit
    if header.e_ident[4] != 2 {
        serial::serial_print("ELF: Not 64-bit for exec\n");
        return None;
    }
    
    serial::serial_print("ELF: Valid exec binary, entry: ");
    serial::serial_print_hex(header.e_entry);
    serial::serial_print("\n");
    
    // DIAGNOSTIC: Print first 16 bytes of ELF buffer
    serial::serial_print("ELF: First 16 bytes: ");
    for i in 0..16 {
        serial::serial_print_hex(elf_data[i] as u64);
        serial::serial_print(" ");
    }
    serial::serial_print("\n");
    
    // DIAGNOSTIC: Print e_entry raw bytes
    serial::serial_print("ELF: e_entry bytes: ");
    let entry_bytes = unsafe { core::slice::from_raw_parts(&header.e_entry as *const u64 as *const u8, 8) };
    for i in 0..8 {
        serial::serial_print_hex(entry_bytes[i] as u64);
        serial::serial_print(" ");
    }
    serial::serial_print("\n");

    // Validate Entry Point
    if header.e_entry > USER_ADDR_MAX {
         serial::serial_print("ELF: Entry point in kernel space (Security Violation)\n");
         return None;
    }
    
    // Iterate over program headers and load segments
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    
    if elf_data.len() < ph_offset + (ph_count * ph_size) {
        serial::serial_print("ELF: Program headers out of bounds for exec\n");
        return None;
    }

    // Check segments for validity BEFORE loading
    for i in 0..ph_count {
        let offset = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[offset..].as_ptr() as *const Elf64ProgramHeader) };
        
        if ph.p_type == PT_LOAD {
            if ph.p_vaddr > USER_ADDR_MAX || (ph.p_vaddr + ph.p_memsz) > USER_ADDR_MAX {
                serial::serial_print("ELF: Segment overlaps kernel space (Security Violation)\n");
                return None;
            }
        }
    }
    
    let page_table_phys = crate::memory::get_cr3();

    // Keep track of mapped 2MB regions
    #[derive(Clone, Copy)]
    struct MappedPage {
        vaddr_base: u64,
        kernel_ptr: *mut u8,
        phys_addr: u64,
    }
    let mut mapped_pages: [Option<MappedPage>; 8] = [None; 8];
    let mut mapped_count = 0;

    for i in 0..ph_count {
        let offset = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[offset..].as_ptr() as *const Elf64ProgramHeader) };
        
        if ph.p_type == PT_LOAD {
            let vaddr_start = ph.p_vaddr;
            let vaddr_page_base = vaddr_start & !0x1FFFFF;
            
            // Find or create mapped page
            let mut current_page: Option<&MappedPage> = None;
            for j in 0..mapped_count {
                if let Some(ref mp) = mapped_pages[j] {
                    if mp.vaddr_base == vaddr_page_base {
                        current_page = Some(mp);
                        break;
                    }
                }
            }
            
            let target_kernel_ptr = if let Some(mp) = current_page {
                mp.kernel_ptr
            } else {
                serial::serial_print("ELF: Mapping page for exec at ");
                serial::serial_print_hex(vaddr_page_base);
                serial::serial_print("\n");
                
                // Allocate new 2MB block
                if let Some((kptr, phys)) = crate::memory::alloc_dma_buffer(0x200000, 0x200000) {
                    // Zero the block
                    unsafe { core::ptr::write_bytes(kptr, 0, 0x200000); }
                    
                    let mp = MappedPage {
                        vaddr_base: vaddr_page_base,
                        kernel_ptr: kptr,
                        phys_addr: phys,
                    };
                    mapped_pages[mapped_count] = Some(mp);
                    mapped_count += 1;
                    
                    // Map it
                    // We map the 2MB block using 4KB pages for consistency and safety
                    for i in 0..512 {
                        let offset = (i as u64) * 0x1000;
                        crate::memory::map_user_page_4kb(
                            page_table_phys, 
                            vaddr_page_base + offset, 
                            phys + offset, 
                            crate::memory::PAGE_USER | crate::memory::PAGE_WRITABLE
                        );
                    }
                    
                    serial::serial_print("ELF: Mapping page for exec at ");
                    serial::serial_print_hex(vaddr_page_base);
                    serial::serial_print(" to phys ");
                    serial::serial_print_hex(phys);
                    serial::serial_print("\n");
                    
                    crate::memory::walk_page_table(page_table_phys, vaddr_page_base);
                    kptr
                } else {
                    serial::serial_print("ELF: Failed to allocate 2MB block\n");
                    return None;
                }
            };

            // Copy segment data
            if ph.p_filesz > 0 {
                serial::serial_print("ELF: Copying segment for exec to ");
                serial::serial_print_hex(vaddr_start);
                serial::serial_print("\n");
                
                let file_offset = ph.p_offset as usize;
                let in_page_offset = (vaddr_start - vaddr_page_base) as usize;
                unsafe {
                    let src = elf_data.as_ptr().add(file_offset);
                    let dst = target_kernel_ptr.add(in_page_offset);
                    core::ptr::copy_nonoverlapping(src, dst, ph.p_filesz as usize);
                }
            }
        }
    }

    // Flush TLB to ensure new mappings are active
    unsafe { x86_64::instructions::tlb::flush_all(); }

    Some(header.e_entry)
}

/// Jump to entry point in userspace (Ring 3)
/// This function never returns
/// 
/// # Safety
/// This function constructs a stack frame and executes `iretq` to switch privilege levels.
/// It MUST be called with a valid userspace entry point and stack top.
/// CR3 should already be set to the correct process address space before calling this.
pub unsafe extern "C" fn jump_to_userspace(entry_point: u64, stack_top: u64) -> ! {
    // FORCE PRINT to ensure we reached this point
    serial::serial_print("ELF: JUMPING TO USERSPACE NOW!\n");
    serial::serial_print("  Entry: ");
    serial::serial_print_hex(entry_point);
    serial::serial_print("\n  Stack: ");
    serial::serial_print_hex(stack_top);
    serial::serial_print("\n");

    // Verify entry point is in user space
    if entry_point >= USER_ADDR_MAX {
        serial::serial_print("ERROR: Entry point in kernel space!\n");
        loop { core::arch::asm!("hlt"); }
    }
    
    // System V ABI x86-64 stack alignment requirements:
    // - At program entry: RSP must be 16-byte aligned (RSP & 0xF == 0)
    // - Before CALL instruction: Stack arranged so (RSP+8) is 16-byte aligned
    //   (because CALL pushes return address, making RSP misaligned after)
    //
    // We're at program entry (not a function call), so RSP should be 16-byte aligned.
    // Stack layout at program start:
    //   [RSP+0]  = argc
    //   [RSP+8]  = argv[0] (or NULL if no args)
    //   [RSP+16] = argv[1] (or NULL as terminator)
    //   ...
    //   [RSP + 8*(argc+1)] = NULL (end of argv)
    //   [RSP + 8*(argc+2)] = envp[0] (or NULL)
    //   ...
    
    // For argc=0, we need 3 quadwords: argc, argv[0]=NULL, envp[0]=NULL
    let adjusted_stack = (stack_top - 24) & !0xF; // 3 quadwords, 16-byte aligned
    
    // Write argc/argv/envp to user stack at the location where RSP will be
    // IMPORTANT: We can access user memory here because CR3 is set to user process's page table
    unsafe {
        let stack_ptr = adjusted_stack as *mut u64;
        *stack_ptr.offset(0) = 0; // argc = 0
        *stack_ptr.offset(1) = 0; // argv[0] = NULL  
        *stack_ptr.offset(2) = 0; // envp[0] = NULL
    }

    let user_cs: u64 = 0x1b; // 0x18 | 3
    let user_ds: u64 = 0x23; // 0x20 | 3
    let rflags: u64 = 0x202; // Interrupciones habilitadas
    
    asm!(
        "cli",              // Disable interrupts during transition
        
        // Build iretq frame manually on kernel stack
        "push {u_ss}",
        "push {u_rsp}",
        "push {u_flags}",
        "push {u_cs}",
        "push {u_rip}",
        
        // Set up user data segments
        "mov ax, {ds_sel:x}",
        "mov ds, ax",
        "mov es, ax",
        
        // Zero FS and GS segments
        "xor ax, ax",
        "mov fs, ax",
        "mov gs, ax",
        
        // COMPLETE REGISTER HYGIENE
        "xor rax, rax",
        "xor rbx, rbx",
        "xor rcx, rcx",
        "xor rdx, rdx",
        "xor rsi, rsi",
        "xor rdi, rdi",
        "xor rbp, rbp",
        "xor r8, r8",
        "xor r9, r9",
        "xor r10, r10",
        "xor r11, r11",
        "xor r12, r12",
        "xor r13, r13",
        "xor r14, r14",
        "xor r15, r15",
        
        "iretq",
        
        u_ss = in(reg) user_ds,
        u_rsp = in(reg) adjusted_stack,
        u_flags = in(reg) rflags,
        u_cs = in(reg) user_cs,
        u_rip = in(reg) entry_point,
        ds_sel = in(reg) user_ds,
        options(noreturn)
    );
}

