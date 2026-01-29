#![cfg_attr(not(feature = "std"), no_std)]

pub mod error;
pub mod filesystem;
pub mod format;
pub mod node;
pub mod types;
pub mod extent;
pub mod blocks;
// pub mod ai_features;
// pub mod quantum_crypto;

// Re-exportar los tipos principales
pub use error::{EclipseFSError, EclipseFSResult};
pub use format::{constants, EclipseFSHeader, InodeTableEntry};
pub use types::{
    Acl, AclEntry, AclEntryType, CompressionInfo, CompressionType, DfResult, EncryptionInfo,
    EncryptionType, FindResult, FsckResult, Snapshot, TransparentEncryptionConfig,
};
pub use extent::{Extent, ExtentTree, ExtentStats, EXTENT_FLAG_UNWRITTEN, EXTENT_FLAG_COMPRESSED, EXTENT_FLAG_ENCRYPTED};
pub use blocks::{BlockAllocator, AllocationGroup, AllocatorStats, BLOCK_SIZE};

// Re-exportar tipos según la feature activa
pub use filesystem::EclipseFS;
pub use node::{EclipseFSNode, NodeKind};

// Re-exportar nuevas características avanzadas (temporalmente deshabilitadas)
// pub use ai_features::{AIEngine, AIFeaturesConfig, AccessPrediction, PerformanceMetrics, OptimizationRecommendation};
// pub use quantum_crypto::{PostQuantumCrypto, PostQuantumConfig, PostQuantumAlgorithm, SecurityLevel, QuantumThreatLevel};

// Módulos específicos solo para std
#[cfg(feature = "std")]
pub mod reader;
#[cfg(feature = "std")]
pub mod writer;
#[cfg(feature = "std")]
pub mod cache;
#[cfg(feature = "std")]
pub mod defragmentation;
#[cfg(feature = "std")]
pub mod load_balancing;
#[cfg(feature = "std")]
pub mod journal;

#[cfg(feature = "std")]
pub use reader::EclipseFSReader;
#[cfg(feature = "std")]
pub use writer::EclipseFSWriter;
#[cfg(feature = "std")]
pub use journal::{Journal, JournalConfig, JournalEntry, JournalStats, TransactionType};

pub const ECLIPSEFS_VERSION: u32 = 0x00030000; // v0.3.0
