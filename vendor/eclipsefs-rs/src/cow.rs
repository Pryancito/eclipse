//! The copy-on-write filesystem (format v0.4).
//!
//! Everything lives in [`btree`](crate::btree) trees: inodes, names, file
//! extents, the list of trees and the reference counts. Neither metadata nor
//! file data is ever overwritten in place once committed: a write goes to
//! new blocks and the tree is updated to point there. A transaction (a group
//! of whole operations) commits by writing its new tree nodes, then switching
//! the superblock to the new root tree; the superblock and the allocation
//! bitmaps change together through the [`journal`](crate::journal), so a
//! crash at any point leaves either the old or the new state, file contents
//! included.
//!
//! Blocks the open transaction allocated, and only those, are rewritten in
//! place (a file being appended to in small pieces). Blocks freed by it are
//! reusable only once it commits.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::blocks::{AllocationGroup, BlockAllocator};
use crate::btree::{Body, Key, Node, Op, Root, Store, MAX_ITEM};
use crate::cowfmt::*;
use crate::crc::crc32c;
use crate::device::BlockDevice;
use crate::format::{
    FileKind, BLOCK_SIZE, GROUP_BLOCKS, MAX_NAME_LEN, MAX_SYMLINK_LEN, MIN_GROUP_BLOCKS,
};
use crate::fs::{
    BalanceReport, CheckReport, ChunkInfo, CompSize, DeviceInfo, DirEntry, FsStat, InodeStat,
    QgroupInfo, ScrubReport, SubvolInfo, COMMIT_OPS,
};
use crate::journal::{JournalArea, Overlay};
use crate::space::{metadata_reserve, BlockSpace, RangeSet, RefMap};
use crate::v3::write_home;
use crate::volume::{
    Chunk, ChunkKind, ChunkMap, Profile, Stripe, Tier, Volume, DEV_RESERVED, MIN_CHUNK,
};
use crate::{Error, Result};

const BS: u64 = BLOCK_SIZE;
/// Blocks handled per step of a large write (bounds the buffer).
const WRITE_CHUNK: u64 = 2048;
/// Largest [`CowFs::write_atomic`] (`RWF_ATOMIC`'s `atomic_write_unit_max`):
/// the caller holds it in one buffer, which the kernel heap keeps at 1 MiB.
pub const ATOMIC_WRITE_MAX: u64 = 1 << 20;
/// Extents at least this long are left alone by [`CowFs::defrag`] (btrfs's
/// default `extent_thresh`).
const DEFRAG_THRESHOLD: u64 = 32 << 20;
/// Most blocks [`CowFs::defrag`] moves to one new run.
const DEFRAG_WINDOW: u64 = WRITE_CHUNK;
/// Commit early once this many tree nodes wait to be written.
const DIRTY_NODES_LIMIT: usize = 4096;
/// With timed commits, commit early once this many data blocks wait for
/// their checksums to go into the tree (256 MiB of data, a few MiB of
/// kernel heap).
const PENDING_SUMS_LIMIT: usize = 1 << 16;
/// Items [`CowFs::purge`] reads in one step. Deleting an inode visits every
/// item it owns, and a multi-GB file has millions of them, so the list they
/// are read into is the allocation that matters, not the work.
const PURGE_BATCH: usize = 1024;
/// File extents read in one step by whatever walks a range that may span a
/// whole file (truncate, `lseek(SEEK_DATA)`, FIEMAP, reflinks, defrag); see
/// [`CowFs::extents_upto`].
const EXTENT_BATCH: usize = 1024;

/// The smallest key greater than `k`, or `None` when `k` is [`Key::MAX`].
///
/// Keys order by `(objectid, ty, offset)`, so stepping carries from the
/// offset up, exactly as the comparison reads them.
fn key_after(k: &Key) -> Option<Key> {
    if k.offset != u64::MAX {
        Some(Key::new(k.objectid, k.ty, k.offset + 1))
    } else if k.ty != u8::MAX {
        Some(Key::new(k.objectid, k.ty + 1, 0))
    } else if k.objectid != u64::MAX {
        Some(Key::new(k.objectid + 1, 0, 0))
    } else {
        None
    }
}

mod bitmap_sums;
mod encrypt;
mod fsync_log;

pub use encrypt::EncryptionPolicy;
mod repair;
mod resize;
mod send;
mod shared;
mod volume_ops;
pub use send::{ReceiveSource, SendSink, SendStats};

/// One file extent as the write and read paths see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ext {
    /// First file block.
    lblk: u64,
    /// First disk block.
    pblk: u64,
    blocks: u64,
    /// Reserved by `fallocate`, not written yet (reads as zeros).
    prealloc: bool,
    /// Compressed: `pblk` is where the extent starts and `lblk` maps to
    /// this block of what it holds.
    z: Option<Zext>,
}

/// The compressed extent behind an [`Ext`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Zext {
    algo: Compression,
    disk_blocks: u64,
    ram_blocks: u64,
    offset: u64,
}

impl Ext {
    fn item(pblk: u64, blocks: u64, prealloc: bool) -> Vec<u8> {
        if prealloc {
            FileExtent::Prealloc { disk: pblk, blocks }.encode()
        } else {
            FileExtent::Reg { disk: pblk, blocks }.encode()
        }
    }

    /// The item for file blocks `from..to` of this extent.
    fn piece(&self, from: u64, to: u64) -> Vec<u8> {
        match self.z {
            None => Self::item(self.pblk + (from - self.lblk), to - from, self.prealloc),
            Some(z) => FileExtent::Comp(Comp {
                algo: z.algo,
                disk: self.pblk,
                disk_blocks: z.disk_blocks,
                ram_blocks: z.ram_blocks,
                offset: z.offset + (from - self.lblk),
                blocks: to - from,
            })
            .encode(),
        }
    }

    /// Disk block right after the data of file block `lb - 1` (a goal for
    /// the blocks of `lb`).
    fn next_disk(&self, lb: u64) -> u64 {
        match self.z {
            None => self.pblk + (lb - self.lblk),
            Some(z) => self.pblk + z.disk_blocks,
        }
    }
}

/// A mounted v0.4 filesystem.
pub struct CowFs {
    /// Superblock and bitmaps, through the journal.
    dev: Arc<Overlay>,
    raw: Arc<dyn BlockDevice>,
    /// `raw` as a volume, when the filesystem is one.
    vol: Option<Arc<Volume>>,
    journal: JournalArea,
    sb: SuperV4,
    sp: BlockSpace,
    st: Store,
    root_tree: Root,
    refs_root: Root,
    /// Checksums of file data.
    csum_root: Root,
    /// Reference counts as last written to the reference tree.
    refs_on_disk: RefMap,
    /// The subvolume the inode-level helpers work on, its tree and item.
    subvol: u64,
    fs_root: Root,
    fs_item: RootItem,
    /// Other subvolumes touched since mount: tree and item.
    subvols: BTreeMap<u64, (Root, RootItem)>,
    /// The subvolume mounted as `/`.
    default_subvol: u64,
    read_only: bool,
    nested: bool,
    pending_ops: u32,
    clock: Option<fn() -> (u64, u32)>,
    /// `compress=`: what new data of every file is compressed with.
    compress: Option<Compression>,
    /// The last compressed extent read: its first block and contents.
    zcache: Option<(u64, Vec<u8>)>,
    /// File blocks of compressed files written uncompressed by this
    /// transaction, by (subvolume, inode).
    zpending: BTreeMap<(u64, u64), RangeSet>,
    /// CRC32C of every group's bitmap as last written (0 for holes).
    bitmap_sums: Vec<u32>,
    /// Blocks of the checksum table to write with the next commit.
    sums_dirty: BTreeSet<usize>,
    /// The table is to be (re)written even if nothing else changed.
    sums_pending: bool,
    /// Encryption keys (memory only) and the source of nonces.
    keyring: encrypt::Keyring,
    random: Option<fn(&mut [u8])>,
    /// What `fsync` may write to the log instead of committing.
    log: fsync_log::LogState,
    /// Timed commits: every this many seconds (by the clock) instead of
    /// every [`COMMIT_OPS`] operations.
    commit_interval: Option<u64>,
    /// Clock time of the last commit (seconds).
    last_commit: u64,
}

/// File blocks among `from..to` that `exts` map.
fn covered(exts: &[Ext], from: u64, to: u64) -> u64 {
    exts.iter()
        .map(|e| (e.lblk + e.blocks).min(to).saturating_sub(e.lblk.max(from)))
        .sum()
}

fn check_name(name: &str) -> Result<&[u8]> {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > MAX_NAME_LEN || b.contains(&b'/') || b.contains(&0) {
        return Err(Error::Invalid);
    }
    if name == "." || name == ".." {
        return Err(Error::Invalid);
    }
    Ok(b)
}

/// What an immutable or append-only inode refuses (EPERM on Linux; the
/// btrfs-compatible error set has no such variant).
const NOT_PERMITTED: Error = Error::Unsupported("immutable or append-only inode");

fn immutable(i: &InodeItem) -> bool {
    i.flags & FL_IMMUTABLE as u64 != 0
}

/// Immutable or append-only: no renames, unlinks or attribute changes.
fn frozen(i: &InodeItem) -> bool {
    i.flags & (FL_IMMUTABLE | FL_APPEND) as u64 != 0
}

/// Fail with [`NOT_PERMITTED`] when `bad`.
fn permit(bad: bool) -> Result<()> {
    if bad {
        Err(NOT_PERMITTED)
    } else {
        Ok(())
    }
}

fn stat_of(ino: u64, i: &InodeItem) -> InodeStat {
    InodeStat {
        ino,
        kind: i.kind(),
        mode: i.mode,
        nlink: i.nlink,
        uid: i.uid,
        gid: i.gid,
        size: i.size,
        nbytes: i.nbytes,
        rdev: i.rdev,
        atime: i.atime,
        mtime: i.mtime,
        ctime: i.ctime,
    }
}

/// One group's bitmap as read: the group (a blank one when the block does
/// not decode and `lenient`), and the CRC32C of the block (`None` then).
fn read_group(
    dev: &dyn BlockDevice,
    id: u64,
    count: u64,
    buf: &mut [u8],
    lenient: bool,
) -> Result<(AllocationGroup, Option<u32>)> {
    dev.read_at((id * GROUP_BLOCKS + 1) * BS, buf)?;
    match AllocationGroup::from_bytes(id as u32, count, buf) {
        Ok(g) => Ok((g, Some(crc32c(buf)))),
        Err(_) if lenient => Ok((AllocationGroup::new(id as u32, count), None)),
        Err(e) => Err(e),
    }
}

/// Groups of a filesystem that is not a volume, and their bitmaps' CRCs.
fn read_groups(
    dev: &dyn BlockDevice,
    total: u64,
    lenient: bool,
) -> Result<(Vec<AllocationGroup>, Vec<Option<u32>>)> {
    let n = total.div_ceil(GROUP_BLOCKS);
    let last = total - (n - 1) * GROUP_BLOCKS;
    let mut buf = vec![0u8; BS as usize];
    let (mut groups, mut crcs) = (Vec::new(), Vec::new());
    for i in 0..n {
        let count = if i + 1 == n { last } else { GROUP_BLOCKS };
        let (g, c) = read_group(dev, i, count, &mut buf, lenient)?;
        groups.push(g);
        crcs.push(c);
    }
    Ok((groups, crcs))
}

/// Groups of a volume: one per chunk, holes where there is none.
fn read_volume_groups(
    dev: &dyn BlockDevice,
    map: &ChunkMap,
    lenient: bool,
) -> Result<(Vec<AllocationGroup>, Vec<Option<u32>>)> {
    let (mut groups, mut crcs) = (Vec::new(), Vec::new());
    let mut buf = vec![0u8; BS as usize];
    for (&g, c) in &map.chunks {
        while (groups.len() as u64) < g {
            groups.push(AllocationGroup::hole(groups.len() as u32));
            crcs.push(Some(0));
        }
        let (mut grp, crc) = read_group(dev, g, c.blocks, &mut buf, lenient)?;
        grp.kind = c.kind;
        grp.tier = map.chunk_tier(c);
        if c.profile.parity() > 0 {
            grp.set_row(c.data_cols() as u64);
        }
        groups.push(grp);
        crcs.push(crc);
    }
    Ok((groups, crcs))
}

/// Whether every block of `c` keeps a copy off the `missing` devices.
fn readable(c: &Chunk, missing: &[u32]) -> bool {
    let up = |s: &Stripe| !missing.contains(&s.devid);
    match c.profile {
        Profile::Raid0 => c.stripes.iter().all(up),
        Profile::Raid10 => c.stripes.chunks(2).all(|p| p.iter().any(up)),
        Profile::Raid5 | Profile::Raid6 => {
            c.stripes.iter().filter(|s| !up(s)).count() <= c.profile.parity()
        }
        _ => c.stripes.iter().any(up),
    }
}

impl CowFs {
    // -----------------------------------------------------------------------
    // Mount and transactions
    // -----------------------------------------------------------------------

    pub fn mount(raw: Arc<dyn BlockDevice>, read_only: bool) -> Result<Self> {
        Self::mount_devices(vec![raw], read_only, false)
    }

    /// Mount a filesystem from its devices (any order). A volume with
    /// devices missing mounts only `degraded`, and only while every chunk
    /// still has a copy of each block.
    pub fn mount_devices(
        devs: Vec<Arc<dyn BlockDevice>>,
        read_only: bool,
        degraded: bool,
    ) -> Result<Self> {
        let mut buf = vec![0u8; BS as usize];
        let mut sbs = Vec::new();
        for d in &devs {
            d.read_at(0, &mut buf)?;
            sbs.push(SuperV4::decode(&buf)?);
        }
        if sbs.is_empty() {
            return Err(Error::Invalid);
        }
        let (raw, vol, home): (Arc<dyn BlockDevice>, Option<Arc<Volume>>, SuperV4) =
            if sbs[0].vol.is_none() {
                if devs.len() != 1 {
                    return Err(Error::Invalid);
                }
                (devs[0].clone(), None, sbs.remove(0))
            } else {
                let uuid = sbs[0].uuid;
                let mut by_id: BTreeMap<u32, Arc<dyn BlockDevice>> = BTreeMap::new();
                for (d, sb) in devs.iter().zip(&sbs) {
                    let v = sb.vol.as_ref().ok_or(Error::Invalid)?;
                    if sb.uuid != uuid || by_id.insert(v.devid, d.clone()).is_some() {
                        return Err(Error::Invalid);
                    }
                }
                let newest = sbs.iter().map(|s| s.generation).max().unwrap_or(0);
                let stale: Vec<u32> = sbs
                    .iter()
                    .filter(|s| s.generation < newest)
                    .filter_map(|s| s.vol.as_ref().map(|v| v.devid))
                    .collect();
                let home = sbs
                    .into_iter()
                    .max_by_key(|s| s.generation)
                    .ok_or(Error::Invalid)?;
                let sys = home.vol.as_ref().ok_or(Error::Invalid)?.sys.clone();
                let mut map = ChunkMap::default();
                map.chunks.insert(0, sys);
                let vol = Arc::new(Volume::new(by_id, map, sb_for_device));
                vol.set_stale(stale);
                (vol.clone(), Some(vol), home)
            };
        if vol.is_none() && home.total_blocks > raw.size() / BS {
            return Err(Error::Corrupt("filesystem larger than device"));
        }
        let journal = JournalArea {
            start: home.journal_start,
            blocks: home.journal_blocks as u64,
        };
        let dev = Arc::new(Overlay::new(raw.clone()));
        let replayed = match journal.read(&*raw, &home.uuid, home.total_blocks)? {
            Some((seq, blocks)) => {
                debug!(
                    "eclipsefs: replaying transaction {} ({} blocks)",
                    seq,
                    blocks.len()
                );
                for (b, data) in blocks {
                    dev.insert(b, data);
                }
                true
            }
            None => false,
        };
        dev.read_at(0, &mut buf)?;
        let sb = SuperV4::decode(&buf)?;
        let mut missing = Vec::new();
        if let Some(v) = &vol {
            let vs = sb.vol.as_ref().ok_or(Error::Corrupt("volume superblock"))?;
            v.with_map(|m| {
                m.chunks.clear();
                m.chunks.insert(0, vs.sys.clone());
            });
            let mut mb = vec![0u8; vs.chunk_map_len as usize];
            dev.read_at(vs.chunk_map * BS, &mut mb)?;
            let mut map = ChunkMap::decode(&mb);
            // A bad copy: try the others (unless it came from the journal).
            for k in 0..vs.sys.profile.copies() {
                if map.is_ok() || dev.contains(vs.chunk_map) {
                    break;
                }
                if v.read_copy(vs.chunk_map * BS, &mut mb, k).is_ok() {
                    map = ChunkMap::decode(&mb);
                }
            }
            let map = map?;
            if map.chunks.get(&0) != Some(&vs.sys) {
                return Err(Error::Corrupt("system chunk"));
            }
            for id in v.present() {
                if !map.devices.contains_key(&id) {
                    // Added or removed by a transaction that did not
                    // commit, or removed since: not part of it now.
                    warn!("eclipsefs: device {} is not in the volume, ignored", id);
                    v.detach(id);
                }
            }
            missing = map
                .devices
                .keys()
                .copied()
                .filter(|&d| v.device(d).is_none())
                .collect();
            if !missing.is_empty() {
                if !degraded {
                    return Err(Error::Unsupported("devices missing (mount degraded)"));
                }
                if !map.chunks.values().all(|c| readable(c, &missing)) {
                    return Err(Error::Unsupported("too many devices missing"));
                }
                warn!("eclipsefs: mounted degraded, devices {:?} missing", missing);
            }
            v.with_map(|m| *m = map);
        }
        if replayed && !read_only {
            write_home(&*raw, &dev.take())?;
            raw.sync()?;
        }
        if vol.is_none() && sb.total_blocks > raw.size() / BS {
            return Err(Error::Corrupt("filesystem larger than device"));
        }
        // With checksums, a bitmap that does not even decode is repaired
        // below like any other bad one.
        let lenient = sb.bitmap_sums.is_some();
        let (groups, crcs) = match &vol {
            Some(v) => read_volume_groups(&*dev, &v.map(), lenient)?,
            None => read_groups(&*dev, sb.total_blocks, lenient)?,
        };
        let mut alloc = BlockAllocator::new(groups);
        alloc.set_deferred_frees(true);
        let mut st = Store::new(
            raw.clone(),
            sb.uuid,
            sb.generation + 1,
            sb.total_blocks,
            extent_of,
        );
        st.mirrors = vol.clone();
        st.repair = !read_only;
        let root_tree = sb.root;
        let item = |st: &mut Store, id: u64| -> Result<RootItem> {
            let d = st
                .get(&root_tree, &Key::new(id, ROOT_ITEM, 0))?
                .ok_or(Error::Corrupt("missing tree"))?;
            RootItem::decode(&d)
        };
        let refs_item = item(&mut st, REFS_TREE)?;
        let subvol = sb.default_subvol;
        let fs_item = item(&mut st, subvol)?;
        let refs_root = refs_item.root;
        let csum_root = item(&mut st, CSUM_TREE)?.root;
        let mut entries = Vec::new();
        for (k, d) in st.range(&refs_root, &Key::MIN, &Key::MAX, usize::MAX)? {
            if k.ty != REF_ITEM {
                return Err(Error::Corrupt("reference tree item"));
            }
            let (len, count) = decode_ref(&d)?;
            entries.push((k.objectid, len, count));
        }
        let refs = RefMap::from_entries(entries)?;
        let mut sp = BlockSpace::new(alloc, refs.clone());
        if let Some(vs) = &sb.vol {
            sp.vol = vol.clone();
            sp.profiles = (vs.data_profile, vs.meta_profile);
            sp.avoid = missing;
        }
        let mut fs = Self {
            dev,
            raw,
            vol,
            journal,
            sp,
            st,
            root_tree,
            refs_root,
            csum_root,
            refs_on_disk: refs,
            subvol,
            fs_root: fs_item.root,
            fs_item,
            subvols: BTreeMap::new(),
            default_subvol: subvol,
            read_only,
            nested: false,
            pending_ops: 0,
            clock: None,
            compress: None,
            zcache: None,
            zpending: BTreeMap::new(),
            bitmap_sums: Vec::new(),
            sums_dirty: BTreeSet::new(),
            sums_pending: false,
            keyring: Default::default(),
            random: None,
            log: Default::default(),
            commit_interval: None,
            last_commit: 0,
            sb,
        };
        fs.refresh_layouts();
        fs.load_bitmap_sums(crcs)?;
        let free = fs.sp.alloc.free_blocks();
        if fs.sb.free_blocks != free {
            debug!(
                "eclipsefs: superblock says {} free blocks, bitmaps {}",
                fs.sb.free_blocks, free
            );
            fs.sb.free_blocks = free;
        }
        fs.sp.reserve = metadata_reserve(fs.sb.total_blocks);
        let root_dir = fs.fs_item.root_dir;
        if fs.inode(root_dir)?.kind() != FileKind::Dir {
            return Err(Error::Corrupt("root is not a directory"));
        }
        let replayed = fs.replay_log()?;
        if replayed > 0 {
            warn!("eclipsefs: replayed {} fsync log records", replayed);
            if !read_only {
                fs.flush()?;
                fs.raw.sync()?;
            }
        }
        if !read_only {
            fs.resync_stale()?;
            fs.drop_unfinished_receives()?;
        }
        Ok(fs)
    }

