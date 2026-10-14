//! EclipseFS filesystem driver (no_std + alloc).
//!
//! The on-disk successor of `eclipsefs-lib` from eclipse-old (v0.3.0), rebuilt
//! as a block-device filesystem that can be mounted read/write as a root:
//!
//! - **Extent-based files** (ext4/XFS): every file maps logical blocks to
//!   physical runs through an [`extent::ExtentTree`]; six extents live in the
//!   inode, the rest in a checksummed overflow chain.
//! - **Allocation groups** (XFS): the device is split into groups of
//!   [`format::GROUP_BLOCKS`] blocks, each with its own free-space bitmap and a
//!   backup superblock, so growing the filesystem is adding groups.
//! - **Goal-directed allocation**: a file's next blocks are searched for right
//!   after its previous ones, which keeps sequential files in one extent.
//! - **Checksums** (CRC32) on the superblock, every inode record and every
//!   extent block.
//! - Inline symlink targets, sparse files, hard links, rename with
//!   directory-cycle checks and auto-grow to the device size.
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
pub mod crc;
pub mod device;
pub mod extent;
pub mod format;
pub mod fs;
pub mod mkfs;

pub use device::BlockDevice;
pub use format::FileKind;
pub use fs::{DirEntry, EclipseFS, FsStat, InodeStat};

/// Version of the on-disk format and of this driver (v0.3.0).
pub const ECLIPSEFS_VERSION: u32 = format::VERSION;

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
