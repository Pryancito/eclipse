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
    serial::serial_print("ELF: Entry point: 0x");
    serial::serial_print_hex(header.e_entry);
    serial::serial_print("\n");
    
    // TODO: Cargar segmentos PT_LOAD en memoria
    // Por ahora, simplemente crear proceso con entry point
    
    let stack_base = 0x700000; // 7MB
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
    
    serial::serial_print("ELF: Valid exec binary, entry: 0x");
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

/// Jump to entry point (for exec())
/// This function never returns
pub unsafe fn jump_to_entry(entry_point: u64) -> ! {
    serial::serial_print("ELF: Jumping to entry point: 0x");
    serial::serial_print_hex(entry_point);
    serial::serial_print("\n");
    
    // Set up a clean stack for the new process
    // Use a fixed stack location for userspace
    let stack_top: u64 = 0x800000; // 8MB - 64KB = stack top
    
    // Clear all general-purpose registers and jump to entry point
    // This simulates a clean process start
    asm!(
        // Clear all general-purpose registers
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
        
        // Set up stack pointer
        "mov rsp, {stack}",
        "mov rbp, rsp",
        
        // Jump to entry point
        "jmp {entry}",
        
        stack = in(reg) stack_top,
        entry = in(reg) entry_point,
        options(noreturn)
    );
}
