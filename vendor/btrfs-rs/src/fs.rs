//! High-level filesystem operations over a mounted btrfs volume.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::alloc_ext::{FreeSpace, PendingOp};
use crate::device::BlockDevice;
use crate::structs::*;
use crate::tree::{RootCache, Tree};
use crate::volume::Volume;
use crate::{Error, Result};

/// Reserved physical area at the start of the device (contains the primary
/// superblock at 64 KiB).
const DEV_RESERVED: u64 = 0x10_0000; // 1 MiB

/// Keep at least this many free metadata blocks before mutating; create a new
/// metadata chunk otherwise.
const META_RESERVE_BLOCKS: u64 = 64;

const DATA_CHUNK_SIZE: u64 = 256 * 1024 * 1024;
const META_CHUNK_SIZE: u64 = 64 * 1024 * 1024;
const SYS_CHUNK_SIZE: u64 = 32 * 1024 * 1024;
const MIN_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const SUPERBLOCK_COMMIT_INTERVAL: u32 = 32;

/// Directory entry returned by [`Btrfs::readdir`].
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

pub struct Btrfs {
    vol: Volume,
    roots: RootCache,
    alloc: FreeSpace,
    next_ino: u64,
    generation: u64,
    read_only: bool,
    sb_dirty: bool,
    deferred_sb_commits: u32,
    clock: Option<fn() -> (u64, u32)>,
    /// Read-side cache of recently read files: inode plus the extent list
    /// looked up so far, in LRU order (most recently used last). Sequential
    /// reads otherwise re-walk the fs tree (inode + extent lookups) on every
    /// call, which dominated read cost. Demand-paging faults interleave reads
    /// across many files (every mapped shared library at once), so a
    /// single-entry cache thrashed on every inode switch — hence a small LRU.
    /// Entries are keyed by the volume's write epoch so any mutation
    /// invalidates them.
    read_cache: Vec<ReadCacheEntry>,
    /// The most recently inflated compressed extent, so that a stream of small
    /// reads over one 128 KiB extent inflates it once instead of once per
    /// call. Keyed by the volume write epoch, like [`ReadCacheEntry`].
    decompressed: Option<DecompressedExtent>,
}

/// One inflated compressed extent (see [`Btrfs::decompressed`]).
struct DecompressedExtent {
    epoch: u64,
    disk_bytenr: u64,
    data: Vec<u8>,
}

/// Number of files whose extent maps are kept cached for reads. Demand paging
/// a large process touches its executable plus every mapped library in an
/// interleaved pattern; a few dozen entries keep all of them warm at a few
/// hundred bytes each.
const READ_CACHE_FILES: usize = 32;

/// Ceiling on the bytes one compressed extent may claim, compressed or not.
/// Linux caps an uncompressed extent at 128 KiB before compressing it
/// (`BTRFS_MAX_UNCOMPRESSED`), so this is generous; it is here only so that a
/// corrupt or hostile item cannot make us allocate an arbitrary buffer from
/// a field we read straight off the disk.
const MAX_COMPRESSED_EXTENT: u64 = 1 << 20;

/// Inflate one compressed extent payload.
///
/// `ram_bytes` is what the extent item claims the result is, and doubles as
/// the output ceiling: a stream that wants to produce more than its own item
/// admits to is not something we should be allocating for.
fn inflate(compression: u8, src: &[u8], ram_bytes: u64) -> Result<Vec<u8>> {
    if ram_bytes > MAX_COMPRESSED_EXTENT {
        return Err(Error::Corrupt("compressed extent too large"));
    }
    match compression {
        COMPRESS_ZLIB => {
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(src, ram_bytes as usize)
                .map_err(|e| {
                    warn!(
                        "btrfs: zlib inflate failed: {:?} ({} compressed bytes, {} expected)",
                        e.status,
                        src.len(),
                        ram_bytes,
                    );
                    Error::Corrupt("zlib inflate failed")
                })
        }
        // Both carry an incompat flag, so `Volume::open` already refused the
        // mount; this arm only exists so that a hand-built item cannot reach
        // the `_` arm and be reported as corruption.
        COMPRESS_LZO => Err(Error::Unsupported("lzo-compressed extent")),
        COMPRESS_ZSTD => Err(Error::Unsupported("zstd-compressed extent")),
        _ => Err(Error::Corrupt("unknown extent compression")),
    }
}

struct ReadCacheEntry {
    ino: u64,
    epoch: u64,
    inode: InodeItem,
    /// Extents looked up so far, in file-offset order.
    extents: Vec<(u64, FileExtent, Vec<u8>)>,
    /// The file range `[cached_start, cached_end)` over which `extents` is
    /// known COMPLETE — every extent overlapping that window is in the list.
    /// A read fully inside it can be served with no tree lookup; a read
    /// extending it forward appends only the newly-scanned extents; a read
    /// OUTSIDE it (a scattered jump, forward past a gap or backward below
    /// `cached_start`) rebuilds the list for exactly the window requested.
    ///
    /// The range used to be just `cached_end` (implicitly `[0, cached_end)`),
    /// and the jump path cleared `extents` while leaving that claim in place —
    /// so a later low-offset read (a library's ELF header re-read after a
    /// scattered mmap fault) was served from the emptied list as ZEROS. musl's
    /// ldso then failed every desktop library with ENOEXEC ("Exec format
    /// error") on the installed btrfs root. Tracking the start closes that
    /// hole: reads below `cached_start` rescan instead of trusting the cache.
    cached_start: u64,
    /// See [`Self::cached_start`].
    cached_end: u64,
}

impl Btrfs {
    // ------------------------------------------------------------------
    // Mount / setup
    // ------------------------------------------------------------------

