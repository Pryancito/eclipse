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

use crate::compress::Compression;
use crate::cow::EncryptionPolicy;
use crate::cow::{CowFs, SendStats};
use crate::device::BlockDevice;
use crate::format::{FileKind, BLOCK_SIZE, MAGIC};
use crate::v3::EclipseFsV3;
use crate::volume::{ChunkKind, Profile, Tier};
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

/// How much a file's data takes on disk (see [`EclipseFS::compsize`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompSize {
    pub disk_bytes: u64,
    pub uncompressed_bytes: u64,
    /// Pieces of compressed extents the file maps.
    pub compressed_extents: u64,
}

/// One mapped piece of a file (see [`EclipseFS::fiemap`]), as
/// `FS_IOC_FIEMAP` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMapping {
    /// Byte offset in the file.
    pub logical: u64,
    /// Byte address on the filesystem's (logical) disk space; 0 for inline
    /// data, which lives in the tree.
    pub physical: u64,
    pub length: u64,
    /// `FIEMAP_EXTENT_*` bits.
    pub flags: u32,
}

/// `FIEMAP_EXTENT_LAST`: the last piece of the file.
pub const FIEMAP_EXTENT_LAST: u32 = 0x1;
/// `FIEMAP_EXTENT_ENCODED`: compressed on disk.
pub const FIEMAP_EXTENT_ENCODED: u32 = 0x8;
/// `FIEMAP_EXTENT_NOT_ALIGNED`: not block aligned (inline data).
pub const FIEMAP_EXTENT_NOT_ALIGNED: u32 = 0x100;
/// `FIEMAP_EXTENT_DATA_INLINE`: stored with the metadata.
pub const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x200;
/// `FIEMAP_EXTENT_UNWRITTEN`: preallocated, reads as zeros.
pub const FIEMAP_EXTENT_UNWRITTEN: u32 = 0x800;
/// `FIEMAP_EXTENT_SHARED`: shared with a snapshot or a reflink.
pub const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

/// One device of the filesystem (see [`EclipseFS::devices`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    pub devid: u32,
    pub size_bytes: u64,
    /// Bytes chunks take on it.
    pub allocated_bytes: u64,
    /// False when mounted degraded without it.
    pub present: bool,
    /// Fast or slow, for tiered placement.
    pub tier: Tier,
}

/// One chunk of the logical space (see [`EclipseFS::chunks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    /// First logical byte.
    pub logical: u64,
    pub kind: ChunkKind,
    pub profile: Profile,
    pub size_bytes: u64,
    pub used_bytes: u64,
    /// Where each stripe starts: device and byte offset.
    pub stripes: Vec<(u32, u64)>,
}

/// What [`EclipseFS::balance`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BalanceReport {
    /// Chunks copied to a new place (and profile).
    pub relocated: u64,
    /// Empty chunks given back to the devices.
    pub freed: u64,
}

/// One subvolume's quota group (see [`EclipseFS::qgroup_show`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QgroupInfo {
    /// Subvolume id.
    pub id: u64,
    /// File bytes its files hold (what the limit applies to).
    pub referenced: u64,
    /// Most `referenced` may grow to.
    pub limit: Option<u64>,
    /// Data bytes on disk its files point at.
    pub disk_referenced: u64,
    /// Of those, the ones no other subvolume points at.
    pub exclusive: u64,
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
    /// Bad copies rewritten from a good one (volumes with DUP or RAID).
    pub repaired: u64,
}

impl ScrubReport {
    pub fn is_clean(&self) -> bool {
        self.bad_tree_nodes.is_empty() && self.bad_data_blocks.is_empty()
    }
}

