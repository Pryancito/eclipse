//! ELF Loader para cargar binarios en userspace

use crate::process::{create_process, current_process_id, get_process, ProcessId};
use crate::memory;
use crate::serial;
use core::arch::asm;
use core::ptr::write_volatile;

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
const PF_X: u32 = 1;  // Segment executable
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const USER_ADDR_MAX: u64 = 0x0000_7FFF_FFFF_FFFF;
/// Minimum sane entry point; anything below is inside ELF header (64 bytes) or bogus
const MIN_ENTRY_POINT: u64 = 0x80;


/// Cargar los segmentos del ELF en el espacio de direcciones especificado
/// Devuelve Ok((entry_point, max_vaddr))
pub fn load_elf_into_space(page_table_phys: u64, elf_data: &[u8]) -> Result<(u64, u64), &'static str> {
    // Verificar header ELF
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: File too small");
    }
    
    let header = unsafe {
        &*(elf_data.as_ptr() as *const Elf64Header)
    };
    
    // Verificar magic number
    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: Invalid magic number");
    }
    
    // Iterate over program headers and load segments
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    
    if elf_data.len() < ph_offset + (ph_count * ph_size) {
        return Err("ELF: Program headers out of bounds");
    }
    if header.e_entry < MIN_ENTRY_POINT {
        return Err("ELF: Entry point in header or invalid (e_entry < 0x80)");
    }
    // Entry must lie inside an executable PT_LOAD segment
    let mut entry_in_exec_segment = false;
    for i in 0..ph_count {
        let off = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD && (ph.p_flags & PF_X) != 0 {
            if header.e_entry >= ph.p_vaddr && header.e_entry < ph.p_vaddr + ph.p_memsz {
                entry_in_exec_segment = true;
                break;
            }
        }
    }
    if !entry_in_exec_segment {
        return Err("ELF: Entry point not in executable segment");
    }

    // Keep track of mapped 2MB regions to handle segments sharing the same page
    #[derive(Clone, Copy)]
    struct MappedPage {
        vaddr_base: u64,
        kernel_ptr: *mut u8,
        phys_addr: u64,
    }
    let mut mapped_pages: [Option<MappedPage>; 64] = [None; 64];
    let mut mapped_count = 0;
    let mut max_vaddr: u64 = 0;

    // Iterate over program headers and load segments
    for i in 0..ph_count {
        let ph_offset_entry = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[ph_offset_entry..].as_ptr() as *const Elf64ProgramHeader) };
        
        if ph.p_type == PT_LOAD {
            let vaddr_start = ph.p_vaddr;
            let vaddr_end = vaddr_start + ph.p_memsz;
            let file_size = ph.p_filesz;
            let file_start_offset = ph.p_offset;
            
            if vaddr_end > max_vaddr {
                max_vaddr = vaddr_end;
            }

            // Align start and end to 2MB boundaries
            let page_start = vaddr_start & !0x1FFFFF;
            let page_end = (vaddr_end + 0x1FFFFF) & !0x1FFFFF;

            let mut current_vaddr = page_start;
            while current_vaddr < page_end {
                // Find or create mapped page
                let mut current_page: Option<MappedPage> = None;
                for j in 0..mapped_count {
                    if let Some(mp) = mapped_pages[j] {
                        if mp.vaddr_base == current_vaddr {
                            current_page = Some(mp);
                            break;
                        }
                    }
                }
                
                let target_kernel_ptr = if let Some(mp) = current_page {
                    mp.kernel_ptr
                } else {
                    if mapped_count >= mapped_pages.len() {
                        return Err("ELF: Too many segments/pages (limit 16)");
                    }

                    // Allocate new 2MB block
                    if let Some((kptr, phys)) = crate::memory::alloc_dma_buffer(0x200000, 0x200000) {
                        // Zero the block
                        unsafe { core::ptr::write_bytes(kptr, 0, 0x200000); }
                        
                        let mp = MappedPage {
                            vaddr_base: current_vaddr,
                            kernel_ptr: kptr,
                            phys_addr: phys,
                        };
                        mapped_pages[mapped_count] = Some(mp);
                        mapped_count += 1;
                        
                        // Map it in 4KB pages
                        for i in 0..512 {
                            let offset = (i as u64) * 0x1000;
                            crate::memory::map_user_page_4kb(
                                page_table_phys, 
                                current_vaddr + offset, 
                                phys + offset, 
                                crate::memory::PAGE_WRITABLE | crate::memory::PAGE_USER
                            );
                        }
                        kptr
                    } else {
                        return Err("Failed to allocate segment 2MB page");
                    }
                };

                // Copy part of the segment data that falls into this 2MB page
                if file_size > 0 {
                    let page_vaddr_start = current_vaddr;
                    let page_vaddr_end = current_vaddr + 0x200000;

                    // Range intersection [vaddr_start, vaddr_start + file_size) AND [page_vaddr_start, page_vaddr_end)
                    let intersect_start = core::cmp::max(vaddr_start, page_vaddr_start);
                    let intersect_end = core::cmp::min(vaddr_start + file_size, page_vaddr_end);

                    if intersect_start < intersect_end {
                        let copy_size = (intersect_end - intersect_start) as usize;
                        let in_file_offset = (intersect_start - vaddr_start) as usize;
                        let in_page_offset = (intersect_start - page_vaddr_start) as usize;

                        unsafe {
                            let src = elf_data.as_ptr().add(file_start_offset as usize + in_file_offset);
                            let dst = target_kernel_ptr.add(in_page_offset);
                            core::ptr::copy_nonoverlapping(src, dst, copy_size);
                        }
                    }
                }

                current_vaddr += 0x200000;
            }
        }
    }
    
    // Align max_vaddr to next 4KB page
    let max_vaddr_aligned = (max_vaddr + 0xFFF) & !0xFFF;
    
    Ok((header.e_entry, max_vaddr_aligned))
}

