//! Sistema de archivos para Eclipse OS
//! 
//! Este módulo implementa un sistema de archivos básico con soporte para:
//! - Estructuras de directorios jerárquicos
//! - Operaciones de archivos (crear, leer, escribir, eliminar)
//! - Gestión de permisos básicos
//! - Cache de archivos
//! - Sistema de bloques para almacenamiento

pub mod vfs;
pub mod inode;
pub mod superblock;
pub mod directory;
pub mod file;
pub mod cache;
pub mod block;
pub mod utils;
pub mod fat32;
pub mod eclipsefs;

// Re-exportar componentes principales
pub use vfs::VfsResult;
pub use superblock::FileSystemType;
// Constantes del sistema de archivos
pub const MAX_FILENAME_LEN: usize = 255;
pub const MAX_PATH_LEN: usize = 4096;
pub const MAX_DIRECTORY_ENTRIES: usize = 1024;
pub const BLOCK_SIZE: usize = 4096;
pub const MAX_FILE_SIZE: u64 = 0xFFFFFFFF; // 4GB
pub const ROOT_INODE: u32 = 1;

// Tipos de archivo soportados
pub const INODE_TYPE_FILE: u16 = 0x8000;
pub const INODE_TYPE_DIR: u16 = 0x4000;
pub const INODE_TYPE_SYMLINK: u16 = 0xA000;
pub const INODE_TYPE_CHARDEV: u16 = 0x2000;
pub const INODE_TYPE_BLOCKDEV: u16 = 0x6000;
pub const INODE_TYPE_FIFO: u16 = 0x1000;
pub const INODE_TYPE_SOCKET: u16 = 0xC000;

// Permisos de archivo
pub const PERM_READ: u16 = 0x4;
pub const PERM_WRITE: u16 = 0x2;
pub const PERM_EXECUTE: u16 = 0x1;

// Modos de apertura de archivo
pub const O_RDONLY: u32 = 0x0000;
pub const O_WRONLY: u32 = 0x0001;
pub const O_RDWR: u32 = 0x0002;
pub const O_CREAT: u32 = 0x0040;
pub const O_TRUNC: u32 = 0x0200;
pub const O_APPEND: u32 = 0x0400;

// Información del sistema de archivos
#[derive(Debug, Clone, Copy)]
pub struct FileSystemInfo {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub used_blocks: u64,
    pub total_inodes: u32,
    pub free_inodes: u32,
    pub used_inodes: u32,
    pub block_size: u32,
    pub max_file_size: u64,
    pub filesystem_type: FileSystemType,
}

impl FileSystemInfo {
    pub fn new() -> Self {
        Self {
            total_blocks: 0,
            free_blocks: 0,
            used_blocks: 0,
            total_inodes: 0,
            free_inodes: 0,
            used_inodes: 0,
            block_size: BLOCK_SIZE as u32,
            max_file_size: MAX_FILE_SIZE,
            filesystem_type: FileSystemType::EclipseFS,
        }
    }
}

// Inicialización del sistema de archivos
pub fn init_filesystem() -> VfsResult<()> {
    // Inicializar VFS
    vfs::init_vfs()?;
    
    // Inicializar cache
    cache::init_file_cache()?;
    
    // Inicializar dispositivo de bloques
    block::init_block_device()?;
    
    // Inicializar EclipseFS (RAM, RW)
    eclipsefs::init()?;
    
    // Inicializar FAT32 (para /boot)
    fat32::init_fat32()?;
    
    // Intentar cargar EclipseFS desde /boot/eclipsefs.img
    // Si no existe, crear estructura básica
    if let Err(_) = load_eclipsefs_from_boot() {
        // Crear directorios básicos si no se pudo cargar
        let _ = eclipsefs::create_dir("/");
        let _ = eclipsefs::create_dir("/run");
        let _ = eclipsefs::create_dir("/var");
        let _ = eclipsefs::create_dir("/var/log");
        let _ = eclipsefs::create_dir("/tmp");
        let _ = eclipsefs::create_dir("/proc");
        let _ = eclipsefs::create_dir("/sys");
        let _ = eclipsefs::create_dir("/boot");
    }
    
    // Montar FAT32 en /boot
    mount_fat32_boot()?;
    
    Ok(())
}

/// Cargar EclipseFS desde /boot/eclipsefs.img
fn load_eclipsefs_from_boot() -> VfsResult<()> {
    // Por ahora, simplemente crear la estructura básica
    // En una implementación real, aquí leeríamos desde FAT32
    let _ = eclipsefs::create_dir("/");
    let _ = eclipsefs::create_dir("/run");
    let _ = eclipsefs::create_dir("/var");
    let _ = eclipsefs::create_dir("/var/log");
    let _ = eclipsefs::create_dir("/tmp");
    let _ = eclipsefs::create_dir("/proc");
    let _ = eclipsefs::create_dir("/sys");
    
    // Crear algunos archivos de sistema básicos
    let _ = eclipsefs::create_file("/proc/version");
    let _ = eclipsefs::write("/proc/version", 0, b"Eclipse OS Kernel v1.0\n");
    
    let _ = eclipsefs::create_file("/proc/cpuinfo");
    let _ = eclipsefs::write("/proc/cpuinfo", 0, b"processor\t: 0\nvendor_id\t: Eclipse\ncpu family\t: 6\nmodel\t\t: 0\nmodel name\t: Eclipse CPU\n");
    
    Ok(())
}

// Obtener información del sistema de archivos
pub fn get_filesystem_info() -> FileSystemInfo {
    FileSystemInfo::new()
}

/// Montar FAT32 en /boot
fn mount_fat32_boot() -> VfsResult<()> {
    // Crear instancia VFS temporal para montar FAT32
    let mut vfs = vfs::Vfs::new();
    vfs.mount_fat32_boot()
}

/// Inicializar sistema de archivos (compatible con main.rs)
pub fn init() {
    // Inicializar sistema de archivos
    // En una implementación real, esto configuraría el sistema global
    let _ = init_filesystem();
}