/// One subvolume or snapshot, from [`EclipseFS::list_subvolumes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubvolInfo {
    pub id: u64,
    /// Handle of its root directory.
    pub root: u64,
    /// Subvolume holding the entry that names it (0 for the top one).
    pub parent: u64,
    /// Handle of the directory holding that entry (0 for the top one).
    pub dir: u64,
    pub name: String,
    pub read_only: bool,
    /// Subvolume it is a snapshot of (0 if none).
    pub snapshot_of: u64,
    /// Generation the snapshot was taken at.
    pub snapshot_gen: u64,
    pub created: (u64, u32),
    pub uuid: [u8; 16],
    /// UUID of the subvolume it is a snapshot of (zero if none).
    pub parent_uuid: [u8; 16],
    /// UUID of the subvolume a `receive` made it from (zero if none).
    pub received_uuid: [u8; 16],
}

/// What the superblock at the start of a device says, read without
/// mounting (what `blkid` and `btrfs device scan` look at).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProbe {
    /// Full on-disk format version (`0x0004_0000` for v0.4).
    pub version: u32,
    pub uuid: [u8; 16],
    pub label: String,
    /// This device's id in its volume (1 on a filesystem of one device).
    pub devid: u32,
    /// Devices the volume is made of.
    pub num_devices: u32,
    /// Generation of the last commit this device saw.
    pub generation: u64,
    /// Bytes of the filesystem this one device must hold: all of it on a
    /// single device, only its own superblock in a volume (whose chunks
    /// the chunk map places).
    pub min_device_bytes: u64,
}

/// Read the superblock of `dev` (see [`DeviceProbe`]). `BadSuperblock` if
/// it holds no EclipseFS, `Unsupported` for a format this crate cannot
/// mount, `Corrupt` if the checksum is wrong.
pub fn probe(dev: &dyn BlockDevice) -> Result<DeviceProbe> {
    if dev.size() < BLOCK_SIZE {
        return Err(Error::BadSuperblock);
    }
    let mut buf = vec![0u8; BLOCK_SIZE as usize];
    dev.read_at(0, &mut buf)?;
    if &buf[0..9] != MAGIC {
        return Err(Error::BadSuperblock);
    }
    let version = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    let label = |l: &[u8]| {
        let end = l.iter().position(|&c| c == 0).unwrap_or(l.len());
        String::from_utf8_lossy(&l[..end]).into_owned()
    };
    match version >> 16 {
        3 => {
            let sb = crate::format::Superblock::decode(&buf)?;
            Ok(DeviceProbe {
                version: sb.version,
                uuid: sb.uuid,
                label: label(&sb.label),
                devid: 1,
                num_devices: 1,
                generation: sb.generation,
                min_device_bytes: sb.total_blocks * BLOCK_SIZE,
            })
        }
        4 => {
            let sb = crate::cowfmt::SuperV4::decode(&buf)?;
            let (devid, num_devices, min) = match &sb.vol {
                Some(v) => (v.devid, v.num_devices, BLOCK_SIZE),
                None => (1, 1, sb.total_blocks * BLOCK_SIZE),
            };
            Ok(DeviceProbe {
                version,
                uuid: sb.uuid,
                label: label(&sb.label),
                devid,
                num_devices,
                generation: sb.generation,
                min_device_bytes: min,
            })
        }
        _ => Err(Error::Unsupported("EclipseFS format version")),
    }
}