/// Preparar el stack de usuario
pub fn setup_user_stack(page_table_phys: u64, stack_base: u64, stack_size: usize) -> Result<u64, &'static str> {
    // Allocate and map user stack
    if let Some((_ptr, phys)) = crate::memory::alloc_dma_buffer(stack_size, 0x200000) {
        // We map the 2MB block using 4KB pages for consistency and safety
        // CRITICAL: Must include PAGE_USER flag so Ring 3 can access the stack
        for i in 0..(stack_size / 4096) {
            let offset = (i as u64) * 0x1000;
            crate::memory::map_user_page_4kb(
                page_table_phys, 
                stack_base + offset, 
                phys + offset, 
                crate::memory::PAGE_WRITABLE | crate::memory::PAGE_USER
            );
        }
        
        // crate::memory::walk_page_table(page_table_phys, stack_base);
        Ok(stack_base + stack_size as u64)
    } else {
        Err("Failed to allocate user stack")
    }
}

/// Cargar binario ELF en memoria y crear proceso
pub fn load_elf(elf_data: &[u8]) -> Option<ProcessId> {
    // We need a temporary space to get page_table_phys
    let cr3 = crate::memory::create_process_paging();

    let (entry_point, max_vaddr) = match load_elf_into_space(cr3, elf_data) {
        Ok(res) => res,
        Err(e) => {
            serial::serial_print("ELF: Load failed: ");
            serial::serial_print(e);
            serial::serial_print("\n");
            return None;
        }
    };
    let (phdr_va, phnum, phentsize) = get_elf_phdr_info(elf_data).ok()?;

    // Default user stack at 512MB
    let stack_base = 0x20000000; // 512MB
    let stack_size = 0x40000;  // 256KB
    
    let pid = create_process(entry_point, stack_base, stack_size, phdr_va, phnum, phentsize, max_vaddr)?;
    crate::fd::fd_init_stdio(pid);

    if let Err(e) = setup_user_stack(cr3, stack_base, stack_size) {
        serial::serial_print("ELF: Stack setup failed: ");
        serial::serial_print(e);
        serial::serial_print("\n");
        return None;
    }

    Some(pid)
}