    /// Forget the cached tree nodes and decompressed data that are clean
    /// (what `echo 3 > /proc/sys/vm/drop_caches` asks of Linux); the next
    /// reads come from the device.
    pub fn drop_caches(&mut self) {
        self.st.drop_clean();
        self.zcache = None;
    }

    pub fn set_clock(&mut self, clock: fn() -> (u64, u32)) {
        self.clock = Some(clock);
        self.last_commit = clock().0;
    }

    /// Commit every `secs` seconds of the clock (`None`: every
    /// [`COMMIT_OPS`] operations, the default) as ext4's and btrfs's
    /// `commit=` do. Operations then only commit early when the
    /// transaction grows large; whoever mounts calls
    /// [`commit_if_due`](Self::commit_if_due) from a timer so an idle
    /// filesystem commits too, and `fsync` uses the log in between.
    pub fn set_commit_interval(&mut self, secs: Option<u64>) {
        self.commit_interval = secs.map(|s| s.max(1));
    }

    pub fn commit_interval(&self) -> Option<u64> {
        self.commit_interval
    }

    /// Commit if the interval has passed since the last commit (timed
    /// commits) and anything changed. Returns whether it committed.
    pub fn commit_if_due(&mut self) -> Result<bool> {
        if self.read_only || self.nested || !self.commit_due() {
            return Ok(false);
        }
        let gen = self.sb.generation;
        self.flush()?;
        Ok(self.sb.generation != gen)
    }

    fn commit_due(&self) -> bool {
        match (self.commit_interval, self.clock) {
            (Some(secs), Some(c)) => c().0.saturating_sub(self.last_commit) >= secs,
            (Some(_), None) => self.pending_ops >= COMMIT_OPS,
            (None, _) => self.pending_ops >= COMMIT_OPS,
        }
    }

    pub fn label(&self) -> String {
        String::from_utf8_lossy(self.sb.label()).into_owned()
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.sb.uuid
    }

    pub fn root_ino(&self) -> u64 {
        handle(self.default_subvol, FIRST_INO)
    }

    pub fn fsinfo(&self) -> FsStat {
        let mut total = self.sp.alloc.total_blocks();
        if let Some(v) = &self.vol {
            // Room for more data chunks, at the data profile's cost.
            let map = v.map();
            let free: u64 = map
                .unallocated(&v.held())
                .iter()
                .filter(|(d, _)| !self.sp.avoid.contains(d))
                .map(|(_, n)| n)
                .sum();
            let devices = map
                .devices
                .keys()
                .filter(|d| !self.sp.avoid.contains(d))
                .count();
            let (raw, data) = self.sp.profiles.0.overhead(devices);
            total += free * data / raw;
        }
        FsStat {
            block_size: BS,
            total_bytes: total * BS,
            bytes_used: (total - self.sp.alloc.free_blocks() - self.sp.alloc.deferred_blocks())
                * BS,
        }
    }

    /// Generation of the last committed transaction.
    pub fn generation(&self) -> u64 {
        self.sb.generation
    }

    fn now(&self) -> (u64, u32) {
        self.clock.map(|c| c()).unwrap_or((0, 0))
    }

    /// Start of every mutating call.
    fn writable(&mut self) -> Result<()> {
        if self.read_only {
            return Err(Error::Unsupported("read-only filesystem"));
        }
        let deferred = self.sp.alloc.deferred_blocks();
        if deferred > 0
            && !self.nested
            && self.sp.alloc.free_blocks() < deferred.max(self.sp.alloc.total_blocks() / 16)
        {
            self.flush()?;
        }
        Ok(())
    }

    /// End of every mutating call.
    fn commit(&mut self) -> Result<()> {
        if self.nested {
            return Ok(());
        }
        self.pending_ops += 1;
        if self.commit_due()
            || self.st.dirty_count() >= DIRTY_NODES_LIMIT
            || self.sp.csums.len() >= PENDING_SUMS_LIMIT
        {
            self.flush()?;
        }
        Ok(())
    }

    /// Commit the open transaction.
    fn flush(&mut self) -> Result<()> {
        self.pending_ops = 0;
        self.compress_pending()?;
        let changed = self.st.dirty_count() > 0
            || self.sp.refs_dirty
            || self.sp.alloc.deferred_blocks() > 0
            || self.sp.alloc.groups().iter().any(|g| g.is_dirty())
            || !self.dev.is_empty()
            || self.sp.chunks_dirty
            || self.sums_pending
            || self.items_changed()?;
        if !changed {
            self.log_committed();
            if let Some(c) = self.clock {
                self.last_commit = c().0;
            }
            return Ok(());
        }
        // Data checksums into the checksum tree.
        self.store_csums()?;
        // Reference counts into the reference tree.
        if self.sp.refs_dirty {
            let old: BTreeMap<u64, (u64, u32)> = self
                .refs_on_disk
                .entries()
                .map(|(s, l, c)| (s, (l, c)))
                .collect();
            let new: BTreeMap<u64, (u64, u32)> = self
                .sp
                .refs
                .entries()
                .map(|(s, l, c)| (s, (l, c)))
                .collect();
            for (s, v) in &old {
                if new.get(s) != Some(v) {
                    self.st
                        .delete(&mut self.sp, &mut self.refs_root, Key::new(*s, REF_ITEM, 0))?;
                }
            }
            for (s, (l, c)) in &new {
                if old.get(s) != Some(&(*l, *c)) {
                    self.st.insert(
                        &mut self.sp,
                        &mut self.refs_root,
                        Key::new(*s, REF_ITEM, 0),
                        encode_ref(*l, *c),
                    )?;
                }
            }
            self.refs_on_disk = self.sp.refs.clone();
        }
        self.store_chunk_map()?;
        // Tree roots into the root tree.
        let gen = self.st.gen;
        self.fs_item.root = self.fs_root;
        let mut items = vec![(self.subvol, self.fs_item.clone())];
        for (&id, (root, item)) in self.subvols.iter_mut() {
            item.root = *root;
            items.push((id, item.clone()));
        }
        for (id, item) in items {
            let key = Key::new(id, ROOT_ITEM, 0);
            let v = item.encode();
            if self.st.get(&self.root_tree, &key)?.as_deref() != Some(&v[..]) {
                self.st.update(&mut self.sp, &mut self.root_tree, key, v)?;
            }
        }
        self.store_root(REFS_TREE, self.refs_root)?;
        self.store_root(CSUM_TREE, self.csum_root)?;
        // New nodes first, made durable before the superblock points at them.
        let nodes = self.st.take_dirty();
        write_home(&*self.raw, &nodes)?;
        self.raw.sync()?;
        // Superblock, bitmaps and their checksums, atomically through the
        // journal.
        self.place_bitmap_sums()?;
        self.sp.alloc.release_deferred()?;
        self.sp.committed();
        self.sb.root = self.root_tree;
        self.sb.generation = gen;
        self.sb.free_blocks = self.sp.alloc.free_blocks();
        for (i, g) in self.sp.alloc.groups_mut().iter_mut().enumerate() {
            if !g.is_dirty() {
                continue;
            }
            let bytes = g.to_bytes();
            self.dev.write_at(g.bitmap_block() * BS, &bytes)?;
            g.clear_dirty();
            let c = if g.is_hole() { 0 } else { crc32c(&bytes) };
            if self.bitmap_sums[i] != c {
                self.bitmap_sums[i] = c;
                self.sums_dirty.insert(i / SUMS_PER_BLOCK);
            }
        }
        self.write_bitmap_sums()?;
        self.dev.write_at(0, &self.sb.encode())?;
        let blocks = self.dev.take();
        let raw = self.raw.clone();
        let j = self.journal;
        let r = if j.fits(blocks.len()) {
            j.write(&*raw, &self.sb.uuid, gen, &blocks)
                .and_then(|_| raw.sync())
                .and_then(|_| write_home(&*raw, &blocks))
                .and_then(|_| raw.sync())
        } else {
            warn!(
                "eclipsefs: {} bitmap blocks exceed the journal, writing in place",
                blocks.len()
            );
            j.invalidate(&*raw)
                .and_then(|_| raw.sync())
                .and_then(|_| write_home(&*raw, &blocks))
                .and_then(|_| raw.sync())
        };
        if r.is_err() {
            for (b, data) in blocks {
                if !self.dev.contains(b) {
                    self.dev.insert(b, data);
                }
            }
        } else {
            if let Some(v) = &self.vol {
                // Chunks moved away from are no longer the committed state.
                v.release();
            }
            self.log_committed();
            if let Some(c) = self.clock {
                self.last_commit = c().0;
            }
        }
        r
    }

    /// The chunk map into group 0 (through the journal, with the
    /// superblock that points at it).
    fn store_chunk_map(&mut self) -> Result<()> {
        let Some(vol) = self.vol.clone() else {
            return Ok(());
        };
        let end = self.sp.alloc.end();
        self.sb.total_blocks = end;
        self.st.total_blocks = end;
        if !self.sp.chunks_dirty {
            return Ok(());
        }
        let map = vol.map();
        let mut bytes = map.encode();
        let need = (bytes.len() as u64).div_ceil(BS);
        let (old, old_n) = {
            let vs = self
                .sb
                .vol
                .as_ref()
                .ok_or(Error::Corrupt("volume superblock"))?;
            (vs.chunk_map, (vs.chunk_map_len as u64).div_ceil(BS))
        };
        let mut at = old;
        if need != old_n {
            at = self.sp.alloc.groups_mut()[0]
                .allocate_run(2, need)
                .ok_or(Error::NoSpace)?;
            if old_n > 0 {
                self.sp.alloc.free(old, old_n)?;
            }
        }
        let len = bytes.len() as u32;
        bytes.resize((need * BS) as usize, 0);
        self.dev.write_at(at * BS, &bytes)?;
        let vs = self
            .sb
            .vol
            .as_mut()
            .ok_or(Error::Corrupt("volume superblock"))?;
        vs.chunk_map = at;
        vs.chunk_map_len = len;
        vs.num_devices = map.devices.len() as u32;
        vs.sys = map
            .chunks
            .get(&0)
            .ok_or(Error::Corrupt("system chunk"))?
            .clone();
        vs.data_profile = self.sp.profiles.0;
        vs.meta_profile = self.sp.profiles.1;
        // Older drivers know neither parity profiles nor tiers.
        let parity = map.chunks.values().any(|c| c.profile.parity() > 0)
            || self.sp.profiles.0.parity() > 0
            || self.sp.profiles.1.parity() > 0;
        for (on, bit) in [(parity, FEAT_RAID56), (!map.tiers.is_empty(), FEAT_TIERS)] {
            if on {
                self.sb.features |= bit;
            } else {
                self.sb.features &= !bit;
            }
        }
        self.sp.chunks_dirty = false;
        Ok(())
    }

