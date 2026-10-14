//! EclipseFS filesystem driver (no_std + alloc).
//!
//! The on-disk successor of `eclipsefs-lib` from eclipse-old (v0.3.0), rebuilt
//! as a block-device filesystem that can be mounted read/write as a root.
//!
//! New filesystems use format v0.4 ([`cow`]): btrfs-style copy-on-write B+
//! trees ([`btree`]) keyed by `(objectid, type, offset)`, with lazy
//! reference counts ([`space`]) so trees and extents can be shared, CRC32C
//! on every node, small files inline in the tree, and atomic commits:
//! neither metadata nor file data is overwritten once committed, so a crash
//! leaves the previous or the next state, file contents included.
//!
//! Images in the older v0.3 format ([`v3`]: extent-mapped inode table,
//! allocation-group bitmaps, metadata journal, in-place updates) still
//! mount; [`EclipseFS`] picks the driver from the superblock.
//!
//! The public API ([`EclipseFS`], [`BlockDevice`], [`mkfs`], [`Error`],
//! [`FileKind`], [`InodeStat`], [`DirEntry`], [`FsStat`]) mirrors the one the
//! kernel and `xtask` use from the in-tree `btrfs` crate, so either can sit
//! behind the same mount adapter.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(warnings)]

extern crate alloc;

#[macro_use]
extern crate log;

pub mod blocks;
pub mod btree;
pub mod compress;
pub mod cow;
pub mod cowfmt;
pub mod crc;
pub mod device;
pub mod extent;
pub mod format;
pub mod fs;
pub mod journal;
pub mod mkfs;
pub mod space;
pub mod v3;

pub use compress::Compression;
pub use cow::SendStats;
pub use device::BlockDevice;
pub use format::FileKind;
pub use fs::{
    CheckReport, CompSize, DirEntry, EclipseFS, FsStat, InodeStat, QgroupInfo, ScrubReport,
    SubvolInfo,
};

/// Version of the on-disk format new filesystems get (v0.4.0).
pub const ECLIPSEFS_VERSION: u32 = cowfmt::VERSION4;

/// Crate-wide error type (same variants as `btrfs::Error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Device I/O failed.
    Io,
    /// Not an EclipseFS filesystem / bad superblock.
    BadSuperblock,
    /// On-disk structure is corrupt.
    Corrupt(&'static str),
    /// Unsupported feature or operation (newer format, read-only mount, ...).
    Unsupported(&'static str),
    /// Entry not found.
    NotFound,
    /// Entry already exists.
    Exists,
    /// Not a directory.
    NotDir,
    /// Is a directory.
    IsDir,
    /// Directory not empty.
    NotEmpty,
    /// No space left on device.
    NoSpace,
    /// Invalid argument (name too long, bad offset, ...).
    Invalid,
}

pub type Result<T> = core::result::Result<T, Error>;
