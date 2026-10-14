//! High-level filesystem operations over a mounted EclipseFS volume.
//!
//! The method set is the one the kernel's mount adapter and `xtask` call on
//! `btrfs::Btrfs`, with the same argument and error conventions: inode
//! numbers are `u64`, writes are POSIX-style partial on a full device, and
//! every mutating call is committed (bitmaps + superblock) before it returns.
//!
//! Metadata is updated in place (no journal yet): an inode record, a bitmap
//! block and the superblock are each one checksummed write.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::blocks::{AllocationGroup, BlockAllocator};
use crate::device::BlockDevice;
use crate::extent::{Extent, MAX_EXTENT_BLOCKS};
use crate::format::*;
use crate::{Error, Result};

/// The inode table grows by this many bytes at a time (1024 inodes).
pub(crate) const ITABLE_GROW: u64 = 64 * BLOCK_SIZE;
/// Inodes kept decoded in memory. The cache is write-through, so eviction
/// only costs a re-read.
const INODE_CACHE: usize = 4096;
/// Directories kept parsed in memory.
const DIR_CACHE: usize = 256;
/// Compact a directory once this many bytes of it are removed entries (and
/// they are at least half of it).
const DIR_COMPACT_MIN: u64 = 4096;
/// A directory is read into memory whole; anything this large is a corrupt
/// size field, not a directory (it would be ~250k entries of 255 bytes).
const MAX_DIR_BYTES: u64 = 64 * 1024 * 1024;

static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

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

#[derive(Debug, Clone, Copy)]
struct DirSlot {
    ino: u64,
    kind: FileKind,
    off: u64,
    len: u64,
}

#[derive(Debug, Default)]
struct DirCache {
    entries: BTreeMap<String, DirSlot>,
    /// Bytes of removed entries still taking space.
    dead: u64,
}

pub struct EclipseFS {
    dev: Arc<dyn BlockDevice>,
    sb: Superblock,
    /// The inode table file (its record lives in the superblock).
    itable: Inode,
    alloc: BlockAllocator,
    inodes: BTreeMap<u64, Inode>,
    dirs: BTreeMap<u64, DirCache>,
    read_only: bool,
    sb_dirty: bool,
    clock: Option<fn() -> (u64, u32)>,
}

// ---------------------------------------------------------------------------
// Block-level helpers shared with mkfs
// ---------------------------------------------------------------------------

/// Write `len` zero bytes at device byte `at`.
pub(crate) fn zero(dev: &dyn BlockDevice, at: u64, len: u64) -> Result<()> {
    let mut done = 0;
    while done < len {
        let n = (len - done).min(ZEROS.len() as u64);
        dev.write_at(at + done, &ZEROS[..n as usize])?;
        done += n;
    }
    Ok(())
}

/// Read file content at `off` (clamped to the file size; holes read as 0).
pub(crate) fn read_data(
    dev: &dyn BlockDevice,
    inode: &Inode,
    off: u64,
    buf: &mut [u8],
) -> Result<usize> {
    if off >= inode.size || buf.is_empty() {
        return Ok(0);
    }
    let n = (buf.len() as u64).min(inode.size - off) as usize;
    if inode.is_inline() {
        let o = off as usize;
        let avail = inode.inline.len().saturating_sub(o).min(n);
        buf[..avail].copy_from_slice(&inode.inline[o..o + avail]);
        buf[avail..n].fill(0);
        return Ok(n);
    }
    let mut done = 0usize;
    while done < n {
        let pos = off + done as u64;
        let lb = pos / BLOCK_SIZE;
        let in_blk = pos % BLOCK_SIZE;
        let left = (n - done) as u64;
        match inode.extents.lookup(lb) {
            Some((phys, run)) => {
                let take = (run * BLOCK_SIZE - in_blk).min(left) as usize;
                dev.read_at(phys * BLOCK_SIZE + in_blk, &mut buf[done..done + take])?;
                done += take;
            }
            None => {
                let hole_end = inode
                    .extents
                    .next_start_after(lb)
                    .map(|s| s * BLOCK_SIZE)
                    .unwrap_or(u64::MAX);
                let take = (hole_end - pos).min(left) as usize;
                buf[done..done + take].fill(0);
                done += take;
            }
        }
    }
    Ok(n)
}

