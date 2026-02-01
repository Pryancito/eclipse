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
    
    // Iterate over program headers and load segments
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let ph_size = header.e_phentsize as usize;
    
    if elf_data.len() < ph_offset + (ph_count * ph_size) {
        serial::serial_print("ELF: Program headers out of bounds\n");
        return None;
    }
    
    for i in 0..ph_count {
        let offset = ph_offset + (i * ph_size);
        let ph = unsafe { &*(elf_data[offset..].as_ptr() as *const Elf64ProgramHeader) };
        
        if ph.p_type == PT_LOAD {
            serial::serial_print("ELF: Loading segment at ");
            serial::serial_print_hex(ph.p_vaddr);
            serial::serial_print(" size: ");
            serial::serial_print_hex(ph.p_memsz);
            serial::serial_print("\n");
            
            // Check if we can copy (filesz > 0)
            if ph.p_filesz > 0 {
                let file_offset = ph.p_offset as usize;
                
                if file_offset + (ph.p_filesz as usize) > elf_data.len() {
                    serial::serial_print("ELF: Segment file content out of bounds\n");
                    return None;
                }
                
                // Copy data to memory
                unsafe {
                    let src = elf_data.as_ptr().add(file_offset);
                    let dst = ph.p_vaddr as *mut u8;
                    core::ptr::copy_nonoverlapping(src, dst, ph.p_filesz as usize);
                }
            }
            
            // Zero out BSS (memsz > filesz)
            if ph.p_memsz > ph.p_filesz {
                let bss_size = (ph.p_memsz - ph.p_filesz) as usize;
                let bss_start = ph.p_vaddr + ph.p_filesz;
                
                serial::serial_print("ELF: Zeroing BSS at ");
                serial::serial_print_hex(bss_start);
                serial::serial_print(" size: ");
                serial::serial_print_hex(bss_size as u64);
                serial::serial_print("\n");
                
                unsafe {
                    let dst = bss_start as *mut u8;
                    core::ptr::write_bytes(dst, 0, bss_size);
                }
            }
        }
    }
    
    // Default user stack at 96MB
    let stack_base = 0x6000000; // 512MB
    let stack_size = 0x10000;  // 64KB
    
    create_process(header.e_entry, stack_base, stack_size)
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
    
    // TODO: In a real implementation, we would:
    // 1. Parse and load PT_LOAD segments into memory
    // 2. Set up proper memory mappings
    // 3. Zero BSS sections
    // 4. Set up heap
    //
    // For now, we just validate the ELF and return the entry point
    // The binary is already in memory (passed as a slice)
    
    Some(header.e_entry)
}

/// Jump to entry point in userspace (Ring 3)
/// This function never returns
/// 
/// # Safety
/// This function constructs a stack frame and executes `iretq` to switch privilege levels.
/// It MUST be called with a valid userspace entry point and stack top.
pub unsafe extern "C" fn jump_to_userspace(entry_point: u64, stack_top: u64) -> ! {
    // FORCE PRINT to ensure we reached this point
    serial::serial_print("ELF: JUMPING TO USERSPACE NOW!\n");
    serial::serial_print("  Entry: ");
    serial::serial_print_hex(entry_point);
    serial::serial_print("\n  Stack: ");
    serial::serial_print_hex(stack_top);
    serial::serial_print("\n");
    
    // Selectors from boot.rs:
    // USER_CODE_SELECTOR: u16 = 0x18 | 3;
    // USER_DATA_SELECTOR: u16 = 0x20 | 3;
    let user_cs: u64 = 0x1b; // 0x18 | 3
    let user_ds: u64 = 0x23; // 0x20 | 3
    let rflags: u64 = 0x202; // Interrupciones habilitadas

    asm!(
        "mov ds, ax",
        "mov es, ax",
        "mov fs, ax",
        "mov gs, ax",
        "push {ss}",     // SS
        "push {rsp}",    // RSP
        "push {rflags}", // RFLAGS
        "push {cs}",     // CS
        "push {rip}",    // RIP
        "iretq",
        ss = in(reg) user_ds,
        rsp = in(reg) stack_top,
        rflags = in(reg) rflags,
        cs = in(reg) user_cs,
        rip = in(reg) entry_point,
        in("ax") user_ds,
        options(noreturn)
    );
}
