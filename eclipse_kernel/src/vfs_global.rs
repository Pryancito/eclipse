//! Global VFS Instance and Integration
//!
//! This module provides a global virtual filesystem instance that can be
//! accessed throughout the kernel.

use crate::virtual_fs::{VirtualFileSystem, FsResult, FilePermissions};
use spin::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    /// Global VFS instance
    pub static ref GLOBAL_VFS: Mutex<VirtualFileSystem> = {
        let mut vfs = VirtualFileSystem::new(10 * 1024 * 1024); // 10MB RAM FS
        
        // Create standard directories
        // Create standard directories
        vfs.create_directory("/proc", FilePermissions::default()).ok();
        vfs.create_directory("/dev", FilePermissions::default()).ok();
        vfs.create_directory("/sys", FilePermissions::default()).ok();
        vfs.create_directory("/sbin", FilePermissions::default()).ok();
        vfs.create_directory("/bin", FilePermissions::default()).ok();
        vfs.create_directory("/usr", FilePermissions::default()).ok();
        vfs.create_directory("/usr/bin", FilePermissions::default()).ok();
        vfs.create_directory("/etc", FilePermissions::default()).ok();
        vfs.create_directory("/etc/eclipse", FilePermissions::default()).ok();
        vfs.create_directory("/etc/eclipse/systemd", FilePermissions::default()).ok();
        vfs.create_directory("/etc/eclipse/systemd/system", FilePermissions::default()).ok();
        vfs.create_directory("/var", FilePermissions::default()).ok();
        vfs.create_directory("/var/log", FilePermissions::default()).ok();
        vfs.create_directory("/tmp", FilePermissions::default()).ok();
        vfs.create_directory("/home", FilePermissions::default()).ok();
        
        Mutex::new(vfs)
    };
}

/// Get reference to global VFS
pub fn get_vfs() -> &'static Mutex<VirtualFileSystem> {
    &GLOBAL_VFS
}

/// Initialize VFS with default structure
pub fn init_vfs() -> FsResult<()> {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    // Create default files
    // Create default files
    vfs_lock.create_file("/etc/hostname", FilePermissions::default())?;
    vfs_lock.write_file("/etc/hostname", b"eclipse-os\n")?;

    vfs_lock.create_file("/etc/os-release", FilePermissions::default())?;
    vfs_lock.write_file("/etc/os-release", b"NAME=\"Eclipse OS\"\nVERSION=\"0.1.0\"\n")?;
    
    Ok(())
}

/// Create minimal systemd stub in VFS for testing - only if not found on disk
pub fn prepare_systemd_binary() -> FsResult<()> {
    use crate::filesystem::vfs::get_vfs as get_vfs_system;
    
    // Primero verificar si systemd ya existe en el filesystem montado
    crate::debug::serial_write_str("PREPARE_SYSTEMD: Verificando si systemd existe en filesystem montado...\n");
    
    if let Some(vfs_system) = get_vfs_system() {
        let vfs_lock = vfs_system.lock();
        
        // Verificar si hay un filesystem montado en /
        if let Some(root_fs) = vfs_lock.get_mount("/") {
            let fs_lock = root_fs.lock();
            
            // Intentar leer systemd desde el filesystem montado
            let paths = ["/sbin/eclipse-systemd", "/usr/sbin/eclipse-systemd", "/sbin/init"];
            
            for path in &paths {
                if let Ok(data) = fs_lock.read_file_path(path) {
                    crate::debug::serial_write_str(&alloc::format!(
                        "PREPARE_SYSTEMD: ✓ Encontrado {} en filesystem montado ({} bytes), no se creará stub\n",
                        path, data.len()
                    ));
                    return Ok(());
                }
            }
        }
    }
    
    // Si no se encontró en el filesystem montado, crear stub en VFS en memoria
    crate::debug::serial_write_str("PREPARE_SYSTEMD: No se encontró systemd en filesystem montado, creando stub en VFS...\n");
    
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    // Create a minimal ELF header stub
    // In a real system, this would be loaded from disk
    let minimal_elf = create_minimal_elf_stub();
    
    vfs_lock.create_file("/sbin/eclipse-systemd", FilePermissions::default())?;
    vfs_lock.write_file("/sbin/eclipse-systemd", &minimal_elf)?;

    vfs_lock.create_file("/sbin/init", FilePermissions::default())?;
    vfs_lock.write_file("/sbin/init", &minimal_elf)?;
    
    crate::debug::serial_write_str("PREPARE_SYSTEMD: Stub creado en VFS en memoria\n");
    Ok(())
}