/// Write file content at `off`, allocating blocks for unmapped ranges as close
/// to the file's previous blocks as possible. Returns how much was written: a
/// full device cuts the write short (`Ok(n)` with `n < data.len()`), and only
/// a write that could place nothing is `Err(NoSpace)`.
pub(crate) fn write_data(
    dev: &dyn BlockDevice,
    alloc: &mut BlockAllocator,
    inode: &mut Inode,
    off: u64,
    data: &[u8],
) -> Result<usize> {
    if data.is_empty() {
        return Ok(0);
    }
    debug_assert!(!inode.is_inline());
    let end = off.checked_add(data.len() as u64).ok_or(Error::Invalid)?;
    let mut pos = off;
    while pos < end {
        let lb = pos / BLOCK_SIZE;
        let (phys, run, fresh) = match inode.extents.lookup(lb) {
            Some((p, r)) => (p, r, false),
            None => {
                let mut want = (end - 1) / BLOCK_SIZE - lb + 1;
                if let Some(next) = inode.extents.next_start_after(lb) {
                    want = want.min(next - lb);
                }
                want = want.min(MAX_EXTENT_BLOCKS);
                let goal = inode.extents.goal_for(lb);
                let Some((p, got)) = alloc.allocate(goal, want) else {
                    break;
                };
                inode.extents.add_extent(Extent::new(lb, p, got as u32))?;
                inode.extents_dirty = true;
                inode.nbytes += got * BLOCK_SIZE;
                (p, got, true)
            }
        };
        let run_start = lb * BLOCK_SIZE;
        let run_end = run_start + run * BLOCK_SIZE;
        let chunk_end = end.min(run_end);
        if fresh {
            // New blocks hold whatever was there before: clear the parts of
            // them this write does not cover.
            if pos > run_start {
                zero(dev, phys * BLOCK_SIZE, pos - run_start)?;
            }
            if chunk_end < run_end {
                zero(
                    dev,
                    phys * BLOCK_SIZE + (chunk_end - run_start),
                    run_end - chunk_end,
                )?;
            }
        }
        dev.write_at(
            phys * BLOCK_SIZE + (pos - run_start),
            &data[(pos - off) as usize..(chunk_end - off) as usize],
        )?;
        pos = chunk_end;
    }
    if pos == off {
        return Err(Error::NoSpace);
    }
    if pos > inode.size {
        inode.size = pos;
    }
    Ok((pos - off) as usize)
}

/// Extend a file with zeroed, allocated blocks up to `to` bytes.
pub(crate) fn extend_zeroed(
    dev: &dyn BlockDevice,
    alloc: &mut BlockAllocator,
    inode: &mut Inode,
    to: u64,
) -> Result<()> {
    while inode.size < to {
        let n = (to - inode.size).min(ZEROS.len() as u64) as usize;
        let at = inode.size;
        let w = write_data(dev, alloc, inode, at, &ZEROS[..n])?;
        if w < n {
            return Err(Error::NoSpace);
        }
    }
    Ok(())
}

/// Write the overflow chain for `inode`'s extents past the inline ones. New
/// blocks are allocated and written before the old chain is released, so a
/// full device leaves the record on disk pointing at a valid chain.
pub(crate) fn write_chain(
    dev: &dyn BlockDevice,
    alloc: &mut BlockAllocator,
    owner: u64,
    inode: &mut Inode,
) -> Result<()> {
    let ext = inode.extents.extents();
    let rest = if inode.is_inline() || ext.len() <= INLINE_EXTENTS {
        &[][..]
    } else {
        &ext[INLINE_EXTENTS..]
    };
    let need = rest.len().div_ceil(EXTENTS_PER_BLOCK);
    let mut blocks = Vec::with_capacity(need);
    let mut goal = rest.first().map(|e| e.physical_block);
    for _ in 0..need {
        match alloc.allocate(goal, 1) {
            Some((b, _)) => {
                blocks.push(b);
                goal = Some(b + 1);
            }
            None => {
                for b in blocks {
                    alloc.free(b, 1)?;
                }
                return Err(Error::NoSpace);
            }
        }
    }
    for (i, chunk) in rest.chunks(EXTENTS_PER_BLOCK).enumerate() {
        let next = blocks.get(i + 1).copied().unwrap_or(0);
        dev.write_at(
            blocks[i] * BLOCK_SIZE,
            &encode_extent_block(owner, next, chunk),
        )?;
    }
    for b in core::mem::replace(&mut inode.chain, blocks) {
        alloc.free(b, 1)?;
    }
    inode.extents_dirty = false;
    Ok(())
}

