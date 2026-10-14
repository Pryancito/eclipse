//! The mounted filesystem: [`EclipseFS`] picks the driver for the image's
//! format version and forwards every call to it.
//!
//! The method set is the one the kernel's mount adapter and `xtask` call on
//! `btrfs::Btrfs`, with the same argument and error conventions: inode
//! numbers are `u64`, writes are POSIX-style partial on a full device, and
//! every mutating call is applied as a whole or not at all after a crash.
//!
//! - v0.4 ([`cow`](crate::cow), what [`mkfs::format`](crate::mkfs::format)
//!   creates): copy-on-write B+ trees, file data never overwritten in place.
//! - v0.3 ([`v3`](crate::v3)): in-place updates under a metadata journal,
//!   kept so existing images still mount.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::cow::CowFs;
use crate::device::BlockDevice;
use crate::format::{FileKind, BLOCK_SIZE, MAGIC};
use crate::v3::EclipseFsV3;
use crate::{Error, Result};

/// Operations grouped into one transaction at most (unless `sync` or the
/// journal's size commits earlier).
pub const COMMIT_OPS: u32 = 64;

/// Directory entry returned by [`EclipseFS::readdir`].
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub kind: FileKind,
}

/// Inode attributes.
#[derive(Debug, Clone)]
pub struct InodeStat {
    pub ino: u64,
    pub kind: FileKind,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nbytes: u64,
    pub rdev: u64,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
}

/// statfs-style numbers.
#[derive(Debug, Clone, Copy)]
pub struct FsStat {
    pub block_size: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
}

/// What [`EclipseFS::check`] verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckReport {
    pub inodes: u64,
    pub directories: u64,
    /// Blocks in use, all accounted for.
    pub blocks_used: u64,
}

/// What [`EclipseFS::scrub`] read and what failed verification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Tree nodes read (each once, however many trees share it).
    pub tree_nodes: u64,
    /// File data blocks read.
    pub data_blocks: u64,
    /// Nodes whose checksum, header or place in the tree is wrong.
    pub bad_tree_nodes: Vec<u64>,
    /// Data blocks whose contents do not match their checksum.
    pub bad_data_blocks: Vec<u64>,
}

impl ScrubReport {
    pub fn is_clean(&self) -> bool {
        self.bad_tree_nodes.is_empty() && self.bad_data_blocks.is_empty()
    }
}

enum Driver {
    V3(EclipseFsV3),
    V4(CowFs),
}

/// A mounted EclipseFS filesystem.
pub struct EclipseFS {
    driver: Driver,
}

macro_rules! dispatch {
    ($self:ident, $fs:ident => $e:expr) => {
        match &mut $self.driver {
            Driver::V3($fs) => $e,
            Driver::V4($fs) => $e,
        }
    };
}

macro_rules! dispatch_ref {
    ($self:ident, $fs:ident => $e:expr) => {
        match &$self.driver {
            Driver::V3($fs) => $e,
            Driver::V4($fs) => $e,
        }
    };
}

