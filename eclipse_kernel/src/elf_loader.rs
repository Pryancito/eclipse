//! ELF Loader para cargar binarios en userspace

use crate::process::{create_process, ProcessId};
use crate::memory;
use crate::serial;

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