fn load_info(elf_data: &[u8]) -> Result<(u64, usize, usize, usize), &'static str> {
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: File too small");
    }
    
    let header = unsafe {
        &*(elf_data.as_ptr() as *const Elf64Header)
    };
    
    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: Invalid magic number");
    }
    
    Ok((header.e_entry, header.e_phoff as usize, header.e_phnum as usize, header.e_phentsize as usize))
}

/// Inicializar ELF loader
pub fn init() {
    serial::serial_print("ELF loader initialized\n");
}

/// Return (phdr_va, phnum, phentsize) for an ELF so the kernel can put AT_PHDR/AT_PHNUM/AT_PHENT in auxv.
/// phdr_va = first PT_LOAD.p_vaddr + e_phoff (program headers live at that VA in the loaded image).
pub fn get_elf_phdr_info(elf_data: &[u8]) -> Result<(u64, u64, u64), &'static str> {
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF: file too small");
    }
    let header = unsafe { &*(elf_data.as_ptr() as *const Elf64Header) };
    if &header.e_ident[0..4] != &ELF_MAGIC {
        return Err("ELF: invalid magic");
    }
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    if elf_data.len() < ph_offset + ph_count * ph_size {
        return Err("ELF: phdr out of bounds");
    }
    let mut first_load_vaddr = None;
    for i in 0..ph_count {
        let off = ph_offset + i * ph_size;
        let ph = unsafe { &*(elf_data[off..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD {
            first_load_vaddr = Some(ph.p_vaddr);
            break;
        }
    }
    let first_load_vaddr = first_load_vaddr.ok_or("ELF: no PT_LOAD")?;
    let phdr_va = first_load_vaddr + header.e_phoff;
    Ok((phdr_va, header.e_phnum as u64, header.e_phentsize as u64))
}

/// Replace current process image with ELF binary (for exec())
/// Returns Ok((entry_point, max_vaddr, phdr_va, phnum, phentsize)) or Err(message) for userspace to display
pub fn replace_process_image(elf_data: &[u8]) -> Result<(u64, u64, u64, u64, u64), &'static str> {
    // Verify ELF header
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        serial::serial_print("ELF: File too small for exec\n");
        return Err("ELF: file too small");
    }
    
    let header = unsafe {
        &*(elf_data.as_ptr() as *const Elf64Header)
    };
    
    // Verify magic number
    if &header.e_ident[0..4] != &ELF_MAGIC {
        serial::serial_print("ELF: Invalid magic number for exec\n");
        return Err("ELF: invalid magic");
    }
    
    // Verify 64-bit
    if header.e_ident[4] != 2 {
        serial::serial_print("ELF: Not 64-bit for exec\n");
        return Err("ELF: not 64-bit");
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

    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;

    // Validate Entry Point
    if header.e_entry > USER_ADDR_MAX {
         serial::serial_print("ELF: Entry point in kernel space (Security Violation)\n");
         return Err("ELF: entry in kernel space");
    }
    if header.e_entry < MIN_ENTRY_POINT {
        serial::serial_print("ELF: Entry point in ELF header or invalid (e_entry < 0x80)\n");
        return Err("ELF: entry < 0x80");
    }
    // Entry must lie inside an executable PT_LOAD segment (avoid jumping into ELF header/data)
    let mut entry_in_exec_segment = false;
    for i in 0..ph_count {
        let offset = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[offset..].as_ptr() as *const Elf64ProgramHeader) };
        if ph.p_type == PT_LOAD && (ph.p_flags & PF_X) != 0 {
            if header.e_entry >= ph.p_vaddr && header.e_entry < ph.p_vaddr + ph.p_memsz {
                entry_in_exec_segment = true;
                break;
            }
        }
    }
    if !entry_in_exec_segment {
        serial::serial_print("ELF: Entry point not in executable segment (invalid or corrupted ELF)\n");
        return Err("ELF: entry not in exec segment");
    }

    if elf_data.len() < ph_offset + (ph_count * ph_size) {
        serial::serial_print("ELF: Program headers out of bounds for exec\n");
        return Err("ELF: phdr out of bounds");
    }

    // Check segments for validity BEFORE loading
    for i in 0..ph_count {
        let offset = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[offset..].as_ptr() as *const Elf64ProgramHeader) };
        
        if ph.p_type == PT_LOAD {
            if ph.p_vaddr > USER_ADDR_MAX || (ph.p_vaddr + ph.p_memsz) > USER_ADDR_MAX {
                serial::serial_print("ELF: Segment overlaps kernel space (Security Violation)\n");
                return Err("ELF: segment in kernel space");
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
    let mut mapped_pages: [Option<MappedPage>; 128] = [None; 128];
    let mut mapped_count = 0;

    let (entry_point, max_vaddr) = load_elf_into_space(page_table_phys, elf_data)?;
    let (phdr_va, phnum, phentsize) = get_elf_phdr_info(elf_data).map_err(|_| "ELF: phdr info")?;
    Ok((entry_point, max_vaddr, phdr_va, phnum, phentsize))
}

/// Jump to entry point in userspace (Ring 3)
/// phdr_va, phnum, and phentsize are written to auxv (AT_PHDR, AT_PHNUM, AT_PHENT) so glibc can find program headers.
/// This function never returns
///
/// # Safety
/// This function constructs a stack frame and executes `iretq` to switch privilege levels.
/// It MUST be called with a valid userspace entry point and stack top.
/// CR3 should already be set to the correct process address space before calling this.
pub unsafe extern "C" fn jump_to_userspace(entry_point: u64, stack_top: u64, phdr_va: u64, phnum: u64, phentsize: u64) -> ! {
    // FORCE PRINT to ensure we reached this point
    serial::serial_print("ELF: JUMPING TO USERSPACE NOW!\n");
    serial::serial_print("  Entry: ");
    serial::serial_print_hex(entry_point);
    serial::serial_print("  Stack: ");
    serial::serial_print_hex(stack_top);
    serial::serial_print("  phdr_va: ");
    serial::serial_print_hex(phdr_va);
    serial::serial_print("  phnum: ");
    serial::serial_print_dec(phnum);
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
    // Stack layout at program start (System V ABI):
    //   [RSP+0]  = argc
    //   [RSP+8]  = argv[0] (or NULL if no args)
    //   [RSP+16] = argv[1] (or NULL as terminator)
    //   ...
    //   [RSP + 8*(argc+1)] = NULL (end of argv)
    //   [RSP + 8*(argc+2)] = envp[0] (or NULL)
    //   ...
    //   [RSP + 8*(argc+2+envc)] = NULL (end of envp)
    //   [RSP + ...] = auxv[0].a_type (auxiliary vector entries)
    //   [RSP + ...] = auxv[0].a_val
    //   ...
    //   [RSP + ...] = AT_NULL (0) to terminate auxv
    //   [RSP + ...] = 0 (value for AT_NULL)
    
    // For argc=0 with auxv so glibc works.
    // - 1 qword: argc = 0
    // - 1 qword: argv[0] = NULL (argv terminator)
    // - 1 qword: envp[0] = NULL (envp terminator)
    // - auxv: AT_PHDR(3), AT_PHENT(4), AT_PHNUM(5), AT_PAGESZ(6), AT_RANDOM(25), AT_NULL(0)
    // - 16 bytes for AT_RANDOM data
    // Total auxv entries: 5 (phdr, phent, phnum, pagesz, random) + 1 (null) = 6 entries * 2 qwords = 12 qwords
    // Total: 3 (argc/v/p) + 12 (auxv) + 2 (random data) = 17 quadwords = 136 bytes.
    // We subtract 144 bytes to keep 16-byte alignment.
    const AT_PAGESZ: u64 = 6;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_RANDOM: u64 = 25;
    const AT_NULL: u64 = 0;
    let adjusted_stack = (stack_top - 144) & !0xF;

    // Ensure we're using the current process's CR3 so the write lands in user space (not kernel view)
    if let Some(pid) = current_process_id() {
        if let Some(proc) = get_process(pid) {
            let want_cr3 = proc.page_table_phys;
            let cur_cr3 = memory::get_cr3();
            if want_cr3 != 0 && want_cr3 != cur_cr3 {
                serial::serial_print("ELF: Setting CR3 to process before writing auxv\n");
                unsafe { memory::set_cr3(want_cr3); }
            }
        }
    }

    // Write argc/argv/envp/auxv. Put AT_PHDR/AT_PHENT/AT_PHNUM first (some glibc inits use them early).
    unsafe {
        let stack_ptr = adjusted_stack as *mut u64;
        write_volatile(stack_ptr.offset(0), 0u64);         // argc = 0
        write_volatile(stack_ptr.offset(1), 0u64);        // argv[0] = NULL (end of argv)
        write_volatile(stack_ptr.offset(2), 0u64);        // envp[0] = NULL (end of envp)
        write_volatile(stack_ptr.offset(3), AT_PHDR);
        write_volatile(stack_ptr.offset(4), phdr_va);
        write_volatile(stack_ptr.offset(5), AT_PHENT);
        write_volatile(stack_ptr.offset(6), phentsize);
        write_volatile(stack_ptr.offset(7), AT_PHNUM);
        write_volatile(stack_ptr.offset(8), phnum);
        write_volatile(stack_ptr.offset(9), AT_PAGESZ);
        write_volatile(stack_ptr.offset(10), 4096u64);
        write_volatile(stack_ptr.offset(11), AT_RANDOM);
        write_volatile(stack_ptr.offset(12), (adjusted_stack + 15 * 8) as u64); // Points to random data
        write_volatile(stack_ptr.offset(13), AT_NULL);
        write_volatile(stack_ptr.offset(14), 0u64);
        // Random data (16 bytes)
        write_volatile(stack_ptr.offset(15), 0x12345678_9ABCDEF0u64);
        write_volatile(stack_ptr.offset(16), 0x0FEDCBA9_87654321u64);
    }

    // Frame iretq: CPU hace pop en orden RIP, CS, RFLAGS, RSP, SS.
    // Construir en static para que el compilador no pueda corromper ni optimizar el RIP.
    use core::ptr::addr_of_mut;
    static mut IRET_FRAME: [u64; 5] = [0, 0x1b, 0x202, 0, 0x23]; // RIP, CS, RFLAGS, RSP, SS
    unsafe {
        let f = addr_of_mut!(IRET_FRAME);
        (*f)[0] = entry_point;
        (*f)[3] = adjusted_stack;
    }
    let frame_ptr = unsafe { core::ptr::addr_of!(IRET_FRAME) };

    // Copiar frame al stack del kernel y iretq
    unsafe {
        asm!(
            "cli",
            "sub rsp, 40",
            "mov rax, [rdi]",
            "mov [rsp], rax",
            "mov rax, [rdi + 8]",
            "mov [rsp + 8], rax",
            "mov rax, [rdi + 16]",
            "mov [rsp + 16], rax",
            "mov rax, [rdi + 24]",
            "mov [rsp + 24], rax",
            "mov rax, [rdi + 32]",
            "mov [rsp + 32], rax",
            "mov ax, 0x23",
            "mov ds, ax",
            "mov es, ax",
            "xor ax, ax",
            "mov fs, ax",
            "mov gs, ax",
            "xor rax, rax",
            "xor rbx, rbx",
            "xor rcx, rcx",
            "xor rdx, rdx",
            "xor rsi, rsi",
            "xor rdi, rdi",
            "xor r8, r8",
            "xor r9, r9",
            "xor r10, r10",
            "xor r11, r11",
            "xor r12, r12",
            "xor r13, r13",
            "xor r14, r14",
            "xor r15, r15",
            "xor rbp, rbp",
            "iretq",
            in("rdi") frame_ptr,
            options(noreturn)
        );
    }
}

