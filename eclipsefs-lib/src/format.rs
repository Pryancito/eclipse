//! Definición del formato EclipseFS

use crate::error::EclipseFSResult;
use crate::EclipseFSError;

/// Constantes del formato
pub const ECLIPSEFS_MAGIC: &[u8] = b"ECLIPSEFS";
pub const ECLIPSEFS_VERSION: u32 = 0x00020000; // v0.2.0
pub const ECLIPSEFS_VERSION_MAJOR: u16 = 2;
pub const ECLIPSEFS_VERSION_MINOR: u16 = 0;
pub const BLOCK_SIZE: usize = 4096; // 4KB

/// Header del sistema de archivos EclipseFS
#[derive(Debug, Clone)]
pub struct EclipseFSHeader {
    pub magic: [u8; 9],
    pub version: u32,
    pub inode_table_offset: u64,
    pub inode_table_size: u64,
    pub total_inodes: u32,
}

impl EclipseFSHeader {
    /// Crear un nuevo header
    pub fn new(total_inodes: u32) -> Self {
        Self {
            magic: {
                let mut m = [0u8; 9];
                m.copy_from_slice(ECLIPSEFS_MAGIC);
                m
            },
            version: ((ECLIPSEFS_VERSION_MAJOR as u32) << 16) | (ECLIPSEFS_VERSION_MINOR as u32),
            inode_table_offset: BLOCK_SIZE as u64, // Después del header
            inode_table_size:
                (total_inodes as u64) * (constants::INODE_TABLE_ENTRY_SIZE as u64),
            total_inodes,
        }
    }

    /// Construir un header a partir de una vista en memoria
    pub fn from_bytes(buffer: &[u8]) -> EclipseFSResult<Self> {
        const RAW_SIZE: usize = 9 + 4 + 8 + 8 + 4;

        if buffer.len() < RAW_SIZE {
            return Err(EclipseFSError::InvalidFormat);
        }

        let mut magic = [0u8; 9];
        magic.copy_from_slice(&buffer[0..9]);

        let version = u32::from_le_bytes([
            buffer[9],
            buffer[10],
            buffer[11],
            buffer[12],
        ]);

        let inode_table_offset = u64::from_le_bytes([
            buffer[13],
            buffer[14],
            buffer[15],
            buffer[16],
            buffer[17],
            buffer[18],
            buffer[19],
            buffer[20],
        ]);

        let inode_table_size = u64::from_le_bytes([
            buffer[21],
            buffer[22],
            buffer[23],
            buffer[24],
            buffer[25],
            buffer[26],
            buffer[27],
            buffer[28],
        ]);

        let total_inodes = u32::from_le_bytes([
            buffer[29],
            buffer[30],
            buffer[31],
            buffer[32],
        ]);

        let header = Self {
            magic,
            version,
            inode_table_offset,
            inode_table_size,
            total_inodes,
        };

        header.validate()?;
        Ok(header)
    }

    /// Validar el header
    pub fn validate(&self) -> EclipseFSResult<()> {
        if &self.magic != ECLIPSEFS_MAGIC {
            return Err(crate::EclipseFSError::InvalidFormat);
        }

        let version_major = (self.version >> 16) as u16;
        let version_minor = (self.version & 0xFFFF) as u16;

        if version_major != ECLIPSEFS_VERSION_MAJOR || version_minor != ECLIPSEFS_VERSION_MINOR {
            return Err(crate::EclipseFSError::UnsupportedVersion);
        }

        if self.total_inodes == 0 {
            return Err(crate::EclipseFSError::InvalidFormat);
        }

        Ok(())
    }

    /// Obtener el tamaño del header
    pub fn size() -> usize {
        BLOCK_SIZE // Header alineado a 4KB
    }
}

/// Entrada en la tabla de inodos
#[derive(Debug, Clone)]
pub struct InodeTableEntry {
    pub inode: u64,
    pub offset: u64,
}

impl InodeTableEntry {
    /// Crear una nueva entrada
    pub fn new(inode: u64, offset: u64) -> Self {
        Self { inode, offset }
    }
}

/// Tags TLV para el formato de nodos
pub mod tlv_tags {
    pub const NODE_TYPE: u16 = 0x0001;
    pub const MODE: u16 = 0x0002;
    pub const UID: u16 = 0x0003;
    pub const GID: u16 = 0x0004;
    pub const SIZE: u16 = 0x0005;
    pub const ATIME: u16 = 0x0006;
    pub const MTIME: u16 = 0x0007;
    pub const CTIME: u16 = 0x0008;
    pub const NLINK: u16 = 0x0009;
    pub const CONTENT: u16 = 0x000A;
    pub const DIRECTORY_ENTRIES: u16 = 0x000B;
}

/// Constantes del formato
pub mod constants {

    /// Tamaño de una entrada de tabla de inodos
    pub const INODE_TABLE_ENTRY_SIZE: usize = 8;
    /// Tamaño del encabezado de un registro de nodo (inode + tamaño del registro)
    pub const NODE_RECORD_HEADER_SIZE: usize = 8;

    /// Número de inodo raíz
    pub const ROOT_INODE: u32 = 1;

    /// Máximo número de inodos
    pub const MAX_INODES: u32 = 0xFFFFFFFF;

    /// Máximo tamaño de nombre de archivo
    pub const MAX_FILENAME_LEN: usize = 255;

    /// Máximo tamaño de archivo
    pub const MAX_FILE_SIZE: u64 = 0xFFFFFFFFFFFFFFFF;
}
