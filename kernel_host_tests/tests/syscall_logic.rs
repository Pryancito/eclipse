//! Pruebas de lógica de syscalls (hardening y buffering) — `kernel_host_tests`.

use kernel_host_tests::policy::*;

#[test]
fn test_rw_buffering_logic() {
    // Verificar que una lectura de 1MB se divide en trozos de 128KB
    let total_len = 1024 * 1024;
    let chunks = calculate_rw_chunks(total_len, SYS_RW_BUFFER_SIZE);
    
    assert_eq!(chunks.len(), 8);
    for chunk in chunks {
        assert_eq!(chunk, 128 * 1024);
    }
    
    // Verificar resto pequeño
    let total_len = 128 * 1024 + 100;
    let chunks = calculate_rw_chunks(total_len, SYS_RW_BUFFER_SIZE);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0], 128 * 1024);
    assert_eq!(chunks[1], 100);
}

#[test]
fn test_mmap_fixed_validation() {
    // Dirección válida (64MB, alineada)
    assert!(is_mmap_fixed_address_valid(0x6000_0000, 4096));
    
    // Dirección nula (inválida)
    assert!(!is_mmap_fixed_address_valid(0, 4096));
    
    // Dirección no alineada (inválida)
    assert!(!is_mmap_fixed_address_valid(0x6000_0001, 4096));
    
    // Dirección en espacio de kernel (inválida)
    // 0xFFFF800000000000 es el inicio del kernel
    assert!(!is_mmap_fixed_address_valid(0xFFFF_8000_0000_0000, 4096));
    
    // Rango que cruza al kernel (inválido)
    let near_boundary = USER_SPACE_BOUNDARY - 4096;
    assert!(!is_mmap_fixed_address_valid(near_boundary, 8192));
}

#[test]
fn test_elf_size_limit() {
    // 32MB es el nuevo límite
    assert!(elf_size_allowed_for_kernel_heap_copy(32 * 1024 * 1024));
    
    // 32MB + 1 byte (inválido)
    assert!(!elf_size_allowed_for_kernel_heap_copy(32 * 1024 * 1024 + 1));
    
    // 0 bytes (inválido)
    assert!(!elf_size_allowed_for_kernel_heap_copy(0));
}

#[test]
fn test_utsname_abi_size() {
    // 6 campos de 65 bytes cada uno = 390 bytes
    assert_eq!(std::mem::size_of::<Utsname>(), 390);
}

#[test]
fn test_sysinfo_abi_size() {
    // Verificar que el tamaño coincide con lo esperado por el kernel (aprox 112 bytes)
    assert_eq!(std::mem::size_of::<SysInfo>(), 112);
}
