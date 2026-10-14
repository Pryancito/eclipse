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
use crate::cow::{CowFs, SendStats};
use crate::device::BlockDevice;
use crate::format::{FileKind, BLOCK_SIZE, MAGIC};
use crate::v3::EclipseFsV3;
use crate::volume::{ChunkKind, Profile};
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

/// One device of the filesystem (see [`EclipseFS::devices`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    pub devid: u32,
    pub size_bytes: u64,
    /// Bytes chunks take on it.
    pub allocated_bytes: u64,
    /// False when mounted degraded without it.
    pub present: bool,
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

    /// Disk and file bytes of a file's data (as `compsize` reports).
    pub fn compsize(&mut self, ino: u64) -> Result<CompSize> {
        self.cow()?.compsize(ino)
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
