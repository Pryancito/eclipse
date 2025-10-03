//! Tipos adicionales para EclipseFS

#[cfg(feature = "std")]
use std::{string::String, vec::Vec};

#[cfg(not(feature = "std"))]
use heapless::{String, Vec};

/// Tipo de cifrado
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EncryptionType {
    None,
    AES256,
    ChaCha20,
}

/// Información de cifrado
#[derive(Debug, Clone)]
pub struct EncryptionInfo {
    pub encryption_type: EncryptionType,
    pub key_id: u32,
    #[cfg(feature = "std")]
    pub iv: Vec<u8>,
    #[cfg(not(feature = "std"))]
    pub iv: Vec<u8, 32>,
}

/// Tipo de compresión
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompressionType {
    None,
    LZ4,
    Zstd,
    Gzip,
}

/// Información de compresión
#[derive(Debug, Clone)]
pub struct CompressionInfo {
    pub compression_type: CompressionType,
    pub original_size: u64,
    pub compressed_size: u64,
}

/// Snapshot del sistema de archivos
#[derive(Debug, Clone)]
pub struct Snapshot {
    #[cfg(feature = "std")]
    pub id: String,
    #[cfg(not(feature = "std"))]
    pub id: String<64>,
    pub timestamp: u64,
    #[cfg(feature = "std")]
    pub description: String,
    #[cfg(not(feature = "std"))]
    pub description: String<128>,
}

/// Tipo de entrada ACL
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AclEntryType {
    User,
    Group,
    Other,
}

/// Entrada ACL
#[derive(Debug, Clone)]
pub struct AclEntry {
    pub entry_type: AclEntryType,
    pub permissions: u32,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// Lista de control de acceso
#[derive(Debug, Clone)]
pub struct Acl {
    #[cfg(feature = "std")]
    pub entries: Vec<AclEntry>,
    #[cfg(not(feature = "std"))]
    pub entries: Vec<AclEntry, 16>,
}

/// Configuración de cifrado transparente
#[derive(Debug, Clone)]
pub struct TransparentEncryptionConfig {
    pub enabled: bool,
    pub key_id: u32,
    pub algorithm: EncryptionType,
}

impl TransparentEncryptionConfig {
    pub fn new() -> Self {
        Self {
            enabled: false,
            key_id: 0,
            algorithm: EncryptionType::None,
        }
    }
}

/// Resultado de fsck
#[derive(Debug, Clone)]
pub struct FsckResult {
    pub errors_found: u32,
    pub errors_fixed: u32,
    pub warnings: u32,
}

/// Resultado de df
#[derive(Debug, Clone)]
pub struct DfResult {
    pub total_blocks: u64,
    pub used_blocks: u64,
    pub free_blocks: u64,
}

/// Resultado de find
#[derive(Debug, Clone)]
pub struct FindResult {
    #[cfg(feature = "std")]
    pub matches: Vec<String>,
    #[cfg(not(feature = "std"))]
    pub matches: Vec<String<256>, 64>,
    pub total_matches: u32,
}

// Implementaciones de new() para compatibilidad
impl EncryptionInfo {
    pub fn new() -> Self {
        Self {
            encryption_type: EncryptionType::None,
            key_id: 0,
            #[cfg(feature = "std")]
            iv: Vec::new(),
            #[cfg(not(feature = "std"))]
            iv: Vec::new(),
        }
    }
}

impl Snapshot {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "std")]
            id: String::new(),
            #[cfg(not(feature = "std"))]
            id: String::new(),
            timestamp: 0,
            #[cfg(feature = "std")]
            description: String::new(),
            #[cfg(not(feature = "std"))]
            description: String::new(),
        }
    }
}

impl FindResult {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "std")]
            matches: Vec::new(),
            #[cfg(not(feature = "std"))]
            matches: Vec::new(),
            total_matches: 0,
        }
    }
}