    /// Whether a subvolume's item in memory (flags, quota counter...)
    /// differs from the root tree's.
    fn items_changed(&mut self) -> Result<bool> {
        let mut items = vec![(self.subvol, self.fs_item.clone())];
        items.extend(self.subvols.iter().map(|(&id, (_, it))| (id, it.clone())));
        for (id, it) in items {
            let on_disk = self.st.get(&self.root_tree, &Key::new(id, ROOT_ITEM, 0))?;
            if on_disk.map(|d| RootItem::decode(&d)).transpose()?.as_ref() != Some(&it) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Point the root tree's item for tree `id` at `root` if it moved.
    fn store_root(&mut self, id: u64, root: Root) -> Result<()> {
        let key = Key::new(id, ROOT_ITEM, 0);
        let mut it = RootItem::decode(
            &self
                .st
                .get(&self.root_tree, &key)?
                .ok_or(Error::Corrupt("missing tree"))?,
        )?;
        if it.root != root {
            it.root = root;
            self.st
                .update(&mut self.sp, &mut self.root_tree, key, it.encode())?;
        }
        Ok(())
    }

    /// Move the pending checksum changes into the checksum tree, one item
    /// per bucket of blocks: first the frees, then what was written after.
    ///
    /// Neither list is copied out whole: a transaction that deleted a file
    /// of tens of GB holds millions of freed blocks, and the copy alone was
    /// a contiguous allocation of tens of MiB.
    fn store_csums(&mut self) -> Result<()> {
        let freed = core::mem::take(&mut self.sp.csums_freed);
        for (start, len) in freed.iter() {
            let end = start + len;
            let mut bucket = start / CSUM_BUCKET * CSUM_BUCKET;
            while bucket < end {
                let from = start.max(bucket);
                let to = end.min(bucket + CSUM_BUCKET);
                self.edit_csum_bucket(bucket, |cb| {
                    for b in from..to {
                        cb.0[(b - bucket) as usize] = None;
                    }
                })?;
                bucket += CSUM_BUCKET;
            }
        }
        while let Some(&first) = self.sp.csums.keys().next() {
            let bucket = first / CSUM_BUCKET * CSUM_BUCKET;
            let pending: Vec<(u64, Option<u32>)> = self
                .sp
                .csums
                .range(bucket..bucket + CSUM_BUCKET)
                .map(|(&b, &c)| (b, c))
                .collect();
            self.edit_csum_bucket(bucket, |cb| {
                for &(b, c) in &pending {
                    cb.0[(b - bucket) as usize] = c;
                }
            })?;
            for (b, _) in pending {
                self.sp.csums.remove(&b);
            }
        }
        Ok(())
    }

    /// Change the checksum item of the bucket starting at disk block
    /// `bucket` (deleting it once empty).
    fn edit_csum_bucket(&mut self, bucket: u64, f: impl FnOnce(&mut CsumBucket)) -> Result<()> {
        let key = Key::new(bucket, CSUM_ITEM, 0);
        let old = self.st.get(&self.csum_root, &key)?;
        let mut cb = match &old {
            Some(d) => CsumBucket::decode(d)?,
            None => CsumBucket::default(),
        };
        f(&mut cb);
        if cb.is_empty() {
            if old.is_some() {
                self.st.delete(&mut self.sp, &mut self.csum_root, key)?;
            }
        } else {
            self.st
                .upsert(&mut self.sp, &mut self.csum_root, key, cb.encode())?;
        }
        Ok(())
    }

    /// Checksums of disk blocks `start..start+n` (`None`: none recorded).
    fn csums(&mut self, start: u64, n: u64) -> Result<Vec<Option<u32>>> {
        let mut out = vec![None; n as usize];
        if n == 0 {
            return Ok(out);
        }
        let mut bucket = start / CSUM_BUCKET * CSUM_BUCKET;
        while bucket < start + n {
            if let Some(d) = self
                .st
                .get(&self.csum_root, &Key::new(bucket, CSUM_ITEM, 0))?
            {
                let from = start.max(bucket);
                let to = (start + n).min(bucket + CSUM_BUCKET);
                for b in from..to {
                    out[(b - start) as usize] = CsumBucket::lookup(&d, (b - bucket) as usize)?;
                }
            }
            bucket += CSUM_BUCKET;
        }
        for (s, l) in self.sp.csums_freed.within(start, start + n) {
            for b in s..s + l {
                out[(b - start) as usize] = None;
            }
        }
        for (&b, &c) in self.sp.csums.range(start..start + n) {
            out[(b - start) as usize] = c;
        }
        Ok(out)
    }

    /// Check whole blocks read from disk block `pblk` on.
    fn verify(&mut self, pblk: u64, data: &[u8]) -> Result<()> {
        let n = data.len() as u64 / BS;
        for (i, c) in self.csums(pblk, n)?.into_iter().enumerate() {
            let block = &data[i * BS as usize..(i + 1) * BS as usize];
            match c {
                Some(c) if crc32c(block) == c => {}
                Some(_) => {
                    warn!(
                        "eclipsefs: data checksum mismatch at block {}",
                        pblk + i as u64
                    );
                    return Err(Error::Corrupt("data checksum"));
                }
                None => return Err(Error::Corrupt("missing data checksum")),
            }
        }
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        if !self.read_only {
            self.flush()?;
        }
        self.raw.sync()
    }

    /// Take in what the device(s) grew by, unless a resize fixed the size.
    pub fn grow_to_device(&mut self) -> Result<bool> {
        self.writable()?;
        if self.sb.features & FEAT_FIXED_SIZE != 0 {
            return Ok(false);
        }
        if let Some(v) = self.vol.clone() {
            return self.grow_volume(&v);
        }
        let changed = self.grow_single(self.raw.size() / BS)?;
        if changed {
            self.flush()?;
        }
        Ok(changed)
    }

    /// Extend the logical space of a filesystem that is not a volume to
    /// `new_total` blocks (left uncommitted).
    fn grow_single(&mut self, new_total: u64) -> Result<bool> {
        let old_total = self.sb.total_blocks;
        if new_total <= old_total {
            return Ok(false);
        }
        let mut total = old_total;
        let first_new = self.sp.alloc.groups().len();
        {
            let groups = self.sp.alloc.groups_mut();
            let last = groups.last_mut().ok_or(Error::Corrupt("no groups"))?;
            let add = (GROUP_BLOCKS - last.block_count).min(new_total - total);
            if add > 0 {
                last.extend(last.block_count + add);
                total += add;
            }
            while total < new_total {
                let count = (new_total - total).min(GROUP_BLOCKS);
                if count < MIN_GROUP_BLOCKS {
                    break;
                }
                groups.push(AllocationGroup::new(groups.len() as u32, count));
                total += count;
            }
        }
        if total == old_total {
            return Ok(false);
        }
        self.sb.total_blocks = total;
        self.st.total_blocks = total;
        self.sp.reserve = metadata_reserve(total);
        let backup = self.sb.encode();
        for gi in first_new..self.sp.alloc.groups().len() {
            self.dev.write_at(gi as u64 * GROUP_BLOCKS * BS, &backup)?;
        }
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Items
    // -----------------------------------------------------------------------

    fn get(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        self.st.get(&self.fs_root, &key)
    }

    fn insert(&mut self, key: Key, v: Vec<u8>) -> Result<()> {
        self.log_item(&key);
        self.st.insert(&mut self.sp, &mut self.fs_root, key, v)
    }

    fn upsert(&mut self, key: Key, v: Vec<u8>) -> Result<()> {
        self.log_item(&key);
        self.st.upsert(&mut self.sp, &mut self.fs_root, key, v)
    }

    fn delete(&mut self, key: Key) -> Result<Vec<u8>> {
        self.log_item(&key);
        self.st.delete(&mut self.sp, &mut self.fs_root, key)
    }

    fn inode(&mut self, ino: u64) -> Result<InodeItem> {
        if ino < FIRST_INO || ino >= self.fs_item.next_ino {
            return Err(Error::NotFound);
        }
        let d = self
            .get(Key::new(ino, INODE_ITEM, 0))?
            .ok_or(Error::NotFound)?;
        InodeItem::decode(&d)
    }

    fn put_inode(&mut self, ino: u64, mut i: InodeItem) -> Result<()> {
        self.log_attr(ino);
        i.seq += 1;
        let nbytes = i.nbytes;
        let old = self.st.modify(
            &mut self.sp,
            &mut self.fs_root,
            Key::new(ino, INODE_ITEM, 0),
            Op::Update(i.encode()),
        )?;
        if self.quota_on() {
            let was = InodeItem::decode(&old.ok_or(Error::NotFound)?)?.nbytes;
            let r = &mut self.fs_item.referenced;
            *r = r.saturating_add(nbytes).saturating_sub(was);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // File extents
    // -----------------------------------------------------------------------

    fn ext_key(ino: u64, lblk: u64) -> Key {
        Key::new(ino, EXTENT_DATA, lblk * BS)
    }

    /// The inline extent, if the file is stored inside its item.
    fn inline_data(&mut self, ino: u64) -> Result<Option<Vec<u8>>> {
        match self.get(Self::ext_key(ino, 0))? {
            Some(d) => match FileExtent::decode(&d)? {
                FileExtent::Inline(v) => Ok(Some(v)),
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Regular extents overlapping file blocks `from..to`.
    ///
    /// Only for ranges known to be short (a write chunk, a block or two):
    /// the whole list is held at once. Anything that may span a whole file
    /// goes through [`extents_upto`](Self::extents_upto) a batch at a time.
    fn extents(&mut self, ino: u64, from: u64, to: u64) -> Result<Vec<Ext>> {
        self.extents_upto(ino, from, to, usize::MAX)
    }

    /// The first extents overlapping file blocks `from..to`, in order: the
    /// one that starts before `from` (if it reaches it) and at most `limit`
    /// after it. Empty only when nothing in `from..to` is mapped.
    ///
    /// Callers walk a large range by asking again from the end of the last
    /// extent returned. A file of tens of GB has over a million extent
    /// items, 48 bytes each plus their data, so reading them all at once is
    /// a contiguous allocation of tens of MiB on the kernel heap. A real
    /// machine died on `memory allocation of 53785440 bytes failed`
    /// handling an 88 GB file.
    fn extents_upto(&mut self, ino: u64, from: u64, to: u64, limit: usize) -> Result<Vec<Ext>> {
        let mut out = Vec::new();
        if from >= to {
            return Ok(out);
        }
        let first = self.st.seek_le(&self.fs_root, &Self::ext_key(ino, from))?;
        let first = first.filter(|(k, _)| k.objectid == ino && k.ty == EXTENT_DATA);
        let lo = Key::new(ino, EXTENT_DATA, from * BS + 1);
        let hi = Key::new(ino, EXTENT_DATA, to * BS - 1);
        let rest = self.st.range(&self.fs_root, &lo, &hi, limit)?;
        for (k, d) in first.into_iter().chain(rest) {
            let (disk, blocks, prealloc, z) = match FileExtent::decode(&d)? {
                FileExtent::Reg { disk, blocks } => (disk, blocks, false, None),
                FileExtent::Prealloc { disk, blocks } => (disk, blocks, true, None),
                FileExtent::Comp(c) => (
                    c.disk,
                    c.blocks,
                    false,
                    Some(Zext {
                        algo: c.algo,
                        disk_blocks: c.disk_blocks,
                        ram_blocks: c.ram_blocks,
                        offset: c.offset,
                    }),
                ),
                FileExtent::Inline(_) => continue,
            };
            {
                if k.offset % BS != 0 {
                    return Err(Error::Corrupt("unaligned file extent"));
                }
                let e = Ext {
                    lblk: k.offset / BS,
                    pblk: disk,
                    blocks,
                    prealloc,
                    z,
                };
                if e.lblk + e.blocks > from && e.lblk < to {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// Point file blocks `lblk..lblk+n` at disk blocks `pblk..`, dropping
    /// (and releasing) whatever mapped them before.
    fn set_mapping(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        lblk: u64,
        n: u64,
        pblk: u64,
    ) -> Result<()> {
        self.map(ino, inode, lblk, n, pblk, false)
    }

    /// [`set_mapping`](Self::set_mapping) for a regular or a preallocated
    /// extent.
    fn map(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        lblk: u64,
        n: u64,
        pblk: u64,
        prealloc: bool,
    ) -> Result<()> {
        self.unmap(ino, inode, lblk, lblk + n)?;
        // Blocks of another owner (a reflink) are more than the log
        // follows.
        if !self.sp.private_fresh(pblk, n) {
            self.log_full(ino);
        }
        let was = core::mem::replace(&mut self.log.in_extents, true);
        let r = self.map_new(ino, inode, lblk, n, pblk, prealloc);
        self.log.in_extents = was;
        r
    }

    /// The item part of [`map`](Self::map), once `lblk..lblk+n` is unmapped.
    fn map_new(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        lblk: u64,
        n: u64,
        pblk: u64,
        prealloc: bool,
    ) -> Result<()> {
        // Extend the previous extent when the new blocks continue it.
        if lblk > 0 {
            if let Some(prev) = self.extents(ino, lblk - 1, lblk)?.first().copied() {
                if prev.z.is_none()
                    && prev.lblk + prev.blocks == lblk
                    && prev.pblk + prev.blocks == pblk
                    && prev.prealloc == prealloc
                    && prev.blocks + n <= MAX_EXTENT_BLOCKS
                {
                    self.upsert(
                        Self::ext_key(ino, prev.lblk),
                        Ext::item(prev.pblk, prev.blocks + n, prealloc),
                    )?;
                    inode.nbytes += n * BS;
                    return Ok(());
                }
            }
        }
        self.insert(Self::ext_key(ino, lblk), Ext::item(pblk, n, prealloc))?;
        inode.nbytes += n * BS;
        Ok(())
    }

    /// Remove the mapping of file blocks `from..to`.
    ///
    /// A batch of [`EXTENT_BATCH`] extents at a time: truncating a large
    /// file walks every extent it has. What a batch handles is gone from
    /// `from..` (or cut to end at `from`) when the next one is read.
    fn unmap(&mut self, ino: u64, inode: &mut InodeItem, from: u64, to: u64) -> Result<()> {
        self.log_range(ino, from, to);
        let was = core::mem::replace(&mut self.log.in_extents, true);
        let r = (|| {
            let mut cur = from;
            while cur < to {
                let batch = self.extents_upto(ino, cur, to, EXTENT_BATCH)?;
                let Some(last) = batch.last() else {
                    break;
                };
                cur = last.lblk + last.blocks;
                self.unmap_batch(ino, inode, from, to, batch)?;
            }
            Ok(())
        })();
        self.log.in_extents = was;
        r
    }

    fn unmap_batch(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        from: u64,
        to: u64,
        batch: Vec<Ext>,
    ) -> Result<()> {
        for e in batch {
            let key = Self::ext_key(ino, e.lblk);
            let end = e.lblk + e.blocks;
            if e.lblk < from {
                self.upsert(key, e.piece(e.lblk, from))?;
            } else {
                self.delete(key)?;
            }
            if end > to {
                self.insert(Self::ext_key(ino, to), e.piece(to, end))?;
            }
            let s = e.lblk.max(from);
            let t = end.min(to);
            match e.z {
                None => self.sp_decref(e.pblk + (s - e.lblk), t - s)?,
                // Every piece of a compressed extent owns all its blocks.
                Some(z) => match (e.lblk < from, end > to) {
                    (false, false) => self.sp_decref(e.pblk, z.disk_blocks)?,
                    (true, true) => {
                        crate::btree::Space::incref(&mut self.sp, e.pblk, z.disk_blocks)?
                    }
                    _ => {}
                },
            }
            inode.nbytes = inode.nbytes.saturating_sub((t - s) * BS);
        }
        Ok(())
    }

    /// A goal for the disk blocks of file block `lb`: right after those of
    /// the block before it.
    fn goal(&mut self, ino: u64, lb: u64) -> Result<Option<u64>> {
        if lb == 0 {
            return Ok(None);
        }
        Ok(self
            .extents(ino, lb - 1, lb)?
            .first()
            .map(|e| e.next_disk(lb)))
    }

    /// Read disk blocks from `pblk` on into `buf`, verifying them against
    /// their checksums if `sums`; on a volume a block that fails comes
    /// from another copy, and the bad copies are rewritten.
    fn read_data(&mut self, pblk: u64, buf: &mut [u8], sums: bool) -> Result<()> {
        self.raw.read_at(pblk * BS, buf)?;
        if !sums {
            return Ok(());
        }
        let err = match self.verify(pblk, buf) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let Some(vol) = self.vol.clone() else {
            return Err(err);
        };
        let want = self.csums(pblk, buf.len() as u64 / BS)?;
        for (i, c) in want.into_iter().enumerate() {
            let blk = &mut buf[i * BS as usize..(i + 1) * BS as usize];
            let Some(c) = c else { continue };
            if crc32c(blk) == c {
                continue;
            }
            let b = pblk + i as u64;
            let mut good = None;
            let mut bad = Vec::new();
            let mut img = vec![0u8; BS as usize];
            for k in 0..vol.copies(b) {
                let ok = vol.read_copy(b * BS, &mut img, k).is_ok() && crc32c(&img) == c;
                if !ok {
                    bad.push(k);
                } else if good.is_none() {
                    good = Some(img.clone());
                }
            }
            let good = good.ok_or(err)?;
            blk.copy_from_slice(&good);
            if !self.read_only {
                for k in bad {
                    let _ = vol.write_copy(b * BS, &good, k);
                }
            }
        }
        Ok(())
    }

    /// Everything the compressed extent at disk block `pblk` holds.
    fn unpacked(&mut self, pblk: u64, z: Zext) -> Result<&[u8]> {
        if !matches!(&self.zcache, Some((p, _)) if *p == pblk) {
            let mut disk = vec![0u8; (z.disk_blocks * BS) as usize];
            self.read_data(pblk, &mut disk, true)?;
            let ram = crate::compress::unpack(z.algo, &disk, (z.ram_blocks * BS) as usize)?;
            self.zcache = Some((pblk, ram));
        }
        match &self.zcache {
            Some((_, ram)) if ram.len() as u64 == z.ram_blocks * BS => Ok(ram),
            _ => Err(Error::Corrupt("compressed extent")),
        }
    }

    /// What new data of `inode` is compressed with, if anything.
    fn compression_for(&self, inode: &InodeItem) -> Option<Compression> {
        let f = inode.flags as u32;
        if f & (FL_NOCOW | FL_NOCOMP | FL_ENCRYPT) != 0 {
            return None;
        }
        inode_compression(inode)
            .or(self.compress)
            .or((f & FL_COMPR != 0).then_some(Compression::Zlib))
    }

    fn sp_decref(&mut self, start: u64, len: u64) -> Result<()> {
        crate::btree::Space::decref_data(&mut self.sp, start, len)
    }

    /// Read file bytes `off..off+buf.len()` (no size check; holes are 0),
    /// verifying the checksum of every block read.
    fn read_blocks(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        // No-COW files have no checksums (as on btrfs).
        let i = self.inode(ino)?;
        let sums = i.flags & FL_NOCOW as u64 == 0;
        let keys = self.file_keys(ino, &i)?;
        self.read_blocks_with(ino, sums, keys.as_deref(), off, buf)
    }

    /// [`read_blocks`](Self::read_blocks) for a caller that already has the
    /// inode: `sums` says whether its blocks carry checksums, `keys` decrypt
    /// them.
    fn read_blocks_with(
        &mut self,
        ino: u64,
        sums: bool,
        keys: Option<&crate::crypt::FileKeys>,
        off: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = off + buf.len() as u64;
        // Extents come in file order and do not overlap, so only the gaps
        // between them (holes, preallocated space) are zeroed, not the
        // whole buffer before it is read over.
        let mut zeroed_to = off;
        for e in self.extents(ino, off / BS, end.div_ceil(BS))? {
            if e.prealloc {
                continue;
            }
            let s = (e.lblk * BS).max(off);
            let t = ((e.lblk + e.blocks) * BS).min(end);
            if s >= t {
                continue;
            }
            if s > zeroed_to {
                buf[(zeroed_to - off) as usize..(s - off) as usize].fill(0);
            }
            zeroed_to = zeroed_to.max(t);
            if let Some(z) = e.z {
                let ram = self.unpacked(e.pblk, z)?;
                let at = (z.offset * BS + (s - e.lblk * BS)) as usize;
                buf[(s - off) as usize..(t - off) as usize]
                    .copy_from_slice(&ram[at..at + (t - s) as usize]);
                continue;
            }
            let mut pos = s;
            while pos < t {
                // Whole blocks, a bounded number at a time.
                let b0 = pos / BS;
                let b1 = t.div_ceil(BS).min(b0 + WRITE_CHUNK);
                let upto = (b1 * BS).min(t);
                let p0 = e.pblk + (b0 - e.lblk);
                let dst = &mut buf[(pos - off) as usize..(upto - off) as usize];
                if pos.is_multiple_of(BS) && upto.is_multiple_of(BS) {
                    self.read_data(p0, dst, sums)?;
                    if let Some(k) = keys {
                        k.decrypt_blocks(b0, dst, BS as usize);
                    }
                } else {
                    let mut tmp = vec![0u8; ((b1 - b0) * BS) as usize];
                    self.read_data(p0, &mut tmp, sums)?;
                    if let Some(k) = keys {
                        k.decrypt_blocks(b0, &mut tmp, BS as usize);
                    }
                    let o = (pos - b0 * BS) as usize;
                    let len = dst.len();
                    dst.copy_from_slice(&tmp[o..o + len]);
                }
                pos = upto;
            }
        }
        if zeroed_to < end {
            buf[(zeroed_to - off) as usize..].fill(0);
        }
        Ok(())
    }

    /// The new contents of file blocks `lb..lb+got` for a write of `data`
    /// at `off`: the data where it covers them, the old contents (or zeros)
    /// of the partly written blocks at the edges.
    fn fill_blocks(
        &mut self,
        ino: u64,
        lb: u64,
        got: u64,
        off: u64,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let end = off + data.len() as u64;
        let bstart = lb * BS;
        let bend = (lb + got) * BS;
        let mut buf = vec![0u8; (got * BS) as usize];
        let s = off.max(bstart);
        let t = end.min(bend);
        if s > bstart {
            let mut head = vec![0u8; BS as usize];
            self.read_blocks(ino, bstart, &mut head)?;
            buf[..BS as usize].copy_from_slice(&head);
        }
        if t < bend {
            let at = bend - BS;
            if at >= bstart + if s > bstart { BS } else { 0 } || got == 1 && s == bstart {
                let mut tail = vec![0u8; BS as usize];
                self.read_blocks(ino, at, &mut tail)?;
                let o = (at - bstart) as usize;
                buf[o..o + BS as usize].copy_from_slice(&tail);
            }
        }
        buf[(s - bstart) as usize..(t - bstart) as usize]
            .copy_from_slice(&data[(s - off) as usize..(t - off) as usize]);
        Ok(buf)
    }

    /// Write `data` at `off` into regular (non-inline) storage, copy on
    /// write. Returns bytes written (short only when the device is full).
    fn write_extents(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        off: u64,
        data: &[u8],
    ) -> Result<usize> {
        // Blocks freed before may be rewritten below.
        self.zcache = None;
        match self.compression_for(inode) {
            Some(algo) => self.write_compressed(ino, inode, off, data, algo),
            None => self.write_plain(ino, inode, off, data),
        }
    }

    /// [`write_extents`](Self::write_extents) without compression.
    fn write_plain(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        off: u64,
        data: &[u8],
    ) -> Result<usize> {
        let end = off + data.len() as u64;
        let mut lb = off / BS;
        let last = end.div_ceil(BS);
        let mut written_to = off;
        // No-COW files rewrite their own unshared blocks in place and keep
        // no checksums (btrfs's nodatacow).
        let nocow = inode.flags & FL_NOCOW as u64 != 0;
        let keys = self.file_keys(ino, inode)?;
        while lb < last {
            let chunk_end = (lb + WRITE_CHUNK).min(last);
            let exts = self.extents(ino, lb, chunk_end)?;
            // The extent covering `lb`, if any.
            let cur = exts
                .iter()
                .find(|e| e.lblk <= lb && lb < e.lblk + e.blocks)
                .copied();
            let (run_end, in_place) = match cur {
                Some(e) => {
                    let run_end = (e.lblk + e.blocks).min(chunk_end);
                    let p = e.pblk + (lb - e.lblk);
                    let n = run_end - lb;
                    let mine = !e.prealloc
                        && e.z.is_none()
                        && (self.sp.private_fresh(p, n)
                            || nocow
                                && (p..p + n).all(|b| self.sp.refs.get(b) == 1)
                                && !self.st.path_shared(
                                    &self.sp,
                                    &self.fs_root,
                                    &Self::ext_key(ino, e.lblk),
                                )?);
                    (run_end, mine.then_some(p))
                }
                None => {
                    // A hole up to the next extent.
                    let next = exts.iter().map(|e| e.lblk).filter(|&s| s > lb).min();
                    (next.unwrap_or(chunk_end).min(chunk_end), None)
                }
            };
            let n = run_end - lb;
            let (pblk, got) = match in_place {
                Some(p) => (p, n),
                None => {
                    // New blocks, next to the file's previous ones.
                    let goal = self.goal(ino, lb)?;
                    match self.sp.alloc_data(goal, n) {
                        Some(r) => r,
                        None => break,
                    }
                }
            };
            // Contents of the blocks: data where the write covers them, the
            // old contents (or zeros) at the edges. If the old contents
            // cannot be read, the new blocks go back.
            let mut buf = match self.fill_blocks(ino, lb, got, off, data) {
                Ok(b) => b,
                Err(e) => {
                    if in_place.is_none() {
                        self.sp_decref(pblk, got)?;
                    }
                    return Err(e);
                }
            };
            let t = end.min((lb + got) * BS);
            if let Some(k) = &keys {
                k.encrypt_blocks(lb, &mut buf, BS as usize);
            }
            self.raw.write_at(pblk * BS, &buf)?;
            for (i, block) in buf.chunks(BS as usize).enumerate() {
                let c = (!nocow).then(|| crc32c(block));
                self.sp.csums.insert(pblk + i as u64, c);
            }
            if in_place.is_none() {
                self.set_mapping(ino, inode, lb, got, pblk)?;
            }
            written_to = t;
            lb += got;
        }
        if written_to == off {
            return Err(Error::NoSpace);
        }
        if written_to > inode.size {
            inode.size = written_to;
        }
        Ok((written_to - off) as usize)
    }

    /// [`write_extents`](Self::write_extents) for a file whose data is
    /// compressed. Runs of [`ZCHUNK`] whole blocks become compressed
    /// extents right away; smaller pieces are written as they are and
    /// compressed together when the transaction commits
    /// ([`compress_pending`](Self::compress_pending)), so a file written a
    /// little at a time still ends up in large compressed extents.
    fn write_compressed(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        off: u64,
        data: &[u8],
        algo: Compression,
    ) -> Result<usize> {
        let end = off + data.len() as u64;
        let first = off.div_ceil(BS);
        let mut pos = off;
        if end / BS >= first + ZCHUNK {
            if first * BS > off {
                let head = &data[..(first * BS - off) as usize];
                let n = self.write_plain(ino, inode, off, head)?;
                self.pend(ino, off, n);
                if n < head.len() {
                    return Ok(n);
                }
            }
            let mut lb = first;
            while end / BS >= lb + ZCHUNK {
                let at = (lb * BS - off) as usize;
                let chunk = &data[at..at + (ZCHUNK * BS) as usize];
                if !self.write_packed(ino, inode, lb, chunk, algo)? {
                    // Incompressible: as it is.
                    let n = self.write_plain(ino, inode, lb * BS, chunk).or_else(|e| {
                        if e == Error::NoSpace {
                            Ok(0)
                        } else {
                            Err(e)
                        }
                    })?;
                    if n < chunk.len() {
                        return Ok(at + n);
                    }
                }
                lb += ZCHUNK;
            }
            pos = lb * BS;
        }
        if pos < end {
            let rest = &data[(pos - off) as usize..];
            let n = match self.write_plain(ino, inode, pos, rest) {
                Ok(n) => n,
                Err(Error::NoSpace) if pos > off => 0,
                Err(e) => return Err(e),
            };
            self.pend(ino, pos, n);
            pos += n as u64;
        }
        if pos > inode.size {
            inode.size = pos;
        }
        Ok((pos - off) as usize)
    }

    /// Remember file bytes `off..off+n` of `ino` for compression at commit.
    fn pend(&mut self, ino: u64, off: u64, n: usize) {
        if n > 0 {
            let s = off / BS;
            let t = (off + n as u64).div_ceil(BS);
            self.zpending
                .entry((self.subvol, ino))
                .or_default()
                .insert(s, t - s);
        }
    }

    /// Store `data` (whole blocks) at file block `lb` as one compressed
    /// extent; false when it does not compress or finds no room.
    fn write_packed(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        lb: u64,
        data: &[u8],
        algo: Compression,
    ) -> Result<bool> {
        let Some(p) = crate::compress::pack(algo, data) else {
            return Ok(false);
        };
        let need = p.len() as u64 / BS;
        let goal = self.goal(ino, lb)?;
        let Some(pb) = self.sp.alloc_data_run(goal, need) else {
            return Ok(false);
        };
        self.zcache = None;
        self.raw.write_at(pb * BS, &p)?;
        for (i, block) in p.chunks(BS as usize).enumerate() {
            self.sp.csums.insert(pb + i as u64, Some(crc32c(block)));
        }
        let n = data.len() as u64 / BS;
        self.unmap(ino, inode, lb, lb + n)?;
        let item = FileExtent::Comp(Comp {
            algo,
            disk: pb,
            disk_blocks: need,
            ram_blocks: n,
            offset: 0,
            blocks: n,
        });
        self.insert(Self::ext_key(ino, lb), item.encode())?;
        inode.nbytes += n * BS;
        inode.size = inode.size.max((lb + n) * BS);
        Ok(true)
    }

    /// Compress what small writes left uncompressed in this transaction:
    /// runs of blocks still mapped by this transaction's own unshared
    /// blocks, [`ZCHUNK`] at a time.
    fn compress_pending(&mut self) -> Result<()> {
        if self.zpending.is_empty() || self.read_only {
            return Ok(());
        }
        let pending = core::mem::take(&mut self.zpending);
        let cur = self.subvol;
        for ((sv, ino), ranges) in pending {
            self.enter(sv)?;
            let Ok(mut inode) = self.inode(ino) else {
                continue;
            };
            let Some(algo) = self.compression_for(&inode) else {
                continue;
            };
            let before = inode.clone();
            let blocks = inode.size.div_ceil(BS);
            for (s, l) in ranges.iter() {
                // Up to the file's chunk grid, then whole chunks, so that
                // appends line up with the extents before them.
                let mut lb = s;
                while lb < (s + l).min(blocks) {
                    let next = ((lb / ZCHUNK + 1) * ZCHUNK).min(blocks);
                    if next - lb >= 2 {
                        self.compact(ino, &mut inode, lb, next - lb, algo)?;
                    }
                    lb = next;
                }
            }
            if inode != before {
                self.put_inode(ino, inode)?;
            }
        }
        self.enter(cur)
    }

    /// Rewrite file blocks `lb..lb+n` as one compressed extent if they are
    /// all mapped by unshared blocks of this transaction (or compressed
    /// extents it wrote) and compress.
    fn compact(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        lb: u64,
        n: u64,
        algo: Compression,
    ) -> Result<()> {
        let mut covered = 0;
        let mut plain = false;
        for e in self.extents(ino, lb, lb + n)? {
            if e.prealloc {
                return Ok(());
            }
            let s = e.lblk.max(lb);
            let t = (e.lblk + e.blocks).min(lb + n);
            let fresh = match e.z {
                None => {
                    plain = true;
                    self.sp.private_fresh(e.pblk + (s - e.lblk), t - s)
                }
                Some(z) => self.sp.private_fresh(e.pblk, z.disk_blocks),
            };
            if !fresh {
                return Ok(());
            }
            covered += t - s;
        }
        if covered != n || !plain {
            return Ok(());
        }
        let mut buf = vec![0u8; (n * BS) as usize];
        self.read_blocks(ino, lb * BS, &mut buf)?;
        let size = inode.size;
        self.write_packed(ino, inode, lb, &buf, algo)?;
        inode.size = size;
        Ok(())
    }

    /// Move an inline file into a regular block.
    fn uninline(&mut self, ino: u64, inode: &mut InodeItem) -> Result<()> {
        let Some(d) = self.inline_data(ino)? else {
            return Ok(());
        };
        self.delete(Self::ext_key(ino, 0))?;
        inode.nbytes = 0;
        if !d.is_empty() {
            let size = inode.size;
            let n = self.write_extents(ino, inode, 0, &d)?;
            inode.size = size;
            if n < d.len() {
                return Err(Error::NoSpace);
            }
        }
        Ok(())
    }

    fn has_extents(&mut self, ino: u64) -> Result<bool> {
        Ok(!self
            .st
            .range(
                &self.fs_root,
                &Key::new(ino, EXTENT_DATA, 0),
                &Key::new(ino, EXTENT_DATA, u64::MAX),
                1,
            )?
            .is_empty())
    }

    /// Write file data (regular files and symlink targets).
    fn write_data(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        off: u64,
        data: &[u8],
    ) -> Result<usize> {
        let end = off.checked_add(data.len() as u64).ok_or(Error::Invalid)?;
        let inline = self.inline_data(ino)?;
        // Encrypted data only goes to blocks, where XTS covers it.
        let small = end <= INLINE_MAX as u64
            && inode.size <= INLINE_MAX as u64
            && inode.flags & FL_ENCRYPT as u64 == 0;
        if small && (inline.is_some() || !self.has_extents(ino)?) {
            let mut v = inline.unwrap_or_default();
            v.resize(inode.size as usize, 0);
            if v.len() < end as usize {
                v.resize(end as usize, 0);
            }
            v[off as usize..end as usize].copy_from_slice(data);
            inode.nbytes = v.len() as u64;
            inode.size = v.len() as u64;
            self.upsert(Self::ext_key(ino, 0), FileExtent::Inline(v).encode())?;
            return Ok(data.len());
        }
        if inline.is_some() {
            self.uninline(ino, inode)?;
        }
        self.write_extents(ino, inode, off, data)
    }

    /// Shrink or grow (sparsely) to `new_size`. Bytes past the end of the
    /// last block always read back as zero.
    fn truncate_data(&mut self, ino: u64, inode: &mut InodeItem, new_size: u64) -> Result<()> {
        if let Some(mut v) = self.inline_data(ino)? {
            if new_size <= INLINE_MAX as u64 {
                v.resize(new_size as usize, 0);
                inode.size = new_size;
                inode.nbytes = new_size;
                return self.upsert(Self::ext_key(ino, 0), FileExtent::Inline(v).encode());
            }
            self.uninline(ino, inode)?;
        }
        if new_size < inode.size {
            let tail = new_size % BS;
            if tail != 0
                && !self
                    .extents(ino, new_size / BS, new_size / BS + 1)?
                    .is_empty()
            {
                let zeros = vec![0u8; (BS - tail).min(inode.size - new_size) as usize];
                let size = inode.size;
                self.write_extents(ino, inode, new_size, &zeros)?;
                inode.size = size;
            }
            self.unmap(ino, inode, new_size.div_ceil(BS), u64::MAX / BS)?;
        }
        inode.size = new_size;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Names
    // -----------------------------------------------------------------------

    fn dir_lookup(&mut self, dir: u64, name: &[u8]) -> Result<Option<DirEnt>> {
        let d = self.inode(dir)?;
        if d.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        match self.get(Key::new(dir, DIR_ITEM, name_hash(name)))? {
            Some(v) => Ok(decode_dir_item(&v)?.into_iter().find(|e| e.name == name)),
            None => Ok(None),
        }
    }

    /// Add a name to `dir`, returning its index. `ino` is a local inode
    /// or, for a subvolume, the handle of its root (no `INODE_REF` then:
    /// the root tree's backref names it).
    fn add_entry(&mut self, dir: u64, name: &[u8], ino: u64, kind: FileKind) -> Result<u64> {
        self.add_entry_at(dir, name, ino, kind, None)
    }

    /// [`add_entry`](Self::add_entry) at a given directory index (for
    /// `receive`), or the next free one.
    fn add_entry_at(
        &mut self,
        dir: u64,
        name: &[u8],
        ino: u64,
        kind: FileKind,
        at: Option<u64>,
    ) -> Result<u64> {
        let mut d = self.inode(dir)?;
        if d.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        let index = match at {
            Some(i) if (2..u64::MAX).contains(&i) => {
                if self.get(Key::new(dir, DIR_INDEX, i))?.is_some() {
                    return Err(Error::Exists);
                }
                i
            }
            Some(_) => return Err(Error::Invalid),
            None => d.dir_next,
        };
        let hkey = Key::new(dir, DIR_ITEM, name_hash(name));
        let mut list = match self.get(hkey)? {
            Some(v) => decode_dir_item(&v)?,
            None => Vec::new(),
        };
        if list.iter().any(|e| e.name == name) {
            return Err(Error::Exists);
        }
        list.push(DirEnt {
            ino,
            index,
            kind,
            name: name.to_vec(),
        });
        let v = encode_dir_item(&list);
        if v.len() > MAX_ITEM {
            return Err(Error::NoSpace);
        }
        self.upsert(hkey, v)?;
        self.insert(
            Key::new(dir, DIR_INDEX, index),
            encode_pair(ino, 0, kind, name),
        )?;
        if !is_link(ino) {
            self.insert(
                Key::new(ino, INODE_REF, ref_hash(dir, index)),
                encode_pair(dir, index, kind, name),
            )?;
        }
        let now = self.now();
        d.dir_next = d.dir_next.max(index + 1);
        d.size += 2 * name.len() as u64;
        d.mtime = now;
        d.ctime = now;
        self.put_inode(dir, d)?;
        Ok(index)
    }

    fn remove_entry(&mut self, dir: u64, name: &[u8]) -> Result<DirEnt> {
        let hkey = Key::new(dir, DIR_ITEM, name_hash(name));
        let mut list = match self.get(hkey)? {
            Some(v) => decode_dir_item(&v)?,
            None => return Err(Error::NotFound),
        };
        let pos = list
            .iter()
            .position(|e| e.name == name)
            .ok_or(Error::NotFound)?;
        let e = list.remove(pos);
        if list.is_empty() {
            self.delete(hkey)?;
        } else {
            self.upsert(hkey, encode_dir_item(&list))?;
        }
        self.delete(Key::new(dir, DIR_INDEX, e.index))?;
        if !is_link(e.ino) {
            self.delete(Key::new(e.ino, INODE_REF, ref_hash(dir, e.index)))?;
        }
        let mut d = self.inode(dir)?;
        let now = self.now();
        d.size = d.size.saturating_sub(2 * name.len() as u64);
        d.mtime = now;
        d.ctime = now;
        self.put_inode(dir, d)?;
        Ok(e)
    }

    fn dir_is_empty(&mut self, dir: u64) -> Result<bool> {
        Ok(self
            .st
            .range(
                &self.fs_root,
                &Key::new(dir, DIR_INDEX, 0),
                &Key::new(dir, DIR_INDEX, u64::MAX),
                1,
            )?
            .is_empty())
    }

    /// Delete an inode and everything it owns.
    ///
    /// A batch at a time, and never the whole item list at once. Every item of
    /// the inode is an `(Key, Vec<u8>)` -- 48 bytes plus its data -- and a
    /// multi-GB file has millions of extents, so asking for all of them meant
    /// asking the allocator for one contiguous slab of tens of MiB. A capture
    /// from a real machine died on exactly that: `memory allocation of
    /// 100663296 bytes failed` (96 MiB, 2^21 items) inside `unlink`, and
    /// because the mount holds its spinlock with interrupts off across the
    /// whole call, the panic took the lock with it and the box wedged. The
    /// total work is the same either way; only the peak differs.
    ///
    /// The window re-opens at the successor of the last key handled rather
    /// than at the same `lo`: the items just deleted are gone from the tree,
    /// so re-reading from `lo` would also terminate, but only as long as every
    /// `delete` really removes its key. Advancing makes the loop finite
    /// whatever `delete` does.
    fn purge(&mut self, ino: u64) -> Result<()> {
        self.zpending.remove(&(self.subvol, ino));
        self.keyring.forget(self.subvol, Some(ino));
        let hi = Key::new(ino, u8::MAX, u64::MAX);
        let mut lo = Key::new(ino, 0, 0);
        loop {
            let items = self.st.range(&self.fs_root, &lo, &hi, PURGE_BATCH)?;
            let Some(last) = items.last().map(|(k, _)| *k) else {
                break;
            };
            for (k, d) in items {
                // The item goes first: copying a leaf shared with a snapshot
                // adds the owners its extents are then released from.
                self.delete(k)?;
                if k.ty == INODE_ITEM && self.quota_on() {
                    let r = &mut self.fs_item.referenced;
                    *r = r.saturating_sub(InodeItem::decode(&d)?.nbytes);
                }
                if let Some((s, l)) = extent_of(&k, &d) {
                    self.sp_decref(s, l)?;
                }
            }
            match key_after(&last) {
                Some(next) => lo = next,
                // `last` was Key::MAX: nothing can follow it.
                None => break,
            }
        }
        self.fs_item.inode_count = self.fs_item.inode_count.saturating_sub(1);
        Ok(())
    }

    /// One name of `ino` is gone (from `dir`).
    fn drop_link(&mut self, dir: u64, ino: u64) -> Result<()> {
        let mut i = self.inode(ino)?;
        if i.kind() == FileKind::Dir {
            let mut p = self.inode(dir)?;
            p.nlink = p.nlink.saturating_sub(1).max(2);
            self.put_inode(dir, p)?;
            return self.purge(ino);
        }
        if i.nlink > 1 {
            i.nlink -= 1;
            i.ctime = self.now();
            if i.parent == dir {
                // Point `parent` at a remaining name.
                if let Some((_, d)) = self
                    .st
                    .range(
                        &self.fs_root,
                        &Key::new(ino, INODE_REF, 0),
                        &Key::new(ino, INODE_REF, u64::MAX),
                        1,
                    )?
                    .pop()
                {
                    i.parent = decode_pair(&d)?.ino;
                }
            }
            self.put_inode(ino, i)
        } else {
            self.purge(ino)
        }
    }

    // -----------------------------------------------------------------------
    // Operations on local inodes of the current subvolume
    // -----------------------------------------------------------------------

    fn stat_l(&mut self, ino: u64) -> Result<InodeStat> {
        let i = self.inode(ino)?;
        Ok(stat_of(ino, &i))
    }

    fn set_attr_l(
        &mut self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<(u64, u32)>,
        mtime: Option<(u64, u32)>,
    ) -> Result<()> {
        self.writable()?;
        let mut i = self.inode(ino)?;
        permit(frozen(&i))?;
        if let Some(m) = mode {
            i.mode = (i.mode & 0o170000) | (m & 0o7777);
        }
        if let Some(u) = uid {
            i.uid = u;
        }
        if let Some(g) = gid {
            i.gid = g;
        }
        if let Some(t) = atime {
            i.atime = t;
        }
        if let Some(t) = mtime {
            i.mtime = t;
        }
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
    }

    fn lookup_l(&mut self, dir: u64, name: &str) -> Result<u64> {
        if self.sb.features & FEAT_ENCRYPT == 0 {
            return Ok(self
                .dir_lookup(dir, name.as_bytes())?
                .ok_or(Error::NotFound)?
                .ino);
        }
        let name = match self.disk_name(dir, name, encrypt::NameUse::Find) {
            Err(Error::Invalid) => return Err(Error::NotFound),
            n => n?,
        };
        Ok(self.dir_lookup(dir, &name)?.ok_or(Error::NotFound)?.ino)
    }

    fn readdir_l(&mut self, dir: u64) -> Result<Vec<DirEntry>> {
        if self.inode(dir)?.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        let items = self.st.range(
            &self.fs_root,
            &Key::new(dir, DIR_INDEX, 0),
            &Key::new(dir, DIR_INDEX, u64::MAX),
            usize::MAX,
        )?;
        let shower = self.name_shower(dir)?;
        let mut out = Vec::with_capacity(items.len());
        for (_, d) in items {
            let e = decode_pair(&d)?;
            let name = match &shower {
                None => e.name,
                Some(keys) => encrypt::shown_name(keys.as_deref(), &e.name)?,
            };
            out.push(DirEntry {
                ino: e.ino,
                name: String::from_utf8(name).map_err(|_| Error::Corrupt("entry name"))?,
                kind: e.kind,
            });
        }
        Ok(out)
    }

    fn create_l(
        &mut self,
        dir: u64,
        name: &str,
        kind: FileKind,
        mode: u32,
        rdev: u64,
    ) -> Result<u64> {
        let name = self.disk_name(dir, name, encrypt::NameUse::Create)?;
        let name = &name[..];
        self.writable()?;
        if self.dir_lookup(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        let d = self.inode(dir)?;
        permit(immutable(&d))?;
        let ino = self.fs_item.next_ino;
        let mut i = InodeItem::new(kind, mode, self.now(), self.st.gen);
        i.rdev = rdev;
        i.parent = dir;
        if matches!(kind, FileKind::Regular | FileKind::Dir) {
            i.flags = d.flags & FL_INHERITED;
        }
        self.inherit_encryption(dir, &d, ino, &mut i)?;
        self.insert(Key::new(ino, INODE_ITEM, 0), i.encode())?;
        self.fs_item.next_ino += 1;
        self.fs_item.inode_count += 1;
        if let Err(e) = self.add_entry(dir, name, ino, kind) {
            let _ = self.purge(ino);
            let _ = self.commit();
            return Err(e);
        }
        if kind == FileKind::Dir {
            let mut p = self.inode(dir)?;
            p.nlink += 1;
            self.put_inode(dir, p)?;
        }
        self.commit()?;
        Ok(ino)
    }

    fn symlink_l(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<u64> {
        if target.is_empty() || target.len() > MAX_SYMLINK_LEN {
            return Err(Error::Invalid);
        }
        self.nested = true;
        let ino = self.create_l(dir, name, FileKind::Symlink, 0o777, 0);
        self.nested = false;
        let ino = match ino {
            Ok(ino) => ino,
            Err(e) => {
                self.commit()?;
                return Err(e);
            }
        };
        let mut i = self.inode(ino)?;
        let r = self.write_data(ino, &mut i, 0, target);
        self.put_inode(ino, i)?;
        let n = r?;
        self.commit()?;
        if n < target.len() {
            return Err(Error::NoSpace);
        }
        Ok(ino)
    }

    fn write_symlink_l(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.writable()?;
        let i = self.inode(ino)?;
        if i.kind() != FileKind::Symlink {
            return Err(Error::Invalid);
        }
        permit(frozen(&i))?;
        let end = (offset as usize)
            .checked_add(data.len())
            .filter(|&e| e <= MAX_SYMLINK_LEN)
            .ok_or(Error::Invalid)?;
        let mut target = self.read_link_l(ino)?;
        if end > target.len() {
            target.resize(end, 0);
        }
        target[offset as usize..end].copy_from_slice(data);
        let mut i = self.inode(ino)?;
        self.truncate_data(ino, &mut i, 0)?;
        let r = self.write_data(ino, &mut i, 0, &target);
        let now = self.now();
        i.mtime = now;
        i.ctime = now;
        self.put_inode(ino, i)?;
        let n = r?;
        self.commit()?;
        if n < target.len() {
            return Err(Error::NoSpace);
        }
        Ok(data.len())
    }

    fn read_link_l(&mut self, ino: u64) -> Result<Vec<u8>> {
        let i = self.inode(ino)?;
        if i.kind() != FileKind::Symlink {
            return Err(Error::Invalid);
        }
        if i.size > MAX_SYMLINK_LEN as u64 {
            return Err(Error::Corrupt("symlink size"));
        }
        let mut buf = vec![0u8; i.size as usize];
        let n = self.read_inode_data(ino, &i, 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn parent_l(&mut self, ino: u64) -> Result<u64> {
        if ino == self.fs_item.root_dir {
            return Ok(ino);
        }
        match self.inode(ino)?.parent {
            0 => Err(Error::NotFound),
            p => Ok(p),
        }
    }

    fn link_l(&mut self, dir: u64, name: &str, ino: u64) -> Result<()> {
        let name = self.disk_name(dir, name, encrypt::NameUse::Create)?;
        let name = &name[..];
        self.writable()?;
        let mut i = self.inode(ino)?;
        if i.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        self.may_link_into(dir, ino)?;
        if self.dir_lookup(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        permit(frozen(&i) || immutable(&self.inode(dir)?))?;
        self.add_entry(dir, name, ino, i.kind())?;
        i.nlink += 1;
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
    }

    fn unlink_l(&mut self, dir: u64, name: &str) -> Result<()> {
        let name = self.disk_name(dir, name, encrypt::NameUse::Find)?;
        let name = &name[..];
        self.writable()?;
        let e = self.dir_lookup(dir, name)?.ok_or(Error::NotFound)?;
        permit(frozen(&self.inode(dir)?) || frozen(&self.inode(e.ino)?))?;
        if e.kind == FileKind::Dir && !self.dir_is_empty(e.ino)? {
            return Err(Error::NotEmpty);
        }
        self.remove_entry(dir, name)?;
        self.drop_link(dir, e.ino)?;
        self.commit()
    }

    fn rename_l(
        &mut self,
        old_dir: u64,
        old_name: &str,
        new_dir: u64,
        new_name: &str,
    ) -> Result<()> {
        let old_name = self.disk_name(old_dir, old_name, encrypt::NameUse::Find)?;
        let old_name = &old_name[..];
        let new_name = self.disk_name(new_dir, new_name, encrypt::NameUse::Create)?;
        let new_name = &new_name[..];
        self.writable()?;
        let e = self.dir_lookup(old_dir, old_name)?.ok_or(Error::NotFound)?;
        if old_dir == new_dir && old_name == new_name {
            return Ok(());
        }
        self.may_link_into(new_dir, e.ino)?;
        let is_dir = e.kind == FileKind::Dir;
        let moving_subvol = is_link(e.ino);
        permit(
            frozen(&self.inode(old_dir)?)
                || frozen(&self.inode(new_dir)?)
                || !moving_subvol && frozen(&self.inode(e.ino)?),
        )?;
        if self.inode(new_dir)?.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        if is_dir && !moving_subvol && old_dir != new_dir {
            let mut cur = new_dir;
            for _ in 0..4096 {
                if cur == e.ino {
                    return Err(Error::Invalid);
                }
                if cur == self.fs_item.root_dir {
                    break;
                }
                cur = self.parent_l(cur)?;
            }
        }
        if let Some(existing) = self.dir_lookup(new_dir, new_name)? {
            if existing.ino == e.ino {
                return Ok(());
            }
            if is_link(existing.ino) {
                // Replacing a subvolume takes deleting it.
                return Err(Error::NotEmpty);
            }
            permit(frozen(&self.inode(existing.ino)?))?;
            match (is_dir, existing.kind == FileKind::Dir) {
                (true, false) => return Err(Error::NotDir),
                (false, true) => return Err(Error::IsDir),
                _ => {}
            }
            if existing.kind == FileKind::Dir && !self.dir_is_empty(existing.ino)? {
                return Err(Error::NotEmpty);
            }
            self.remove_entry(new_dir, new_name)?;
            self.drop_link(new_dir, existing.ino)?;
        }
        self.remove_entry(old_dir, old_name)?;
        let index = self.add_entry(new_dir, new_name, e.ino, e.kind)?;
        if moving_subvol {
            let (id, _) = split(e.ino);
            self.set_backref(id, self.subvol, new_dir, index, new_name)?;
            return self.commit();
        }
        let now = self.now();
        let mut i = self.inode(e.ino)?;
        i.ctime = now;
        i.parent = new_dir;
        self.put_inode(e.ino, i)?;
        if is_dir && old_dir != new_dir {
            let mut o = self.inode(old_dir)?;
            o.nlink = o.nlink.saturating_sub(1).max(2);
            self.put_inode(old_dir, o)?;
            let mut n = self.inode(new_dir)?;
            n.nlink += 1;
            self.put_inode(new_dir, n)?;
        }
        self.commit()
    }

    fn read_inode_data(
        &mut self,
        ino: u64,
        i: &InodeItem,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        if offset >= i.size || buf.is_empty() {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(i.size - offset) as usize;
        // Inline data never outgrows INLINE_MAX (writes and truncates move
        // it to extents first, and fsck checks it), so a larger file has
        // none and its read skips that lookup.
        if i.size <= INLINE_MAX as u64 {
            if let Some(v) = self.inline_data(ino)? {
                let o = offset as usize;
                let avail = v.len().saturating_sub(o).min(n);
                buf[..avail].copy_from_slice(&v[o..o + avail]);
                buf[avail..n].fill(0);
                return Ok(n);
            }
        }
        let sums = i.flags & FL_NOCOW as u64 == 0;
        let keys = self.file_keys(ino, i)?;
        self.read_blocks_with(ino, sums, keys.as_deref(), offset, &mut buf[..n])?;
        Ok(n)
    }

    fn read_l(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let i = self.inode(ino)?;
        if i.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        self.read_inode_data(ino, &i, offset, buf)
    }

    fn write_l(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.writable()?;
        let mut i = self.inode(ino)?;
        match i.kind() {
            FileKind::Dir => return Err(Error::IsDir),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
        offset
            .checked_add(data.len() as u64)
            .ok_or(Error::Invalid)?;
        // Append-only files only grow at the end.
        permit(immutable(&i) || frozen(&i) && offset != i.size)?;
        let (from, to) = (offset / BS, (offset + data.len() as u64).div_ceil(BS));
        let holes = to - from - self.mapped(ino, from, to)?;
        self.charge(holes)?;
        let r = self.write_data(ino, &mut i, offset, data);
        if r.is_ok() {
            let now = self.now();
            i.mtime = now;
            i.ctime = now;
        }
        let stored = self.put_inode(ino, i);
        let committed = self.commit();
        let n = r?;
        stored?;
        committed?;
        Ok(n)
    }

    fn truncate_l(&mut self, ino: u64, new_size: u64) -> Result<()> {
        self.writable()?;
        let mut i = self.inode(ino)?;
        match i.kind() {
            FileKind::Dir => return Err(Error::IsDir),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
        permit(frozen(&i))?;
        if i.size == new_size {
            return Ok(());
        }
        let r = self.truncate_data(ino, &mut i, new_size);
        let now = self.now();
        i.mtime = now;
        i.ctime = now;
        self.put_inode(ino, i)?;
        self.commit()?;
        r
    }

    // -----------------------------------------------------------------------
    // Subvolumes
    // -----------------------------------------------------------------------

    /// Make `subvol` the one the inode-level helpers work on.
    fn enter(&mut self, subvol: u64) -> Result<()> {
        if subvol == self.subvol {
            return Ok(());
        }
        let (root, item) = match self.subvols.remove(&subvol) {
            Some(t) => t,
            None => {
                if subvol != FS_TREE && subvol < FIRST_SUBVOL {
                    return Err(Error::NotFound);
                }
                let d = self
                    .st
                    .get(&self.root_tree, &Key::new(subvol, ROOT_ITEM, 0))?
                    .ok_or(Error::NotFound)?;
                let item = RootItem::decode(&d)?;
                (item.root, item)
            }
        };
        let old_root = core::mem::replace(&mut self.fs_root, root);
        let old_item = core::mem::replace(&mut self.fs_item, item);
        self.subvols.insert(self.subvol, (old_root, old_item));
        self.subvol = subvol;
        Ok(())
    }

    /// The local inode of handle `h`, with its subvolume entered.
    fn at(&mut self, h: u64) -> Result<u64> {
        let (sv, ino) = split(h);
        self.enter(sv)?;
        Ok(ino)
    }

    /// Same as [`at`](Self::at) for a change: the subvolume must be writable.
    fn at_rw(&mut self, h: u64) -> Result<u64> {
        let ino = self.at(h)?;
        if self.fs_item.flags & ROOT_FLAG_RDONLY != 0 {
            return Err(Error::Unsupported("read-only subvolume"));
        }
        Ok(ino)
    }

    /// Handle of a local inode (or entry target) of the current subvolume.
    fn out(&self, ino: u64) -> u64 {
        if is_link(ino) {
            ino
        } else {
            handle(self.subvol, ino)
        }
    }

    /// Two handles in one subvolume (links and renames cannot cross).
    fn same_subvol(a: u64, b: u64) -> Result<()> {
        if split(a).0 != split(b).0 {
            return Err(Error::Unsupported("cross-subvolume link or rename"));
        }
        Ok(())
    }

    fn root_items(&mut self) -> Result<Vec<(u64, RootItem)>> {
        let mut out = Vec::new();
        for (k, d) in self.st.range(
            &self.root_tree,
            &Key::new(0, ROOT_ITEM, 0),
            &Key::MAX,
            usize::MAX,
        )? {
            if k.ty == ROOT_ITEM && (k.objectid == FS_TREE || k.objectid >= FIRST_SUBVOL) {
                out.push((k.objectid, RootItem::decode(&d)?));
            }
        }
        Ok(out)
    }

    /// The entry naming subvolume `id`: (parent subvolume, directory,
    /// index, name).
    fn backref(&mut self, id: u64) -> Result<Option<(u64, DirEnt)>> {
        let mut v = self.st.range(
            &self.root_tree,
            &Key::new(id, ROOT_BACKREF, 0),
            &Key::new(id, ROOT_BACKREF, u64::MAX),
            2,
        )?;
        if v.len() > 1 {
            return Err(Error::Corrupt("subvolume named twice"));
        }
        match v.pop() {
            Some((k, d)) => Ok(Some((k.offset, decode_pair(&d)?))),
            None => Ok(None),
        }
    }

    fn set_backref(
        &mut self,
        id: u64,
        parent: u64,
        dir: u64,
        index: u64,
        name: &[u8],
    ) -> Result<()> {
        if let Some((p, _)) = self.backref(id)? {
            self.st.delete(
                &mut self.sp,
                &mut self.root_tree,
                Key::new(id, ROOT_BACKREF, p),
            )?;
        }
        self.st.insert(
            &mut self.sp,
            &mut self.root_tree,
            Key::new(id, ROOT_BACKREF, parent),
            encode_pair(dir, index, FileKind::Dir, name),
        )
    }

    fn new_subvol_id(&mut self) -> Result<u64> {
        let last = self
            .root_items()?
            .into_iter()
            .map(|(id, _)| id)
            .max()
            .unwrap_or(0);
        let id = (last + 1).max(FIRST_SUBVOL);
        if id > MAX_SUBVOL {
            return Err(Error::NoSpace);
        }
        Ok(id)
    }

    /// Name subvolume `id` as `name` in local directory `dir` of the
    /// current subvolume.
    fn link_subvol(&mut self, dir: u64, name: &[u8], id: u64) -> Result<()> {
        let index = self.add_entry(dir, name, handle(id, FIRST_INO), FileKind::Dir)?;
        self.set_backref(id, self.subvol, dir, index, name)
    }

    /// Check `dir`/`name` can be created and enter `dir`'s subvolume.
    fn new_name<'a>(&mut self, dir: u64, name: &'a str) -> Result<(u64, &'a [u8])> {
        let name = check_name(name)?;
        let d = self.at_rw(dir)?;
        self.writable()?;
        if self.inode(d)?.flags & FL_ENCRYPT as u64 != 0 {
            return Err(Error::Unsupported("subvolume in an encrypted directory"));
        }
        if self.dir_lookup(d, name)?.is_some() {
            return Err(Error::Exists);
        }
        permit(immutable(&self.inode(d)?))?;
        Ok((d, name))
    }

    pub fn create_subvolume(&mut self, dir: u64, name: &str) -> Result<u64> {
        let (d, name) = self.new_name(dir, name)?;
        let id = self.new_subvol_id()?;
        let now = self.now();
        let mut root = self.st.create(&mut self.sp, id, 0)?;
        let mut top = InodeItem::new(FileKind::Dir, 0o755, now, self.st.gen);
        top.parent = FIRST_INO;
        self.st.insert(
            &mut self.sp,
            &mut root,
            Key::new(FIRST_INO, INODE_ITEM, 0),
            top.encode(),
        )?;
        let item = RootItem {
            root,
            next_ino: FIRST_INO + 1,
            inode_count: 1,
            root_dir: FIRST_INO,
            flags: 0,
            otime: now,
            parent: 0,
            parent_gen: 0,
            uuid: subvol_uuid(&self.sb.uuid, id, self.st.gen, now),
            ..Default::default()
        };
        self.st.insert(
            &mut self.sp,
            &mut self.root_tree,
            Key::new(id, ROOT_ITEM, 0),
            item.encode(),
        )?;
        self.link_subvol(d, name, id)?;
        self.commit()?;
        Ok(handle(id, FIRST_INO))
    }

    pub fn snapshot(&mut self, src: u64, dir: u64, name: &str, read_only: bool) -> Result<u64> {
        let (src_sv, src_ino) = split(src);
        if src_ino != FIRST_INO {
            return Err(Error::Invalid);
        }
        let (d, name) = self.new_name(dir, name)?;
        let dir_sv = self.subvol;
        // Commit first: afterwards no block is private to the open
        // transaction, so nothing the snapshot shares is rewritten in place.
        self.flush()?;
        self.enter(src_sv)?;
        let root = self.fs_root;
        let mut item = self.fs_item.clone();
        let id = self.new_subvol_id()?;
        self.st.share(&mut self.sp, root)?;
        item.root = root;
        item.flags = 0;
        item.otime = self.now();
        item.parent = src_sv;
        item.parent_gen = self.sb.generation;
        item.parent_uuid = item.uuid;
        item.uuid = subvol_uuid(&self.sb.uuid, id, self.st.gen, item.otime);
        item.received_uuid = [0; 16];
        item.rtime = (0, 0);
        // Same counter (same files), no limit of its own.
        item.limit = 0;
        self.st.insert(
            &mut self.sp,
            &mut self.root_tree,
            Key::new(id, ROOT_ITEM, 0),
            item.encode(),
        )?;
        // Subvolumes nested in the source are not part of the snapshot.
        let nested: Vec<DirEnt> = self
            .backrefs()?
            .into_iter()
            .filter(|(_, p, _)| *p == src_sv)
            .map(|(_, _, e)| e)
            .collect();
        if !nested.is_empty() {
            self.enter(id)?;
            for e in nested {
                self.remove_entry(e.ino, &e.name)?;
            }
        }
        if read_only {
            self.enter(id)?;
            self.fs_item.flags |= ROOT_FLAG_RDONLY;
        }
        self.enter(dir_sv)?;
        self.link_subvol(d, name, id)?;
        self.commit()?;
        Ok(handle(id, FIRST_INO))
    }

    /// Every backref: (subvolume, parent subvolume, entry).
    fn backrefs(&mut self) -> Result<Vec<(u64, u64, DirEnt)>> {
        let mut out = Vec::new();
        for (k, d) in self
            .st
            .range(&self.root_tree, &Key::MIN, &Key::MAX, usize::MAX)?
        {
            if k.ty == ROOT_BACKREF {
                out.push((k.objectid, k.offset, decode_pair(&d)?));
            }
        }
        Ok(out)
    }

    pub fn delete_subvolume(&mut self, dir: u64, name: &str) -> Result<()> {
        let name = check_name(name)?;
        let d = self.at_rw(dir)?;
        self.writable()?;
        let e = self.dir_lookup(d, name)?.ok_or(Error::NotFound)?;
        if !is_link(e.ino) {
            return Err(Error::Invalid);
        }
        permit(frozen(&self.inode(d)?))?;
        self.drop_subvol(d, name, split(e.ino).0)
    }

    /// Remove entry `dir`/`name` of the current subvolume, which names
    /// subvolume `id`, and the subvolume with it.
    fn drop_subvol(&mut self, dir: u64, name: &[u8], id: u64) -> Result<()> {
        if id == self.default_subvol || id == self.sb.default_subvol {
            return Err(Error::Unsupported("deleting the default subvolume"));
        }
        if self.backrefs()?.iter().any(|(_, p, _)| *p == id) {
            return Err(Error::NotEmpty);
        }
        self.remove_entry(dir, name)?;
        self.zpending.retain(|&(sv, _), _| sv != id);
        self.keyring.forget(id, None);
        let (root, _) = match self.subvols.remove(&id) {
            Some(t) => t,
            None => {
                let d = self
                    .st
                    .get(&self.root_tree, &Key::new(id, ROOT_ITEM, 0))?
                    .ok_or(Error::Corrupt("missing subvolume"))?;
                let item = RootItem::decode(&d)?;
                (item.root, item)
            }
        };
        self.st.delete(
            &mut self.sp,
            &mut self.root_tree,
            Key::new(id, ROOT_ITEM, 0),
        )?;
        self.st.delete(
            &mut self.sp,
            &mut self.root_tree,
            Key::new(id, ROOT_BACKREF, self.subvol),
        )?;
        self.st.release(&mut self.sp, root)?;
        self.commit()
    }

    /// Every subvolume's item as it is now: the root tree's, or newer for
    /// the subvolumes touched since the last commit.
    fn current_root_items(&mut self) -> Result<BTreeMap<u64, RootItem>> {
        let mut items: BTreeMap<u64, RootItem> = self.root_items()?.into_iter().collect();
        items.insert(self.subvol, self.fs_item.clone());
        for (&id, (_, it)) in &self.subvols {
            items.insert(id, it.clone());
        }
        Ok(items)
    }

    pub fn list_subvolumes(&mut self) -> Result<Vec<SubvolInfo>> {
        let items = self.current_root_items()?;
        let mut back: BTreeMap<u64, (u64, DirEnt)> = BTreeMap::new();
        for (id, p, e) in self.backrefs()? {
            back.insert(id, (p, e));
        }
        let mut out = Vec::new();
        for (id, it) in items {
            if it.flags & ROOT_FLAG_RECEIVING != 0 {
                continue;
            }
            let (parent, dir, name) = match back.remove(&id) {
                Some((p, e)) => (
                    p,
                    handle(p, e.ino),
                    String::from_utf8(e.name).map_err(|_| Error::Corrupt("entry name"))?,
                ),
                None => (0, 0, String::new()),
            };
            out.push(SubvolInfo {
                id,
                root: handle(id, FIRST_INO),
                parent,
                dir,
                name,
                read_only: it.flags & ROOT_FLAG_RDONLY != 0,
                snapshot_of: it.parent,
                snapshot_gen: it.parent_gen,
                created: it.otime,
                uuid: it.uuid,
                parent_uuid: it.parent_uuid,
                received_uuid: it.received_uuid,
            });
        }
        Ok(out)
    }

    pub fn set_subvolume_read_only(&mut self, id: u64, read_only: bool) -> Result<()> {
        if self.read_only {
            return Err(Error::Unsupported("read-only filesystem"));
        }
        self.enter(id)?;
        self.writable()?;
        if read_only {
            self.fs_item.flags |= ROOT_FLAG_RDONLY;
        } else {
            self.fs_item.flags &= !ROOT_FLAG_RDONLY;
            // Changed from here on: no longer what was received (btrfs
            // clears it too, so later incremental streams are refused).
            self.fs_item.received_uuid = [0; 16];
        }
        self.commit()
    }

    pub fn set_default_subvolume(&mut self, id: u64) -> Result<()> {
        self.writable()?;
        self.enter(id)?;
        self.sb.default_subvol = id;
        // Nothing else may have changed: queue the superblock so the commit
        // has something to write.
        self.dev.write_at(0, &self.sb.encode())?;
        self.flush()
    }

    // -----------------------------------------------------------------------
    // Reflinks and deduplication
    // -----------------------------------------------------------------------

    pub fn clone_range(
        &mut self,
        src: u64,
        src_off: u64,
        dst: u64,
        dst_off: u64,
        len: u64,
    ) -> Result<u64> {
        let s_ino = self.at(src)?;
        let si = self.inode(s_ino)?;
        match si.kind() {
            FileKind::Regular => {}
            FileKind::Dir => return Err(Error::IsDir),
            _ => return Err(Error::Invalid),
        }
        let len = if len == 0 {
            si.size.saturating_sub(src_off)
        } else {
            len
        };
        let s_end = src_off.checked_add(len).ok_or(Error::Invalid)?;
        let d_end = dst_off.checked_add(len).ok_or(Error::Invalid)?;
        if s_end > si.size || !src_off.is_multiple_of(BS) || !dst_off.is_multiple_of(BS) {
            return Err(Error::Invalid);
        }
        if len == 0 {
            return Ok(0);
        }
        let blocks = len.div_ceil(BS);
        if src == dst && src_off < d_end && dst_off < s_end {
            return Err(Error::Invalid);
        }
        let inline = self.inline_data(s_ino)?;
        let s0 = src_off / BS;
        // What the source maps, counted while its subvolume is entered.
        let shared = match inline {
            Some(_) => blocks,
            None => self.mapped(s_ino, s0, s0 + blocks)?,
        };
        let d_ino = self.at_rw(dst)?;
        self.writable()?;
        let mut di = self.inode(d_ino)?;
        match di.kind() {
            FileKind::Regular => {}
            FileKind::Dir => return Err(Error::IsDir),
            _ => return Err(Error::Invalid),
        }
        permit(frozen(&di))?;
        // Encrypted blocks only decrypt in their own file, at their place.
        self.refuse_encrypted(&si)?;
        self.refuse_encrypted(&di)?;
        // Checksummed and no-COW files cannot share blocks (as on btrfs).
        if (di.flags ^ si.flags) & FL_NOCOW as u64 != 0 {
            return Err(Error::Invalid);
        }
        // A partial last block only at the end of both files.
        if len % BS != 0 && (s_end != si.size || d_end < di.size) {
            return Err(Error::Invalid);
        }
        // New to the destination: blocks the source maps over its holes,
        // at most.
        let d0 = dst_off / BS;
        let holes = blocks - self.mapped(d_ino, d0, d0 + blocks)?;
        self.charge(shared.min(holes))?;
        let r = match inline {
            Some(mut v) => {
                v.resize(si.size as usize, 0);
                let bytes = &v[src_off as usize..s_end as usize];
                self.write_data(d_ino, &mut di, dst_off, bytes)
                    .and_then(|n| {
                        if n < bytes.len() {
                            Err(Error::NoSpace)
                        } else {
                            Ok(())
                        }
                    })
            }
            None => {
                let src = (split(src).0, s_ino);
                self.share_extents(d_ino, &mut di, src, s0, d0, blocks)
            }
        };
        if r.is_ok() {
            di.size = di.size.max(d_end);
            let now = self.now();
            di.mtime = now;
            di.ctime = now;
        }
        self.put_inode(d_ino, di)?;
        self.commit()?;
        r.map(|_| len)
    }

    /// Map file blocks `d0..d0+blocks` of `ino` (in the current subvolume)
    /// to the blocks that file `src` (`(subvolume, inode)`) maps at `s0..`,
    /// as one more owner of them (holes stay holes). The ranges do not
    /// overlap.
    ///
    /// The source extents are read a batch at a time, each with the source
    /// subvolume entered: a reflinked copy of a large file (`cp`, through
    /// `copy_file_range`) covers all of them.
    fn share_extents(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        src: (u64, u64),
        s0: u64,
        d0: u64,
        blocks: u64,
    ) -> Result<()> {
        if self.inline_data(ino)?.is_some() {
            self.uninline(ino, inode)?;
        }
        self.unmap(ino, inode, d0, d0 + blocks)?;
        let dst_sv = self.subvol;
        let mut cur = s0;
        while cur < s0 + blocks {
            self.enter(src.0)?;
            let batch = self.extents_upto(src.1, cur, s0 + blocks, EXTENT_BATCH);
            self.enter(dst_sv)?;
            let batch = batch?;
            let Some(last) = batch.last() else {
                break;
            };
            cur = last.lblk + last.blocks;
            self.share_batch(ino, inode, &batch, s0, d0, blocks)?;
        }
        Ok(())
    }

    fn share_batch(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        exts: &[Ext],
        s0: u64,
        d0: u64,
        blocks: u64,
    ) -> Result<()> {
        // Preallocated ranges stay holes: they read as zeros either way.
        for e in exts.iter().filter(|e| !e.prealloc) {
            let from = e.lblk.max(s0);
            let to = (e.lblk + e.blocks).min(s0 + blocks);
            if from >= to {
                continue;
            }
            if let Some(z) = e.z {
                crate::btree::Space::incref(&mut self.sp, e.pblk, z.disk_blocks)?;
                self.insert(Self::ext_key(ino, d0 + (from - s0)), e.piece(from, to))?;
                inode.nbytes += (to - from) * BS;
                continue;
            }
            let p = e.pblk + (from - e.lblk);
            crate::btree::Space::incref(&mut self.sp, p, to - from)?;
            self.set_mapping(ino, inode, d0 + (from - s0), to - from, p)?;
        }
        Ok(())
    }

    pub fn dedupe_range(
        &mut self,
        src: u64,
        src_off: u64,
        dst: u64,
        dst_off: u64,
        len: u64,
    ) -> Result<u64> {
        const MAX_DEDUPE: u64 = 16 * 1024 * 1024;
        let len = len.min(MAX_DEDUPE);
        if len == 0 {
            return Ok(0);
        }
        let mut a = vec![0u8; len as usize];
        let mut b = vec![0u8; len as usize];
        let n = self.read(src, src_off, &mut a)?;
        let m = self.read(dst, dst_off, &mut b)?;
        if n != len as usize || m != len as usize || a != b {
            return Ok(0);
        }
        self.clone_range(src, src_off, dst, dst_off, len)
    }

    // -----------------------------------------------------------------------
    // Extended attributes
    // -----------------------------------------------------------------------

    fn check_xattr_name(name: &str) -> Result<&[u8]> {
        let b = name.as_bytes();
        if b.is_empty() || b.len() > XATTR_NAME_MAX || b.contains(&0) {
            return Err(Error::Invalid);
        }
        if !XATTR_PREFIXES
            .iter()
            .any(|p| name.len() > p.len() && name.starts_with(p))
        {
            return Err(Error::Unsupported("xattr namespace"));
        }
        Ok(b)
    }

    fn xattr_list(&mut self, ino: u64, name: &[u8]) -> Result<(Key, Xattrs)> {
        let key = Key::new(ino, XATTR_ITEM, name_hash(name));
        let list = match self.get(key)? {
            Some(d) => decode_xattrs(&d)?,
            None => Vec::new(),
        };
        Ok((key, list))
    }

    pub fn get_xattr(&mut self, h: u64, name: &str) -> Result<Vec<u8>> {
        let n = Self::check_xattr_name(name)?;
        let ino = self.at(h)?;
        self.inode(ino)?;
        let (_, list) = self.xattr_list(ino, n)?;
        list.into_iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v)
            .ok_or(Error::NotFound)
    }

    pub fn list_xattrs(&mut self, h: u64) -> Result<Vec<String>> {
        let ino = self.at(h)?;
        self.inode(ino)?;
        let mut out = Vec::new();
        for (_, d) in self.st.range(
            &self.fs_root,
            &Key::new(ino, XATTR_ITEM, 0),
            &Key::new(ino, XATTR_ITEM, u64::MAX),
            usize::MAX,
        )? {
            for (n, _) in decode_xattrs(&d)? {
                out.push(String::from_utf8(n).map_err(|_| Error::Corrupt("xattr name"))?);
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn set_xattr(&mut self, h: u64, name: &str, value: &[u8], flags: u32) -> Result<()> {
        let n = Self::check_xattr_name(name)?;
        if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 || value.len() > XATTR_SIZE_MAX {
            return Err(Error::Invalid);
        }
        let ino = self.at_rw(h)?;
        self.writable()?;
        let mut i = self.inode(ino)?;
        permit(frozen(&i))?;
        let (key, mut list) = self.xattr_list(ino, n)?;
        match list.iter().position(|(k, _)| k == n) {
            Some(_) if flags & XATTR_CREATE != 0 => return Err(Error::Exists),
            Some(p) => list[p].1 = value.to_vec(),
            None if flags & XATTR_REPLACE != 0 => return Err(Error::NotFound),
            None => list.push((n.to_vec(), value.to_vec())),
        }
        let v = encode_xattrs(&list);
        if v.len() > MAX_ITEM {
            // Too large for a tree leaf (E2BIG).
            return Err(Error::NoSpace);
        }
        self.upsert(key, v)?;
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
    }

    pub fn remove_xattr(&mut self, h: u64, name: &str) -> Result<()> {
        let n = Self::check_xattr_name(name)?;
        let ino = self.at_rw(h)?;
        self.writable()?;
        let mut i = self.inode(ino)?;
        permit(frozen(&i))?;
        let (key, mut list) = self.xattr_list(ino, n)?;
        let p = list
            .iter()
            .position(|(k, _)| k == n)
            .ok_or(Error::NotFound)?;
        list.remove(p);
        if list.is_empty() {
            self.delete(key)?;
        } else {
            self.upsert(key, encode_xattrs(&list))?;
        }
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
    }

    // -----------------------------------------------------------------------
    // Inode flags (chattr)
    // -----------------------------------------------------------------------

    pub fn get_flags(&mut self, h: u64) -> Result<u32> {
        let ino = self.at(h)?;
        Ok(self.inode(ino)?.flags as u32 & (FL_SETTABLE | FL_ENCRYPT))
    }

    pub fn set_flags(&mut self, h: u64, flags: u32) -> Result<()> {
        // Shown by `get_flags` but only a policy sets it: ignored, as on
        // Linux, so `chattr` can hand back what it read.
        let flags = flags & !FL_ENCRYPT;
        if flags & !FL_SETTABLE != 0 || flags & FL_COMPR != 0 && flags & FL_NOCOMP != 0 {
            return Err(Error::Invalid);
        }
        let ino = self.at_rw(h)?;
        self.writable()?;
        let mut i = self.inode(ino)?;
        let old = i.flags as u32;
        // No-COW only changes on an empty file (its blocks would otherwise
        // be half checksummed); directories pass it on to new files.
        if (old ^ flags) & FL_NOCOW != 0
            && i.kind() == FileKind::Regular
            && (i.size > 0 || i.nbytes > 0)
        {
            return Err(Error::Invalid);
        }
        // No-COW data is never compressed.
        if flags & FL_NOCOW != 0 && (flags & FL_COMPR != 0 || inode_compression(&i).is_some()) {
            return Err(Error::Invalid);
        }
        i.flags = (i.flags & !(FL_SETTABLE as u64)) | flags as u64;
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
    }

    // -----------------------------------------------------------------------
    // Compression
    // -----------------------------------------------------------------------

    /// `compress=`: compress new data of every file (unless marked no-COW
    /// or no-compression) with `algo`, until unmounted.
    pub fn set_mount_compression(&mut self, algo: Option<Compression>) {
        self.compress = algo;
    }

    pub fn mount_compression(&self) -> Option<Compression> {
        self.compress
    }

    /// The compression property of `h` (`btrfs property get compression`).
    pub fn compression(&mut self, h: u64) -> Result<Option<Compression>> {
        let ino = self.at(h)?;
        Ok(inode_compression(&self.inode(ino)?))
    }

    /// Compress new data of `h` (files created in it too, for a directory)
    /// with `algo`, or stop doing so. Data already written stays as it is.
    pub fn set_compression(&mut self, h: u64, algo: Option<Compression>) -> Result<()> {
        let ino = self.at_rw(h)?;
        self.writable()?;
        let mut i = self.inode(ino)?;
        permit(frozen(&i))?;
        if algo.is_some() && i.flags & FL_NOCOW as u64 != 0 {
            return Err(Error::Invalid);
        }
        set_inode_compression(&mut i, algo);
        i.flags &= !((FL_COMPR | FL_NOCOMP) as u64);
        if algo.is_some() {
            i.flags |= FL_COMPR as u64;
        }
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
    }

    /// Disk and file bytes of `h`'s data (`compsize`): each extent counted
    /// once however many pieces of it the file maps.
    /// Data pieces of the file with handle `h` (see
    /// [`EclipseFS::fiemap`](crate::EclipseFS::fiemap)).
    pub fn fiemap(&mut self, h: u64, start: u64, len: u64) -> Result<Vec<crate::fs::FileMapping>> {
        use crate::fs::*;
        let ino = self.at(h)?;
        let inode = self.inode(ino)?;
        if inode.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        let mut out = Vec::new();
        let end = start.saturating_add(len);
        if len == 0 || start >= inode.size {
            return Ok(out);
        }
        if inode.size <= INLINE_MAX as u64 {
            if let Some(v) = self.inline_data(ino)? {
                out.push(FileMapping {
                    logical: 0,
                    physical: 0,
                    length: v.len() as u64,
                    flags: FIEMAP_EXTENT_DATA_INLINE
                        | FIEMAP_EXTENT_NOT_ALIGNED
                        | FIEMAP_EXTENT_LAST,
                });
                return Ok(out);
            }
        }
        let last_blk = inode.size.div_ceil(BS);
        let to = end.div_ceil(BS).min(last_blk);
        let mut cur = start / BS;
        while cur < to {
            let batch = self.extents_upto(ino, cur, to, EXTENT_BATCH)?;
            let Some(last) = batch.last() else {
                break;
            };
            cur = last.lblk + last.blocks;
            self.fiemap_batch(ino, batch, &mut out)?;
        }
        // LAST goes on the piece that ends the file's mapped data, which
        // the range asked for may not reach.
        if let Some(m) = out.last_mut() {
            let from = m.logical / BS + m.length / BS;
            if self.extents_upto(ino, from, last_blk.max(1), 1)?.is_empty() {
                m.flags |= FIEMAP_EXTENT_LAST;
            }
        }
        Ok(out)
    }

    fn fiemap_batch(
        &mut self,
        ino: u64,
        batch: Vec<Ext>,
        out: &mut Vec<crate::fs::FileMapping>,
    ) -> Result<()> {
        use crate::fs::*;
        for e in batch {
            let mut flags = 0;
            if e.prealloc {
                flags |= FIEMAP_EXTENT_UNWRITTEN;
            }
            let (physical, disk_blocks) = match e.z {
                Some(z) => {
                    flags |= FIEMAP_EXTENT_ENCODED;
                    (e.pblk * BS, z.disk_blocks)
                }
                None => (e.pblk * BS, e.blocks),
            };
            let shared = (e.pblk..e.pblk + disk_blocks).any(|b| self.sp.refs.get(b) > 1)
                || self
                    .st
                    .path_shared(&self.sp, &self.fs_root, &Self::ext_key(ino, e.lblk))?;
            if shared {
                flags |= FIEMAP_EXTENT_SHARED;
            }
            out.push(FileMapping {
                logical: e.lblk * BS,
                physical,
                length: e.blocks * BS,
                flags,
            });
        }
        Ok(())
    }

    /// The first data piece at or after byte `off` of inode `ino`, as a
    /// `(start, end)` byte range clipped to the size (adjacent extents
    /// joined), with the size; preallocated space is a hole.
    ///
    /// Reads extents a batch at a time and stops at the first gap after the
    /// piece: `lseek(SEEK_DATA/SEEK_HOLE)` on a large file used to read
    /// every extent from `off` to the end on each call.
    fn first_data_range(&mut self, ino: u64, off: u64) -> Result<(u64, Option<(u64, u64)>)> {
        let inode = self.inode(ino)?;
        if inode.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        let size = inode.size;
        if off >= size {
            return Ok((size, None));
        }
        if size <= INLINE_MAX as u64 && self.inline_data(ino)?.is_some() {
            return Ok((size, Some((0, size))));
        }
        let last_blk = size.div_ceil(BS);
        let mut found: Option<(u64, u64)> = None;
        let mut cur = off / BS;
        while cur < last_blk {
            let batch = self.extents_upto(ino, cur, last_blk, EXTENT_BATCH)?;
            let Some(last) = batch.last() else {
                break;
            };
            cur = last.lblk + last.blocks;
            for e in batch {
                if e.prealloc {
                    if found.is_some() {
                        return Ok((size, found));
                    }
                    continue;
                }
                let s = (e.lblk * BS).max(off);
                let t = ((e.lblk + e.blocks) * BS).min(size);
                if s >= t {
                    continue;
                }
                match &mut found {
                    None => found = Some((s, t)),
                    Some(r) if r.1 == s => r.1 = t,
                    Some(_) => return Ok((size, found)),
                }
            }
        }
        Ok((size, found))
    }

    pub fn seek_data(&mut self, h: u64, off: u64) -> Result<Option<u64>> {
        let ino = self.at(h)?;
        let (_, first) = self.first_data_range(ino, off)?;
        Ok(first.map(|(s, _)| s.max(off)))
    }

    pub fn seek_hole(&mut self, h: u64, off: u64) -> Result<Option<u64>> {
        let ino = self.at(h)?;
        let (size, first) = self.first_data_range(ino, off)?;
        if off >= size {
            return Ok(None);
        }
        Ok(Some(match first {
            Some((s, t)) if s <= off => t,
            _ => off,
        }))
    }

    /// `btrfs filesystem defragment`: rewrite the fragmented data of file
    /// bytes `start..start+len` into contiguous extents. Runs of adjacent
    /// extents smaller than `threshold` bytes (0: 32 MiB) that are not one
    /// after the other on disk move to one new run; with `compress`, every
    /// extent not already compressed that way is rewritten compressed.
    /// Like btrfs, rewritten data stops being shared with snapshots and
    /// clones. Holes and preallocated space stay as they are. Returns the
    /// bytes rewritten.
    pub fn defrag(
        &mut self,
        h: u64,
        start: u64,
        len: u64,
        threshold: u64,
        compress: Option<Compression>,
    ) -> Result<u64> {
        let ino = self.at_rw(h)?;
        self.writable()?;
        let inode = self.inode(ino)?;
        match inode.kind() {
            // btrfs defragments the metadata of a directory; the trees here
            // are rewritten whole on every change, so there is nothing to do.
            FileKind::Dir => return Ok(0),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
        permit(immutable(&inode))?;
        if len == 0 || start >= inode.size || self.inline_data(ino)?.is_some() {
            return Ok(0);
        }
        // Small writes waiting to be compressed settle first.
        if !self.nested {
            self.flush()?;
        }
        let mut inode = self.inode(ino)?;
        let before = inode.clone();
        // No-COW and encrypted data is never compressed.
        let nocow = inode.flags & FL_NOCOW as u64 != 0;
        let keys = self.file_keys(ino, &inode)?;
        let compress = compress.filter(|_| !nocow && keys.is_none());
        let threshold = match threshold {
            0 => DEFRAG_THRESHOLD,
            t => t,
        }
        .div_ceil(BS)
        .max(1);
        let first = start / BS;
        let last = start
            .saturating_add(len)
            .div_ceil(BS)
            .min(inode.size.div_ceil(BS));
        let wanted = |e: &Ext| match (compress, e.z) {
            _ if e.prealloc => false,
            (Some(algo), z) => z.map(|z| z.algo) != Some(algo),
            (None, z) => z.is_none() && e.blocks < threshold,
        };
        // Windows of adjacent extents worth rewriting, as (first block,
        // blocks, pieces on disk). The extents are read a batch at a time
        // and the windows a batch closes are rewritten before the next one
        // is read; the last stays open, as the next batch may extend it.
        let mut windows: Vec<(u64, u64, u64)> = Vec::new();
        let mut prev: Option<Ext> = None;
        let mut rewritten = 0;
        let mut cur = first;
        loop {
            let batch = if cur < last {
                self.extents_upto(ino, cur, last, EXTENT_BATCH)?
            } else {
                Vec::new()
            };
            let scanned = batch.is_empty();
            if let Some(e) = batch.last() {
                cur = e.lblk + e.blocks;
            }
            for mut e in batch {
                // Clip to the range asked for.
                if e.lblk < first {
                    let cut = first - e.lblk;
                    if e.z.is_none() {
                        e.pblk += cut;
                    }
                    e.lblk = first;
                    e.blocks -= cut;
                }
                e.blocks = e.blocks.min(last - e.lblk);
                if !wanted(&e) {
                    prev = None;
                    continue;
                }
                let after = prev.map(|p| p.lblk + p.blocks == e.lblk).unwrap_or(false);
                let joined = after
                    && prev.map(|p| p.z.is_none() && e.z.is_none() && p.pblk + p.blocks == e.pblk)
                        == Some(true);
                match windows.last_mut() {
                    Some(w) if after && w.1 + e.blocks <= DEFRAG_WINDOW => {
                        w.1 += e.blocks;
                        if !joined {
                            w.2 += 1;
                        }
                    }
                    _ => windows.push((e.lblk, e.blocks, 1)),
                }
                prev = Some(e);
            }
            let open = usize::from(!scanned);
            let ready: Vec<_> = windows
                .drain(..windows.len().saturating_sub(open))
                .collect();
            for (lb, n, pieces) in ready {
                if compress.is_none() && pieces < 2 {
                    continue;
                }
                let mut buf = vec![0u8; (n * BS) as usize];
                self.read_blocks(ino, lb * BS, &mut buf)?;
                self.zcache = None;
                let mut done = 0;
                if let Some(algo) = compress {
                    // Chunks of up to ZCHUNK blocks; a single block does not
                    // shrink by compressing.
                    while n - done >= 2 {
                        let c = ZCHUNK.min(n - done);
                        let at = (done * BS) as usize;
                        let chunk = &buf[at..at + (c * BS) as usize];
                        if !self.write_packed(ino, &mut inode, lb + done, chunk, algo)? {
                            break;
                        }
                        done += c;
                    }
                }
                // The rest, as one run; when no run that long is free the data
                // stays where it is.
                let rest = n - done;
                if rest > 0 {
                    let goal = self.goal(ino, lb + done)?;
                    if let Some(pb) = self.sp.alloc_data_run(goal, rest) {
                        let data = &mut buf[(done * BS) as usize..];
                        if let Some(k) = &keys {
                            k.encrypt_blocks(lb + done, data, BS as usize);
                        }
                        self.raw.write_at(pb * BS, data)?;
                        for (i, block) in data.chunks(BS as usize).enumerate() {
                            let c = (!nocow).then(|| crc32c(block));
                            self.sp.csums.insert(pb + i as u64, c);
                        }
                        self.set_mapping(ino, &mut inode, lb + done, rest, pb)?;
                        done = n;
                    }
                }
                rewritten += done * BS;
            }
            if scanned {
                break;
            }
        }
        // Rewriting keeps the size; compressed pieces must not stretch it.
        inode.size = before.size;
        if inode != before {
            self.put_inode(ino, inode)?;
        }
        self.commit()?;
        Ok(rewritten)
    }

    pub fn compsize(&mut self, h: u64) -> Result<CompSize> {
        let ino = self.at(h)?;
        self.inode(ino)?;
        let mut out = CompSize::default();
        let mut seen = BTreeSet::new();
        let hi = Key::new(ino, EXTENT_DATA, u64::MAX);
        let mut lo = Key::new(ino, EXTENT_DATA, 0);
        loop {
            let items = self.st.range(&self.fs_root, &lo, &hi, EXTENT_BATCH)?;
            let Some(last) = items.last().map(|(k, _)| *k) else {
                break;
            };
            for (_, d) in items {
                Self::compsize_item(&mut out, &mut seen, &d)?;
            }
            match key_after(&last) {
                Some(next) => lo = next,
                None => break,
            }
        }
        Ok(out)
    }

    fn compsize_item(out: &mut CompSize, seen: &mut BTreeSet<u64>, d: &[u8]) -> Result<()> {
        match FileExtent::decode(d)? {
            FileExtent::Reg { blocks, .. } => {
                out.disk_bytes += blocks * BS;
                out.uncompressed_bytes += blocks * BS;
            }
            FileExtent::Comp(c) => {
                out.compressed_extents += 1;
                if seen.insert(c.disk) {
                    out.disk_bytes += c.disk_blocks * BS;
                    out.uncompressed_bytes += c.ram_blocks * BS;
                }
            }
            FileExtent::Inline(v) => {
                out.disk_bytes += v.len() as u64;
                out.uncompressed_bytes += v.len() as u64;
            }
            FileExtent::Prealloc { .. } => {}
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Quotas
    // -----------------------------------------------------------------------

    fn quota_on(&self) -> bool {
        self.sb.features & FEAT_QUOTA != 0
    }

    /// File blocks among `from..to` of `ino` that something maps.
    fn mapped(&mut self, ino: u64, from: u64, to: u64) -> Result<u64> {
        let mut n = 0;
        let mut cur = from;
        while cur < to {
            let batch = self.extents_upto(ino, cur, to, EXTENT_BATCH)?;
            let Some(last) = batch.last() else {
                break;
            };
            n += covered(&batch, cur, to);
            cur = last.lblk + last.blocks;
        }
        Ok(n)
    }

    /// Refuse adding `blocks` file blocks to the current subvolume if
    /// that takes it past its limit (EDQUOT, as `NoSpace`).
    fn charge(&mut self, blocks: u64) -> Result<()> {
        let it = &self.fs_item;
        if self.quota_on()
            && it.limit != 0
            && blocks > 0
            && it.referenced.saturating_add(blocks * BS) > it.limit
        {
            return Err(Error::NoSpace);
        }
        Ok(())
    }

    pub fn quota_enabled(&self) -> bool {
        self.quota_on()
    }

    /// Turn quotas on (`btrfs quota enable`): every subvolume's counter is
    /// recounted and kept from here on.
    pub fn quota_enable(&mut self) -> Result<()> {
        self.writable()?;
        self.sb.features |= FEAT_QUOTA;
        self.quota_rescan_items()?;
        self.dev.write_at(0, &self.sb.encode())?;
        self.flush()
    }

    /// Turn quotas off: counters stop being kept and limits stop applying
    /// (they are kept for the next `quota_enable`).
    pub fn quota_disable(&mut self) -> Result<()> {
        self.writable()?;
        self.sb.features &= !FEAT_QUOTA;
        self.dev.write_at(0, &self.sb.encode())?;
        self.flush()
    }

    /// Recount every subvolume (`btrfs quota rescan`).
    pub fn quota_rescan(&mut self) -> Result<()> {
        self.writable()?;
        if !self.quota_on() {
            return Err(Error::Unsupported("quotas are off"));
        }
        self.quota_rescan_items()?;
        self.flush()
    }

    fn quota_rescan_items(&mut self) -> Result<()> {
        let here = self.subvol;
        let ids: Vec<u64> = self.current_root_items()?.into_keys().collect();
        for id in ids {
            self.enter(id)?;
            let mut total = 0u64;
            for (k, d) in self.st.range(
                &self.fs_root,
                &Key::new(0, INODE_ITEM, 0),
                &Key::MAX,
                usize::MAX,
            )? {
                if k.ty == INODE_ITEM {
                    total += InodeItem::decode(&d)?.nbytes;
                }
            }
            self.fs_item.referenced = total;
        }
        self.enter(here)
    }

    /// Most file bytes subvolume `id` may reference, or no limit
    /// (`btrfs qgroup limit`). Quotas must be on.
    pub fn set_qgroup_limit(&mut self, id: u64, limit: Option<u64>) -> Result<()> {
        self.writable()?;
        if !self.quota_on() {
            return Err(Error::Unsupported("quotas are off"));
        }
        if limit == Some(0) {
            return Err(Error::Invalid);
        }
        self.enter(id)?;
        if self.fs_item.flags & ROOT_FLAG_RECEIVING != 0 {
            return Err(Error::NotFound);
        }
        self.fs_item.limit = limit.unwrap_or(0);
        self.dev.write_at(0, &self.sb.encode())?;
        self.flush()
    }

    /// Every subvolume's quota group (`btrfs qgroup show`): its counter
    /// and limit, and the data blocks it references on disk, alone or
    /// shared with other subvolumes.
    pub fn qgroup_show(&mut self) -> Result<Vec<QgroupInfo>> {
        if !self.quota_on() {
            return Err(Error::Unsupported("quotas are off"));
        }
        let items = self.current_root_items()?;
        // Data blocks each subvolume references: (start, +1 / -1, id).
        let mut edges: Vec<(u64, i8, u64)> = Vec::new();
        let mut out = Vec::new();
        for (&id, it) in &items {
            if it.flags & ROOT_FLAG_RECEIVING != 0 {
                continue;
            }
            let root = if id == self.subvol {
                self.fs_root
            } else {
                match self.subvols.get(&id) {
                    Some((r, _)) => *r,
                    None => it.root,
                }
            };
            let mut ranges = RangeSet::default();
            for (k, d) in self
                .st
                .range(&root, &Key::new(0, 0, 0), &Key::MAX, usize::MAX)?
            {
                if k.ty != EXTENT_DATA {
                    continue;
                }
                if let Some((s, l)) = extent_of(&k, &d) {
                    ranges.insert(s, l);
                }
            }
            let mut disk = 0;
            for (s, l) in ranges.iter() {
                disk += l * BS;
                edges.push((s, 1, id));
                edges.push((s + l, -1, id));
            }
            out.push(QgroupInfo {
                id,
                referenced: it.referenced,
                limit: (it.limit != 0).then_some(it.limit),
                disk_referenced: disk,
                exclusive: 0,
            });
        }
        // Sweep: a stretch only one subvolume references is exclusive.
        edges.sort_unstable();
        let mut active: BTreeMap<u64, u32> = BTreeMap::new();
        let mut excl: BTreeMap<u64, u64> = BTreeMap::new();
        let mut at = 0;
        for (pos, d, id) in edges {
            if active.len() == 1 {
                let only = *active.keys().next().unwrap();
                *excl.entry(only).or_default() += (pos - at) * BS;
            }
            at = pos;
            if d > 0 {
                *active.entry(id).or_default() += 1;
            } else if let Some(c) = active.get_mut(&id) {
                *c -= 1;
                if *c == 0 {
                    active.remove(&id);
                }
            }
        }
        for q in &mut out {
            q.exclusive = excl.get(&q.id).copied().unwrap_or(0);
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // fallocate
    // -----------------------------------------------------------------------

    pub fn fallocate(&mut self, h: u64, mode: u32, offset: u64, len: u64) -> Result<()> {
        if mode & !(FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE | FALLOC_FL_ZERO_RANGE) != 0 {
            return Err(Error::Unsupported("fallocate mode"));
        }
        let punch = mode & FALLOC_FL_PUNCH_HOLE != 0;
        let zero = mode & FALLOC_FL_ZERO_RANGE != 0;
        let keep = mode & FALLOC_FL_KEEP_SIZE != 0;
        if len == 0 || punch && (!keep || zero) {
            return Err(Error::Invalid);
        }
        let end = offset.checked_add(len).ok_or(Error::Invalid)?;
        let ino = self.at_rw(h)?;
        self.writable()?;
        let mut i = self.inode(ino)?;
        match i.kind() {
            FileKind::Regular => {}
            FileKind::Dir => return Err(Error::IsDir),
            _ => return Err(Error::Invalid),
        }
        permit(frozen(&i))?;
        if !punch {
            let (from, to) = (offset / BS, end.div_ceil(BS));
            let holes = to - from - self.mapped(ino, from, to)?;
            self.charge(holes)?;
        }
        let r = (|| {
            if self.inline_data(ino)?.is_some() {
                self.uninline(ino, &mut i)?;
            }
            if punch || zero {
                self.punch(ino, &mut i, offset, end)?;
            }
            if !punch {
                self.preallocate(ino, &mut i, offset, end)?;
                if !keep && end > i.size {
                    i.size = end;
                }
            }
            Ok(())
        })();
        let now = self.now();
        i.mtime = now;
        i.ctime = now;
        self.put_inode(ino, i)?;
        self.commit()?;
        r
    }

    /// Make bytes `off..end` (within the file) read as zeros, giving back
    /// the whole blocks among them.
    fn punch(&mut self, ino: u64, i: &mut InodeItem, off: u64, end: u64) -> Result<()> {
        let end = end.min(i.size);
        if off >= end {
            return Ok(());
        }
        let first = off.div_ceil(BS);
        let last = end / BS;
        let size = i.size;
        // Partial blocks at the edges get zeros written.
        let head_end = (first * BS).min(end);
        if off < head_end {
            let z = vec![0u8; (head_end - off) as usize];
            self.write_extents(ino, i, off, &z)?;
        }
        let tail_start = (last * BS).max(head_end);
        if tail_start < end {
            let z = vec![0u8; (end - tail_start) as usize];
            self.write_extents(ino, i, tail_start, &z)?;
        }
        i.size = size;
        if first < last {
            self.unmap(ino, i, first, last)?;
        }
        Ok(())
    }

    /// Reserve blocks for the holes among file blocks covering `off..end`.
    fn preallocate(&mut self, ino: u64, i: &mut InodeItem, off: u64, end: u64) -> Result<()> {
        let mut lb = off / BS;
        let last = end.div_ceil(BS);
        while lb < last {
            let chunk_end = (lb + WRITE_CHUNK).min(last);
            let exts = self.extents(ino, lb, chunk_end)?;
            if let Some(e) = exts.iter().find(|e| e.lblk <= lb && lb < e.lblk + e.blocks) {
                lb = e.lblk + e.blocks;
                continue;
            }
            let hole_end = exts
                .iter()
                .map(|e| e.lblk)
                .filter(|&s| s > lb)
                .min()
                .unwrap_or(chunk_end)
                .min(chunk_end);
            let goal = self.goal(ino, lb)?;
            let (p, n) = self
                .sp
                .alloc_data(goal, hole_end - lb)
                .ok_or(Error::NoSpace)?;
            self.map(ino, i, lb, n, p, true)?;
            lb += n;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Public API (same as `btrfs::Btrfs`): inode numbers are handles
    // -----------------------------------------------------------------------

    pub fn stat(&mut self, h: u64) -> Result<InodeStat> {
        let ino = self.at(h)?;
        let mut s = self.stat_l(ino)?;
        s.ino = h;
        Ok(s)
    }

    pub fn set_attr(
        &mut self,
        h: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        atime: Option<(u64, u32)>,
        mtime: Option<(u64, u32)>,
    ) -> Result<()> {
        let ino = self.at_rw(h)?;
        self.set_attr_l(ino, mode, uid, gid, atime, mtime)
    }

    pub fn lookup(&mut self, dir: u64, name: &str) -> Result<u64> {
        let d = self.at(dir)?;
        let ino = self.lookup_l(d, name)?;
        Ok(self.out(ino))
    }

    pub fn readdir(&mut self, dir: u64) -> Result<Vec<DirEntry>> {
        let d = self.at(dir)?;
        let mut v = self.readdir_l(d)?;
        for e in &mut v {
            e.ino = self.out(e.ino);
        }
        Ok(v)
    }

    pub fn create(
        &mut self,
        dir: u64,
        name: &str,
        kind: FileKind,
        mode: u32,
        rdev: u64,
    ) -> Result<u64> {
        let d = self.at_rw(dir)?;
        let ino = self.create_l(d, name, kind, mode, rdev)?;
        Ok(self.out(ino))
    }

    pub fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<u64> {
        let d = self.at_rw(dir)?;
        let ino = self.symlink_l(d, name, target)?;
        Ok(self.out(ino))
    }

    pub fn write_symlink(&mut self, h: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let ino = self.at_rw(h)?;
        self.write_symlink_l(ino, offset, data)
    }

    pub fn read_link(&mut self, h: u64) -> Result<Vec<u8>> {
        let ino = self.at(h)?;
        self.read_link_l(ino)
    }

    pub fn parent(&mut self, h: u64) -> Result<u64> {
        let (sv, ino) = split(h);
        if ino == FIRST_INO {
            if h == self.root_ino() {
                return Ok(h);
            }
            return match self.backref(sv)? {
                Some((p, e)) => Ok(handle(p, e.ino)),
                // The top subvolume (when another one is mounted as `/`).
                None if sv == FS_TREE => Ok(h),
                None => Err(Error::NotFound),
            };
        }
        self.enter(sv)?;
        let p = self.parent_l(ino)?;
        Ok(self.out(p))
    }

    pub fn link(&mut self, dir: u64, name: &str, h: u64) -> Result<()> {
        Self::same_subvol(dir, h)?;
        let d = self.at_rw(dir)?;
        self.link_l(d, name, split(h).1)
    }

    pub fn unlink(&mut self, dir: u64, name: &str) -> Result<()> {
        let n = check_name(name)?;
        let d = self.at_rw(dir)?;
        if let Some(e) = self.dir_lookup(d, n)? {
            if is_link(e.ino) {
                // rmdir of a subvolume: only when it is empty.
                self.writable()?;
                permit(frozen(&self.inode(d)?))?;
                let (id, _) = split(e.ino);
                let parent = self.subvol;
                self.enter(id)?;
                let empty = self.dir_is_empty(FIRST_INO)?;
                self.enter(parent)?;
                if !empty {
                    return Err(Error::NotEmpty);
                }
                return self.drop_subvol(d, n, id);
            }
        }
        self.unlink_l(d, name)
    }

    pub fn rename(
        &mut self,
        old_dir: u64,
        old_name: &str,
        new_dir: u64,
        new_name: &str,
    ) -> Result<()> {
        Self::same_subvol(old_dir, new_dir)?;
        let od = self.at_rw(old_dir)?;
        self.rename_l(od, old_name, split(new_dir).1, new_name)
    }

    pub fn read(&mut self, h: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let ino = self.at(h)?;
        self.read_l(ino, offset, buf)
    }

    pub fn write(&mut self, h: u64, offset: u64, data: &[u8]) -> Result<usize> {
        let ino = self.at_rw(h)?;
        self.write_l(ino, offset, data)
    }

    pub fn truncate(&mut self, h: u64, new_size: u64) -> Result<()> {
        let ino = self.at_rw(h)?;
        self.truncate_l(ino, new_size)
    }

    /// Whether [`write_atomic`](Self::write_atomic) works on file `h`: a
    /// regular file that is not no-COW (those are rewritten in place).
    pub fn atomic_write_supported(&mut self, h: u64) -> Result<bool> {
        let ino = self.at(h)?;
        let i = self.inode(ino)?;
        Ok(i.kind() == FileKind::Regular && i.flags & FL_NOCOW as u64 == 0)
    }

    /// Write all of `data` at `offset` or nothing (`RWF_ATOMIC`): never a
    /// short write, and a crash shows the old bytes or the new ones, never
    /// a mix (copy-on-write and the fsync log already give that to every
    /// write; what this adds is the up-front space check). At most
    /// [`ATOMIC_WRITE_MAX`] bytes.
    pub fn write_atomic(&mut self, h: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if data.is_empty() || data.len() as u64 > ATOMIC_WRITE_MAX {
            return Err(Error::Invalid);
        }
        if !self.atomic_write_supported(h)? {
            return Err(Error::Unsupported("atomic write on this file"));
        }
        let ino = self.at_rw(h)?;
        self.writable()?;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(Error::Invalid)?;
        // Every block of the range is rewritten, plus the inline data a
        // small file may have to move out.
        let need = end.div_ceil(BS) - offset / BS + 1;
        if !self.sp.ensure_data_room(need) {
            return Err(Error::NoSpace);
        }
        let n = self.write_l(ino, offset, data)?;
        if n != data.len() {
            // The space was there a moment ago: nothing else ran since.
            warn!(
                "eclipsefs: atomic write came out short ({} of {})",
                n,
                data.len()
            );
            return Err(Error::Io);
        }
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // Consistency check
    // -----------------------------------------------------------------------

    /// Full consistency check (fsck without repair), see
    /// [`EclipseFS::check`](crate::EclipseFS::check).
    pub fn check(&mut self) -> Result<CheckReport> {
        if !self.read_only {
            self.flush()?;
        }
        let total = self.sb.total_blocks;
        // Owners seen per block (nodes and data).
        let mut owners: BTreeMap<u64, u32> = BTreeMap::new();
        fn own(owners: &mut BTreeMap<u64, u32>, total: u64, s: u64, l: u64) -> Result<u32> {
            let mut first = 0;
            for b in s..s.checked_add(l).ok_or(Error::Corrupt("fsck: extent"))? {
                if b >= total {
                    return Err(Error::Corrupt("fsck: block past the end"));
                }
                let c = owners.entry(b).or_default();
                *c += 1;
                if b == s {
                    first = *c;
                }
            }
            Ok(first)
        }
        for g in self.sp.alloc.groups().iter().filter(|g| !g.is_hole()) {
            own(&mut owners, total, g.start_block, 2)?;
        }
        own(&mut owners, total, self.journal.start, self.journal.blocks)?;
        if let Some((at, n)) = self.sb.bitmap_sums {
            own(&mut owners, total, at, n as u64)?;
        }
        self.check_bitmap_sums()?;
        if let Some(v) = self.vol.clone() {
            let map = v.map();
            map.check()?;
            let vs = self
                .sb
                .vol
                .as_ref()
                .ok_or(Error::Corrupt("fsck: volume superblock"))?;
            if map.chunks.get(&0) != Some(&vs.sys) || vs.num_devices as usize != map.devices.len() {
                return Err(Error::Corrupt("fsck: system chunk"));
            }
            own(
                &mut owners,
                total,
                vs.chunk_map,
                (vs.chunk_map_len as u64).div_ceil(BS),
            )?;
            if vs.chunk_map + (vs.chunk_map_len as u64).div_ceil(BS) > vs.sys.blocks {
                return Err(Error::Corrupt("fsck: chunk map outside group 0"));
            }
            let mut ok = map.chunks.len()
                == self
                    .sp
                    .alloc
                    .groups()
                    .iter()
                    .filter(|g| !g.is_hole())
                    .count();
            for (&g, c) in &map.chunks {
                ok &= self
                    .sp
                    .alloc
                    .groups()
                    .get(g as usize)
                    .is_some_and(|grp| grp.block_count == c.blocks && grp.kind == c.kind);
            }
            if !ok {
                return Err(Error::Corrupt("fsck: chunks and groups disagree"));
            }
        }
        // Trees: a node shared by several parents is counted once per parent
        // and walked once.
        let mut roots = vec![self.root_tree];
        for (k, d) in self.st.range(
            &self.root_tree,
            &Key::new(0, ROOT_ITEM, 0),
            &Key::MAX,
            usize::MAX,
        )? {
            if k.ty == ROOT_ITEM {
                roots.push(RootItem::decode(&d)?.root);
            }
        }
        let mut stack: Vec<(u64, u8, Option<u64>)> = roots
            .iter()
            .map(|r| (r.bytenr, r.level, Some(r.gen)))
            .collect();
        let mut data = RangeSet::default();
        let mut summed = RangeSet::default();
        while let Some((b, level, gen)) = stack.pop() {
            if own(&mut owners, total, b, 1)? > 1 {
                continue;
            }
            let node = self.st.node(b, level, gen)?.clone();
            match node.body {
                Body::Internal(ptrs) => {
                    for p in ptrs {
                        stack.push((p.bytenr, level - 1, Some(p.gen)));
                    }
                }
                Body::Leaf(items) => {
                    for (k, d) in &items {
                        if let Some((s, l)) = extent_of(k, d) {
                            own(&mut owners, total, s, l)?;
                            if matches!(
                                FileExtent::decode(d)?,
                                FileExtent::Reg { .. } | FileExtent::Comp(_)
                            ) {
                                data.insert(s, l);
                            }
                        }
                        if k.ty == CSUM_ITEM {
                            for (i, c) in CsumBucket::decode(d)?.0.into_iter().enumerate() {
                                if c.is_some() {
                                    summed.insert(k.objectid + i as u64, 1);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Only written data blocks have checksums.
        if !summed.is_subset(&data) {
            return Err(Error::Corrupt("fsck: data checksums"));
        }
        let ranges: Vec<(u64, u64)> = self
            .sp
            .alloc
            .groups()
            .iter()
            .map(|g| (g.start_block, g.start_block + g.block_count))
            .collect();
        if owners.keys().any(|b| {
            let g = self.sp.alloc.groups().get((b / GROUP_BLOCKS) as usize);
            g.is_none_or(|g| b % GROUP_BLOCKS >= g.block_count)
        }) {
            return Err(Error::Corrupt("fsck: block outside every chunk"));
        }
        for b in ranges.into_iter().flat_map(|(s, e)| s..e) {
            let seen = owners.get(&b).copied().unwrap_or(0);
            if (seen > 0) != self.sp.alloc.is_used(b) {
                return Err(Error::Corrupt(if seen > 0 {
                    "fsck: block in use marked free"
                } else {
                    "fsck: block marked used but unowned"
                }));
            }
            if seen > 0 && seen != self.sp.refs.get(b) {
                return Err(Error::Corrupt("fsck: reference count"));
            }
        }
        if self.sb.free_blocks != self.sp.alloc.free_blocks() {
            return Err(Error::Corrupt("fsck: free block count"));
        }
        let blocks_used = owners.len() as u64;

        // Every subvolume, and the entries linking them together.
        let mut inode_total = 0;
        let mut dir_total = 0;
        let mut links = BTreeSet::new();
        let subvols = self.root_items()?;
        for (id, item) in &subvols {
            if item.flags & ROOT_FLAG_RECEIVING != 0 {
                // Half received: checked when the receive ends.
                continue;
            }
            let (n, d, l, need) = self.check_subvol(item)?;
            // Every block written to a file that keeps checksums has one.
            if !need.is_subset(&summed) {
                return Err(Error::Corrupt("fsck: data checksums"));
            }
            inode_total += n;
            dir_total += d;
            for (dir, index, name, target) in l {
                links.insert((target, *id, dir, index, name));
            }
        }
        let mut backs = BTreeSet::new();
        for (id, p, e) in self.backrefs()? {
            backs.insert((id, p, e.ino, e.index, e.name));
        }
        if links != backs {
            return Err(Error::Corrupt(
                "fsck: subvolume entries and backrefs differ",
            ));
        }
        for (id, item) in &subvols {
            let named = backs.iter().filter(|b| b.0 == *id).count();
            let receiving = item.flags & ROOT_FLAG_RECEIVING != 0;
            if named != usize::from(*id != FS_TREE && !receiving) {
                return Err(Error::Corrupt("fsck: subvolume not named once"));
            }
        }
        if backs
            .iter()
            .any(|b| !subvols.iter().any(|(id, _)| *id == b.0))
        {
            return Err(Error::Corrupt("fsck: entry for a missing subvolume"));
        }
        Ok(CheckReport {
            inodes: inode_total,
            directories: dir_total,
            blocks_used,
        })
    }

    /// Namespace of one subvolume: (inodes, directories, entries naming
    /// other subvolumes as (directory, index, name, subvolume)).
    #[allow(clippy::type_complexity)]
    fn check_subvol(
        &mut self,
        item: &RootItem,
    ) -> Result<(u64, u64, Vec<(u64, u64, Vec<u8>, u64)>, RangeSet)> {
        let mut links = Vec::new();
        // Blocks of files that keep checksums (an inode's item comes
        // before its extents).
        let mut need = RangeSet::default();
        let mut inodes: BTreeMap<u64, InodeItem> = BTreeMap::new();
        let mut names: BTreeMap<u64, u32> = BTreeMap::new();
        let mut refs: BTreeMap<u64, u32> = BTreeMap::new();
        let mut subdirs: BTreeMap<u64, u32> = BTreeMap::new();
        let mut bytes: BTreeMap<u64, u64> = BTreeMap::new();
        let mut index_names: BTreeSet<(u64, u64, Vec<u8>, u64)> = BTreeSet::new();
        let mut item_names: BTreeSet<(u64, u64, Vec<u8>, u64)> = BTreeSet::new();
        let mut last_ext_end: BTreeMap<u64, u64> = BTreeMap::new();
        let mut xattr_owners: BTreeSet<u64> = BTreeSet::new();
        let mut contexts: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        // Inodes with inline or compressed data.
        let mut packed: BTreeSet<u64> = BTreeSet::new();
        // A batch of items at a time: a file of tens of GB has millions.
        let mut lo = Key::MIN;
        loop {
            let items = self.st.range(&item.root, &lo, &Key::MAX, PURGE_BATCH)?;
            let Some(last) = items.last().map(|(k, _)| *k) else {
                break;
            };
            for (k, d) in &items {
                let summed = inodes
                    .get(&k.objectid)
                    .is_some_and(|i| i.flags & FL_NOCOW as u64 == 0);
                match k.ty {
                    INODE_ITEM => {
                        inodes.insert(k.objectid, InodeItem::decode(d)?);
                    }
                    ENC_CONTEXT => {
                        if k.offset != 0 {
                            return Err(Error::Corrupt("fsck: encryption context key"));
                        }
                        contexts.insert(k.objectid, d.clone());
                    }
                    INODE_REF => {
                        *refs.entry(k.objectid).or_default() += 1;
                        let e = decode_pair(d)?;
                        if k.offset != ref_hash(e.ino, e.index) {
                            return Err(Error::Corrupt("fsck: inode ref key"));
                        }
                    }
                    DIR_ITEM => {
                        for e in decode_dir_item(d)? {
                            if name_hash(&e.name) != k.offset {
                                return Err(Error::Corrupt("fsck: name hash"));
                            }
                            item_names.insert((k.objectid, e.index, e.name, e.ino));
                        }
                    }
                    DIR_INDEX => {
                        let e = decode_pair(d)?;
                        if is_link(e.ino) {
                            if e.kind != FileKind::Dir || split(e.ino).1 != FIRST_INO {
                                return Err(Error::Corrupt("fsck: subvolume entry"));
                            }
                            links.push((k.objectid, k.offset, e.name.clone(), split(e.ino).0));
                            index_names.insert((k.objectid, k.offset, e.name, e.ino));
                            continue;
                        }
                        *names.entry(e.ino).or_default() += 1;
                        if e.kind == FileKind::Dir {
                            *subdirs.entry(k.objectid).or_default() += 1;
                        }
                        index_names.insert((k.objectid, k.offset, e.name, e.ino));
                    }
                    EXTENT_DATA => match FileExtent::decode(d)? {
                        FileExtent::Reg { disk, blocks } => {
                            if summed {
                                need.insert(disk, blocks);
                            }
                            let end = last_ext_end.entry(k.objectid).or_default();
                            if k.offset % BS != 0 || k.offset / BS < *end {
                                return Err(Error::Corrupt("fsck: overlapping extents"));
                            }
                            *end = k.offset / BS + blocks;
                            *bytes.entry(k.objectid).or_default() += blocks * BS;
                        }
                        FileExtent::Inline(v) => {
                            if k.offset != 0 {
                                return Err(Error::Corrupt("fsck: inline extent offset"));
                            }
                            if v.len() > INLINE_MAX {
                                return Err(Error::Corrupt("fsck: inline extent too large"));
                            }
                            packed.insert(k.objectid);
                            *bytes.entry(k.objectid).or_default() += v.len() as u64;
                        }
                        FileExtent::Comp(c) => {
                            packed.insert(k.objectid);
                            if summed {
                                need.insert(c.disk, c.disk_blocks);
                            }
                            let end = last_ext_end.entry(k.objectid).or_default();
                            if k.offset % BS != 0 || k.offset / BS < *end {
                                return Err(Error::Corrupt("fsck: overlapping extents"));
                            }
                            *end = k.offset / BS + c.blocks;
                            *bytes.entry(k.objectid).or_default() += c.blocks * BS;
                        }
                        FileExtent::Prealloc { blocks, .. } => {
                            let end = last_ext_end.entry(k.objectid).or_default();
                            if k.offset % BS != 0 || k.offset / BS < *end {
                                return Err(Error::Corrupt("fsck: overlapping extents"));
                            }
                            *end = k.offset / BS + blocks;
                            *bytes.entry(k.objectid).or_default() += blocks * BS;
                        }
                    },
                    XATTR_ITEM => {
                        let list = decode_xattrs(d)?;
                        if list.is_empty() || list.iter().any(|(n, _)| name_hash(n) != k.offset) {
                            return Err(Error::Corrupt("fsck: xattr item"));
                        }
                        xattr_owners.insert(k.objectid);
                    }
                    _ => return Err(Error::Corrupt("fsck: unknown item")),
                }
            }
            match key_after(&last) {
                Some(next) => lo = next,
                None => break,
            }
        }
        if index_names != item_names {
            return Err(Error::Corrupt("fsck: name indexes differ"));
        }
        if xattr_owners.iter().any(|o| !inodes.contains_key(o)) {
            return Err(Error::Corrupt("fsck: xattr of a missing inode"));
        }
        let root = item.root_dir;
        for (&ino, i) in &inodes {
            if ino >= item.next_ino || ino < FIRST_INO {
                return Err(Error::Corrupt("fsck: inode number"));
            }
            let n = names.get(&ino).copied().unwrap_or(0);
            let r = refs.get(&ino).copied().unwrap_or(0);
            let ok = if i.kind() == FileKind::Dir {
                n == u32::from(ino != root)
                    && i.nlink == 2 + subdirs.get(&ino).copied().unwrap_or(0)
            } else {
                n > 0 && i.nlink == n
            };
            if !ok || r != n {
                return Err(Error::Corrupt("fsck: link count"));
            }
            if bytes.get(&ino).copied().unwrap_or(0) != i.nbytes {
                return Err(Error::Corrupt("fsck: nbytes"));
            }
            if i.kind() != FileKind::Dir
                && i.kind() != FileKind::Regular
                && i.kind() != FileKind::Symlink
                && i.nbytes != 0
            {
                return Err(Error::Corrupt("fsck: data on a special file"));
            }
        }
        for (_, _, _, ino) in &index_names {
            if !is_link(*ino) && !inodes.contains_key(ino) {
                return Err(Error::Corrupt("fsck: entry for a free inode"));
            }
        }
        for &ino in bytes.keys() {
            if !inodes.contains_key(&ino) {
                return Err(Error::Corrupt("fsck: extent of a free inode"));
            }
        }
        if self.quota_on() && inodes.values().map(|i| i.nbytes).sum::<u64>() != item.referenced {
            return Err(Error::Corrupt("fsck: quota counter"));
        }
        let dirs: Vec<u64> = inodes
            .iter()
            .filter(|(_, i)| i.kind() == FileKind::Dir)
            .map(|(&n, _)| n)
            .collect();
        for &d in &dirs {
            let mut cur = d;
            let mut steps = 0;
            while cur != root {
                cur = inodes
                    .get(&cur)
                    .ok_or(Error::Corrupt("fsck: parent"))?
                    .parent;
                steps += 1;
                if steps > dirs.len() {
                    return Err(Error::Corrupt("fsck: directory cycle"));
                }
            }
        }
        for (dir, _, _, ino) in index_names.iter().filter(|e| !is_link(e.3)) {
            let i = &inodes[ino];
            if i.kind() == FileKind::Dir && i.parent != *dir {
                return Err(Error::Corrupt("fsck: directory parent"));
            }
        }
        if inodes.len() as u64 != item.inode_count {
            return Err(Error::Corrupt("fsck: inode count"));
        }
        if !contexts.is_empty() || inodes.values().any(|i| i.flags & FL_ENCRYPT as u64 != 0) {
            if self.sb.features & FEAT_ENCRYPT == 0 {
                return Err(Error::Corrupt("fsck: encryption without the feature"));
            }
            encrypt::check(&inodes, &contexts, &packed, &index_names)?;
        }
        if !inodes.contains_key(&root) {
            return Err(Error::Corrupt("fsck: root"));
        }
        Ok((inodes.len() as u64, dirs.len() as u64, links, need))
    }
}

impl CowFs {
    /// See [`EclipseFS::scrub`](crate::EclipseFS::scrub).
    pub fn scrub(&mut self) -> Result<ScrubReport> {
        if !self.read_only {
            self.flush()?;
        }
        let mut r = ScrubReport::default();
        let mut seen = BTreeSet::new();
        let mut data = RangeSet::default();
        let root = self.root_tree;
        let mut stack = vec![(root.bytenr, root.level, Some(root.gen))];
        while let Some((b, level, gen)) = stack.pop() {
            if !seen.insert(b) {
                continue;
            }
            r.tree_nodes += 1;
            let node = match self.scrub_node(b, level, gen, &mut r.repaired) {
                Ok(n) => n,
                Err(Error::Io) => return Err(Error::Io),
                Err(_) => {
                    r.bad_tree_nodes.push(b);
                    continue;
                }
            };
            match node.body {
                Body::Internal(ptrs) => {
                    for p in ptrs {
                        stack.push((p.bytenr, level - 1, Some(p.gen)));
                    }
                }
                Body::Leaf(items) => {
                    for (k, d) in &items {
                        if let Some((s, l)) = extent_of(k, d) {
                            // A damaged extent pointing past the end has
                            // nothing there to read (check reports it).
                            let total = self.sb.total_blocks;
                            if s < total
                                && matches!(
                                    FileExtent::decode(d),
                                    Ok(FileExtent::Reg { .. } | FileExtent::Comp(_))
                                )
                            {
                                data.insert(s, l.min(total - s));
                            }
                        } else if k.ty == ROOT_ITEM {
                            if let Ok(it) = RootItem::decode(d) {
                                stack.push((it.root.bytenr, it.root.level, Some(it.root.gen)));
                            }
                        }
                    }
                }
            }
        }
        let mut buf = vec![0u8; (CSUM_BUCKET * BS) as usize];
        for (start, len) in data.iter() {
            let mut b = start;
            while b < start + len {
                let n = (start + len - b).min(CSUM_BUCKET - b % CSUM_BUCKET);
                let chunk = &mut buf[..(n * BS) as usize];
                r.data_blocks += n;
                let sums = match self.csums(b, n) {
                    Ok(sums) => sums,
                    Err(Error::Io) => return Err(Error::Io),
                    // The checksums themselves are unreadable.
                    Err(_) => {
                        r.bad_data_blocks.extend(b..b + n);
                        b += n;
                        continue;
                    }
                };
                match self.vol.clone() {
                    None => {
                        self.raw.read_at(b * BS, chunk)?;
                        for (i, c) in sums.into_iter().enumerate() {
                            let block = &chunk[i * BS as usize..(i + 1) * BS as usize];
                            // No-COW files keep no checksums.
                            if c.is_some_and(|c| c != crc32c(block)) {
                                r.bad_data_blocks.push(b + i as u64);
                            }
                        }
                    }
                    Some(vol) => {
                        let (bad, fixed) = self.scrub_copies(&vol, b, chunk, |i, img| {
                            sums[i].is_none_or(|c| c == crc32c(img))
                        })?;
                        r.bad_data_blocks.extend(bad);
                        r.repaired += fixed;
                    }
                }
                b += n;
            }
        }
        if !r.is_clean() {
            warn!(
                "eclipsefs: scrub found {} bad tree nodes and {} bad data blocks",
                r.bad_tree_nodes.len(),
                r.bad_data_blocks.len()
            );
        }
        Ok(r)
    }
}

impl CowFs {
    /// A tree node as scrub sees it: on a volume every copy is read, the
    /// bad ones are rewritten from a good one (counted in `repaired`).
    fn scrub_node(
        &mut self,
        b: u64,
        level: u8,
        gen: Option<u64>,
        repaired: &mut u64,
    ) -> Result<Node> {
        let Some(vol) = self.vol.clone() else {
            return self.st.read_disk(b, level, gen);
        };
        let mut img = vec![0u8; BS as usize];
        let mut node = None;
        let (bad, fixed) = self.scrub_copies(&vol, b, &mut img, |_, d| {
            match self.st.check_node(d, b, level, gen) {
                Ok(n) => {
                    node.get_or_insert(n);
                    true
                }
                Err(_) => false,
            }
        })?;
        *repaired += fixed;
        match node {
            Some(n) if bad.is_empty() => Ok(n),
            _ => Err(Error::Corrupt("tree node")),
        }
    }

    /// Read every copy of blocks `b..` (as many as `buf` holds), judging
    /// each block image with `good(index, image)`; bad copies of a block
    /// with a good one are rewritten (unless mounted read-only) and `buf`
    /// gets the good images. Returns the blocks with no good copy and the
    /// copies rewritten.
    fn scrub_copies(
        &self,
        vol: &Volume,
        b: u64,
        buf: &mut [u8],
        mut good: impl FnMut(usize, &[u8]) -> bool,
    ) -> Result<(Vec<u64>, u64)> {
        let bs = BS as usize;
        let n = buf.len() / bs;
        let copies = vol.copies(b).max(1);
        let mut imgs = Vec::with_capacity(copies);
        for k in 0..copies {
            let mut img = vec![0u8; buf.len()];
            let ok = vol.read_copy(b * BS, &mut img, k).is_ok();
            imgs.push(ok.then_some(img));
        }
        let mut bad_blocks = Vec::new();
        let mut fixed = 0;
        for i in 0..n {
            let r = i * bs..(i + 1) * bs;
            let verdict: Vec<bool> = imgs
                .iter()
                .map(|img| img.as_ref().is_some_and(|img| good(i, &img[r.clone()])))
                .collect();
            let Some(k) = verdict.iter().position(|&v| v) else {
                bad_blocks.push(b + i as u64);
                continue;
            };
            let img = imgs[k]
                .as_ref()
                .map(|img| img[r.clone()].to_vec())
                .unwrap_or_default();
            buf[r.clone()].copy_from_slice(&img);
            for (j, ok) in verdict.iter().enumerate() {
                if !ok && !self.read_only && vol.write_copy((b + i as u64) * BS, &img, j).is_ok() {
                    fixed += 1;
                }
            }
        }
        if fixed > 0 {
            vol.sync()?;
        }
        Ok((bad_blocks, fixed))
    }
}

impl Drop for CowFs {
    fn drop(&mut self) {
        if !self.read_only {
            if let Err(e) = self.flush().and_then(|_| self.raw.sync()) {
                warn!("eclipsefs: commit on unmount failed: {:?}", e);
            }
        }
    }
}

/// Create an empty v0.4 filesystem (called by [`mkfs::format`](crate::mkfs::format)).
pub(crate) fn format(
    dev: &dyn BlockDevice,
    mut alloc: BlockAllocator,
    total: u64,
    journal: JournalArea,
    label: [u8; 64],
    uuid: [u8; 16],
    now: (u64, u32),
) -> Result<()> {
    alloc.set_deferred_frees(false);
    let mut sp = BlockSpace::new(alloc, RefMap::default());
    sp.meta_goal = journal.start + journal.blocks;
    let null: Arc<dyn BlockDevice> = Arc::new(NullDev(dev.size()));
    let gen = 1;
    let mut st = Store::new(null, uuid, gen, total, extent_of);
    let mut root_tree = st.create(&mut sp, ROOT_TREE, 0)?;
    let refs_root = st.create(&mut sp, REFS_TREE, 0)?;
    let csum_root = st.create(&mut sp, CSUM_TREE, 0)?;
    let mut fs_root = st.create(&mut sp, FS_TREE, 0)?;
    let mut dir = InodeItem::new(FileKind::Dir, 0o755, now, gen);
    dir.parent = FIRST_INO;
    st.insert(
        &mut sp,
        &mut fs_root,
        Key::new(FIRST_INO, INODE_ITEM, 0),
        dir.encode(),
    )?;
    let item = |root: Root, next_ino: u64, count: u64| RootItem {
        root,
        next_ino,
        inode_count: count,
        root_dir: FIRST_INO,
        flags: 0,
        otime: now,
        parent: 0,
        parent_gen: 0,
        ..Default::default()
    };
    // The root tree's own changes below do not move the other roots.
    st.insert(
        &mut sp,
        &mut root_tree,
        Key::new(REFS_TREE, ROOT_ITEM, 0),
        item(refs_root, 0, 0).encode(),
    )?;
    st.insert(
        &mut sp,
        &mut root_tree,
        Key::new(CSUM_TREE, ROOT_ITEM, 0),
        item(csum_root, 0, 0).encode(),
    )?;
    st.insert(
        &mut sp,
        &mut root_tree,
        Key::new(FS_TREE, ROOT_ITEM, 0),
        RootItem {
            uuid: subvol_uuid(&uuid, FS_TREE, gen, now),
            ..item(fs_root, FIRST_INO + 1, 1)
        }
        .encode(),
    )?;
    let nodes = st.take_dirty();
    write_home(dev, &nodes)?;
    let mut alloc = sp.alloc;
    let n = alloc.groups().len();
    let need = n.div_ceil(SUMS_PER_BLOCK).max(1) as u64;
    let bitmap_sums = alloc.groups_mut()[0]
        .allocate_run(2, need)
        .map(|at| (at, need as u32));
    let mut sums = Vec::with_capacity(n);
    for g in alloc.groups() {
        let bytes = g.to_bytes();
        dev.write_at(g.bitmap_block() * BS, &bytes)?;
        sums.push(crc32c(&bytes));
    }
    if let Some((at, _)) = bitmap_sums {
        for (k, part) in sums.chunks(SUMS_PER_BLOCK).enumerate() {
            dev.write_at((at + k as u64) * BS, &encode_sums_block(part))?;
        }
    }
    let sb = SuperV4 {
        total_blocks: total,
        free_blocks: alloc.free_blocks(),
        generation: gen,
        created: now.0,
        features: 0,
        uuid,
        label,
        root: root_tree,
        journal_start: journal.start,
        journal_blocks: journal.blocks as u32,
        default_subvol: FS_TREE,
        vol: None,
        bitmap_sums,
    };
    let enc = sb.encode();
    for g in alloc.groups().iter().skip(1) {
        dev.write_at(g.start_block * BS, &enc)?;
    }
    dev.sync()?;
    dev.write_at(0, &enc)?;
    dev.sync()
}

/// Stand-in device for mkfs's in-memory trees (nothing is read back).
struct NullDev(u64);

impl BlockDevice for NullDev {
    fn read_at(&self, _: u64, _: &mut [u8]) -> Result<()> {
        Err(Error::Io)
    }
    fn write_at(&self, _: u64, _: &[u8]) -> Result<()> {
        Err(Error::Io)
    }
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// `purge` re-opens its window at the successor of the last key it
    /// handled, so the step has to be the SMALLEST key above it: skip one and
    /// an item of the inode survives the delete.
    #[test]
    fn the_key_after_one_is_the_next_one_there_is() {
        assert_eq!(key_after(&Key::new(7, 3, 9)), Some(Key::new(7, 3, 10)));
    }

    /// Keys compare by `(objectid, ty, offset)`, so a full offset carries into
    /// the type and a full type into the objectid. Stepping the offset alone
    /// would loop on the same key for ever.
    #[test]
    fn a_full_field_carries_into_the_one_that_outranks_it() {
        assert_eq!(
            key_after(&Key::new(7, 3, u64::MAX)),
            Some(Key::new(7, 4, 0))
        );
        assert_eq!(
            key_after(&Key::new(7, u8::MAX, u64::MAX)),
            Some(Key::new(8, 0, 0))
        );
    }

    /// Nothing follows the largest key, and the caller has to stop rather than
    /// wrap round to the smallest one and start again.
    #[test]
    fn the_largest_key_has_no_successor() {
        assert_eq!(key_after(&Key::MAX), None);
    }

    /// The step is only useful if it really is a step: whatever the fields
    /// hold, the successor sorts above its key, and asking twice gives the
    /// same answer.
    #[test]
    fn every_successor_sorts_straight_above_its_key() {
        for k in [
            Key::MIN,
            Key::new(0, 0, u64::MAX),
            Key::new(1, 200, 5),
            Key::new(u64::MAX, u8::MAX, 0),
            Key::new(u64::MAX, 0, u64::MAX),
        ] {
            let next = key_after(&k).expect("not the largest key");
            assert!(next > k, "{:?} is not above {:?}", next, k);
            assert_eq!(key_after(&k), Some(next));
        }
    }
}