// One per mounted filesystem: the size difference does not matter.
#[allow(clippy::large_enum_variant)]
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

    /// Mount a filesystem made of several devices (given in any order),
    /// as `mount -o device=...` does for btrfs. A single device works too.
    pub fn mount_devices(devs: Vec<Arc<dyn BlockDevice>>, read_only: bool) -> Result<Self> {
        Self::mount_volume(devs, read_only, false)
    }

    /// [`mount_devices`](Self::mount_devices) with devices missing
    /// (`-o degraded`): works while every block keeps a copy on the
    /// devices given. Writes go to the copies that are there; new chunks
    /// stay off the missing devices.
    pub fn mount_degraded(devs: Vec<Arc<dyn BlockDevice>>, read_only: bool) -> Result<Self> {
        Self::mount_volume(devs, read_only, true)
    }

    fn mount_volume(
        devs: Vec<Arc<dyn BlockDevice>>,
        read_only: bool,
        degraded: bool,
    ) -> Result<Self> {
        if devs.len() == 1 && !degraded {
            return Self::mount(devs[0].clone(), read_only);
        }
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        for d in &devs {
            d.read_at(0, &mut buf)?;
            let version = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
            if &buf[0..9] != MAGIC || version >> 16 != 4 {
                return Err(Error::BadSuperblock);
            }
        }
        Ok(Self {
            driver: Driver::V4(CowFs::mount_devices(devs, read_only, degraded)?),
        })
    }

    /// Generation of the last committed transaction (btrfs `transid`).
    pub fn generation(&self) -> u64 {
        match &self.driver {
            Driver::V3(f) => f.generation(),
            Driver::V4(f) => f.generation(),
        }
    }

    /// Major format version of the mounted image (3 or 4).
    pub fn format_version(&self) -> u32 {
        match self.driver {
            Driver::V3(_) => 3,
            Driver::V4(_) => 4,
        }
    }

    /// Forget clean cached metadata (v0.4; nothing on v0.3), so the next
    /// reads come from the device.
    pub fn drop_caches(&mut self) {
        if let Driver::V4(fs) = &mut self.driver {
            fs.drop_caches();
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

    /// Make file `ino` durable (`fsync`/`fdatasync`). On v0.4 a regular
    /// file whose changes since the last commit are writes, truncates and
    /// attribute changes gets one record in the fsync log and one device
    /// flush; anything else commits, as [`sync`](Self::sync) does.
    pub fn fsync(&mut self, ino: u64) -> Result<()> {
        match &mut self.driver {
            Driver::V3(fs) => fs.sync(),
            Driver::V4(fs) => fs.fsync(ino),
        }
    }

    /// Commit every `secs` seconds of the clock instead of every
    /// [`COMMIT_OPS`] operations (v0.4; `None` restores the default). The
    /// caller runs [`commit_if_due`](Self::commit_if_due) from a timer.
    pub fn set_commit_interval(&mut self, secs: Option<u64>) {
        if let Driver::V4(fs) = &mut self.driver {
            fs.set_commit_interval(secs);
        }
    }

    /// Commit if the commit interval has passed (and anything changed).
    pub fn commit_if_due(&mut self) -> Result<bool> {
        match &mut self.driver {
            Driver::V3(fs) => fs.sync().map(|_| false),
            Driver::V4(fs) => fs.commit_if_due(),
        }
    }

    /// Whether [`write_atomic`](Self::write_atomic) works on `ino` (v0.4
    /// regular files that are not no-COW).
    pub fn atomic_write_supported(&mut self, ino: u64) -> Result<bool> {
        match &mut self.driver {
            Driver::V3(_) => Ok(false),
            Driver::V4(fs) => fs.atomic_write_supported(ino),
        }
    }

    /// `RWF_ATOMIC`: write all of `data` or nothing, untorn by a crash (at
    /// most [`ATOMIC_WRITE_MAX`](crate::cow::ATOMIC_WRITE_MAX) bytes).
    pub fn write_atomic(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.cow()?.write_atomic(ino, offset, data)
    }

    /// fsync log records written since mount (0 on v0.3).
    pub fn fsync_log_records(&self) -> u64 {
        match &self.driver {
            Driver::V3(_) => 0,
            Driver::V4(fs) => fs.log_records(),
        }
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

    /// [`read`](Self::read) through `&self`, so that readers holding the
    /// filesystem shared run side by side. `Ok(None)`: this read needs the
    /// exclusive path (v0.3, compressed extents, a block that fails its
    /// checksum and wants repair; see `cow::shared`).
    pub fn read_shared(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<Option<usize>> {
        match &self.driver {
            Driver::V3(_) => Ok(None),
            Driver::V4(fs) => fs.read_shared(ino, offset, buf),
        }
    }

    /// [`stat`](Self::stat) through `&self`; `Ok(None)` as for
    /// [`read_shared`](Self::read_shared).
    pub fn stat_shared(&self, ino: u64) -> Result<Option<InodeStat>> {
        match &self.driver {
            Driver::V3(_) => Ok(None),
            Driver::V4(fs) => fs.stat_shared(ino),
        }
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

    fn cow(&mut self) -> Result<&mut CowFs> {
        match &mut self.driver {
            Driver::V3(_) => Err(Error::Unsupported("needs format v0.4")),
            Driver::V4(fs) => Ok(fs),
        }
    }

    /// New empty subvolume named `name` in `dir`; returns its root
    /// directory (btrfs `subvolume create`).
    pub fn create_subvolume(&mut self, dir: u64, name: &str) -> Result<u64> {
        self.cow()?.create_subvolume(dir, name)
    }

    /// Snapshot of the subvolume whose root directory is `src`, named `name`
    /// in `dir`; returns its root directory. It shares every block with the
    /// source until either changes. Subvolumes nested in the source are not
    /// part of it (as on btrfs).
    pub fn snapshot(&mut self, src: u64, dir: u64, name: &str, read_only: bool) -> Result<u64> {
        self.cow()?.snapshot(src, dir, name, read_only)
    }

    /// Delete the subvolume named `name` in `dir`, whatever it holds (but
    /// not while other subvolumes are nested in it).
    pub fn delete_subvolume(&mut self, dir: u64, name: &str) -> Result<()> {
        self.cow()?.delete_subvolume(dir, name)
    }

    /// Id of the subvolume inode `ino` lives in (5 for the top one).
    pub fn subvolume_of(&self, ino: u64) -> u64 {
        crate::cowfmt::split(ino).0
    }

    pub fn list_subvolumes(&mut self) -> Result<Vec<SubvolInfo>> {
        self.cow()?.list_subvolumes()
    }

    /// Make subvolume `id` read-only or writable again.
    pub fn set_subvolume_read_only(&mut self, id: u64, read_only: bool) -> Result<()> {
        self.cow()?.set_subvolume_read_only(id, read_only)
    }

    /// Subvolume mounted as `/` from the next mount on.
    pub fn set_default_subvolume(&mut self, id: u64) -> Result<()> {
        self.cow()?.set_default_subvolume(id)
    }

    /// Make `dst` share the blocks of `src` (reflink, `FICLONERANGE`).
    /// Offsets are block-aligned; `len` is too unless the range ends at the
    /// end of `src` (0 means up to there). Works across subvolumes. Returns
    /// the bytes cloned.
    pub fn clone_range(
        &mut self,
        src: u64,
        src_off: u64,
        dst: u64,
        dst_off: u64,
        len: u64,
    ) -> Result<u64> {
        self.cow()?.clone_range(src, src_off, dst, dst_off, len)
    }

    /// `FIDEDUPERANGE`: if the two ranges hold the same bytes, make `dst`
    /// share the blocks of `src`. Returns the bytes deduplicated (0 if
    /// they differ), at most 16 MiB per call.
    pub fn dedupe_range(
        &mut self,
        src: u64,
        src_off: u64,
        dst: u64,
        dst_off: u64,
        len: u64,
    ) -> Result<u64> {
        self.cow()?.dedupe_range(src, src_off, dst, dst_off, len)
    }

    /// Value of extended attribute `name` (`user.`, `trusted.`, `security.`
    /// or `system.` namespace; POSIX ACLs are `system.posix_acl_*`).
    pub fn get_xattr(&mut self, ino: u64, name: &str) -> Result<Vec<u8>> {
        self.cow()?.get_xattr(ino, name)
    }

    /// Names of every extended attribute of `ino`, sorted.
    pub fn list_xattrs(&mut self, ino: u64) -> Result<Vec<String>> {
        self.cow()?.list_xattrs(ino)
    }

    /// Set an extended attribute; `flags` is `XATTR_CREATE`, `XATTR_REPLACE`
    /// or 0, as in setxattr(2).
    pub fn set_xattr(&mut self, ino: u64, name: &str, value: &[u8], flags: u32) -> Result<()> {
        self.cow()?.set_xattr(ino, name, value, flags)
    }

    pub fn remove_xattr(&mut self, ino: u64, name: &str) -> Result<()> {
        self.cow()?.remove_xattr(ino, name)
    }

    /// Inode flags (`FS_IOC_GETFLAGS`, Linux `FS_*_FL` values).
    pub fn get_flags(&mut self, ino: u64) -> Result<u32> {
        self.cow()?.get_flags(ino)
    }

    /// `FS_IOC_SETFLAGS` (chattr): immutable, append-only, no-COW, ...
    pub fn set_flags(&mut self, ino: u64, flags: u32) -> Result<()> {
        self.cow()?.set_flags(ino, flags)
    }

    /// fallocate(2): reserve blocks (`0` or `FALLOC_FL_KEEP_SIZE`), punch a
    /// hole (`PUNCH_HOLE | KEEP_SIZE`) or zero a range (`ZERO_RANGE`).
    pub fn fallocate(&mut self, ino: u64, mode: u32, offset: u64, len: u64) -> Result<()> {
        self.cow()?.fallocate(ino, mode, offset, len)
    }

    /// `compress=` mount option: compress new data of every file (not the
    /// no-COW or no-compression ones) with `algo`. Not stored on disk.
    pub fn set_mount_compression(&mut self, algo: Option<Compression>) -> Result<()> {
        self.cow()?.set_mount_compression(algo);
        Ok(())
    }

    /// Compression property of `ino` (`btrfs property get ... compression`).
    pub fn compression(&mut self, ino: u64) -> Result<Option<Compression>> {
        self.cow()?.compression(ino)
    }

    /// Compress new data of `ino` with `algo` (or stop); a directory passes
    /// it on to files created in it.
    pub fn set_compression(&mut self, ino: u64, algo: Option<Compression>) -> Result<()> {
        self.cow()?.set_compression(ino, algo)
    }

    /// The pieces of file `ino` that overlap bytes `start..start+len`, in
    /// file order (`FS_IOC_FIEMAP`, `filefrag`). Holes are left out;
    /// preallocated space is reported as unwritten.
    pub fn fiemap(&mut self, ino: u64, start: u64, len: u64) -> Result<Vec<FileMapping>> {
        self.cow()?.fiemap(ino, start, len)
    }

    /// `lseek(SEEK_DATA)`: the first offset at or after `off` that holds
    /// data, or `None` when only holes (or preallocated space) follow.
    pub fn seek_data(&mut self, ino: u64, off: u64) -> Result<Option<u64>> {
        self.cow()?.seek_data(ino, off)
    }

    /// `lseek(SEEK_HOLE)`: the first offset at or after `off` in a hole;
    /// the end of the file counts as one. `None` when `off` is past the end.
    pub fn seek_hole(&mut self, ino: u64, off: u64) -> Result<Option<u64>> {
        self.cow()?.seek_hole(ino, off)
    }

    /// `btrfs filesystem defragment` of file bytes `start..start+len`:
    /// fragmented runs of extents shorter than `threshold` bytes (0 for
    /// the 32 MiB default) move to one contiguous run each; with
    /// `compress`, the data is rewritten compressed that way. Rewritten
    /// data is no longer shared with snapshots or clones. Returns the
    /// bytes rewritten.
    pub fn defrag(
        &mut self,
        ino: u64,
        start: u64,
        len: u64,
        threshold: u64,
        compress: Option<Compression>,
    ) -> Result<u64> {
        self.cow()?.defrag(ino, start, len, threshold, compress)
    }

    /// Disk and file bytes of a file's data (as `compsize` reports).
    pub fn compsize(&mut self, ino: u64) -> Result<CompSize> {
        self.cow()?.compsize(ino)
    }

    /// Source of random bytes for encryption nonces (the kernel's RNG).
    /// Without one, no encrypted inode can be created.
    pub fn set_random(&mut self, random: fn(&mut [u8])) {
        if let Driver::V4(fs) = &mut self.driver {
            fs.set_random(random);
        }
    }

    /// Add an encryption master key (16 to 64 bytes) for this mount;
    /// returns its identifier (`FS_IOC_ADD_ENCRYPTION_KEY`).
    pub fn add_encryption_key(&mut self, key: &[u8]) -> Result<[u8; 16]> {
        self.cow()?.add_encryption_key(key)
    }

    /// Forget a master key and every key derived from it
    /// (`FS_IOC_REMOVE_ENCRYPTION_KEY`).
    pub fn remove_encryption_key(&mut self, id: &[u8; 16]) -> Result<()> {
        self.cow()?.remove_encryption_key(id)
    }

    /// Whether master key `id` has been added
    /// (`FS_IOC_GET_ENCRYPTION_KEY_STATUS`).
    pub fn encryption_key_present(&mut self, id: &[u8; 16]) -> Result<bool> {
        Ok(self.cow()?.encryption_key_present(id))
    }

    /// Encrypt empty directory `dir` with `policy`, whose key must have
    /// been added (`FS_IOC_SET_ENCRYPTION_POLICY`). What is created under
    /// it inherits the policy.
    pub fn set_encryption_policy(&mut self, dir: u64, policy: &EncryptionPolicy) -> Result<()> {
        self.cow()?.set_encryption_policy(dir, policy)
    }

    /// The policy of `ino`, if it is encrypted
    /// (`FS_IOC_GET_ENCRYPTION_POLICY_EX`).
    pub fn encryption_policy(&mut self, ino: u64) -> Result<Option<EncryptionPolicy>> {
        match &mut self.driver {
            Driver::V3(_) => Ok(None),
            Driver::V4(fs) => fs.encryption_policy(ino),
        }
    }

    /// The nonce of encrypted inode `ino` (`FS_IOC_GET_ENCRYPTION_NONCE`).
    pub fn encryption_nonce(&mut self, ino: u64) -> Result<[u8; 16]> {
        self.cow()?.encryption_nonce(ino)
    }

    /// `btrfs filesystem resize [devid:]size|max`: device `devid` (1 on
    /// a filesystem that is not a volume) becomes `size` bytes, or all of
    /// the device for `None`. Shrinking moves whatever is past the new end
    /// below it first (`NoSpace` if it does not fit). A size given stays
    /// over remounts; `None` lets mounting grow with the device again.
    /// Returns the new size in bytes.
    pub fn resize(&mut self, devid: u32, size: Option<u64>) -> Result<u64> {
        self.cow()?.resize(devid, size)
    }

    /// `btrfs device add`: one more device; new chunks may use it at
    /// once, [`balance`](Self::balance) moves old ones. Returns its id.
    /// A filesystem on one device becomes a volume (nothing moves).
    pub fn add_device(&mut self, dev: Arc<dyn BlockDevice>) -> Result<u32> {
        self.cow()?.add_device(dev)
    }

    /// `btrfs device remove`: move every chunk off device `devid` (which
    /// may be missing) and drop it; its superblock is wiped.
    pub fn remove_device(&mut self, devid: u32) -> Result<()> {
        self.cow()?.remove_device(devid)
    }

    /// `btrfs replace`: `new` takes the place of device `devid` (which may
    /// be missing). Returns the new device's id.
    pub fn replace_device(&mut self, devid: u32, new: Arc<dyn BlockDevice>) -> Result<u32> {
        self.cow()?.replace_device(devid, new)
    }

    /// `btrfs balance` (with `-dconvert`/`-mconvert` when given): rewrite
    /// every chunk with the data and metadata profiles, giving empty
    /// chunks back and spreading the rest over every device.
    pub fn balance(
        &mut self,
        data: Option<Profile>,
        meta: Option<Profile>,
    ) -> Result<BalanceReport> {
        self.cow()?.balance(data, meta)
    }

    /// Mark device `devid` fast (SSD) or slow (HDD), or neither. With both
    /// kinds present, metadata and new data go to fast devices first.
    pub fn set_device_tier(&mut self, devid: u32, tier: Tier) -> Result<()> {
        self.cow()?.set_device_tier(devid, tier)
    }

    /// The tier of device `devid` ([`Tier::Any`] when it has none).
    pub fn device_tier(&self, devid: u32) -> Tier {
        match &self.driver {
            Driver::V4(c) => c.device_tier(devid),
            Driver::V3(_) => Tier::Any,
        }
    }

    /// `btrfs device add` with a tier for the new device.
    pub fn add_device_tier(&mut self, dev: Arc<dyn BlockDevice>, tier: Tier) -> Result<u32> {
        let id = self.add_device(dev)?;
        if tier != Tier::Any {
            self.set_device_tier(id, tier)?;
        }
        Ok(id)
    }

    /// Move full data chunks from fast to slow devices and metadata chunks
    /// to fast ones (every data chunk on a fast device with `all`).
    pub fn tier_migrate(&mut self, all: bool) -> Result<BalanceReport> {
        self.cow()?.tier_migrate(all)
    }

    /// Profiles of new data and metadata chunks.
    pub fn profiles(&self) -> (Profile, Profile) {
        match &self.driver {
            Driver::V4(c) => c.profiles(),
            Driver::V3(_) => (Profile::Single, Profile::Single),
        }
    }

    /// `btrfs device usage`: every device of the filesystem.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        match &self.driver {
            Driver::V4(c) => c.devices(),
            Driver::V3(_) => Vec::new(),
        }
    }

    /// Every chunk: logical range, kind, profile, use and stripes.
    pub fn chunks(&self) -> Vec<ChunkInfo> {
        match &self.driver {
            Driver::V4(c) => c.chunks(),
            Driver::V3(_) => Vec::new(),
        }
    }

    /// Whether quotas are on.
    pub fn quota_enabled(&self) -> bool {
        match &self.driver {
            Driver::V4(c) => c.quota_enabled(),
            Driver::V3(_) => false,
        }
    }

    /// `btrfs quota enable`: count the file bytes of every subvolume from
    /// now on, so limits can apply.
    pub fn quota_enable(&mut self) -> Result<()> {
        self.cow()?.quota_enable()
    }

    /// `btrfs quota disable`: stop counting; limits stop applying.
    pub fn quota_disable(&mut self) -> Result<()> {
        self.cow()?.quota_disable()
    }

    /// `btrfs quota rescan`: recount every subvolume.
    pub fn quota_rescan(&mut self) -> Result<()> {
        self.cow()?.quota_rescan()
    }

    /// `btrfs qgroup limit`: most file bytes subvolume `id` may hold
    /// (`None`: no limit). Writes past it fail with `NoSpace` (EDQUOT).
    pub fn set_qgroup_limit(&mut self, id: u64, limit: Option<u64>) -> Result<()> {
        self.cow()?.set_qgroup_limit(id, limit)
    }

    /// `btrfs qgroup show`: every subvolume's counter, limit and disk use.
    pub fn qgroup_show(&mut self) -> Result<Vec<QgroupInfo>> {
        self.cow()?.qgroup_show()
    }

    /// btrfs send: write the read-only snapshot whose root directory is
    /// `snap` to `out` as a stream, in full or (with `parent`, an older
    /// read-only snapshot) as the changes since `parent`. Reflinks inside
    /// the snapshot are sent as clones.
    pub fn send(
        &mut self,
        snap: u64,
        parent: Option<u64>,
        out: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<SendStats> {
        self.cow()?.send(snap, parent, out)
    }

    /// btrfs receive: make a read-only subvolume in `dir` from a stream
    /// [`send`](Self::send) wrote, read with `input` (which fills the
    /// buffer it is given). An incremental stream needs its parent
    /// received before. Nothing is left behind if the stream is cut or
    /// damaged. Returns the new subvolume's root directory.
    pub fn receive(
        &mut self,
        dir: u64,
        input: &mut dyn FnMut(&mut [u8]) -> Result<()>,
    ) -> Result<u64> {
        self.cow()?.receive(dir, input)
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
