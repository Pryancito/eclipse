//! Gestión de archivos para Eclipse OS

// Modos de archivo
#[derive(Debug, Clone, Copy)]
pub enum FileMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
    Append,
}

impl FileMode {
    pub fn from_u32(mode: u32) -> Self {
        match mode & 0x3 {
            0 => FileMode::ReadOnly,
            1 => FileMode::WriteOnly,
            2 => FileMode::ReadWrite,
            _ => FileMode::ReadWrite,
        }
    }
}

// Handle de archivo
#[derive(Debug, Clone, Copy)]
pub struct FileHandle {
    pub inode: u32,
    pub position: u64,
    pub mode: FileMode,
    pub flags: u32,
}

impl FileHandle {
    pub fn new(inode: u32, mode: FileMode, flags: u32) -> Self {
        Self {
            inode,
            position: 0,
            mode,
            flags,
        }
    }
}

// Archivo
#[derive(Debug, Clone)]
pub struct File {
    pub inode: u32,
    pub size: u64,
    pub data: [u8; 4096], // Buffer simplificado
}

impl File {
    pub fn new(inode: u32) -> Self {
        Self {
            inode,
            size: 0,
            data: [0; 4096],
        }
    }
}