/// Create minimal ELF stub for testing
fn create_minimal_elf_stub() -> alloc::vec::Vec<u8> {
    use alloc::vec::Vec;
    
    // ELF64 header (minimal valid ELF)
    let mut elf = Vec::new();
    
    // ELF magic
    elf.extend_from_slice(&[0x7F, b'E', b'L', b'F']);
    
    // 64-bit, little-endian, version 1
    elf.push(2); // ELFCLASS64
    elf.push(1); // ELFDATA2LSB  
    elf.push(1); // EV_CURRENT
    elf.push(0); // ELFOSABI_SYSV
    elf.extend_from_slice(&[0; 8]); // Padding
    
    // e_type = ET_EXEC (2), e_machine = EM_X86_64 (0x3E)
    elf.extend_from_slice(&[2, 0, 0x3E, 0]);
    
    // e_version = 1
    elf.extend_from_slice(&[1, 0, 0, 0]);
    
    // e_entry = 0x400000 (entry point)
    elf.extend_from_slice(&[0, 0, 0x40, 0, 0, 0, 0, 0]);
    
    // e_phoff = 64 (program header offset)
    elf.extend_from_slice(&[64, 0, 0, 0, 0, 0, 0, 0]);
    
    // e_shoff = 0 (no section headers for now)
    elf.extend_from_slice(&[0; 8]);
    
    // e_flags = 0
    elf.extend_from_slice(&[0; 4]);
    
    // e_ehsize = 64
    elf.extend_from_slice(&[64, 0]);
    
    // e_phentsize = 56
    elf.extend_from_slice(&[56, 0]);
    
    // e_phnum = 1
    elf.extend_from_slice(&[1, 0]);
    
    // e_shentsize = 0
    elf.extend_from_slice(&[0, 0]);
    
    // e_shnum = 0
    elf.extend_from_slice(&[0, 0]);
    
    // e_shstrndx = 0
    elf.extend_from_slice(&[0, 0]);
    
    // Program header
    // p_type = PT_LOAD (1)
    elf.extend_from_slice(&[1, 0, 0, 0]);
    
    // p_flags = PF_R | PF_X (5) - Readable and Executable
    elf.extend_from_slice(&[5, 0, 0, 0]);
    
    // p_offset = 0x1000 (4096 - skip headers, actual code starts after headers)
    elf.extend_from_slice(&[0, 0x10, 0, 0, 0, 0, 0, 0]);
    
    // p_vaddr = 0x400000
    elf.extend_from_slice(&[0, 0, 0x40, 0, 0, 0, 0, 0]);
    
    // p_paddr = 0x400000
    elf.extend_from_slice(&[0, 0, 0x40, 0, 0, 0, 0, 0]);
    
    // p_filesz = 4096 (size in file)
    elf.extend_from_slice(&[0, 0x10, 0, 0, 0, 0, 0, 0]);
    
    // p_memsz = 4096 (size in memory)
    elf.extend_from_slice(&[0, 0x10, 0, 0, 0, 0, 0, 0]);
    
    // p_align = 4096
    elf.extend_from_slice(&[0, 0x10, 0, 0, 0, 0, 0, 0]);
    
    // Pad header to 4096 bytes
    while elf.len() < 4096 {
        elf.push(0);
    }
    
    // Now add actual executable code at offset 4096 (which maps to 0x400000)
    // Simple userland program that makes a syscall and loops
    // This represents eclipse-systemd stub
    
    // Simple infinite loop with HLT to be CPU-friendly:
    // .loop:
    //   hlt       ; Halt until interrupt
    //   jmp .loop ; Jump back to loop
    elf.extend_from_slice(&[
        0xF4,       // hlt
        0xEB, 0xFD, // jmp -3 (loop back to hlt)
    ]);
    
    // Pad the code section to make it 4KB total
    while elf.len() < 8192 {
        elf.push(0x90); // NOP padding
    }
    
    elf
}

/// Create default service files for systemd
pub fn create_default_service_files() -> FsResult<()> {
    let vfs = get_vfs();
    let mut vfs_lock = vfs.lock();
    
    // Create basic.target
    let basic_target = b"\
[Unit]
Description=Basic System
Documentation=man:systemd.special(7)
";
    vfs_lock.create_file("/etc/eclipse/systemd/system/basic.target", FilePermissions::default())?;
    vfs_lock.write_file("/etc/eclipse/systemd/system/basic.target", basic_target)?;
    
    // Create multi-user.target
    let multi_user = b"\
[Unit]
Description=Multi-User System
Documentation=man:systemd.special(7)
Requires=basic.target
After=basic.target
";
    vfs_lock.create_file("/etc/eclipse/systemd/system/multi-user.target", FilePermissions::default())?;
    vfs_lock.write_file("/etc/eclipse/systemd/system/multi-user.target", multi_user)?;
    
    Ok(())
}