    pub fn mount(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<Self> {
        let vol = Volume::open(dev)?;
        let generation = vol.sb.generation();
        // A non-empty log tree comes from an unclean Linux shutdown. We do
        // not replay it, so writable mounts are unsafe until replay happens.
        let stale_log = vol.sb.log_root() != 0;
        if stale_log && !read_only {
            return Err(Error::Unsupported("log replay required for writable mount"));
        }
        let mut fs = Self {
            vol,
            roots: RootCache::new(),
            alloc: FreeSpace::default(),
            next_ino: FIRST_FREE_OBJECTID,
            generation,
            read_only,
            sb_dirty: false,
            deferred_sb_commits: 0,
            read_cache: Vec::new(),
            decompressed: None,
            clock: None,
        };
        fs.alloc.nodesize = fs.vol.nodesize as u64;
        fs.alloc.sectorsize = fs.vol.sectorsize as u64;
        fs.load_space_info()?;
        fs.load_next_ino()?;
        Ok(fs)
    }

    /// Provide a wall-clock source for timestamps (secs, nanos).
    pub fn set_clock(&mut self, clock: fn() -> (u64, u32)) {
        self.clock = Some(clock);
    }

    fn now(&self) -> (u64, u32) {
        self.clock.map(|f| f()).unwrap_or((0, 0))
    }

    pub fn label(&self) -> String {
        self.vol.sb.label()
    }

    pub fn nodesize(&self) -> usize {
        self.vol.nodesize
    }

    pub fn sectorsize(&self) -> usize {
        self.vol.sectorsize
    }

    pub fn root_ino(&self) -> u64 {
        FIRST_FREE_OBJECTID
    }

    pub fn fsinfo(&self) -> FsStat {
        FsStat {
            block_size: self.vol.sectorsize as u64,
            total_bytes: self.vol.sb.total_bytes(),
            bytes_used: (self.vol.sb.bytes_used() as i64 + self.alloc.bytes_used_delta) as u64,
        }
    }

    fn writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::Unsupported("read-only filesystem"));
        }
        let unknown_ro = self.vol.sb.compat_ro_flags()
            & !(COMPAT_RO_FREE_SPACE_TREE | COMPAT_RO_FREE_SPACE_TREE_VALID);
        if unknown_ro != 0 {
            return Err(Error::Unsupported("compat_ro flags"));
        }
        Ok(())
    }

    /// Gate for every mutation: check writability, then make sure a free space
    /// tree we are about to invalidate is marked for rebuild.
    fn begin_write(&mut self) -> Result<()> {
        self.writable()?;
        self.invalidate_free_space_tree()
    }

    /// Drop `FREE_SPACE_TREE_VALID` before the first mutation of a volume that
    /// carries a free space tree.
    ///
    /// We allocate and free extents in the extent tree but do not maintain the
    /// free space tree (`mkfs.btrfs` has enabled it by default since
    /// btrfs-progs 5.15, so this is the *common* case for a volume formatted on
    /// Linux). Leaving `VALID` set would tell Linux that the stale tree still
    /// describes free space, and it would hand out extents we already
    /// allocated — silent corruption of live data.
    ///
    /// `FREE_SPACE_TREE` without `FREE_SPACE_TREE_VALID` is exactly the state
    /// the kernel treats as "rebuild it at mount", and is what btrfs-progs
    /// leaves behind when it changes a filesystem without maintaining the tree.
    /// Clearing the bit costs one rebuild on the next Linux mount and keeps the
    /// allocator honest.
    fn invalidate_free_space_tree(&mut self) -> Result<()> {
        let flags = self.vol.sb.compat_ro_flags();
        if flags & COMPAT_RO_FREE_SPACE_TREE == 0 || flags & COMPAT_RO_FREE_SPACE_TREE_VALID == 0 {
            return Ok(());
        }
        info!(
            "btrfs: clearing FREE_SPACE_TREE_VALID; Linux rebuilds the free space tree at its next mount"
        );
        self.vol
            .sb
            .set_compat_ro_flags(flags & !COMPAT_RO_FREE_SPACE_TREE_VALID);
        // Push it out now, before the mutation it guards. Superblock writes are
        // otherwise deferred (`SUPERBLOCK_COMMIT_INTERVAL`), and a crash between
        // the first allocation reaching disk and the next commit would leave a
        // volume whose extent tree moved while `VALID` still claimed the free
        // space tree described it. This runs once per mount.
        self.vol.flush_dirty()?;
        self.vol.write_superblock()?;
        self.sb_dirty = false;
        self.deferred_sb_commits = 0;
        Ok(())
    }

    fn tree(&mut self) -> Tree<'_> {
        Tree {
            vol: &mut self.vol,
            roots: &mut self.roots,
            alloc: &mut self.alloc,
        }
    }

    /// Scan the extent tree (block groups + allocated extents) and the dev
    /// tree (device extents) to build the in-memory allocator state.
    fn load_space_info(&mut self) -> Result<()> {
        let nodesize = self.vol.nodesize as u64;
        let mut bgs: Vec<(u64, u64, BlockGroupItem)> = Vec::new();
        let mut used: Vec<(u64, u64)> = Vec::new();
        {
            let mut t = self.tree();
            t.iter_from(EXTENT_TREE, Key::MIN, |key, data| {
                match key.item_type {
                    BLOCK_GROUP_ITEM_KEY => {
                        let item = BlockGroupItem::parse(data).ok_or(Error::Corrupt("bg item"))?;
                        bgs.push((key.objectid, key.offset, item));
                    }
                    EXTENT_ITEM_KEY => used.push((key.objectid, key.offset)),
                    METADATA_ITEM_KEY => used.push((key.objectid, nodesize)),
                    _ => {}
                }
                Ok(true)
            })?;
        }
        for (start, len, item) in &bgs {
            self.alloc.bgs.insert(
                *start,
                crate::alloc_ext::BlockGroup {
                    start: *start,
                    len: *len,
                    flags: item.flags,
                    used: item.used,
                    dirty: false,
                },
            );
            self.alloc.free.insert(*start, *len);
        }
        for (start, len) in &used {
            // Tolerate extents recorded outside any block group (corrupt
            // foreign images): they simply are not usable space.
            let _ = self.alloc.free.take(*start, *len);
        }

        // Device free space: device minus dev extents minus reserved areas.
        let dev_item = self.vol.sb.dev_item().ok_or(Error::Corrupt("dev item"))?;
        self.alloc.dev_free.insert(
            DEV_RESERVED,
            dev_item.total_bytes.saturating_sub(DEV_RESERVED),
        );
        let mut dev_used: Vec<(u64, u64)> = Vec::new();
        {
            let mut t = self.tree();
            t.iter_from(DEV_TREE, Key::MIN, |key, data| {
                if key.item_type == DEV_EXTENT_KEY {
                    if let Some(ext) = DevExtent::parse(data) {
                        dev_used.push((key.offset, ext.length));
                    }
                }
                Ok(true)
            })?;
        }
        for (start, len) in dev_used {
            let _ = self.alloc.dev_free.take(start, len);
        }
        // Never allocate over superblock mirrors.
        for &off in SUPERBLOCK_OFFSETS.iter() {
            let _ = self.alloc.dev_free.take(off, SUPERBLOCK_SIZE as u64);
        }
        Ok(())
    }

    fn load_next_ino(&mut self) -> Result<()> {
        let mut t = self.tree();
        let last = t.prev_item(FS_TREE, Key::new(LAST_FREE_OBJECTID, u8::MAX, u64::MAX))?;
        self.next_ino = match last {
            Some((key, _)) if key.objectid >= FIRST_FREE_OBJECTID => key.objectid + 1,
            _ => FIRST_FREE_OBJECTID,
        };
        Ok(())
    }

    /// Grow the filesystem to fill the whole device (used after the installer
    /// copies a small image onto a big partition). Returns true if grown.
    pub fn grow_to_device(&mut self) -> Result<bool> {
        self.begin_write()?;
        let dev_size = self.vol.dev.size() / 4096 * 4096;
        let dev_item = self.vol.sb.dev_item().ok_or(Error::Corrupt("dev item"))?;
        if dev_size <= dev_item.total_bytes {
            return Ok(false);
        }
        let old = dev_item.total_bytes;
        self.vol.sb.set_dev_item_total_bytes(dev_size);
        let total = self.vol.sb.total_bytes() + (dev_size - old);
        self.vol.sb.set_total_bytes(total);
        // DEV_ITEM in the chunk tree mirrors the superblock copy.
        {
            let mut t = self.tree();
            t.update_in_place(
                CHUNK_TREE,
                Key::new(DEV_ITEMS_OBJECTID, DEV_ITEM_KEY, 1),
                |data| put_u64(data, 8, dev_size),
            )?;
        }
        self.alloc.dev_free.insert(old, dev_size - old);
        for &off in SUPERBLOCK_OFFSETS.iter() {
            let _ = self.alloc.dev_free.take(off, SUPERBLOCK_SIZE as u64);
        }
        self.apply_pending()?;
        self.commit(true)?;
        warn!("btrfs: grown from {} to {} bytes", old, dev_size);
        Ok(true)
    }

    // ------------------------------------------------------------------
    // Pending extent-tree bookkeeping / commit
    // ------------------------------------------------------------------

    fn skinny_metadata(&self) -> bool {
        self.vol.sb.incompat_flags() & INCOMPAT_SKINNY_METADATA != 0
    }

    fn apply_pending(&mut self) -> Result<()> {
        // Applying ops can enqueue more (tree splits inside the extent tree).
        for _ in 0..64 {
            let ops = self.alloc.take_pending();
            if ops.is_empty() {
                return Ok(());
            }
            for op in ops {
                self.apply_one(op)?;
            }
        }
        Err(Error::Corrupt("extent bookkeeping did not converge"))
    }

    fn apply_one(&mut self, op: PendingOp) -> Result<()> {
        let nodesize = self.vol.nodesize as u64;
        let generation = self.generation;
        let skinny = self.skinny_metadata();
        let mut t = self.tree();
        match op {
            PendingOp::AddMeta {
                bytenr,
                owner,
                level,
            } => {
                let (key, data) = if skinny {
                    let mut d = alloc::vec![0u8; EXTENT_ITEM_LEN + 9];
                    put_u64(&mut d, 0, 1); // refs
                    put_u64(&mut d, 8, generation);
                    put_u64(&mut d, 16, EXTENT_FLAG_TREE_BLOCK);
                    d[24] = TREE_BLOCK_REF_KEY;
                    put_u64(&mut d, 25, owner);
                    (Key::new(bytenr, METADATA_ITEM_KEY, level as u64), d)
                } else {
                    let mut d = alloc::vec![0u8; EXTENT_ITEM_LEN + 18 + 9];
                    put_u64(&mut d, 0, 1);
                    put_u64(&mut d, 8, generation);
                    put_u64(&mut d, 16, EXTENT_FLAG_TREE_BLOCK);
                    // tree_block_info: key (zeroed) + level
                    d[EXTENT_ITEM_LEN + 17] = level;
                    d[EXTENT_ITEM_LEN + 18] = TREE_BLOCK_REF_KEY;
                    put_u64(&mut d, EXTENT_ITEM_LEN + 19, owner);
                    (Key::new(bytenr, EXTENT_ITEM_KEY, nodesize), d)
                };
                t.insert(EXTENT_TREE, key, &data)?;
            }
            PendingOp::DelMeta { bytenr, level, .. } => {
                let key = if skinny {
                    Key::new(bytenr, METADATA_ITEM_KEY, level as u64)
                } else {
                    Key::new(bytenr, EXTENT_ITEM_KEY, nodesize)
                };
                match t.get(EXTENT_TREE, key)? {
                    Some(data) if get_u64(&data, 0) > 1 => {
                        t.update_in_place(EXTENT_TREE, key, |d| {
                            let refs = get_u64(d, 0);
                            put_u64(d, 0, refs - 1);
                        })?;
                    }
                    Some(_) => t.delete(EXTENT_TREE, key)?,
                    None => warn!("btrfs: freed tree block {} has no extent item", bytenr),
                }
            }
            PendingOp::AddData {
                bytenr,
                len,
                root,
                objectid,
                offset,
            } => {
                let mut d = alloc::vec![0u8; EXTENT_ITEM_LEN + 1 + 28];
                put_u64(&mut d, 0, 1); // refs
                put_u64(&mut d, 8, generation);
                put_u64(&mut d, 16, EXTENT_FLAG_DATA);
                d[24] = EXTENT_DATA_REF_KEY;
                put_u64(&mut d, 25, root);
                put_u64(&mut d, 33, objectid);
                put_u64(&mut d, 41, offset);
                put_u32(&mut d, 49, 1); // count
                t.insert(EXTENT_TREE, Key::new(bytenr, EXTENT_ITEM_KEY, len), &d)?;
            }
            PendingOp::DelData { bytenr, len, .. } => {
                let key = Key::new(bytenr, EXTENT_ITEM_KEY, len);
                match t.get(EXTENT_TREE, key)? {
                    Some(data) if get_u64(&data, 0) > 1 => {
                        t.update_in_place(EXTENT_TREE, key, |d| {
                            let refs = get_u64(d, 0);
                            put_u64(d, 0, refs - 1);
                        })?;
                    }
                    Some(_) => t.delete(EXTENT_TREE, key)?,
                    None => warn!("btrfs: freed data extent {} has no extent item", bytenr),
                }
            }
        }
        Ok(())
    }

    /// Flush dirty block-group items and (when needed) the superblock.
    fn commit(&mut self, force_sb: bool) -> Result<()> {
        self.apply_pending()?;
        loop {
            let dirty = self.alloc.take_dirty_bgs();
            if dirty.is_empty() {
                break;
            }
            for (start, len, item) in dirty {
                let mut t = self.tree();
                t.set_item(
                    EXTENT_TREE,
                    Key::new(start, BLOCK_GROUP_ITEM_KEY, len),
                    &item.encode(),
                )?;
            }
            self.apply_pending()?;
        }
        if self.alloc.bytes_used_delta != 0 {
            let used = (self.vol.sb.bytes_used() as i64 + self.alloc.bytes_used_delta) as u64;
            self.vol.sb.set_bytes_used(used);
            self.alloc.bytes_used_delta = 0;
            self.sb_dirty = true;
        }
        if self.alloc.dev_used_delta != 0 {
            let dev_item = self.vol.sb.dev_item().ok_or(Error::Corrupt("dev item"))?;
            let used = (dev_item.bytes_used as i64 + self.alloc.dev_used_delta) as u64;
            self.vol.sb.set_dev_item_bytes_used(used);
            // The chunk tree carries an authoritative copy of the dev item.
            let mut t = self.tree();
            t.update_in_place(
                CHUNK_TREE,
                Key::new(DEV_ITEMS_OBJECTID, DEV_ITEM_KEY, dev_item.devid),
                |d| put_u64(d, 16, used),
            )?;
            self.alloc.dev_used_delta = 0;
            self.sb_dirty = true;
        }
        if force_sb || (self.sb_dirty && self.deferred_sb_commits >= SUPERBLOCK_COMMIT_INTERVAL) {
            // Write-back invariant: every dirty tree block must reach the device
            // before the superblock that references it, otherwise a crash would
            // leave the SB pointing at unwritten blocks.
            self.vol.flush_dirty()?;
            self.vol.write_superblock()?;
            self.sb_dirty = false;
            self.deferred_sb_commits = 0;
        } else if self.sb_dirty {
            self.deferred_sb_commits = self.deferred_sb_commits.saturating_add(1);
        }
        Ok(())
    }

    /// Flush everything to the device.
    pub fn sync(&mut self) -> Result<()> {
        self.commit(true)?;
        self.vol.dev.sync()
    }

    // ------------------------------------------------------------------
    // Chunk management
    // ------------------------------------------------------------------

    fn ensure_metadata_space(&mut self) -> Result<()> {
        let nodesize = self.vol.nodesize as u64;
        if self.alloc.meta_free() >= META_RESERVE_BLOCKS * nodesize {
            return Ok(());
        }
        match self.create_chunk(BLOCK_GROUP_METADATA, META_CHUNK_SIZE) {
            Ok(()) | Err(Error::NoSpace) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn ensure_data_space(&mut self, want: u64) -> Result<()> {
        if self.alloc.data_free() >= want {
            return Ok(());
        }
        let size = want.max(
            DATA_CHUNK_SIZE.min(
                self.alloc
                    .dev_free
                    .largest_in(0, u64::MAX)
                    .map(|r| r.1)
                    .unwrap_or(0),
            ),
        );
        match self.create_chunk(BLOCK_GROUP_DATA, size) {
            Ok(()) => Ok(()),
            Err(Error::NoSpace) if self.alloc.data_free() > 0 => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn ensure_system_space(&mut self) -> Result<()> {
        let nodesize = self.vol.nodesize as u64;
        // O(#block-groups) via accounted `used`, not O(#free-fragments); see
        // `FreeSpace::free_in_groups`. Runs on every mutation, so the old
        // fragment sum made large writes quadratic.
        let free = self.alloc.free_in_groups(BLOCK_GROUP_SYSTEM);
        if free >= 8 * nodesize {
            return Ok(());
        }
        match self.create_chunk(BLOCK_GROUP_SYSTEM, SYS_CHUNK_SIZE) {
            Ok(()) | Err(Error::NoSpace) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Create a new chunk/block group of `flags`, sized `want` (shrunk to the
    /// largest free device range when needed).
    fn create_chunk(&mut self, flags: u64, want: u64) -> Result<()> {
        const ALIGN: u64 = 0x10_0000;
        let (_, largest) = self
            .alloc
            .dev_free
            .largest_in(0, u64::MAX)
            .ok_or(Error::NoSpace)?;
        let size = want.min(largest) / ALIGN * ALIGN;
        if size < MIN_CHUNK_SIZE {
            return Err(Error::NoSpace);
        }
        let phys = self
            .alloc
            .dev_free
            .alloc_in(0, u64::MAX, size, ALIGN)
            .ok_or(Error::NoSpace)?;
        let logical = self.alloc.logical_end().max(
            self.vol
                .chunks()
                .iter()
                .map(|c| c.logical + c.length)
                .max()
                .unwrap_or(0),
        );
        let dev_item = self.vol.sb.dev_item().ok_or(Error::Corrupt("dev item"))?;
        let chunk = ChunkItem {
            length: size,
            owner: EXTENT_TREE,
            stripe_len: 65536,
            type_: flags,
            io_align: 65536,
            io_width: 65536,
            sector_size: self.vol.sectorsize as u32,
            sub_stripes: 1,
            stripes: alloc::vec![Stripe {
                devid: dev_item.devid,
                offset: phys,
            }],
        };
        warn!(
            "btrfs: new chunk flags={:#x} logical={:#x} phys={:#x} size={:#x}",
            flags, logical, phys, size
        );
        // Make the new space usable before editing trees, so those edits can
        // allocate from it if needed.
        self.vol.register_chunk(logical, &chunk)?;
        self.alloc.bgs.insert(
            logical,
            crate::alloc_ext::BlockGroup {
                start: logical,
                len: size,
                flags,
                used: 0,
                dirty: true,
            },
        );
        self.alloc.free.insert(logical, size);
        self.alloc.dev_used_delta += size as i64;

        let chunk_key = Key::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, logical);
        let chunk_data = chunk.encode(&dev_item.uuid);
        let dev_ext = DevExtent {
            chunk_offset: logical,
            length: size,
        };
        let chunk_tree_uuid = self.vol.chunk_tree_uuid;
        {
            let mut t = self.tree();
            t.insert(CHUNK_TREE, chunk_key, &chunk_data)?;
            t.insert(
                DEV_TREE,
                Key::new(dev_item.devid, DEV_EXTENT_KEY, phys),
                &dev_ext.encode(&chunk_tree_uuid),
            )?;
            t.insert(
                EXTENT_TREE,
                Key::new(logical, BLOCK_GROUP_ITEM_KEY, size),
                &BlockGroupItem { used: 0, flags }.encode(),
            )?;
        }
        if flags & BLOCK_GROUP_SYSTEM != 0 {
            self.append_sys_chunk(&chunk_key, &chunk_data)?;
        }
        self.sb_dirty = true;
        self.apply_pending()?;
        Ok(())
    }

    fn append_sys_chunk(&mut self, key: &Key, chunk_data: &[u8]) -> Result<()> {
        let cur = get_u32(&self.vol.sb.raw, sb::OFF_SYS_CHUNK_ARRAY_SIZE) as usize;
        let need = KEY_SIZE + chunk_data.len();
        if cur + need > sb::SYS_CHUNK_ARRAY_LEN {
            return Err(Error::NoSpace);
        }
        let base = sb::OFF_SYS_CHUNK_ARRAY + cur;
        key.write(&mut self.vol.sb.raw, base);
        self.vol.sb.raw[base + KEY_SIZE..base + need].copy_from_slice(chunk_data);
        put_u32(
            &mut self.vol.sb.raw,
            sb::OFF_SYS_CHUNK_ARRAY_SIZE,
            (cur + need) as u32,
        );
        Ok(())
    }

    /// Reserve space ahead of a mutating operation.
    fn prepare_mutation(&mut self) -> Result<()> {
        self.begin_write()?;
        self.ensure_system_space()?;
        self.ensure_metadata_space()?;
        self.apply_pending()
    }

    // ------------------------------------------------------------------
    // Inodes
    // ------------------------------------------------------------------

    pub fn read_inode(&mut self, ino: u64) -> Result<InodeItem> {
        let mut t = self.tree();
        let data = t
            .get(FS_TREE, Key::new(ino, INODE_ITEM_KEY, 0))?
            .ok_or(Error::NotFound)?;
        InodeItem::parse(&data).ok_or(Error::Corrupt("inode item"))
    }

    fn write_inode(&mut self, ino: u64, inode: &InodeItem) -> Result<()> {
        let enc = inode.encode();
        let mut t = self.tree();
        t.update_in_place(FS_TREE, Key::new(ino, INODE_ITEM_KEY, 0), |d| {
            d[..INODE_ITEM_LEN].copy_from_slice(&enc)
        })
    }

    pub fn stat(&mut self, ino: u64) -> Result<InodeStat> {
        let i = self.read_inode(ino)?;
        Ok(InodeStat {
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
        })
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
        self.begin_write()?;
        let mut inode = self.read_inode(ino)?;
        if let Some(mode) = mode {
            // Keep the file-type bits.
            inode.mode = (inode.mode & S_IFMT) | (mode & !S_IFMT);
        }
        if let Some(uid) = uid {
            inode.uid = uid;
        }
        if let Some(gid) = gid {
            inode.gid = gid;
        }
        if let Some(t) = atime {
            inode.atime = t;
        }
        if let Some(t) = mtime {
            inode.mtime = t;
        }
        inode.ctime = self.now();
        self.write_inode(ino, &inode)?;
        self.commit(false)
    }

    // ------------------------------------------------------------------
    // Directories
    // ------------------------------------------------------------------

    pub fn lookup(&mut self, dir: u64, name: &str) -> Result<u64> {
        let name = check_name(name)?;
        let hash = crate::crc::name_hash(name);
        let mut t = self.tree();
        let item = t.get(FS_TREE, Key::new(dir, DIR_ITEM_KEY, hash))?;
        let item = match item {
            Some(i) => i,
            None => {
                // Distinguish "no such entry" from "not a directory".
                let inode = self.read_inode(dir)?;
                if inode.kind() != FileKind::Dir {
                    return Err(Error::NotDir);
                }
                return Err(Error::NotFound);
            }
        };
        for (_, entry) in parse_dir_entries(&item) {
            if entry.name == name {
                if entry.location.item_type != INODE_ITEM_KEY {
                    return Err(Error::Unsupported("subvolume entry"));
                }
                return Ok(entry.location.objectid);
            }
        }
        Err(Error::NotFound)
    }

    pub fn readdir(&mut self, dir: u64) -> Result<Vec<DirEntry>> {
        let inode = self.read_inode(dir)?;
        if inode.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        let mut out = Vec::new();
        let mut t = self.tree();
        t.iter_from(FS_TREE, Key::new(dir, DIR_INDEX_KEY, 0), |key, data| {
            if key.objectid != dir || key.item_type != DIR_INDEX_KEY {
                return Ok(false);
            }
            for (_, e) in parse_dir_entries(data) {
                if e.location.item_type != INODE_ITEM_KEY {
                    continue;
                }
                out.push(DirEntry {
                    ino: e.location.objectid,
                    name: String::from_utf8_lossy(&e.name).into_owned(),
                    kind: match e.dir_type {
                        FT_DIR => FileKind::Dir,
                        FT_SYMLINK => FileKind::Symlink,
                        FT_CHRDEV => FileKind::CharDevice,
                        FT_BLKDEV => FileKind::BlockDevice,
                        FT_FIFO => FileKind::Fifo,
                        FT_SOCK => FileKind::Socket,
                        _ => FileKind::Regular,
                    },
                });
            }
            Ok(true)
        })?;
        Ok(out)
    }

    fn dir_is_empty(&mut self, dir: u64) -> Result<bool> {
        let mut empty = true;
        let mut t = self.tree();
        t.iter_from(FS_TREE, Key::new(dir, DIR_ITEM_KEY, 0), |key, _| {
            if key.objectid == dir
                && (key.item_type == DIR_ITEM_KEY || key.item_type == DIR_INDEX_KEY)
            {
                empty = false;
            }
            Ok(false)
        })?;
        Ok(empty)
    }

    fn next_dir_index(&mut self, dir: u64) -> Result<u64> {
        let mut t = self.tree();
        match t.prev_item(FS_TREE, Key::new(dir, DIR_INDEX_KEY, u64::MAX))? {
            Some((key, _)) if key.objectid == dir && key.item_type == DIR_INDEX_KEY => {
                Ok(key.offset + 1)
            }
            _ => Ok(2),
        }
    }

    /// Add name → ino entries (DIR_ITEM, DIR_INDEX, INODE_REF) and grow the
    /// parent size. Does not touch nlink.
    fn add_entry(&mut self, dir: u64, name: &[u8], ino: u64, dir_type: u8) -> Result<u64> {
        let index = self.next_dir_index(dir)?;
        let generation = self.generation;
        let entry = DirEntryRaw {
            location: Key::new(ino, INODE_ITEM_KEY, 0),
            transid: generation,
            dir_type,
            name: name.to_vec(),
            data: Vec::new(),
        };
        let enc = entry.encode();
        let hash = crate::crc::name_hash(name);
        // A directory entry is three items that only make sense together. Any
        // of the three inserts can fail -- with NoSpace once metadata is
        // genuinely exhausted, since a leaf split needs a tree block -- so a
        // failed one is undone before returning. Without this, a create that
        // ran out of space left a DIR_ITEM with no DIR_INDEX and no
        // INODE_REF: `btrfs check` reports it as `unresolved ref ... no dir
        // index, no inode ref`, and the name is neither usable nor
        // removable. Unwinding only ever shrinks or deletes items that were
        // just written, which never needs an allocation of its own.
        {
            let dir_key = Key::new(dir, DIR_ITEM_KEY, hash);
            let ref_key = Key::new(ino, INODE_REF_KEY, dir);
            let index_key = Key::new(dir, DIR_INDEX_KEY, index);
            let ref_entry = encode_inode_ref(index, name);

            let mut t = self.tree();
            // DIR_ITEM (append on hash collision).
            let dir_item_was = t.get(FS_TREE, dir_key)?;
            match &dir_item_was {
                Some(existing) => {
                    let mut grown = existing.clone();
                    grown.extend_from_slice(&enc);
                    t.set_item(FS_TREE, dir_key, &grown)?;
                }
                None => t.insert(FS_TREE, dir_key, &enc)?,
            }
            // DIR_INDEX.
            if let Err(e) = t.insert(FS_TREE, index_key, &enc) {
                undo_item(&mut t, dir_key, dir_item_was.as_deref());
                return Err(e);
            }
            // INODE_REF.
            let inode_ref_was = match t.get(FS_TREE, ref_key) {
                Ok(v) => v,
                Err(e) => {
                    let _ = t.delete(FS_TREE, index_key);
                    undo_item(&mut t, dir_key, dir_item_was.as_deref());
                    return Err(e);
                }
            };
            let wrote_ref = match &inode_ref_was {
                Some(existing) => {
                    let mut grown = existing.clone();
                    grown.extend_from_slice(&ref_entry);
                    t.set_item(FS_TREE, ref_key, &grown)
                }
                None => t.insert(FS_TREE, ref_key, &ref_entry),
            };
            if let Err(e) = wrote_ref {
                let _ = t.delete(FS_TREE, index_key);
                undo_item(&mut t, dir_key, dir_item_was.as_deref());
                return Err(e);
            }
        }
        // Directory size grows by name_len for each of DIR_ITEM and DIR_INDEX.
        let mut parent = self.read_inode(dir)?;
        parent.size += 2 * name.len() as u64;
        let now = self.now();
        parent.mtime = now;
        parent.ctime = now;
        self.write_inode(dir, &parent)?;
        Ok(index)
    }

    /// Remove the entries for `name` from `dir`; returns (ino, dir_type).
    fn remove_entry(&mut self, dir: u64, name: &[u8]) -> Result<(u64, u8)> {
        let hash = crate::crc::name_hash(name);
        let dir_key = Key::new(dir, DIR_ITEM_KEY, hash);
        let (ino, dir_type) = {
            let mut t = self.tree();
            let item = t.get(FS_TREE, dir_key)?.ok_or(Error::NotFound)?;
            let entries = parse_dir_entries(&item);
            let found = entries
                .iter()
                .find(|(_, e)| e.name == name)
                .ok_or(Error::NotFound)?;
            let (range, entry) = (found.0.clone(), found.1.clone());
            if entries.len() == 1 {
                t.delete(FS_TREE, dir_key)?;
            } else {
                let mut rest = Vec::with_capacity(item.len() - range.len());
                rest.extend_from_slice(&item[..range.start]);
                rest.extend_from_slice(&item[range.end..]);
                t.set_item(FS_TREE, dir_key, &rest)?;
            }
            (entry.location.objectid, entry.dir_type)
        };
        // INODE_REF (find the index there, then drop the DIR_INDEX).
        let ref_key = Key::new(ino, INODE_REF_KEY, dir);
        let mut index = None;
        {
            let mut t = self.tree();
            if let Some(item) = t.get(FS_TREE, ref_key)? {
                let refs = parse_inode_refs(&item);
                if let Some((range, idx, _)) = refs.iter().find(|(_, _, n)| n == name) {
                    index = Some(*idx);
                    if refs.len() == 1 {
                        t.delete(FS_TREE, ref_key)?;
                    } else {
                        let mut rest = Vec::with_capacity(item.len() - range.len());
                        rest.extend_from_slice(&item[..range.start]);
                        rest.extend_from_slice(&item[range.end..]);
                        t.set_item(FS_TREE, ref_key, &rest)?;
                    }
                }
            }
        }
        let index = match index {
            Some(i) => Some(i),
            None => {
                // Fallback: scan DIR_INDEX items for the name.
                let mut found = None;
                let mut t = self.tree();
                t.iter_from(FS_TREE, Key::new(dir, DIR_INDEX_KEY, 0), |key, data| {
                    if key.objectid != dir || key.item_type != DIR_INDEX_KEY {
                        return Ok(false);
                    }
                    for (_, e) in parse_dir_entries(data) {
                        if e.name == name {
                            found = Some(key.offset);
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })?;
                found
            }
        };
        if let Some(index) = index {
            let mut t = self.tree();
            match t.delete(FS_TREE, Key::new(dir, DIR_INDEX_KEY, index)) {
                Ok(()) | Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        let mut parent = self.read_inode(dir)?;
        parent.size = parent.size.saturating_sub(2 * name.len() as u64);
        let now = self.now();
        parent.mtime = now;
        parent.ctime = now;
        self.write_inode(dir, &parent)?;
        Ok((ino, dir_type))
    }

    // ------------------------------------------------------------------
    // Create / link / unlink / rename
    // ------------------------------------------------------------------

    pub fn create(
        &mut self,
        dir: u64,
        name: &str,
        kind: FileKind,
        mode: u32,
        rdev: u64,
    ) -> Result<u64> {
        let name = check_name(name)?;
        self.prepare_mutation()?;
        let parent = self.read_inode(dir)?;
        if parent.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        if self.lookup_raw(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        if self.next_ino >= LAST_FREE_OBJECTID {
            return Err(Error::NoSpace);
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        let now = self.now();
        let flags = if kind == FileKind::Regular {
            INODE_NODATASUM | INODE_NODATACOW
        } else {
            0
        };
        let inode = InodeItem {
            generation: self.generation,
            transid: self.generation,
            size: 0,
            nbytes: 0,
            block_group: 0,
            nlink: 1,
            uid: 0,
            gid: 0,
            mode: kind.mode_bits() | (mode & !S_IFMT),
            rdev,
            flags,
            sequence: 0,
            atime: now,
            ctime: now,
            mtime: now,
            otime: now,
        };
        {
            let mut t = self.tree();
            if let Err(e) = t.insert(FS_TREE, Key::new(ino, INODE_ITEM_KEY, 0), &inode.encode()) {
                self.next_ino = ino;
                return Err(e);
            }
        }
        // An inode with no name is an orphan `btrfs check` reports and
        // nothing can reach or delete; drop it if the entry does not land.
        if let Err(e) = self.add_entry(dir, name, ino, kind.dir_type()) {
            {
                let mut t = self.tree();
                let _ = t.delete(FS_TREE, Key::new(ino, INODE_ITEM_KEY, 0));
            }
            self.next_ino = ino;
            return Err(e);
        }
        self.commit(false)?;
        Ok(ino)
    }

    pub fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<u64> {
        if target.is_empty()
            || target.len() + FILE_EXTENT_HDR_LEN + ITEM_SIZE > self.vol.nodesize - HEADER_SIZE
        {
            return Err(Error::Invalid);
        }
        let ino = self.create(dir, name, FileKind::Symlink, 0o777, 0)?;
        let ext = FileExtent::encode_inline(self.generation, target);
        {
            let mut t = self.tree();
            t.insert(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0), &ext)?;
        }
        let mut inode = self.read_inode(ino)?;
        inode.size = target.len() as u64;
        inode.nbytes = target.len() as u64;
        self.write_inode(ino, &inode)?;
        self.commit(false)?;
        Ok(ino)
    }

    /// Splice `data` into a symlink's target at `offset` (the rcore-fs VFS
    /// creates symlinks empty and fills the target through `write_at`).
    pub fn write_symlink(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.prepare_mutation()?;
        let mut inode = self.read_inode(ino)?;
        if inode.kind() != FileKind::Symlink {
            return Err(Error::Invalid);
        }
        let mut target = alloc::vec![0u8; inode.size as usize];
        self.read(ino, 0, &mut target)?;
        let end = offset as usize + data.len();
        if end > MAX_NAME_LEN * 16 {
            return Err(Error::Invalid);
        }
        if end > target.len() {
            target.resize(end, 0);
        }
        target[offset as usize..end].copy_from_slice(data);
        let enc = FileExtent::encode_inline(self.generation, &target);
        {
            let mut t = self.tree();
            t.set_item(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0), &enc)?;
        }
        inode.size = target.len() as u64;
        inode.nbytes = target.len() as u64;
        let now = self.now();
        inode.mtime = now;
        inode.ctime = now;
        self.write_inode(ino, &inode)?;
        self.commit(false)?;
        Ok(data.len())
    }

    pub fn read_link(&mut self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        if inode.kind() != FileKind::Symlink {
            return Err(Error::Invalid);
        }
        let mut buf = alloc::vec![0u8; inode.size as usize];
        let n = self.read(ino, 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Parent directory of `ino`: the offset of its first INODE_REF item
    /// (btrfs keys back-references as `(ino, INODE_REF, parent_dir)`). The
    /// subvolume root refers to itself, so the parent of `/` is `/`.
    pub fn parent(&mut self, ino: u64) -> Result<u64> {
        let mut parent = None;
        let mut t = self.tree();
        t.iter_from(FS_TREE, Key::new(ino, INODE_REF_KEY, 0), |k, _| {
            if k.objectid == ino && k.item_type == INODE_REF_KEY {
                parent = Some(k.offset);
            }
            Ok(false)
        })?;
        parent.ok_or(Error::NotFound)
    }

    pub fn link(&mut self, dir: u64, name: &str, ino: u64) -> Result<()> {
        let name = check_name(name)?;
        self.prepare_mutation()?;
        let mut inode = self.read_inode(ino)?;
        if inode.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        if self.lookup_raw(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        self.add_entry(dir, name, ino, inode.kind().dir_type())?;
        inode.nlink += 1;
        inode.ctime = self.now();
        self.write_inode(ino, &inode)?;
        self.commit(false)
    }

    pub fn unlink(&mut self, dir: u64, name: &str) -> Result<()> {
        let name = check_name(name)?;
        self.prepare_mutation()?;
        let ino = self.lookup_raw(dir, name)?.ok_or(Error::NotFound)?;
        let mut inode = self.read_inode(ino)?;
        if inode.kind() == FileKind::Dir && !self.dir_is_empty(ino)? {
            return Err(Error::NotEmpty);
        }
        self.remove_entry(dir, name)?;
        if inode.nlink > 1 {
            inode.nlink -= 1;
            inode.ctime = self.now();
            self.write_inode(ino, &inode)?;
        } else {
            self.purge_inode(ino, &inode)?;
        }
        self.commit(false)
    }

    /// Remove every item belonging to `ino`, freeing its data extents.
    fn purge_inode(&mut self, ino: u64, inode: &InodeItem) -> Result<()> {
        let had_csums = inode.flags & INODE_NODATASUM == 0;
        // Collect all keys of this object plus the extents to free.
        let mut keys = Vec::new();
        let mut extents = Vec::new();
        {
            let mut t = self.tree();
            t.iter_from(FS_TREE, Key::new(ino, 0, 0), |key, data| {
                if key.objectid != ino {
                    return Ok(false);
                }
                keys.push(*key);
                if key.item_type == EXTENT_DATA_KEY {
                    if let Some(FileExtent::Regular {
                        disk_bytenr,
                        disk_num_bytes,
                        ..
                    }) = FileExtent::parse(data)
                    {
                        if disk_bytenr != 0 {
                            extents.push((disk_bytenr, disk_num_bytes, key.offset));
                        }
                    }
                }
                Ok(true)
            })?;
        }
        for key in keys {
            let mut t = self.tree();
            t.delete(FS_TREE, key)?;
        }
        for (bytenr, len, file_off) in extents {
            self.alloc.free_data(bytenr, len, FS_TREE, ino, file_off)?;
            if had_csums {
                self.remove_csum_range(bytenr, bytenr + len)?;
            }
        }
        self.apply_pending()
    }

    pub fn rename(
        &mut self,
        old_dir: u64,
        old_name: &str,
        new_dir: u64,
        new_name: &str,
    ) -> Result<()> {
        let old_name = check_name(old_name)?;
        let new_name = check_name(new_name)?;
        self.prepare_mutation()?;
        let ino = self.lookup_raw(old_dir, old_name)?.ok_or(Error::NotFound)?;
        if old_dir == new_dir && old_name == new_name {
            return Ok(());
        }
        // Replace an existing destination (like rename(2)).
        if let Some(existing) = self.lookup_raw(new_dir, new_name)? {
            if existing == ino {
                return Ok(());
            }
            let target = self.read_inode(existing)?;
            if target.kind() == FileKind::Dir && !self.dir_is_empty(existing)? {
                return Err(Error::NotEmpty);
            }
            self.remove_entry(new_dir, new_name)?;
            if target.nlink > 1 {
                let mut t2 = target.clone();
                t2.nlink -= 1;
                self.write_inode(existing, &t2)?;
            } else {
                self.purge_inode(existing, &target)?;
            }
        }
        // Write the new name first and only then drop the old one. Adding is
        // the step that can fail -- NoSpace, once metadata is exhausted --
        // and `add_entry` leaves nothing behind when it does, so the file
        // keeps the name it had. The other order lost the file outright: the
        // old name was already gone, and putting it back needed the very
        // space that had just run out. Removing an entry only deletes items,
        // so it cannot fail for want of space.
        let dir_type = self.read_inode(ino)?.kind().dir_type();
        self.add_entry(new_dir, new_name, ino, dir_type)?;
        if let Err(e) = self.remove_entry(old_dir, old_name) {
            let _ = self.remove_entry(new_dir, new_name);
            return Err(e);
        }
        let mut inode = self.read_inode(ino)?;
        inode.ctime = self.now();
        self.write_inode(ino, &inode)?;
        self.commit(false)
    }

    fn lookup_raw(&mut self, dir: u64, name: &[u8]) -> Result<Option<u64>> {
        let hash = crate::crc::name_hash(name);
        let mut t = self.tree();
        if let Some(item) = t.get(FS_TREE, Key::new(dir, DIR_ITEM_KEY, hash))? {
            for (_, entry) in parse_dir_entries(&item) {
                if entry.name == name {
                    if entry.location.item_type != INODE_ITEM_KEY {
                        return Err(Error::Unsupported("subvolume entry"));
                    }
                    return Ok(Some(entry.location.objectid));
                }
            }
        }
        Ok(None)
    }

    // ------------------------------------------------------------------
    // File data
    // ------------------------------------------------------------------

    /// All file extents of `ino` overlapping `[start, end)` as
    /// (file_offset, extent).
    fn extents_in_range(
        &mut self,
        ino: u64,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, FileExtent, Vec<u8>)>> {
        let mut out = Vec::new();
        let from = {
            let mut t = self.tree();
            match t.prev_item(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, start))? {
                Some((key, _)) if key.objectid == ino && key.item_type == EXTENT_DATA_KEY => {
                    key.offset
                }
                _ => 0,
            }
        };
        let mut t = self.tree();
        t.iter_from(
            FS_TREE,
            Key::new(ino, EXTENT_DATA_KEY, from),
            |key, data| {
                if key.objectid != ino || key.item_type != EXTENT_DATA_KEY || key.offset >= end {
                    return Ok(false);
                }
                // Skipping an item we cannot parse leaves a gap that `read`
                // fills with zeros, so an encrypted or truncated extent used
                // to read back as silent garbage. Fail the read instead.
                match FileExtent::parse(data) {
                    Some(ext) => out.push((key.offset, ext, data.to_vec())),
                    None => return Err(Error::Corrupt("file extent")),
                }
                Ok(true)
            },
        )?;
        Ok(out)
    }

    pub fn read(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        // Locate (or rebuild) this file's read-cache entry and move it to the
        // MRU slot (the vector's tail): the code below always works on
        // `read_cache.last()`. Entries predating the last volume write are
        // dropped wholesale — a CoW mutation anywhere may have relocated any
        // file's extents.
        let epoch = self.vol.write_epoch();
        self.read_cache.retain(|c| c.epoch == epoch);
        match self.read_cache.iter().position(|c| c.ino == ino) {
            Some(i) => {
                let entry = self.read_cache.remove(i);
                self.read_cache.push(entry);
            }
            None => {
                let inode = self.read_inode(ino)?;
                if inode.kind() == FileKind::Dir {
                    return Err(Error::IsDir);
                }
                if self.read_cache.len() >= READ_CACHE_FILES {
                    self.read_cache.remove(0);
                }
                self.read_cache.push(ReadCacheEntry {
                    ino,
                    epoch,
                    inode,
                    extents: Vec::new(),
                    cached_start: 0,
                    cached_end: 0,
                });
            }
        }
        let size = self.read_cache.last().unwrap().inode.size;
        if offset >= size {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(size - offset) as usize;
        let buf = &mut buf[..want];
        buf.fill(0);
        let end = offset + want as u64;
        // Make `extents` complete over the requested window, doing only the
        // tree lookups that window needs. Three cases against the cached
        // complete range `[cached_start, cached_end)`:
        //
        //  * fully inside — serve straight from the cache, no lookup;
        //  * forward extension (starts inside/at the boundary, ends past it) —
        //    scan only `[cached_end, end)` and append the NEW extents. The
        //    `e.0 >= cached_end` filter is the dedup for the one extent that
        //    merely spans the boundary: by the completeness invariant it is
        //    already in the list;
        //  * a scattered jump (forward past a gap, or backward below
        //    `cached_start` — the exact shape of demand-paged `mmap`: ld.so's
        //    page faults land scattered across a library, then its ELF header
        //    is re-read at offset 0). Rebuild the list for exactly the window
        //    requested, keeping EVERY extent `extents_in_range` returns —
        //    including the one that starts before `offset` but spans into the
        //    window (that is why it does the `prev_item` probe). The old code
        //    filtered that covering extent out after clearing the list, so the
        //    window silently read back as zeros — and the stale
        //    `[0, cached_end)` claim then served later low-offset reads from
        //    the emptied list as zeros too ("Exec format error" on every
        //    desktop library on the installed btrfs root; see
        //    tests/scattered_reads.rs).
        //
        // A jump still costs work proportional to the window actually
        // requested, not to how far it is from the last one — the goal of the
        // original optimization — it just no longer lies about coverage.
        let (cs, ce) = {
            let c = self.read_cache.last().unwrap();
            (c.cached_start, c.cached_end)
        };
        if offset >= cs && end <= ce {
            // Fully cached: nothing to look up.
        } else if offset >= cs && offset <= ce {
            // Forward extension of the cached run.
            let mut found = self.extents_in_range(ino, ce, end)?;
            let c = self.read_cache.last_mut().unwrap();
            for e in found.drain(..) {
                if e.0 >= ce {
                    c.extents.push(e);
                }
            }
            c.cached_end = end;
        } else {
            // Scattered jump: rebuild for this window, keep the covering extent.
            let mut found = self.extents_in_range(ino, offset, end)?;
            let c = self.read_cache.last_mut().unwrap();
            c.extents.clear();
            c.extents.append(&mut found);
            c.cached_start = offset;
            c.cached_end = end;
        }
        // Uncompressed extents are served here, straight into `buf`, without
        // cloning anything out of the read cache. Compressed ones need
        // `&mut self` (they inflate through `self.decompressed`), which the
        // borrow on `self.read_cache` rules out, so they are set aside and
        // handled in a second pass. Compression is rare enough that paying a
        // clone for it keeps the common path allocation-free.
        let mut compressed: Vec<(u64, FileExtent, Vec<u8>)> = Vec::new();
        let cache = self.read_cache.last().unwrap();
        for (file_off, ext, raw) in cache.extents.iter() {
            let file_off = *file_off;
            if ext.compression() != COMPRESS_NONE {
                compressed.push((file_off, ext.clone(), raw.clone()));
                continue;
            }
            match ext {
                FileExtent::Inline {
                    ram_bytes,
                    data_off,
                    ..
                } => {
                    let data = &raw[*data_off..];
                    let len = (*ram_bytes as usize).min(data.len());
                    // Inline extents always start at file offset 0.
                    let lo = offset.max(file_off) as usize;
                    let hi = (end as usize).min(len);
                    if lo < hi {
                        buf[lo - offset as usize..hi - offset as usize]
                            .copy_from_slice(&data[lo..hi]);
                    }
                }
                FileExtent::Regular {
                    disk_bytenr,
                    offset: ext_off,
                    num_bytes,
                    ..
                } => {
                    if *disk_bytenr == 0 {
                        continue; // hole
                    }
                    // Extents past the requested range can be skipped (the list
                    // covers the whole file).
                    if file_off >= end || file_off + *num_bytes <= offset {
                        continue;
                    }
                    let lo = offset.max(file_off);
                    let hi = end.min(file_off + *num_bytes);
                    if lo >= hi {
                        continue;
                    }
                    let disk = disk_bytenr + ext_off + (lo - file_off);
                    self.vol.read_logical(
                        disk,
                        &mut buf[(lo - offset) as usize..(hi - offset) as usize],
                    )?;
                }
            }
        }
        for (file_off, ext, raw) in compressed {
            self.read_compressed_extent(file_off, &ext, &raw, offset, end, buf)?;
        }
        Ok(want)
    }

    /// Copy the part of one compressed extent that falls inside
    /// `[offset, end)` into `buf` (which starts at file offset `offset`).
    fn read_compressed_extent(
        &mut self,
        file_off: u64,
        ext: &FileExtent,
        raw: &[u8],
        offset: u64,
        end: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        match *ext {
            FileExtent::Inline {
                ram_bytes,
                data_off,
                compression,
            } => {
                // An inline extent is the whole file and always starts at
                // offset 0, so the decompressed buffer is indexed by file
                // offset directly. They are at most one sector, so there is
                // nothing worth caching.
                let plain = inflate(compression, &raw[data_off..], ram_bytes)?;
                let lo = offset.max(file_off) as usize;
                let hi = (end as usize).min(plain.len());
                if lo < hi {
                    buf[lo - offset as usize..hi - offset as usize].copy_from_slice(&plain[lo..hi]);
                }
                Ok(())
            }
            FileExtent::Regular {
                disk_bytenr,
                disk_num_bytes,
                offset: ext_off,
                num_bytes,
                ram_bytes,
                compression,
            } => {
                if disk_bytenr == 0 {
                    return Ok(()); // hole; cannot be compressed, but be safe
                }
                if file_off >= end || file_off + num_bytes <= offset {
                    return Ok(());
                }
                let lo = offset.max(file_off);
                let hi = end.min(file_off + num_bytes);
                if lo >= hi {
                    return Ok(());
                }
                // `offset` and `num_bytes` index the DECOMPRESSED extent, so
                // the whole thing has to be inflated even for a 4 KiB read.
                let epoch = self.vol.write_epoch();
                let hit = matches!(
                    &self.decompressed,
                    Some(d) if d.epoch == epoch && d.disk_bytenr == disk_bytenr
                );
                if !hit {
                    if disk_num_bytes > MAX_COMPRESSED_EXTENT {
                        return Err(Error::Corrupt("compressed extent too large"));
                    }
                    let mut raw_extent = alloc::vec![0u8; disk_num_bytes as usize];
                    self.vol.read_logical(disk_bytenr, &mut raw_extent)?;
                    let data = inflate(compression, &raw_extent, ram_bytes)?;
                    self.decompressed = Some(DecompressedExtent {
                        epoch,
                        disk_bytenr,
                        data,
                    });
                }
                let plain = &self.decompressed.as_ref().unwrap().data;
                let from = (ext_off + (lo - file_off)) as usize;
                let to = from + (hi - lo) as usize;
                if to > plain.len() {
                    // The extent decompressed to less than its own item claims.
                    return Err(Error::Corrupt("short compressed extent"));
                }
                buf[(lo - offset) as usize..(hi - offset) as usize]
                    .copy_from_slice(&plain[from..to]);
                Ok(())
            }
        }
    }

    /// Allocate, zero and record one data extent for `[pos, pos+len)`.
    fn install_data_extent(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        pos: u64,
        bytenr: u64,
        len: u64,
        skip_file_ranges: &[(u64, u64)],
    ) -> Result<()> {
        self.alloc.note_data_extent(bytenr, len, FS_TREE, ino, pos);
        let extent_start = pos;
        let extent_end = pos + len;
        let mut covered = Vec::new();
        for &(start, end) in skip_file_ranges {
            if end <= start || end <= extent_start || start >= extent_end {
                continue;
            }
            covered.push((start.max(extent_start), end.min(extent_end)));
        }
        covered.sort_unstable_by_key(|(start, _)| *start);
        let mut merged = Vec::new();
        for (start, end) in covered {
            if let Some((_, prev_end)) = merged.last_mut() {
                if start <= *prev_end {
                    *prev_end = (*prev_end).max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        let zeros = alloc::vec![0u8; 64 * 1024];
        let mut cursor = extent_start;
        for (start, end) in merged {
            if cursor < start {
                let mut z = cursor - extent_start;
                let stop = start - extent_start;
                while z < stop {
                    let take = zeros.len().min((stop - z) as usize);
                    self.vol.write_logical(bytenr + z, &zeros[..take])?;
                    z += take as u64;
                }
            }
            cursor = cursor.max(end);
        }
        if cursor < extent_end {
            let mut z = cursor - extent_start;
            while z < len {
                let take = zeros.len().min((len - z) as usize);
                self.vol.write_logical(bytenr + z, &zeros[..take])?;
                z += take as u64;
            }
        }
        let ext = FileExtent::encode_regular(self.generation, bytenr, len, 0, len);
        {
            let mut t = self.tree();
            t.insert(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, pos), &ext)?;
        }
        inode.nbytes += len;
        self.apply_pending()
    }

    /// Make sure the file has allocated extents covering `[0, end)`; newly
    /// allocated space is zeroed on disk. Flips the inode to NODATASUM first
    /// (a structural change invalidates any pre-existing checksums) and
    /// converts inline extents to regular ones.
    ///
    /// Returns the byte offset covered by extents afterwards. On disk-full
    /// this is smaller than requested (POSIX-style short writes); the
    /// filesystem stays consistent.
    fn ensure_coverage(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        end: u64,
        write_start: u64,
        write_end: u64,
    ) -> Result<u64> {
        let sector = self.vol.sectorsize as u64;
        let mut target = end.div_ceil(sector) * sector;
        // Current coverage: end of the last extent.
        let mut covered = 0u64;
        let mut inline = None;
        {
            // Scan backward past any hole extents (disk_bytenr == 0) to find
            // the last *real* extent.  Holes do not constitute actual on-disk
            // coverage; counting them as coverage would cause write_extents to
            // hit a "write into hole" error for every write following a
            // truncate-up (the common ftruncate + write pattern used by apk).
            let mut search_bound = u64::MAX;
            loop {
                let result = {
                    let mut t = self.tree();
                    t.prev_item(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, search_bound))?
                };
                match result {
                    Some((key, data))
                        if key.objectid == ino && key.item_type == EXTENT_DATA_KEY =>
                    {
                        match FileExtent::parse(&data) {
                            Some(FileExtent::Regular {
                                num_bytes,
                                disk_bytenr,
                                ..
                            }) => {
                                if disk_bytenr != 0 {
                                    covered = key.offset + num_bytes;
                                    break;
                                }
                                // Hole extent (disk_bytenr == 0): keep scanning
                                // backward for a real extent.
                                if key.offset == 0 {
                                    break;
                                }
                                search_bound = key.offset - 1;
                            }
                            Some(FileExtent::Inline { ram_bytes, .. }) => {
                                inline = Some(ram_bytes as usize);
                                break;
                            }
                            None => return Err(Error::Corrupt("file extent")),
                        }
                    }
                    _ => break,
                }
            }
        }
        if let Some(inline_len) = inline {
            // Inline → regular conversion must not lose data on ENOSPC:
            // reserve the whole replacement up front, and only then drop the
            // inline item.
            target = target.max((inline_len as u64).div_ceil(sector) * sector);
            let _ = self.ensure_data_space(target);
            let mut reserved: Vec<(u64, u64)> = Vec::new();
            let mut got_total = 0u64;
            while got_total < target {
                match self.alloc.alloc_data(target - got_total) {
                    Ok((bytenr, got)) => {
                        reserved.push((bytenr, got));
                        got_total += got;
                    }
                    Err(Error::NoSpace) => {
                        for (bytenr, got) in reserved {
                            self.alloc.unreserve_data(bytenr, got)?;
                        }
                        return Err(Error::NoSpace);
                    }
                    Err(e) => return Err(e),
                }
            }
            let mut data = alloc::vec![0u8; inline_len];
            self.read(ino, 0, &mut data)?;
            {
                let mut t = self.tree();
                t.delete(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0))?;
            }
            inode.nbytes = 0;
            self.set_nodatasum(ino, inode)?;
            let mut pos = 0u64;
            let inline_skip = (0, inline_len as u64);
            let write_skip = (write_start, write_end);
            for (bytenr, got) in reserved {
                self.install_data_extent(ino, inode, pos, bytenr, got, &[inline_skip, write_skip])?;
                pos += got;
            }
            if !data.is_empty() {
                self.write_extents(ino, 0, &data)?;
            }
            return Ok(pos);
        }
        if covered >= target {
            return Ok(covered);
        }
        self.set_nodatasum(ino, inode)?;
        // Remove hole extents (disk_bytenr == 0) that the truncate-up path
        // may have left in [covered, target).  We must delete them before
        // calling install_data_extent so that the B-tree insert does not fail
        // with Error::Exists on the same key offset.
        {
            let holes: Vec<u64> = self
                .extents_in_range(ino, covered, target)?
                .into_iter()
                .filter_map(|(file_off, ext, _)| match ext {
                    FileExtent::Regular { disk_bytenr: 0, .. } => Some(file_off),
                    _ => None,
                })
                .collect();
            for file_off in holes {
                let mut t = self.tree();
                t.delete(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, file_off))?;
            }
        }
        // Linux-style speculative preallocation. A naive driver records one
        // extent — and runs one synchronous metadata commit — per write(), so
        // extracting a big package in 4 KiB chunks (libarchive) costs tens of
        // thousands of tree updates and stalls for seconds. Instead, when a file
        // grows, allocate a *geometrically growing* contiguous run past the
        // requested range (up to MAX_PREALLOC) and record it as one extent. The
        // following stream of small sequential writes then lands in already
        // covered space (`covered >= target` on entry) and only writes data —
        // no per-write extent insert. The speculative tail is zeroed like any
        // freshly allocated space and sits beyond i_size until later writes fill
        // it, so reads and `btrfs check` stay correct.
        const MAX_PREALLOC: u64 = 1024 * 1024;
        let prealloc = covered.min(MAX_PREALLOC);
        let alloc_target = {
            let want = covered.saturating_add((target - covered).max(prealloc));
            (want.div_ceil(sector) * sector).max(target)
        };
        let _ = self.ensure_data_space(alloc_target.saturating_sub(covered));
        let mut pos = covered;
        while pos < alloc_target {
            match self.alloc.alloc_data(alloc_target - pos) {
                Ok((bytenr, got)) => {
                    self.install_data_extent(
                        ino,
                        inode,
                        pos,
                        bytenr,
                        got,
                        &[(write_start, write_end)],
                    )?;
                    pos += got;
                }
                // Disk full: stop. The needed prefix (up to `target`) is tried
                // first since allocation starts at `covered`, so the write still
                // succeeds for whatever was covered; the speculative tail is
                // simply skipped.
                Err(Error::NoSpace) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(pos)
    }

    /// Set NODATASUM/NODATACOW on the inode, dropping any stale checksums.
    fn set_nodatasum(&mut self, ino: u64, inode: &mut InodeItem) -> Result<()> {
        if inode.flags & INODE_NODATASUM != 0 {
            return Ok(());
        }
        inode.flags |= INODE_NODATASUM | INODE_NODATACOW;
        let extents = self.extents_in_range(ino, 0, u64::MAX)?;
        for (_, ext, _) in extents {
            if let FileExtent::Regular {
                disk_bytenr,
                disk_num_bytes,
                ..
            } = ext
            {
                if disk_bytenr != 0 {
                    self.remove_csum_range(disk_bytenr, disk_bytenr + disk_num_bytes)?;
                }
            }
        }
        Ok(())
    }

    /// Write `data` into already-covered extents.
    fn write_extents(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset + data.len() as u64;
        let extents = self.extents_in_range(ino, offset, end)?;
        let extent_count = extents.len();
        let mut done = offset;
        for (file_off, ext, _) in extents {
            // `disk_bytenr + ext_off` addresses COMPRESSED bytes on a
            // compressed extent, so writing plaintext there would destroy the
            // stream and every other mapping into it. We only ever create
            // uncompressed extents, so this is about foreign volumes written
            // by Linux with `compress=zlib`: they stay readable, not writable.
            if ext.compression() != COMPRESS_NONE {
                return Err(Error::Unsupported("write to a compressed extent"));
            }
            if let FileExtent::Regular {
                disk_bytenr,
                offset: ext_off,
                num_bytes,
                ..
            } = ext
            {
                if disk_bytenr == 0 {
                    // A hole extent intersects the write range: ensure_coverage
                    // should have filled it. Log the geometry so a large-file
                    // EIO (e.g. `libLLVM.so` extraction) can be pinned to the
                    // btrfs coverage path rather than the block device.
                    warn!(
                        "btrfs: write_extents hole in range ino={} off={:#x} end={:#x} \
                         done={:#x} hole@{:#x} num_bytes={:#x} extents={}",
                        ino, offset, end, done, file_off, num_bytes, extent_count,
                    );
                    return Err(Error::Corrupt("write into hole"));
                }
                let lo = done.max(file_off);
                let hi = end.min(file_off + num_bytes);
                if lo >= hi {
                    continue;
                }
                let disk = disk_bytenr + ext_off + (lo - file_off);
                self.vol
                    .write_logical(disk, &data[(lo - offset) as usize..(hi - offset) as usize])?;
                done = hi;
            }
        }
        if done < end {
            warn!(
                "btrfs: write_extents uncovered ino={} off={:#x} end={:#x} done={:#x} \
                 (gap={:#x}) extents={}",
                ino,
                offset,
                end,
                done,
                end - done,
                extent_count,
            );
            return Err(Error::Corrupt("uncovered write range"));
        }
        Ok(())
    }

    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.prepare_mutation()?;
        let mut inode = self.read_inode(ino)?;
        match inode.kind() {
            FileKind::Dir => return Err(Error::IsDir),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
        let end = offset + data.len() as u64;
        // Any write through this driver invalidates pre-existing data
        // checksums (we do not maintain the csum tree), so make the inode
        // NODATASUM up front.
        if inode.flags & INODE_NODATASUM == 0 {
            self.set_nodatasum(ino, &mut inode)?;
            self.write_inode(ino, &inode)?;
        }
        let covered = match self.ensure_coverage(ino, &mut inode, end, offset, end) {
            Ok(covered) => covered,
            Err(e) => {
                // Keep nbytes consistent with whatever extents were added.
                warn!(
                    "btrfs: write ino={} off={:#x} len={} ensure_coverage failed: {:?}",
                    ino,
                    offset,
                    data.len(),
                    e,
                );
                let _ = self.write_inode(ino, &inode);
                let _ = self.commit(false);
                return Err(e);
            }
        };
        // Disk-full can leave the coverage short: do a POSIX-style partial
        // write of the covered prefix.
        let write_end = end.min(covered);
        if write_end <= offset {
            self.write_inode(ino, &inode)?;
            self.commit(false)?;
            return Err(Error::NoSpace);
        }
        if let Err(e) = self.write_extents(ino, offset, &data[..(write_end - offset) as usize]) {
            warn!(
                "btrfs: write ino={} off={:#x} len={} write_extents failed: {:?}",
                ino,
                offset,
                data.len(),
                e,
            );
            return Err(e);
        }
        if write_end > inode.size {
            inode.size = write_end;
        }
        let now = self.now();
        inode.mtime = now;
        inode.ctime = now;
        self.write_inode(ino, &inode)?;
        if let Err(e) = self.commit(false) {
            warn!(
                "btrfs: write ino={} off={:#x} len={} commit failed: {:?}",
                ino,
                offset,
                data.len(),
                e,
            );
            return Err(e);
        }
        Ok((write_end - offset) as usize)
    }

    pub fn truncate(&mut self, ino: u64, new_size: u64) -> Result<()> {
        self.prepare_mutation()?;
        let mut inode = self.read_inode(ino)?;
        match inode.kind() {
            FileKind::Dir => return Err(Error::IsDir),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
        if new_size == inode.size {
            return Ok(());
        }
        if new_size > inode.size {
            // NO_HOLES filesystems read missing ranges as zeros; for older
            // layouts insert an explicit hole extent.
            if self.vol.sb.incompat_flags() & INCOMPAT_NO_HOLES == 0 {
                self.set_nodatasum(ino, &mut inode)?;
                let sector = self.vol.sectorsize as u64;
                let start = inode.size.div_ceil(sector) * sector;
                let end = new_size.div_ceil(sector) * sector;
                if end > start {
                    let mut hole = [0u8; FILE_EXTENT_REG_LEN];
                    hole.copy_from_slice(&FileExtent::encode_regular(
                        self.generation,
                        0,
                        0,
                        0,
                        end - start,
                    ));
                    let mut t = self.tree();
                    t.insert(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, start), &hole)?;
                }
            }
        } else {
            self.set_nodatasum(ino, &mut inode)?;
            let sector = self.vol.sectorsize as u64;
            let keep = new_size.div_ceil(sector) * sector;
            let extents = self.extents_in_range(ino, 0, u64::MAX)?;
            for (file_off, ext, _) in extents {
                match ext {
                    FileExtent::Inline { ram_bytes, .. } => {
                        if new_size == 0 {
                            let mut t = self.tree();
                            t.delete(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0))?;
                            inode.nbytes = inode.nbytes.saturating_sub(ram_bytes);
                        } else if new_size < ram_bytes {
                            let mut data = alloc::vec![0u8; new_size as usize];
                            self.read(ino, 0, &mut data)?;
                            let enc = FileExtent::encode_inline(self.generation, &data);
                            let mut t = self.tree();
                            t.set_item(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0), &enc)?;
                            inode.nbytes = new_size;
                        }
                    }
                    FileExtent::Regular {
                        disk_bytenr,
                        disk_num_bytes,
                        num_bytes,
                        compression,
                        ..
                    } => {
                        if file_off >= keep {
                            // Fully beyond: drop and free.
                            {
                                let mut t = self.tree();
                                t.delete(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, file_off))?;
                            }
                            if disk_bytenr != 0 {
                                self.alloc.free_data(
                                    disk_bytenr,
                                    disk_num_bytes,
                                    FS_TREE,
                                    ino,
                                    file_off,
                                )?;
                            }
                            inode.nbytes = inode.nbytes.saturating_sub(num_bytes);
                        } else if file_off + num_bytes > keep {
                            // Straddling: shrink the mapping (the disk extent
                            // stays allocated in full). Only sound when the
                            // extent is stored plain -- `num_bytes` then
                            // matches the bytes on disk. On a compressed
                            // extent, rewriting ram_bytes would tell the next
                            // reader (us or Linux) to inflate to the wrong
                            // size.
                            if compression != COMPRESS_NONE {
                                return Err(Error::Unsupported(
                                    "truncate inside a compressed extent",
                                ));
                            }
                            let new_len = keep - file_off;
                            let mut t = self.tree();
                            t.update_in_place(
                                FS_TREE,
                                Key::new(ino, EXTENT_DATA_KEY, file_off),
                                |d| {
                                    put_u64(d, 8, new_len); // ram_bytes
                                    put_u64(d, 45, new_len); // num_bytes
                                },
                            )?;
                            inode.nbytes = inode.nbytes.saturating_sub(num_bytes - new_len);
                        }
                    }
                }
            }
            self.apply_pending()?;
        }
        inode.size = new_size;
        let now = self.now();
        inode.mtime = now;
        inode.ctime = now;
        self.write_inode(ino, &inode)?;
        self.commit(false)
    }

    // ------------------------------------------------------------------
    // Checksum-tree cleanup (foreign images only)
    // ------------------------------------------------------------------

    /// Remove EXTENT_CSUM coverage for the logical byte range `[start, end)`.
    fn remove_csum_range(&mut self, start: u64, end: u64) -> Result<()> {
        let sector = self.vol.sectorsize as u64;
        loop {
            // Find a csum item overlapping the range.
            let found = {
                let mut t = self.tree();
                let prev = t.prev_item(
                    CSUM_TREE,
                    Key::new(EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY, end - 1),
                )?;
                match prev {
                    Some((key, data))
                        if key.objectid == EXTENT_CSUM_OBJECTID
                            && key.item_type == EXTENT_CSUM_KEY =>
                    {
                        let covered = key.offset + (data.len() as u64 / 4) * sector;
                        if covered > start {
                            Some((key, data))
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            };
            let (key, data) = match found {
                Some(x) => x,
                None => return Ok(()),
            };
            let item_start = key.offset;
            let item_end = item_start + (data.len() as u64 / 4) * sector;
            let mut t = self.tree();
            if item_start >= start && item_end <= end {
                t.delete(CSUM_TREE, key)?;
            } else if item_start < start && item_end > end {
                // Split: keep head and tail.
                let head = &data[..((start - item_start) / sector * 4) as usize];
                let tail = &data[((end - item_start) / sector * 4) as usize..];
                let head = head.to_vec();
                let tail = tail.to_vec();
                t.set_item(CSUM_TREE, key, &head)?;
                t.insert(
                    CSUM_TREE,
                    Key::new(EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY, end),
                    &tail,
                )?;
            } else if item_start < start {
                // Keep head only.
                let head = data[..((start - item_start) / sector * 4) as usize].to_vec();
                t.set_item(CSUM_TREE, key, &head)?;
            } else {
                // item_end > end: keep tail, re-keyed at `end`.
                let tail = data[((end - item_start) / sector * 4) as usize..].to_vec();
                t.delete(CSUM_TREE, key)?;
                t.insert(
                    CSUM_TREE,
                    Key::new(EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY, end),
                    &tail,
                )?;
            }
            self.apply_pending()?;
        }
    }
}

/// Put an item back the way it was before a just-attempted edit: restore its
/// previous bytes, or delete it if it did not exist. Both shrink the leaf, so
/// neither can fail for want of space; a failure here would mean the tree is
/// already damaged, and there is nothing better to do than report the error
/// that caused the unwind.
fn undo_item(t: &mut Tree<'_>, key: Key, was: Option<&[u8]>) {
    let _ = match was {
        Some(prev) => t.set_item(FS_TREE, key, prev),
        None => t.delete(FS_TREE, key),
    };
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

// ---------------------------------------------------------------------------
// Compressed-extent tests
// ---------------------------------------------------------------------------
//
// These live inside the crate rather than in `tests/` because building a
// compressed extent needs the private tree and allocator APIs: nothing in
// this driver ever *writes* compression, and nothing in this container can
// mount btrfs to have Linux write one for us (no btrfs module, no privileged
// mount). So the item is hand-built here -- and then handed to btrfs-progs,
// which parses it with the real `struct btrfs_file_extent_item`, as an
// independent check that the layout is right rather than merely
// self-consistent.
#[cfg(all(test, feature = "std"))]
mod compressed_tests {
    use super::*;
    use crate::device::FileDevice;
    use crate::mkfs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn tmpfile(name: &str, size: u64) -> PathBuf {
        let path =
            std::env::temp_dir().join(std::format!("btrfs-zlib-{}-{}", std::process::id(), name));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(size).unwrap();
        path
    }

    fn open_dev(path: &Path) -> Arc<dyn BlockDevice> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        Arc::new(FileDevice::open(f).unwrap())
    }

    fn opts() -> mkfs::MkfsOptions {
        let mut seed = 0x0bad_c0de_dead_beefu64;
        let mut uuid = || {
            let mut u = [0u8; 16];
            for b in u.iter_mut() {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *b = (seed >> 33) as u8;
            }
            u[6] = (u[6] & 0x0f) | 0x40;
            u[8] = (u[8] & 0x3f) | 0x80;
            u
        };
        mkfs::MkfsOptions {
            label: "eclipse".into(),
            fsid: uuid(),
            chunk_uuid: uuid(),
            dev_uuid: uuid(),
            subvol_uuid: uuid(),
            now: (1_700_000_000, 0),
        }
    }

    fn have_progs() -> bool {
        Command::new("btrfs")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Data that compresses well but is not a constant run, so a decoder that
    /// silently produced zeros (or dropped a block) cannot pass by accident.
    fn payload(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut seed = 1u32;
        while out.len() < len {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let word = std::format!("word{} ", (seed >> 20) % 64);
            out.extend_from_slice(word.as_bytes());
        }
        out.truncate(len);
        out
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
    }

    /// Same field layout as [`FileExtent::encode_regular`], plus compression.
    fn encode_compressed_regular(
        generation: u64,
        disk_bytenr: u64,
        disk_num_bytes: u64,
        offset: u64,
        num_bytes: u64,
        ram_bytes: u64,
    ) -> [u8; FILE_EXTENT_REG_LEN] {
        let mut b = [0u8; FILE_EXTENT_REG_LEN];
        put_u64(&mut b, 0, generation);
        put_u64(&mut b, 8, ram_bytes);
        b[16] = COMPRESS_ZLIB;
        b[20] = FILE_EXTENT_REG;
        put_u64(&mut b, 21, disk_bytenr);
        put_u64(&mut b, 29, disk_num_bytes);
        put_u64(&mut b, 37, offset);
        put_u64(&mut b, 45, num_bytes);
        b
    }

    fn encode_compressed_inline(generation: u64, ram_bytes: u64, zdata: &[u8]) -> Vec<u8> {
        let mut b = alloc::vec![0u8; FILE_EXTENT_HDR_LEN + zdata.len()];
        put_u64(&mut b, 0, generation);
        put_u64(&mut b, 8, ram_bytes);
        b[16] = COMPRESS_ZLIB;
        b[20] = FILE_EXTENT_INLINE;
        b[FILE_EXTENT_HDR_LEN..].copy_from_slice(zdata);
        b
    }

    /// Create `name` under the root and give it one zlib-compressed regular
    /// extent holding `plain`, the way `compress=zlib` on Linux would.
    fn install_zlib_file(fs: &mut Btrfs, name: &str, plain: &[u8]) -> u64 {
        let root = fs.root_ino();
        let ino = fs.create(root, name, FileKind::Regular, 0o644, 0).unwrap();
        let sector = fs.vol.sectorsize as u64;
        let z = zlib(plain);
        let disk_len = (z.len() as u64).div_ceil(sector) * sector;

        fs.ensure_data_space(disk_len).unwrap();
        let (bytenr, got) = fs.alloc.alloc_data(disk_len).unwrap();
        assert_eq!(got, disk_len, "allocator split the extent");
        // The tail of the last sector is padding; zero it so the image is
        // deterministic, then lay the stream down.
        let mut sectors = alloc::vec![0u8; disk_len as usize];
        sectors[..z.len()].copy_from_slice(&z);
        fs.vol.write_logical(bytenr, &sectors).unwrap();
        fs.alloc.note_data_extent(bytenr, disk_len, FS_TREE, ino, 0);

        let item = encode_compressed_regular(
            fs.generation,
            bytenr,
            disk_len,
            0,
            plain.len() as u64,
            plain.len() as u64,
        );
        fs.tree()
            .insert(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0), &item)
            .unwrap();
        fs.apply_pending().unwrap();

        // Linux always checksums compressed data, and btrfs-progs enforces it:
        // `check_file_extent` raises I_ERR_BAD_FILE_EXTENT for
        // `compression && nodatasum`, and I_ERR_SOME_CSUM_MISSING unless the
        // csums cover disk_num_bytes (the COMPRESSED length). This driver
        // creates every file NODATASUM|NODATACOW, so an image that is to pass
        // `btrfs check` has to undo that here and lay the csums down itself.
        install_csums(fs, bytenr, &sectors);

        let mut inode = fs.read_inode(ino).unwrap();
        inode.size = plain.len() as u64;
        // btrfs accounts a file's nbytes in DECOMPRESSED bytes: `btrfs check`
        // sums the extents' `num_bytes`, not what they occupy on disk.
        inode.nbytes = plain.len() as u64;
        inode.flags &= !(INODE_NODATASUM | INODE_NODATACOW);
        fs.write_inode(ino, &inode).unwrap();
        fs.commit(true).unwrap();
        ino
    }

    /// Checksum every sector of `data` (which lives at logical `bytenr`) into
    /// one EXTENT_CSUM item, the way Linux would for a compressed extent.
    fn install_csums(fs: &mut Btrfs, bytenr: u64, data: &[u8]) {
        let sector = fs.vol.sectorsize;
        assert_eq!(data.len() % sector, 0);
        let mut item = Vec::with_capacity(data.len() / sector * 4);
        for chunk in data.chunks(sector) {
            item.extend_from_slice(&crate::crc::checksum(chunk).to_le_bytes());
        }
        fs.tree()
            .insert(
                CSUM_TREE,
                Key::new(EXTENT_CSUM_OBJECTID, EXTENT_CSUM_KEY, bytenr),
                &item,
            )
            .unwrap();
    }

    #[test]
    fn regular_zlib_extent_reads_back() {
        let path = tmpfile("regular", 64 * 1024 * 1024);
        let dev = open_dev(&path);
        mkfs::format(&*dev, &opts()).unwrap();
        let mut fs = Btrfs::mount(dev, false).unwrap();
        let plain = payload(96 * 1024);
        let ino = install_zlib_file(&mut fs, "z", &plain);
        drop(fs);

        // Re-mount: nothing may be served out of a cache built while writing.
        let mut fs = Btrfs::mount(open_dev(&path), true).unwrap();
        let mut got = alloc::vec![0u8; plain.len()];
        assert_eq!(fs.read(ino, 0, &mut got).unwrap(), plain.len());
        assert_eq!(got, plain, "whole-file read");

        // Partial reads: the mapping is indexed in DECOMPRESSED bytes, so an
        // implementation that seeks into the compressed stream lands wrong.
        for &(off, len) in &[
            (0u64, 4096usize),
            (4096, 4096),
            (40_000, 1234),
            (95_000, 1000),
        ] {
            let mut got = alloc::vec![0u8; len];
            let n = fs.read(ino, off, &mut got).unwrap();
            assert_eq!(n, len, "short read at {}", off);
            assert_eq!(
                got,
                &plain[off as usize..off as usize + len],
                "mismatch at offset {}",
                off
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn inline_zlib_extent_reads_back() {
        let path = tmpfile("inline", 64 * 1024 * 1024);
        let dev = open_dev(&path);
        mkfs::format(&*dev, &opts()).unwrap();
        let mut fs = Btrfs::mount(dev, false).unwrap();
        let root = fs.root_ino();
        let ino = fs
            .create(root, "small", FileKind::Regular, 0o644, 0)
            .unwrap();
        let plain = payload(2000);
        let z = zlib(&plain);
        let item = encode_compressed_inline(fs.generation, plain.len() as u64, &z);
        fs.tree()
            .insert(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0), &item)
            .unwrap();
        let mut inode = fs.read_inode(ino).unwrap();
        inode.size = plain.len() as u64;
        inode.nbytes = plain.len() as u64;
        inode.flags &= !(INODE_NODATASUM | INODE_NODATACOW);
        fs.write_inode(ino, &inode).unwrap();
        fs.commit(true).unwrap();
        drop(fs);

        let mut fs = Btrfs::mount(open_dev(&path), true).unwrap();
        let mut got = alloc::vec![0u8; plain.len()];
        assert_eq!(fs.read(ino, 0, &mut got).unwrap(), plain.len());
        assert_eq!(got, plain);
        let mut tail = alloc::vec![0u8; 500];
        assert_eq!(fs.read(ino, 1500, &mut tail).unwrap(), 500);
        assert_eq!(tail, &plain[1500..2000]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writing_over_a_compressed_extent_is_refused() {
        let path = tmpfile("rw", 64 * 1024 * 1024);
        let dev = open_dev(&path);
        mkfs::format(&*dev, &opts()).unwrap();
        let mut fs = Btrfs::mount(dev, false).unwrap();
        let plain = payload(96 * 1024);
        let ino = install_zlib_file(&mut fs, "z", &plain);
        drop(fs);

        let mut fs = Btrfs::mount(open_dev(&path), false).unwrap();
        // Overwriting in place would scribble plaintext into the middle of
        // the deflate stream and destroy the whole extent.
        assert_eq!(
            fs.write(ino, 8192, b"hello"),
            Err(Error::Unsupported("write to a compressed extent"))
        );
        // Truncating inside it would leave a ram_bytes that lies.
        assert_eq!(
            fs.truncate(ino, 40_000),
            Err(Error::Unsupported("truncate inside a compressed extent"))
        );
        // ...and the data is still intact after both refusals.
        let mut got = alloc::vec![0u8; plain.len()];
        assert_eq!(fs.read(ino, 0, &mut got).unwrap(), plain.len());
        assert_eq!(got, plain);
        let _ = std::fs::remove_file(&path);
    }

    /// The layout oracle: btrfs-progs parses the item we built with the real
    /// `struct btrfs_file_extent_item`. If any field sat at the wrong offset,
    /// `btrfs check` would reject the image and the dump would not report a
    /// zlib extent of the right size.
    #[test]
    fn btrfs_progs_agrees_with_our_compressed_item() {
        if !have_progs() {
            std::eprintln!("btrfs-progs not available; skipping");
            return;
        }
        let path = tmpfile("progs", 64 * 1024 * 1024);
        let dev = open_dev(&path);
        mkfs::format(&*dev, &opts()).unwrap();
        let mut fs = Btrfs::mount(dev, false).unwrap();
        let plain = payload(96 * 1024);
        install_zlib_file(&mut fs, "z", &plain);
        drop(fs);

        let out = Command::new("btrfs")
            .args(["check", "--force"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "btrfs check failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        let out = Command::new("btrfs")
            .args(["inspect-internal", "dump-tree", "-t", "5"])
            .arg(&path)
            .output()
            .unwrap();
        let dump = String::from_utf8_lossy(&out.stdout);
        // Every one of these lines is btrfs-progs reading a field of our item
        // through the real `struct btrfs_file_extent_item`; each would read
        // as garbage if we had put that field at the wrong offset.
        for expect in [
            "extent data disk byte 13631488 nr 20480",
            "extent data offset 0 nr 98304 ram 98304",
            "extent compression 1 (zlib)",
        ] {
            assert!(
                dump.lines().any(|l| l.trim() == expect),
                "btrfs-progs did not read back {:?}; dump:\n{}",
                expect,
                dump
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}