impl EclipseFS {
    /// Mount the filesystem on `dev`, replaying the journal first.
    pub fn mount(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<Self> {
        if dev.size() < BLOCK_SIZE {
            return Err(Error::BadSuperblock);
        }
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        dev.read_at(0, &mut buf)?;
        if &buf[0..9] != MAGIC {
            return Err(Error::BadSuperblock);
        }
        let version = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let driver = match version >> 16 {
            3 => Driver::V3(EclipseFsV3::mount(dev, read_only)?),
            4 => Driver::V4(CowFs::mount(dev, read_only)?),
            _ => return Err(Error::Unsupported("EclipseFS format version")),
        };
        Ok(Self { driver })
    }

    /// Major format version of the mounted image (3 or 4).
    pub fn format_version(&self) -> u32 {
        match self.driver {
            Driver::V3(_) => 3,
            Driver::V4(_) => 4,
        }
    }

    /// Clock used for timestamps (seconds, nanoseconds since the epoch).
    pub fn set_clock(&mut self, clock: fn() -> (u64, u32)) {
        dispatch!(self, fs => fs.set_clock(clock))
    }

    pub fn label(&self) -> String {
        dispatch_ref!(self, fs => fs.label())
    }

    pub fn uuid(&self) -> [u8; 16] {
        dispatch_ref!(self, fs => fs.uuid())
    }

    pub fn nodesize(&self) -> usize {
        BLOCK_SIZE as usize
    }

    pub fn sectorsize(&self) -> usize {
        BLOCK_SIZE as usize
    }

    pub fn root_ino(&self) -> u64 {
        dispatch_ref!(self, fs => fs.root_ino())
    }

    pub fn fsinfo(&self) -> FsStat {
        dispatch_ref!(self, fs => fs.fsinfo())
    }

    /// Commit everything and flush the device.
    pub fn sync(&mut self) -> Result<()> {
        dispatch!(self, fs => fs.sync())
    }

    /// Grow the filesystem to the device's current size. `Ok(true)` if it grew.
    pub fn grow_to_device(&mut self) -> Result<bool> {
        dispatch!(self, fs => fs.grow_to_device())
    }

    pub fn stat(&mut self, ino: u64) -> Result<InodeStat> {
        dispatch!(self, fs => fs.stat(ino))
    }

    pub fn set_attr(
        &mut self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<(u64, u32)>,
        mtime: Option<(u64, u32)>,
    ) -> Result<()> {
        dispatch!(self, fs => fs.set_attr(ino, mode, uid, gid, atime, mtime))
    }

    pub fn lookup(&mut self, dir: u64, name: &str) -> Result<u64> {
        dispatch!(self, fs => fs.lookup(dir, name))
    }

    /// Entries of `dir` in creation order, without `.` and `..`.
    pub fn readdir(&mut self, dir: u64) -> Result<Vec<DirEntry>> {
        dispatch!(self, fs => fs.readdir(dir))
    }

    pub fn create(
        &mut self,
        dir: u64,
        name: &str,
        kind: FileKind,
        mode: u32,
        rdev: u64,
    ) -> Result<u64> {
        dispatch!(self, fs => fs.create(dir, name, kind, mode, rdev))
    }

    pub fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<u64> {
        dispatch!(self, fs => fs.symlink(dir, name, target))
    }

    pub fn write_symlink(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        dispatch!(self, fs => fs.write_symlink(ino, offset, data))
    }

    pub fn read_link(&mut self, ino: u64) -> Result<Vec<u8>> {
        dispatch!(self, fs => fs.read_link(ino))
    }

    /// Parent directory of directory `ino`.
    pub fn parent(&mut self, ino: u64) -> Result<u64> {
        dispatch!(self, fs => fs.parent(ino))
    }

    pub fn link(&mut self, dir: u64, name: &str, ino: u64) -> Result<()> {
        dispatch!(self, fs => fs.link(dir, name, ino))
    }

    pub fn unlink(&mut self, dir: u64, name: &str) -> Result<()> {
        dispatch!(self, fs => fs.unlink(dir, name))
    }

    pub fn rename(
        &mut self,
        old_dir: u64,
        old_name: &str,
        new_dir: u64,
        new_name: &str,
    ) -> Result<()> {
        dispatch!(self, fs => fs.rename(old_dir, old_name, new_dir, new_name))
    }

    pub fn read(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        dispatch!(self, fs => fs.read(ino, offset, buf))
    }

    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        dispatch!(self, fs => fs.write(ino, offset, data))
    }

    pub fn truncate(&mut self, ino: u64, new_size: u64) -> Result<()> {
        dispatch!(self, fs => fs.truncate(ino, new_size))
    }

    /// Full consistency check (fsck without repair); commits first on a
    /// writable mount.
    pub fn check(&mut self) -> Result<CheckReport> {
        dispatch!(self, fs => fs.check())
    }

    /// Read every tree node and data block from the device and verify its
    /// checksum (btrfs scrub, without repair: there is no second copy yet).
    /// Commits first on a writable mount. Needs format v0.4.
    pub fn scrub(&mut self) -> Result<ScrubReport> {
        match &mut self.driver {
            Driver::V3(_) => Err(Error::Unsupported("scrub needs format v0.4")),
            Driver::V4(fs) => fs.scrub(),
        }
    }
}
