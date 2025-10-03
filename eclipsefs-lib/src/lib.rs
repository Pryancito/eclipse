#![cfg_attr(not(feature = "std"), no_std)]

pub mod error;
pub mod filesystem;
pub mod format;
pub mod node;
pub mod types;

// Re-exportar los tipos principales
pub use error::{EclipseFSError, EclipseFSResult};
pub use format::{constants, EclipseFSHeader, InodeTableEntry};
pub use types::{
    Acl, AclEntry, AclEntryType, CompressionInfo, CompressionType, DfResult, EncryptionInfo,
    EncryptionType, FindResult, FsckResult, Snapshot, TransparentEncryptionConfig,
};

// Re-exportar tipos según la feature activa
pub use filesystem::EclipseFS;
pub use node::{EclipseFSNode, NodeKind};

// Módulos específicos solo para std
#[cfg(feature = "std")]
pub mod reader;
#[cfg(feature = "std")]
pub mod writer;

#[cfg(feature = "std")]
pub use reader::EclipseFSReader;
#[cfg(feature = "std")]
pub use writer::EclipseFSWriter;

pub const ECLIPSEFS_VERSION: u32 = 0x00020000; // v0.2.0