/// Read the overflow chain of an inode just decoded from its record.
fn load_chain(
    dev: &dyn BlockDevice,
    owner: u64,
    total_blocks: u64,
    mut next: u64,
    count: u32,
    inode: &mut Inode,
) -> Result<()> {
    if inode.is_inline() {
        return Ok(());
    }
    let mut list = inode.extents.extents().to_vec();
    let mut buf = vec![0u8; BLOCK_SIZE as usize];
    while next != 0 {
        if next >= total_blocks || inode.chain.len() * EXTENTS_PER_BLOCK > count as usize {
            return Err(Error::Corrupt("extent chain"));
        }
        dev.read_at(next * BLOCK_SIZE, &mut buf)?;
        let (n, extents) = decode_extent_block(owner, &buf)?;
        list.extend(extents);
        inode.chain.push(next);
        next = n;
    }
    if list.len() != count as usize {
        return Err(Error::Corrupt("extent count"));
    }
    inode.extents = crate::extent::ExtentTree::from_extents(list)?;
    Ok(())
}

/// Shrink (freeing blocks) or grow (sparsely) a file to `new_size` bytes.
fn truncate_inode(
    dev: &dyn BlockDevice,
    alloc: &mut BlockAllocator,
    inode: &mut Inode,
    new_size: u64,
) -> Result<()> {
    if inode.is_inline() {
        inode.inline.resize(new_size as usize, 0);
        inode.size = new_size;
        inode.nbytes = new_size;
        return Ok(());
    }
    if new_size < inode.size {
        let freed = inode.extents.truncate_from(new_size.div_ceil(BLOCK_SIZE));
        if !freed.is_empty() {
            inode.extents_dirty = true;
        }
        for (start, len) in freed {
            alloc.free(start, len)?;
            inode.nbytes = inode.nbytes.saturating_sub(len * BLOCK_SIZE);
        }
        // Keep the invariant that bytes past EOF read back as zero should the
        // file grow again.
        let tail = new_size % BLOCK_SIZE;
        if tail != 0 {
            if let Some((p, _)) = inode.extents.lookup(new_size / BLOCK_SIZE) {
                zero(dev, p * BLOCK_SIZE + tail, BLOCK_SIZE - tail)?;
            }
        }
    }
    inode.size = new_size;
    Ok(())
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

fn stat_of(ino: u64, i: &Inode) -> InodeStat {
    InodeStat {
        ino,
        kind: i.kind,
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

// ---------------------------------------------------------------------------
// Mount / superblock
// ---------------------------------------------------------------------------

impl EclipseFS {
    pub fn mount(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<Self> {
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        if dev.size() < BLOCK_SIZE {
            return Err(Error::BadSuperblock);
        }
        dev.read_at(0, &mut buf)?;
        let sb = Superblock::decode(&buf)?;
        if sb.total_blocks > dev.size() / BLOCK_SIZE {
            return Err(Error::Corrupt("filesystem larger than device"));
        }
        let (n, last) = sb.groups();
        let mut groups = Vec::with_capacity(n);
        for i in 0..n {
            let count = if i + 1 == n { last } else { GROUP_BLOCKS };
            dev.read_at((i as u64 * GROUP_BLOCKS + 1) * BLOCK_SIZE, &mut buf)?;
            groups.push(AllocationGroup::from_bytes(i as u32, count, &buf)?);
        }
        let (mut itable, overflow, count) =
            Inode::decode(&sb.itable)?.ok_or(Error::Corrupt("inode table record"))?;
        load_chain(
            &*dev,
            ITABLE_INO,
            sb.total_blocks,
            overflow,
            count,
            &mut itable,
        )?;
        let mut fs = Self {
            dev,
            sb,
            itable,
            alloc: BlockAllocator::new(groups),
            inodes: BTreeMap::new(),
            dirs: BTreeMap::new(),
            read_only,
            sb_dirty: false,
            clock: None,
        };
        let free = fs.alloc.free_blocks();
        if fs.sb.free_blocks != free {
            // The superblock counter is refreshed on sync; the bitmaps are
            // written on every change and are what counts.
            debug!(
                "eclipsefs: superblock says {} free blocks, bitmaps {}",
                fs.sb.free_blocks, free
            );
            fs.sb.free_blocks = free;
        }
        let root = fs.sb.root_ino;
        if fs.inode(root)?.kind != FileKind::Dir {
            return Err(Error::Corrupt("root is not a directory"));
        }
        Ok(fs)
    }

    /// Wall-clock source for timestamps (seconds, nanoseconds).
    pub fn set_clock(&mut self, clock: fn() -> (u64, u32)) {
        self.clock = Some(clock);
    }

    pub fn label(&self) -> String {
        String::from_utf8_lossy(self.sb.label()).into_owned()
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.sb.uuid
    }

    pub fn nodesize(&self) -> usize {
        BLOCK_SIZE as usize
    }

    pub fn sectorsize(&self) -> usize {
        BLOCK_SIZE as usize
    }

    pub fn root_ino(&self) -> u64 {
        self.sb.root_ino
    }

    pub fn fsinfo(&self) -> FsStat {
        let total = self.alloc.total_blocks();
        FsStat {
            block_size: BLOCK_SIZE,
            total_bytes: total * BLOCK_SIZE,
            bytes_used: (total - self.alloc.free_blocks()) * BLOCK_SIZE,
        }
    }

    fn now(&self) -> (u64, u32) {
        self.clock.map(|c| c()).unwrap_or((0, 0))
    }

    fn writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::Unsupported("read-only filesystem"));
        }
        Ok(())
    }

    /// Write out dirty bitmaps and, if it changed, the superblock.
    fn commit(&mut self) -> Result<()> {
        for g in self.alloc.groups_mut().iter_mut().filter(|g| g.is_dirty()) {
            self.dev
                .write_at(g.bitmap_block() * BLOCK_SIZE, &g.to_bytes())?;
            g.clear_dirty();
        }
        if self.sb_dirty {
            self.sb.free_blocks = self.alloc.free_blocks();
            self.sb.generation += 1;
            self.dev.write_at(0, &self.sb.encode())?;
            self.sb_dirty = false;
        }
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        if !self.read_only {
            // Keep the free-block count in the superblock honest for tools
            // that read it without walking the bitmaps.
            if self.sb.free_blocks != self.alloc.free_blocks() {
                self.sb_dirty = true;
            }
            self.commit()?;
        }
        self.dev.sync()
    }

    /// Grow the filesystem to the size of the device: the last allocation
    /// group is filled up and new groups are added after it. Returns whether
    /// anything changed.
    pub fn grow_to_device(&mut self) -> Result<bool> {
        self.writable()?;
        let new_total = self.dev.size() / BLOCK_SIZE;
        let old_total = self.sb.total_blocks;
        if new_total <= old_total {
            return Ok(false);
        }
        let mut total = old_total;
        let first_new = self.alloc.groups().len();
        {
            let groups = self.alloc.groups_mut();
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
        self.sb_dirty = true;
        self.commit()?;
        let backup = self.sb.encode();
        for gi in first_new..self.alloc.groups().len() {
            self.dev
                .write_at(gi as u64 * GROUP_BLOCKS * BLOCK_SIZE, &backup)?;
        }
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Inode cache
    // -----------------------------------------------------------------------

    fn load(&mut self, ino: u64) -> Result<()> {
        if self.inodes.contains_key(&ino) {
            return Ok(());
        }
        if ino == ITABLE_INO || ino >= self.sb.next_ino {
            return Err(Error::NotFound);
        }
        let mut rec = [0u8; INODE_SIZE];
        let n = read_data(&*self.dev, &self.itable, ino * INODE_SIZE as u64, &mut rec)?;
        if n < INODE_SIZE {
            return Err(Error::NotFound);
        }
        let (mut inode, overflow, count) = Inode::decode(&rec)?.ok_or(Error::NotFound)?;
        load_chain(
            &*self.dev,
            ino,
            self.sb.total_blocks,
            overflow,
            count,
            &mut inode,
        )?;
        if self.inodes.len() >= INODE_CACHE {
            self.inodes.pop_first();
        }
        self.inodes.insert(ino, inode);
        Ok(())
    }

    fn inode(&mut self, ino: u64) -> Result<&Inode> {
        self.load(ino)?;
        Ok(&self.inodes[&ino])
    }

    fn take(&mut self, ino: u64) -> Result<Inode> {
        self.load(ino)?;
        Ok(self.inodes.remove(&ino).unwrap())
    }

    /// Write `inode` back (record and extent chain) and cache it. The cached
    /// copy is kept even if the write fails: it is the one that knows which
    /// blocks are allocated.
    fn put(&mut self, ino: u64, mut inode: Inode) -> Result<()> {
        let r = self.store(ino, &mut inode);
        if self.inodes.len() >= INODE_CACHE {
            self.inodes.pop_first();
        }
        self.inodes.insert(ino, inode);
        r
    }

    fn store(&mut self, ino: u64, inode: &mut Inode) -> Result<()> {
        if inode.extents_dirty {
            write_chain(&*self.dev, &mut self.alloc, ino, inode)?;
        }
        let rec = inode.encode();
        self.write_record(ino, &rec)
    }

    fn write_record(&mut self, ino: u64, rec: &[u8; INODE_SIZE]) -> Result<()> {
        self.ensure_itable(ino)?;
        let w = write_data(
            &*self.dev,
            &mut self.alloc,
            &mut self.itable,
            ino * INODE_SIZE as u64,
            rec,
        )?;
        if w < INODE_SIZE {
            return Err(Error::NoSpace);
        }
        self.itable_changed()
    }

    /// Make sure the inode table covers `ino`'s slot.
    fn ensure_itable(&mut self, ino: u64) -> Result<()> {
        let need = (ino + 1) * INODE_SIZE as u64;
        if need <= self.itable.size {
            return Ok(());
        }
        let to = need.div_ceil(ITABLE_GROW) * ITABLE_GROW;
        let r = extend_zeroed(&*self.dev, &mut self.alloc, &mut self.itable, to);
        self.itable_changed()?;
        r
    }

    fn itable_changed(&mut self) -> Result<()> {
        if self.itable.extents_dirty {
            write_chain(&*self.dev, &mut self.alloc, ITABLE_INO, &mut self.itable)?;
        }
        let rec = self.itable.encode();
        if rec != self.sb.itable {
            self.sb.itable = rec;
            self.sb_dirty = true;
        }
        Ok(())
    }

    /// Free everything an inode owns and clear its slot.
    fn purge(&mut self, ino: u64) -> Result<()> {
        let mut inode = self.take(ino)?;
        self.dirs.remove(&ino);
        if !inode.is_inline() {
            for (start, len) in inode.extents.truncate_from(0) {
                self.alloc.free(start, len)?;
            }
        }
        for b in inode.chain.drain(..) {
            self.alloc.free(b, 1)?;
        }
        self.write_record(ino, &[0u8; INODE_SIZE])?;
        self.sb.inode_count = self.sb.inode_count.saturating_sub(1);
        self.sb_dirty = true;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Directories
    // -----------------------------------------------------------------------

    fn load_dir(&mut self, dir: u64) -> Result<()> {
        if self.dirs.contains_key(&dir) {
            return Ok(());
        }
        let inode = self.inode(dir)?;
        if inode.kind != FileKind::Dir {
            return Err(Error::NotDir);
        }
        if inode.size > MAX_DIR_BYTES {
            return Err(Error::Corrupt("directory size"));
        }
        let mut data = vec![0u8; inode.size as usize];
        let inode = &self.inodes[&dir];
        read_data(&*self.dev, inode, 0, &mut data)?;
        let mut cache = DirCache::default();
        for r in parse_dirents(&data)? {
            if r.ino == 0 {
                cache.dead += r.rec_len as u64;
                continue;
            }
            let name =
                core::str::from_utf8(r.name).map_err(|_| Error::Corrupt("directory entry name"))?;
            cache.entries.insert(
                String::from(name),
                DirSlot {
                    ino: r.ino,
                    kind: r.kind.unwrap(),
                    off: r.off,
                    len: r.rec_len as u64,
                },
            );
        }
        if self.dirs.len() >= DIR_CACHE {
            self.dirs.pop_first();
        }
        self.dirs.insert(dir, cache);
        Ok(())
    }

    fn lookup_raw(&mut self, dir: u64, name: &str) -> Result<Option<DirSlot>> {
        self.load_dir(dir)?;
        Ok(self.dirs[&dir].entries.get(name).copied())
    }

    fn dir_is_empty(&mut self, dir: u64) -> Result<bool> {
        self.load_dir(dir)?;
        Ok(self.dirs[&dir].entries.is_empty())
    }

    fn add_entry(&mut self, dir: u64, name: &str, ino: u64, kind: FileKind) -> Result<()> {
        self.load_dir(dir)?;
        let rec = encode_dirent(ino, kind, name.as_bytes());
        let mut d = self.take(dir)?;
        let off = d.size;
        let r = write_data(&*self.dev, &mut self.alloc, &mut d, off, &rec);
        let r = match r {
            Ok(n) if n == rec.len() => Ok(()),
            Ok(_) => Err(Error::NoSpace),
            Err(e) => Err(e),
        };
        if r.is_err() {
            // Whatever part of the record landed is past EOF again.
            d.size = off;
        } else {
            let now = self.now();
            d.mtime = now;
            d.ctime = now;
        }
        self.put(dir, d)?;
        r?;
        if let Some(c) = self.dirs.get_mut(&dir) {
            c.entries.insert(
                String::from(name),
                DirSlot {
                    ino,
                    kind,
                    off,
                    len: rec.len() as u64,
                },
            );
        }
        Ok(())
    }

    fn remove_entry(&mut self, dir: u64, name: &str) -> Result<DirSlot> {
        self.load_dir(dir)?;
        let slot = *self.dirs[&dir].entries.get(name).ok_or(Error::NotFound)?;
        let mut d = self.take(dir)?;
        let r = write_data(
            &*self.dev,
            &mut self.alloc,
            &mut d,
            slot.off,
            &0u64.to_le_bytes(),
        );
        let now = self.now();
        d.mtime = now;
        d.ctime = now;
        self.put(dir, d)?;
        r?;
        if let Some(c) = self.dirs.get_mut(&dir) {
            c.entries.remove(name);
            c.dead += slot.len;
        }
        self.maybe_compact(dir)?;
        Ok(slot)
    }

    /// Rewrite a directory without its removed entries once they dominate it.
    fn maybe_compact(&mut self, dir: u64) -> Result<()> {
        let size = self.inode(dir)?.size;
        let Some(c) = self.dirs.get_mut(&dir) else {
            return Ok(());
        };
        if c.dead < DIR_COMPACT_MIN || c.dead * 2 < size {
            return Ok(());
        }
        let mut order: Vec<(&String, &mut DirSlot)> = c.entries.iter_mut().collect();
        order.sort_by_key(|(_, s)| s.off);
        let mut data = Vec::new();
        for (name, slot) in order {
            slot.off = data.len() as u64;
            data.extend(encode_dirent(slot.ino, slot.kind, name.as_bytes()));
        }
        c.dead = 0;
        let mut d = self.take(dir)?;
        let r = write_data(&*self.dev, &mut self.alloc, &mut d, 0, &data)
            .and_then(|_| truncate_inode(&*self.dev, &mut self.alloc, &mut d, data.len() as u64));
        self.put(dir, d)?;
        if r.is_err() {
            self.dirs.remove(&dir);
        }
        r
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    pub fn stat(&mut self, ino: u64) -> Result<InodeStat> {
        let inode = self.inode(ino)?;
        Ok(stat_of(ino, inode))
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
        let mut inode = self.take(ino)?;
        if let Some(m) = mode {
            inode.mode = (inode.mode & S_IFMT) | (m & !S_IFMT);
        }
        if let Some(u) = uid {
            inode.uid = u;
        }
        if let Some(g) = gid {
            inode.gid = g;
        }
        if let Some(t) = atime {
            inode.atime = t;
        }
        if let Some(t) = mtime {
            inode.mtime = t;
        }
        inode.ctime = self.now();
        self.put(ino, inode)?;
        self.commit()
    }

    pub fn lookup(&mut self, dir: u64, name: &str) -> Result<u64> {
        Ok(self.lookup_raw(dir, name)?.ok_or(Error::NotFound)?.ino)
    }

    /// Entries of `dir` in creation order, without `.` and `..`.
    pub fn readdir(&mut self, dir: u64) -> Result<Vec<DirEntry>> {
        self.load_dir(dir)?;
        let mut list: Vec<(u64, DirEntry)> = self.dirs[&dir]
            .entries
            .iter()
            .map(|(name, s)| {
                (
                    s.off,
                    DirEntry {
                        ino: s.ino,
                        name: name.clone(),
                        kind: s.kind,
                    },
                )
            })
            .collect();
        list.sort_by_key(|(off, _)| *off);
        Ok(list.into_iter().map(|(_, e)| e).collect())
    }

    pub fn create(
        &mut self,
        dir: u64,
        name: &str,
        kind: FileKind,
        mode: u32,
        rdev: u64,
    ) -> Result<u64> {
        check_name(name)?;
        self.writable()?;
        if self.lookup_raw(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        let ino = self.sb.next_ino;
        self.ensure_itable(ino)?;
        self.sb.next_ino += 1;
        self.sb.inode_count += 1;
        self.sb_dirty = true;
        let mut inode = Inode::new(kind, mode, self.now());
        inode.rdev = rdev;
        inode.parent = dir;
        if kind == FileKind::Dir {
            inode.nlink = 2;
        }
        let r = self
            .put(ino, inode)
            .and_then(|_| self.add_entry(dir, name, ino, kind));
        if let Err(e) = r {
            // An inode with no name can never be reached or freed.
            let _ = self.purge(ino);
            let _ = self.commit();
            return Err(e);
        }
        if kind == FileKind::Dir {
            let mut p = self.take(dir)?;
            p.nlink += 1;
            self.put(dir, p)?;
        }
        self.commit()?;
        Ok(ino)
    }

    pub fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<u64> {
        if target.is_empty() || target.len() > MAX_SYMLINK_LEN {
            return Err(Error::Invalid);
        }
        let ino = self.create(dir, name, FileKind::Symlink, 0o777, 0)?;
        let mut inode = self.take(ino)?;
        let r = self.set_link_target(&mut inode, target);
        self.put(ino, inode)?;
        r?;
        self.commit()?;
        Ok(ino)
    }

    /// Replace a symlink's target: inline when it fits in the inode record,
    /// in a data block otherwise.
    fn set_link_target(&mut self, inode: &mut Inode, target: &[u8]) -> Result<()> {
        if !inode.is_inline() {
            for (start, len) in inode.extents.truncate_from(0) {
                self.alloc.free(start, len)?;
                inode.extents_dirty = true;
            }
        }
        inode.nbytes = 0;
        inode.size = 0;
        inode.inline.clear();
        if target.len() <= INLINE_LEN {
            inode.flags |= INODE_FLAG_INLINE;
            inode.inline = target.to_vec();
            inode.size = target.len() as u64;
            inode.nbytes = inode.size;
            Ok(())
        } else {
            inode.flags &= !INODE_FLAG_INLINE;
            let n = write_data(&*self.dev, &mut self.alloc, inode, 0, target)?;
            if n < target.len() {
                return Err(Error::NoSpace);
            }
            Ok(())
        }
    }

    /// Splice `data` into a symlink's target at `offset` (the rcore-fs VFS
    /// creates symlinks empty and fills the target through `write_at`).
    pub fn write_symlink(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.writable()?;
        if self.inode(ino)?.kind != FileKind::Symlink {
            return Err(Error::Invalid);
        }
        let mut target = self.read_link(ino)?;
        let end = (offset as usize)
            .checked_add(data.len())
            .filter(|&e| e <= MAX_SYMLINK_LEN)
            .ok_or(Error::Invalid)?;
        if end > target.len() {
            target.resize(end, 0);
        }
        target[offset as usize..end].copy_from_slice(data);
        let mut inode = self.take(ino)?;
        let r = self.set_link_target(&mut inode, &target);
        let now = self.now();
        inode.mtime = now;
        inode.ctime = now;
        self.put(ino, inode)?;
        r?;
        self.commit()?;
        Ok(data.len())
    }

    pub fn read_link(&mut self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.inode(ino)?;
        if inode.kind != FileKind::Symlink {
            return Err(Error::Invalid);
        }
        if inode.size > MAX_SYMLINK_LEN as u64 {
            return Err(Error::Corrupt("symlink size"));
        }
        let mut buf = vec![0u8; inode.size as usize];
        let n = read_data(&*self.dev, &self.inodes[&ino], 0, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Parent directory of `ino` (the root is its own parent).
    pub fn parent(&mut self, ino: u64) -> Result<u64> {
        if ino == self.sb.root_ino {
            return Ok(ino);
        }
        match self.inode(ino)?.parent {
            0 => Err(Error::NotFound),
            p => Ok(p),
        }
    }

    pub fn link(&mut self, dir: u64, name: &str, ino: u64) -> Result<()> {
        check_name(name)?;
        self.writable()?;
        let kind = self.inode(ino)?.kind;
        if kind == FileKind::Dir {
            return Err(Error::IsDir);
        }
        if self.lookup_raw(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        self.add_entry(dir, name, ino, kind)?;
        let mut inode = self.take(ino)?;
        inode.nlink += 1;
        inode.ctime = self.now();
        self.put(ino, inode)?;
        self.commit()
    }

    /// Drop one link to `ino`, freeing it with the last one. `dir` is the
    /// directory that held the entry just removed.
    fn drop_link(&mut self, dir: u64, ino: u64) -> Result<()> {
        let (kind, nlink) = {
            let i = self.inode(ino)?;
            (i.kind, i.nlink)
        };
        if kind == FileKind::Dir {
            let mut p = self.take(dir)?;
            p.nlink = p.nlink.saturating_sub(1).max(2);
            self.put(dir, p)?;
            self.purge(ino)
        } else if nlink > 1 {
            let mut inode = self.take(ino)?;
            inode.nlink -= 1;
            inode.ctime = self.now();
            self.put(ino, inode)
        } else {
            self.purge(ino)
        }
    }

    pub fn unlink(&mut self, dir: u64, name: &str) -> Result<()> {
        check_name(name)?;
        self.writable()?;
        let slot = self.lookup_raw(dir, name)?.ok_or(Error::NotFound)?;
        if slot.kind == FileKind::Dir && !self.dir_is_empty(slot.ino)? {
            return Err(Error::NotEmpty);
        }
        self.remove_entry(dir, name)?;
        self.drop_link(dir, slot.ino)?;
        self.commit()
    }

    pub fn rename(
        &mut self,
        old_dir: u64,
        old_name: &str,
        new_dir: u64,
        new_name: &str,
    ) -> Result<()> {
        check_name(old_name)?;
        check_name(new_name)?;
        self.writable()?;
        let slot = self.lookup_raw(old_dir, old_name)?.ok_or(Error::NotFound)?;
        if old_dir == new_dir && old_name == new_name {
            return Ok(());
        }
        let is_dir = slot.kind == FileKind::Dir;
        if is_dir && old_dir != new_dir {
            // A directory cannot move under itself.
            let mut cur = new_dir;
            for _ in 0..4096 {
                if cur == slot.ino {
                    return Err(Error::Invalid);
                }
                if cur == self.sb.root_ino {
                    break;
                }
                cur = self.parent(cur)?;
            }
        }
        if let Some(existing) = self.lookup_raw(new_dir, new_name)? {
            if existing.ino == slot.ino {
                return Ok(());
            }
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
        // New name first: a full device must not leave the inode nameless.
        self.add_entry(new_dir, new_name, slot.ino, slot.kind)?;
        self.remove_entry(old_dir, old_name)?;
        let now = self.now();
        let mut inode = self.take(slot.ino)?;
        inode.ctime = now;
        if is_dir && old_dir != new_dir {
            inode.parent = new_dir;
            self.put(slot.ino, inode)?;
            let mut o = self.take(old_dir)?;
            o.nlink = o.nlink.saturating_sub(1).max(2);
            self.put(old_dir, o)?;
            let mut n = self.take(new_dir)?;
            n.nlink += 1;
            self.put(new_dir, n)?;
        } else {
            if !is_dir {
                inode.parent = new_dir;
            }
            self.put(slot.ino, inode)?;
        }
        self.commit()
    }

    pub fn read(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.inode(ino)?;
        if inode.kind == FileKind::Dir {
            return Err(Error::IsDir);
        }
        read_data(&*self.dev, &self.inodes[&ino], offset, buf)
    }

    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.writable()?;
        match self.inode(ino)?.kind {
            FileKind::Dir => return Err(Error::IsDir),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
        offset
            .checked_add(data.len() as u64)
            .ok_or(Error::Invalid)?;
        let mut inode = self.take(ino)?;
        let r = write_data(&*self.dev, &mut self.alloc, &mut inode, offset, data);
        if r.is_ok() {
            let now = self.now();
            inode.mtime = now;
            inode.ctime = now;
        }
        let stored = self.put(ino, inode);
        let committed = self.commit();
        let n = r?;
        stored?;
        committed?;
        Ok(n)
    }

    pub fn truncate(&mut self, ino: u64, new_size: u64) -> Result<()> {
        self.writable()?;
        match self.inode(ino)?.kind {
            FileKind::Dir => return Err(Error::IsDir),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
        if self.inodes[&ino].size == new_size {
            return Ok(());
        }
        let mut inode = self.take(ino)?;
        let r = truncate_inode(&*self.dev, &mut self.alloc, &mut inode, new_size);
        let now = self.now();
        inode.mtime = now;
        inode.ctime = now;
        self.put(ino, inode)?;
        self.commit()?;
        r
    }
}
