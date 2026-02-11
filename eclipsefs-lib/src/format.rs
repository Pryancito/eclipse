//! Definición del formato EclipseFS

use crate::error::EclipseFSResult;
use crate::EclipseFSError;

/// Constantes del formato
pub const ECLIPSEFS_MAGIC: &[u8] = b"ECLIPSEFS";
pub const ECLIPSEFS_VERSION: u32 = 0x00020000; // v0.2.0
pub const ECLIPSEFS_VERSION_MAJOR: u16 = 2;
pub const ECLIPSEFS_VERSION_MINOR: u16 = 0;
pub const BLOCK_SIZE: usize = 4096; // 4KB

/// Header del sistema de archivos EclipseFS (inspirado en RedoxFS)
#[derive(Debug, Clone)]
pub struct EclipseFSHeader {
    pub magic: [u8; 9],
    pub version: u32,
    pub inode_table_offset: u64,
    pub inode_table_size: u64,
    pub total_inodes: u32,
    // Mejoras inspiradas en RedoxFS
    pub header_checksum: u32,      // CRC32 del header para integridad
    pub metadata_checksum: u32,    // CRC32 de metadatos críticos
    pub data_checksum: u32,        // CRC32 de datos del sistema
    pub creation_time: u64,        // Timestamp de creación
    pub last_check: u64,           // Última verificación de integridad
    pub flags: u32,                // Flags del sistema (encriptación, compresión, etc.)
}

impl EclipseFSHeader {
    /// Crear un nuevo header (inspirado en RedoxFS)
    pub fn new(total_inodes: u32) -> Self {
        let now = Self::current_timestamp();
        let mut header = Self {
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
            // Inicializar checksums en 0 (se calcularán después)
            header_checksum: 0,
            metadata_checksum: 0,
            data_checksum: 0,
            creation_time: now,
            last_check: now,
            flags: 0, // Sin flags especiales por defecto
        };
        
        // Calcular checksums
        header.update_checksums();
        header
    }

    /// Construir un header a partir de una vista en memoria (actualizado para RedoxFS)
    pub fn from_bytes(buffer: &[u8]) -> EclipseFSResult<Self> {
        const RAW_SIZE: usize = 9 + 4 + 8 + 8 + 4 + 4 + 4 + 4 + 8 + 8 + 4; // Nuevos campos

        if buffer.len() < RAW_SIZE {
            return Err(EclipseFSError::InvalidFormat);
        }

        let mut magic = [0u8; 9];
        magic.copy_from_slice(&buffer[0..9]);

        let version = u32::from_le_bytes([
            buffer[9], buffer[10], buffer[11], buffer[12],
        ]);

        let inode_table_offset = u64::from_le_bytes([
            buffer[13], buffer[14], buffer[15], buffer[16],
            buffer[17], buffer[18], buffer[19], buffer[20],
        ]);

        let inode_table_size = u64::from_le_bytes([
            buffer[21], buffer[22], buffer[23], buffer[24],
            buffer[25], buffer[26], buffer[27], buffer[28],
        ]);

        let total_inodes = u32::from_le_bytes([
            buffer[29], buffer[30], buffer[31], buffer[32],
        ]);

        // Nuevos campos inspirados en RedoxFS
        let header_checksum = u32::from_le_bytes([
            buffer[33], buffer[34], buffer[35], buffer[36],
        ]);

        let metadata_checksum = u32::from_le_bytes([
            buffer[37], buffer[38], buffer[39], buffer[40],
        ]);

        let data_checksum = u32::from_le_bytes([
            buffer[41], buffer[42], buffer[43], buffer[44],
        ]);

        let creation_time = u64::from_le_bytes([
            buffer[45], buffer[46], buffer[47], buffer[48],
            buffer[49], buffer[50], buffer[51], buffer[52],
        ]);

        let last_check = u64::from_le_bytes([
            buffer[53], buffer[54], buffer[55], buffer[56],
            buffer[57], buffer[58], buffer[59], buffer[60],
        ]);

        let flags = u32::from_le_bytes([
            buffer[61], buffer[62], buffer[63], buffer[64],
        ]);

        let header = Self {
            magic,
            version,
            inode_table_offset,
            inode_table_size,
            total_inodes,
            header_checksum,
            metadata_checksum,
            data_checksum,
            creation_time,
            last_check,
            flags,
        };

        header.validate()?;
        Ok(header)
    }

    /// Validar el header
    pub fn validate(&self) -> EclipseFSResult<()> {
        if self.magic != ECLIPSEFS_MAGIC {
            return Err(crate::EclipseFSError::InvalidFormat);
        }

        let version_major = (self.version >> 16) as u16;
        let version_minor = (self.version & 0xFFFF) as u16;

        // Solo soporte para versión 2.0
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
    
    /// Obtener timestamp actual (simulado para no_std)
    fn current_timestamp() -> u64 {
        // En un entorno real, esto vendría del kernel o RTC
        // Por ahora, simulamos con un valor fijo
        1640995200 // 2022-01-01 00:00:00 UTC
    }
    
    /// Calcular checksum CRC32 simple (inspirado en RedoxFS)
    fn calculate_crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFFFFFF;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xEDB88320;
                } else {
                    crc >>= 1;
                }
            }
        }
        crc ^ 0xFFFFFFFF
    }
    
    /// Actualizar todos los checksums del header
    pub fn update_checksums(&mut self) {
        // Calcular checksum del header (excluyendo los campos de checksum)
        let header_data = self.serialize_for_checksum();
        self.header_checksum = Self::calculate_crc32(&header_data);
        
        // Los otros checksums se calcularán cuando se tengan los datos
        self.metadata_checksum = 0; // Se calculará con la tabla de inodos
        self.data_checksum = 0;     // Se calculará con los datos del sistema
    }
    
    /// Serializar header para cálculo de checksum (excluyendo checksums)
    fn serialize_for_checksum(&self) -> heapless::Vec<u8, 128> {
        let mut data = heapless::Vec::new();
        let _ = data.extend_from_slice(&self.magic);
        let _ = data.extend_from_slice(&self.version.to_le_bytes());
        let _ = data.extend_from_slice(&self.inode_table_offset.to_le_bytes());
        let _ = data.extend_from_slice(&self.inode_table_size.to_le_bytes());
        let _ = data.extend_from_slice(&self.total_inodes.to_le_bytes());
        let _ = data.extend_from_slice(&self.creation_time.to_le_bytes());
        let _ = data.extend_from_slice(&self.last_check.to_le_bytes());
        let _ = data.extend_from_slice(&self.flags.to_le_bytes());
        data
    }
    
    /// Verificar integridad del header
    pub fn verify_integrity(&self) -> EclipseFSResult<()> {
        let expected_checksum = Self::calculate_crc32(&self.serialize_for_checksum());
        if self.header_checksum != expected_checksum {
            return Err(EclipseFSError::InvalidFormat);
        }
        Ok(())
    }
    
    /// Marcar verificación de integridad como completada
    pub fn mark_check_completed(&mut self) {
        self.last_check = Self::current_timestamp();
        self.update_checksums();
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
