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
    sb_dirty: bool,
    clock: Option<fn() -> (u64, u32)>,
}

impl Btrfs {
    // ------------------------------------------------------------------
    // Mount / setup
    // ------------------------------------------------------------------

    pub fn mount(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        let vol = Volume::open(dev)?;
        let generation = vol.sb.generation();
        let mut fs = Self {
            vol,
            roots: RootCache::new(),
            alloc: FreeSpace::default(),
            next_ino: FIRST_FREE_OBJECTID,
            generation,
            sb_dirty: false,
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
        let unknown_ro = self.vol.sb.compat_ro_flags()
            & !(COMPAT_RO_FREE_SPACE_TREE | COMPAT_RO_FREE_SPACE_TREE_VALID);
        if unknown_ro != 0 {
            return Err(Error::Unsupported("compat_ro flags"));
        }
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
                        let item =
                            BlockGroupItem::parse(data).ok_or(Error::Corrupt("bg item"))?;
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
        self.alloc.dev_free.insert(DEV_RESERVED, dev_item.total_bytes.saturating_sub(DEV_RESERVED));
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
            let _ = self
                .alloc
                .dev_free
                .take(off, SUPERBLOCK_SIZE as u64);
        }
        Ok(())
    }

    fn load_next_ino(&mut self) -> Result<()> {
        let mut t = self.tree();
        let last = t.prev_item(
            FS_TREE,
            Key::new(LAST_FREE_OBJECTID, u8::MAX, u64::MAX),
        )?;
        self.next_ino = match last {
            Some((key, _)) if key.objectid >= FIRST_FREE_OBJECTID => key.objectid + 1,
            _ => FIRST_FREE_OBJECTID,
        };
        Ok(())
    }

    /// Grow the filesystem to fill the whole device (used after the installer
    /// copies a small image onto a big partition). Returns true if grown.
    pub fn grow_to_device(&mut self) -> Result<bool> {
        self.writable()?;
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
        info!("btrfs: grown from {} to {} bytes", old, dev_size);
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
        if force_sb || self.sb_dirty {
            self.vol.write_superblock()?;
            self.sb_dirty = false;
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
        let size = want.max(DATA_CHUNK_SIZE.min(self.alloc.dev_free.largest_in(0, u64::MAX).map(|r| r.1).unwrap_or(0)));
        match self.create_chunk(BLOCK_GROUP_DATA, size) {
            Ok(()) => Ok(()),
            Err(Error::NoSpace) if self.alloc.data_free() > 0 => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn ensure_system_space(&mut self) -> Result<()> {
        let nodesize = self.vol.nodesize as u64;
        let free: u64 = self
            .alloc
            .bgs
            .values()
            .filter(|bg| bg.flags & BLOCK_GROUP_SYSTEM != 0)
            .map(|bg| self.alloc.free.total_free_in(bg.start, bg.start + bg.len))
            .sum();
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
        let logical = self
            .alloc
            .logical_end()
            .max(self.vol.chunks().iter().map(|c| c.logical + c.length).max().unwrap_or(0));
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
        info!(
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
        self.writable()?;
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
        self.writable()?;
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
                drop(t);
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
        {
            let mut t = self.tree();
            // DIR_ITEM (append on hash collision).
            let key = Key::new(dir, DIR_ITEM_KEY, hash);
            match t.get(FS_TREE, key)? {
                Some(mut existing) => {
                    existing.extend_from_slice(&enc);
                    t.set_item(FS_TREE, key, &existing)?;
                }
                None => t.insert(FS_TREE, key, &enc)?,
            }
            // DIR_INDEX.
            t.insert(FS_TREE, Key::new(dir, DIR_INDEX_KEY, index), &enc)?;
            // INODE_REF.
            let ref_key = Key::new(ino, INODE_REF_KEY, dir);
            let ref_entry = encode_inode_ref(index, name);
            match t.get(FS_TREE, ref_key)? {
                Some(mut existing) => {
                    existing.extend_from_slice(&ref_entry);
                    t.set_item(FS_TREE, ref_key, &existing)?;
                }
                None => t.insert(FS_TREE, ref_key, &ref_entry)?,
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
            t.insert(FS_TREE, Key::new(ino, INODE_ITEM_KEY, 0), &inode.encode())?;
        }
        self.add_entry(dir, name, ino, kind.dir_type())?;
        self.commit(false)?;
        Ok(ino)
    }

    pub fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<u64> {
        if target.is_empty() || target.len() + FILE_EXTENT_HDR_LEN + ITEM_SIZE > self.vol.nodesize - HEADER_SIZE {
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
        let (_, dir_type) = self.remove_entry(old_dir, old_name)?;
        self.add_entry(new_dir, new_name, ino, dir_type)?;
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
        t.iter_from(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, from), |key, data| {
            if key.objectid != ino || key.item_type != EXTENT_DATA_KEY || key.offset >= end {
                return Ok(false);
            }
            if let Some(ext) = FileExtent::parse(data) {
                out.push((key.offset, ext, data.to_vec()));
            }
            Ok(true)
        })?;
        Ok(out)
    }

    pub fn read(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.read_inode(ino)?;
        if inode.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        if offset >= inode.size {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(inode.size - offset) as usize;
        let buf = &mut buf[..want];
        buf.fill(0);
        let end = offset + want as u64;
        let extents = self.extents_in_range(ino, offset, end)?;
        for (file_off, ext, raw) in extents {
            match ext {
                FileExtent::Inline {
                    ram_bytes,
                    data_off,
                } => {
                    let data = &raw[data_off..];
                    let len = (ram_bytes as usize).min(data.len());
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
                    if disk_bytenr == 0 {
                        continue; // hole
                    }
                    let lo = offset.max(file_off);
                    let hi = end.min(file_off + num_bytes);
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
        Ok(want)
    }

    /// Make sure the file has allocated extents covering `[0, end)`; newly
    /// allocated space is zeroed on disk. Flips the inode to NODATASUM first
    /// (a structural change invalidates any pre-existing checksums) and
    /// converts inline extents to regular ones.
    fn ensure_coverage(&mut self, ino: u64, inode: &mut InodeItem, end: u64) -> Result<()> {
        let sector = self.vol.sectorsize as u64;
        let mut target = (end + sector - 1) / sector * sector;
        // Current coverage: end of the last extent.
        let mut covered = 0u64;
        let mut inline = None;
        {
            let mut t = self.tree();
            if let Some((key, data)) =
                t.prev_item(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, u64::MAX))?
            {
                if key.objectid == ino && key.item_type == EXTENT_DATA_KEY {
                    match FileExtent::parse(&data) {
                        Some(FileExtent::Regular { num_bytes, .. }) => {
                            covered = key.offset + num_bytes;
                        }
                        Some(FileExtent::Inline { ram_bytes, .. }) => {
                            inline = Some(ram_bytes as usize);
                        }
                        None => return Err(Error::Corrupt("file extent")),
                    }
                }
            }
        }
        // Convert an inline extent into regular coverage: stash the data,
        // drop the inline item, then write the data back below.
        let mut writeback = None;
        if let Some(inline_len) = inline {
            let mut data = alloc::vec![0u8; inline_len];
            self.read(ino, 0, &mut data)?;
            {
                let mut t = self.tree();
                t.delete(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, 0))?;
            }
            inode.nbytes = 0;
            covered = 0;
            target = target.max((inline_len as u64 + sector - 1) / sector * sector);
            writeback = Some(data);
        }
        if covered >= target && writeback.is_none() {
            return Ok(());
        }
        self.set_nodatasum(ino, inode)?;
        self.ensure_data_space(target.saturating_sub(covered))?;
        let zeros = alloc::vec![0u8; 64 * 1024];
        let mut pos = covered;
        while pos < target {
            let (bytenr, got) = self.alloc.alloc_data(target - pos)?;
            self.alloc
                .note_data_extent(bytenr, got, FS_TREE, ino, pos);
            // Zero the freshly allocated region.
            let mut z = 0u64;
            while z < got {
                let take = zeros.len().min((got - z) as usize);
                self.vol.write_logical(bytenr + z, &zeros[..take])?;
                z += take as u64;
            }
            let ext = FileExtent::encode_regular(self.generation, bytenr, got, 0, got);
            {
                let mut t = self.tree();
                t.insert(FS_TREE, Key::new(ino, EXTENT_DATA_KEY, pos), &ext)?;
            }
            inode.nbytes += got;
            pos += got;
            self.apply_pending()?;
        }
        if let Some(data) = writeback {
            if !data.is_empty() {
                self.write_extents(ino, 0, &data)?;
            }
        }
        Ok(())
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
        let mut done = offset;
        for (file_off, ext, _) in extents {
            if let FileExtent::Regular {
                disk_bytenr,
                offset: ext_off,
                num_bytes,
                ..
            } = ext
            {
                if disk_bytenr == 0 {
                    return Err(Error::Corrupt("write into hole"));
                }
                let lo = done.max(file_off);
                let hi = end.min(file_off + num_bytes);
                if lo >= hi {
                    continue;
                }
                let disk = disk_bytenr + ext_off + (lo - file_off);
                self.vol.write_logical(
                    disk,
                    &data[(lo - offset) as usize..(hi - offset) as usize],
                )?;
                done = hi;
            }
        }
        if done < end {
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
        self.ensure_coverage(ino, &mut inode, end)?;
        self.write_extents(ino, offset, data)?;
        if end > inode.size {
            inode.size = end;
        }
        let now = self.now();
        inode.mtime = now;
        inode.ctime = now;
        self.write_inode(ino, &inode)?;
        self.commit(false)?;
        Ok(data.len())
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
                let start = (inode.size + sector - 1) / sector * sector;
                let end = (new_size + sector - 1) / sector * sector;
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
            let keep = (new_size + sector - 1) / sector * sector;
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
                            // stays allocated in full).
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
                            inode.nbytes =
                                inode.nbytes.saturating_sub(num_bytes - new_len);
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
            drop(t);
            self.apply_pending()?;
        }
    }
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
