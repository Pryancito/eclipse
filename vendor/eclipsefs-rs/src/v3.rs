//! The v0.3 format: in-place updates with a metadata journal (kept so images
//! made with it still mount; new filesystems use the copy-on-write v0.4).
//!
//! The method set is the one the kernel's mount adapter and `xtask` call on
//! `btrfs::Btrfs`, with the same argument and error conventions: inode
//! numbers are `u64`, writes are POSIX-style partial on a full device, and
//! every mutating call is applied as a whole or not at all after a crash.
//!
//! Metadata changes collect in a [`journal::Overlay`](crate::journal) and are
//! committed through the journal in groups of operations (on `sync`, when
//! the overlay grows past half the journal, or every [`COMMIT_OPS`]
//! operations). File data goes straight to the device ahead of the metadata
//! that points at it. Images formatted before the journal existed are
//! updated in place after every operation, as before.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::blocks::{AllocationGroup, BlockAllocator};
use crate::device::BlockDevice;
use crate::extent::{Extent, MAX_EXTENT_BLOCKS};
use crate::format::*;
use crate::fs::{CheckReport, DirEntry, FsStat, InodeStat, COMMIT_OPS};
use crate::journal::{JournalArea, Overlay};
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

/// A mounted v0.3 filesystem.
pub struct EclipseFsV3 {
    /// Metadata view of the device: uncommitted blocks first.
    dev: Arc<Overlay>,
    journal: Option<JournalArea>,
    /// Operations in the open transaction.
    pending_ops: u32,
    /// Set while one public call is built out of another (`symlink` calls
    /// `create`), so the inner one does not commit half of the outer.
    nested: bool,
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

/// Write committed block images to their home locations, one device write
/// per run of consecutive blocks.
pub(crate) fn write_home(dev: &dyn BlockDevice, blocks: &[(u64, Vec<u8>)]) -> Result<()> {
    let mut i = 0;
    while i < blocks.len() {
        let mut j = i + 1;
        while j < blocks.len() && blocks[j].0 == blocks[j - 1].0 + 1 && j - i < 256 {
            j += 1;
        }
        if j == i + 1 {
            dev.write_at(blocks[i].0 * BLOCK_SIZE, &blocks[i].1)?;
        } else {
            let mut run = Vec::with_capacity((j - i) * BLOCK_SIZE as usize);
            for (_, d) in &blocks[i..j] {
                run.extend_from_slice(d);
            }
            dev.write_at(blocks[i].0 * BLOCK_SIZE, &run)?;
        }
        i = j;
    }
    Ok(())
}

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

impl EclipseFsV3 {
    pub fn mount(raw: Arc<dyn BlockDevice>, read_only: bool) -> Result<Self> {
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        if raw.size() < BLOCK_SIZE {
            return Err(Error::BadSuperblock);
        }
        raw.read_at(0, &mut buf)?;
        let home = Superblock::decode(&buf)?;
        if home.total_blocks > raw.size() / BLOCK_SIZE {
            return Err(Error::Corrupt("filesystem larger than device"));
        }
        let dev = Arc::new(Overlay::new(raw.clone()));
        let journal = home.has_journal().then_some(JournalArea {
            start: home.journal_start,
            blocks: home.journal_blocks as u64,
        });
        if let Some(j) = journal {
            if let Some((seq, blocks)) = j.read(&*raw, &home.uuid, home.total_blocks)? {
                debug!(
                    "eclipsefs: replaying journal transaction {} ({} blocks)",
                    seq,
                    blocks.len()
                );
                for (b, data) in blocks {
                    dev.insert(b, data);
                }
                if !read_only {
                    // Write it home now; a read-only mount keeps it in memory.
                    write_home(&*raw, &dev.take())?;
                    raw.sync()?;
                }
            }
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
        let mut alloc = BlockAllocator::new(groups);
        alloc.set_deferred_frees(journal.is_some());
        let mut fs = Self {
            dev,
            journal,
            pending_ops: 0,
            nested: false,
            sb,
            itable,
            alloc,
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

    /// Bumped on every superblock write.
    pub fn generation(&self) -> u64 {
        self.sb.generation
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
            bytes_used: (total - self.alloc.free_blocks() - self.alloc.deferred_blocks())
                * BLOCK_SIZE,
        }
    }

    fn now(&self) -> (u64, u32) {
        self.clock.map(|c| c()).unwrap_or((0, 0))
    }

    /// Start of every mutating call.
    fn writable(&mut self) -> Result<()> {
        if self.read_only {
            return Err(Error::Unsupported("read-only filesystem"));
        }
        // Space freed by the open transaction is not reusable until it
        // commits; commit early when that space is what is missing.
        let deferred = self.alloc.deferred_blocks();
        if deferred > 0
            && !self.nested
            && self.alloc.free_blocks() < deferred.max(self.alloc.total_blocks() / 16)
        {
            self.flush()?;
        }
        Ok(())
    }

    /// Queue the dirty bitmaps and, if it changed, the superblock.
    fn write_meta(&mut self) -> Result<()> {
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

    /// Commit the open transaction: journal it, then write it home.
    fn flush(&mut self) -> Result<()> {
        if self.alloc.deferred_blocks() > 0 {
            self.alloc.release_deferred()?;
        }
        // Each transaction carries an exact free-block count.
        if self.sb.free_blocks != self.alloc.free_blocks() {
            self.sb_dirty = true;
        }
        self.write_meta()?;
        self.pending_ops = 0;
        if self.dev.is_empty() {
            return Ok(());
        }
        let blocks = self.dev.take();
        let raw = self.dev.inner().clone();
        let r = match self.journal {
            Some(j) if j.fits(blocks.len()) => j
                .write(&*raw, &self.sb.uuid, self.sb.generation, &blocks)
                .and_then(|_| raw.sync())
                .and_then(|_| write_home(&*raw, &blocks))
                .and_then(|_| raw.sync()),
            Some(j) => {
                warn!(
                    "eclipsefs: transaction of {} blocks exceeds the journal, writing in place",
                    blocks.len()
                );
                j.invalidate(&*raw)
                    .and_then(|_| raw.sync())
                    .and_then(|_| write_home(&*raw, &blocks))
                    .and_then(|_| raw.sync())
            }
            None => write_home(&*raw, &blocks),
        };
        if r.is_err() {
            // Keep the changes visible; the next commit retries them.
            for (b, data) in blocks {
                if !self.dev.contains(b) {
                    self.dev.insert(b, data);
                }
            }
        }
        r
    }

    /// End of every mutating call: queue its metadata and commit the
    /// transaction if it is due.
    fn commit(&mut self) -> Result<()> {
        self.write_meta()?;
        if self.nested {
            return Ok(());
        }
        self.pending_ops += 1;
        let due = match self.journal {
            None => true,
            Some(j) => self.pending_ops >= COMMIT_OPS || self.dev.len() * 2 >= j.capacity(),
        };
        if due {
            self.flush()?;
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
            self.flush()?;
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
        let backup = self.sb.encode();
        for gi in first_new..self.alloc.groups().len() {
            self.dev
                .write_at(gi as u64 * GROUP_BLOCKS * BLOCK_SIZE, &backup)?;
        }
        self.commit()?;
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
        // One transaction for the inode and its target.
        self.nested = true;
        let ino = self.create(dir, name, FileKind::Symlink, 0o777, 0);
        self.nested = false;
        let ino = match ino {
            Ok(ino) => ino,
            Err(e) => {
                self.commit()?;
                return Err(e);
            }
        };
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
        // File data bypasses the overlay (ordered mode).
        let raw = self.dev.inner().clone();
        let r = write_data(&*raw, &mut self.alloc, &mut inode, offset, data);
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
        let raw = self.dev.inner().clone();
        let r = truncate_inode(&*raw, &mut self.alloc, &mut inode, new_size);
        let now = self.now();
        inode.mtime = now;
        inode.ctime = now;
        self.put(ino, inode)?;
        self.commit()?;
        r
    }
}

impl EclipseFsV3 {
    /// Full consistency check (fsck without repair). Commits the open
    /// transaction first on a writable mount. Verifies that every block in
    /// use belongs to exactly one owner (group headers, journal, inode table,
    /// an inode's extents or extent chain) and that the bitmaps agree, that
    /// every live inode is reachable with the right link count and parent,
    /// and the superblock counters. The first problem is returned as
    /// `Error::Corrupt`.
    pub fn check(&mut self) -> Result<CheckReport> {
        if !self.read_only {
            self.flush()?;
        }
        let total = self.sb.total_blocks;
        let mut owned = vec![0u64; total.div_ceil(64) as usize];
        let mut count = 0u64;
        let mut mark = |start: u64, len: u64| -> Result<()> {
            for b in start
                ..start
                    .checked_add(len)
                    .ok_or(Error::Corrupt("fsck: extent"))?
            {
                if b >= total {
                    return Err(Error::Corrupt("fsck: block past the end"));
                }
                let (w, bit) = ((b / 64) as usize, b % 64);
                if owned[w] & (1 << bit) != 0 {
                    return Err(Error::Corrupt("fsck: block used twice"));
                }
                owned[w] |= 1 << bit;
                count += 1;
            }
            Ok(())
        };
        for g in self.alloc.groups() {
            mark(g.start_block, 2)?;
        }
        if let Some(j) = self.journal {
            mark(j.start, j.blocks)?;
        }
        fn mark_inode(mark: &mut impl FnMut(u64, u64) -> Result<()>, inode: &Inode) -> Result<()> {
            if inode.is_inline() {
                if inode.inline.len() as u64 != inode.size || inode.nbytes != inode.size {
                    return Err(Error::Corrupt("fsck: inline size"));
                }
                return Ok(());
            }
            let mut blocks = 0;
            for e in inode.extents.extents() {
                mark(e.physical_block, e.length as u64)?;
                blocks += e.length as u64;
            }
            for &b in &inode.chain {
                mark(b, 1)?;
            }
            if blocks * BLOCK_SIZE != inode.nbytes {
                return Err(Error::Corrupt("fsck: nbytes"));
            }
            Ok(())
        }
        mark_inode(&mut mark, &self.itable)?;

        // Live inodes, with what each one needs to see from directories.
        let mut live: BTreeMap<u64, (FileKind, u32, u64)> = BTreeMap::new();
        for ino in ROOT_INO..self.sb.next_ino {
            match self.inode(ino) {
                Ok(i) => {
                    let i = i.clone();
                    mark_inode(&mut mark, &i)?;
                    live.insert(ino, (i.kind, i.nlink, i.parent));
                }
                Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        if live.len() as u64 != self.sb.inode_count {
            return Err(Error::Corrupt("fsck: inode count"));
        }
        let root = self.sb.root_ino;
        if live.get(&root).map(|l| l.0) != Some(FileKind::Dir) {
            return Err(Error::Corrupt("fsck: root"));
        }
        let mut names: BTreeMap<u64, u32> = BTreeMap::new();
        let mut subdirs: BTreeMap<u64, u32> = BTreeMap::new();
        let dirs: Vec<u64> = live
            .iter()
            .filter(|(_, l)| l.0 == FileKind::Dir)
            .map(|(&i, _)| i)
            .collect();
        for &d in &dirs {
            for e in self.readdir(d)? {
                let Some(&(kind, _, parent)) = live.get(&e.ino) else {
                    return Err(Error::Corrupt("fsck: entry for a free inode"));
                };
                if kind != e.kind {
                    return Err(Error::Corrupt("fsck: entry kind"));
                }
                *names.entry(e.ino).or_default() += 1;
                if kind == FileKind::Dir {
                    if parent != d {
                        return Err(Error::Corrupt("fsck: directory parent"));
                    }
                    *subdirs.entry(d).or_default() += 1;
                }
            }
        }
        for (&ino, &(kind, nlink, _)) in &live {
            let refs = names.get(&ino).copied().unwrap_or(0);
            let ok = if kind == FileKind::Dir {
                refs == u32::from(ino != root)
                    && nlink == 2 + subdirs.get(&ino).copied().unwrap_or(0)
            } else {
                refs > 0 && nlink == refs
            };
            if !ok {
                return Err(Error::Corrupt("fsck: link count"));
            }
        }
        // Every directory reaches the root (no detached cycles).
        for &d in &dirs {
            let mut cur = d;
            let mut steps = 0;
            while cur != root {
                cur = live[&cur].2;
                steps += 1;
                if steps > dirs.len() {
                    return Err(Error::Corrupt("fsck: directory cycle"));
                }
            }
        }

        for b in 0..total {
            let mine = owned[(b / 64) as usize] & (1 << (b % 64)) != 0;
            if mine != self.alloc.is_used(b) {
                return Err(Error::Corrupt(if mine {
                    "fsck: block in use marked free"
                } else {
                    "fsck: block marked used but unowned"
                }));
            }
        }
        if !self.read_only && self.sb.free_blocks != self.alloc.free_blocks() {
            return Err(Error::Corrupt("fsck: free block count"));
        }
        Ok(CheckReport {
            inodes: live.len() as u64,
            directories: dirs.len() as u64,
            blocks_used: count,
        })
    }
}

impl Drop for EclipseFsV3 {
    /// Commit what is still open, like an unmount would.
    fn drop(&mut self) {
        if !self.read_only {
            if let Err(e) = self.flush().and_then(|_| self.dev.inner().sync()) {
                warn!("eclipsefs: commit on unmount failed: {:?}", e);
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use core::cell::RefCell;

    /// Device whose fresh contents are 0xA5, so missed zero-filling shows up.
    struct MemDev(RefCell<Vec<u8>>);

    // Tests run each device on one thread.
    unsafe impl Sync for MemDev {}
    unsafe impl Send for MemDev {}

    impl MemDev {
        fn new(blocks: u64) -> Self {
            Self(RefCell::new(vec![0xA5; (blocks * BLOCK_SIZE) as usize]))
        }
        fn writes_to(&self, block: u64) -> Vec<u8> {
            let d = self.0.borrow();
            let at = (block * BLOCK_SIZE) as usize;
            d[at..at + BLOCK_SIZE as usize].to_vec()
        }
    }

    impl BlockDevice for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let d = self.0.borrow();
            let o = offset as usize;
            let s = d.get(o..o + buf.len()).ok_or(Error::Io)?;
            buf.copy_from_slice(s);
            Ok(())
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            let mut d = self.0.borrow_mut();
            let o = offset as usize;
            d.get_mut(o..o + buf.len())
                .ok_or(Error::Io)?
                .copy_from_slice(buf);
            Ok(())
        }
        fn sync(&self) -> Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            self.0.borrow().len() as u64
        }
    }

    fn setup(blocks: u64) -> (MemDev, BlockAllocator, Inode) {
        let dev = MemDev::new(blocks);
        let alloc = BlockAllocator::new(vec![AllocationGroup::new(0, blocks)]);
        (dev, alloc, Inode::new(FileKind::Regular, 0o644, (0, 0)))
    }

    fn read_vec(dev: &MemDev, inode: &Inode, off: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0xCC; len];
        let n = read_data(dev, inode, off, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }

    #[test]
    fn zero_spans_several_buffers() {
        let dev = MemDev::new(80);
        let len = ZEROS.len() as u64 * 2 + 10;
        zero(&dev, 3, len).unwrap();
        let d = dev.0.borrow();
        assert_eq!(d[2], 0xA5);
        assert!(d[3..3 + len as usize].iter().all(|&b| b == 0));
        assert_eq!(d[3 + len as usize], 0xA5);
    }

    #[test]
    fn unaligned_write_zero_fills_fresh_blocks() {
        let (dev, mut alloc, mut inode) = setup(64);
        assert_eq!(
            write_data(&dev, &mut alloc, &mut inode, 100, b"hi").unwrap(),
            2
        );
        assert_eq!(inode.size, 102);
        assert_eq!(inode.nbytes, BLOCK_SIZE);
        let (phys, _) = inode.extents.lookup(0).unwrap();
        let blk = dev.writes_to(phys);
        assert!(blk[..100].iter().all(|&b| b == 0));
        assert_eq!(&blk[100..102], b"hi");
        assert!(blk[102..].iter().all(|&b| b == 0));
    }

    #[test]
    fn sequential_writes_extend_one_extent() {
        let (dev, mut alloc, mut inode) = setup(256);
        for i in 0..10u64 {
            let data = vec![i as u8; BLOCK_SIZE as usize];
            write_data(&dev, &mut alloc, &mut inode, i * BLOCK_SIZE, &data).unwrap();
        }
        assert_eq!(inode.extents.extents().len(), 1);
        assert_eq!(inode.size, 10 * BLOCK_SIZE);
        let back = read_vec(&dev, &inode, 0, 10 * BLOCK_SIZE as usize);
        for i in 0..10 {
            let s = (i * BLOCK_SIZE) as usize;
            assert!(back[s..s + BLOCK_SIZE as usize]
                .iter()
                .all(|&b| b == i as u8));
        }
    }

    #[test]
    fn holes_read_as_zero_and_are_filled_in_place() {
        let (dev, mut alloc, mut inode) = setup(64);
        write_data(&dev, &mut alloc, &mut inode, 0, &[1; 10]).unwrap();
        write_data(&dev, &mut alloc, &mut inode, 3 * BLOCK_SIZE, &[3; 10]).unwrap();
        assert_eq!(inode.nbytes, 2 * BLOCK_SIZE);
        let mid = read_vec(&dev, &inode, BLOCK_SIZE - 5, 20);
        assert_eq!(&mid[..5], &[0; 5]);
        assert_eq!(&mid[5..], &[0; 15]);
        // A write spanning the hole and the mapped block after it fills the
        // hole only up to the next extent.
        let data = vec![7u8; (2 * BLOCK_SIZE + 4) as usize];
        write_data(&dev, &mut alloc, &mut inode, BLOCK_SIZE, &data).unwrap();
        assert_eq!(inode.nbytes, 4 * BLOCK_SIZE);
        let all = read_vec(&dev, &inode, 0, inode.size as usize);
        assert_eq!(&all[..10], &[1; 10]);
        assert!(all[10..BLOCK_SIZE as usize].iter().all(|&b| b == 0));
        assert!(all[BLOCK_SIZE as usize..(3 * BLOCK_SIZE + 4) as usize]
            .iter()
            .all(|&b| b == 7));
        assert_eq!(&all[(3 * BLOCK_SIZE + 4) as usize..], &[3; 6]);
    }

    #[test]
    fn reads_clamp_to_size() {
        let (dev, mut alloc, mut inode) = setup(64);
        write_data(&dev, &mut alloc, &mut inode, 0, b"abcdef").unwrap();
        assert_eq!(read_vec(&dev, &inode, 4, 100), b"ef");
        assert_eq!(read_vec(&dev, &inode, 6, 100), b"");
        assert_eq!(read_vec(&dev, &inode, u64::MAX, 100), b"");
    }

    #[test]
    fn inline_reads_and_truncates() {
        let dev = MemDev::new(8);
        let mut inode = Inode::new(FileKind::Symlink, 0o777, (0, 0));
        inode.flags |= INODE_FLAG_INLINE;
        inode.inline = b"target".to_vec();
        inode.size = 6;
        assert_eq!(read_vec(&dev, &inode, 2, 10), b"rget");
        let mut alloc = BlockAllocator::new(vec![AllocationGroup::new(0, 8)]);
        truncate_inode(&dev, &mut alloc, &mut inode, 3).unwrap();
        assert_eq!(read_vec(&dev, &inode, 0, 10), b"tar");
        truncate_inode(&dev, &mut alloc, &mut inode, 5).unwrap();
        assert_eq!(read_vec(&dev, &inode, 0, 10), b"tar\0\0");
        assert_eq!(inode.nbytes, 5);
    }

    #[test]
    fn full_device_gives_short_write_then_no_space() {
        // 64 blocks, two taken by the superblock and the bitmap.
        let (dev, mut alloc, mut inode) = setup(64);
        let data = vec![9u8; (100 * BLOCK_SIZE) as usize];
        let n = write_data(&dev, &mut alloc, &mut inode, 0, &data).unwrap();
        assert_eq!(n as u64, 62 * BLOCK_SIZE);
        assert_eq!(alloc.free_blocks(), 0);
        assert!(matches!(
            write_data(&dev, &mut alloc, &mut inode, n as u64, &data),
            Err(Error::NoSpace)
        ));
        // Overwriting mapped blocks needs no space.
        assert_eq!(
            write_data(&dev, &mut alloc, &mut inode, 0, b"x").unwrap(),
            1
        );
    }

    #[test]
    fn write_overflowing_offsets_is_invalid() {
        let (dev, mut alloc, mut inode) = setup(64);
        assert!(matches!(
            write_data(&dev, &mut alloc, &mut inode, u64::MAX, b"ab"),
            Err(Error::Invalid)
        ));
        assert_eq!(
            write_data(&dev, &mut alloc, &mut inode, u64::MAX, b"").unwrap(),
            0
        );
    }

    #[test]
    fn extend_zeroed_allocates_zeros() {
        let (dev, mut alloc, mut inode) = setup(128);
        extend_zeroed(&dev, &mut alloc, &mut inode, 5 * BLOCK_SIZE + 1).unwrap();
        assert_eq!(inode.size, 5 * BLOCK_SIZE + 1);
        assert_eq!(inode.nbytes, 6 * BLOCK_SIZE);
        let (phys, run) = inode.extents.lookup(0).unwrap();
        assert_eq!(run, 6);
        for b in phys..phys + 6 {
            assert!(dev.writes_to(b).iter().all(|&x| x == 0));
        }
        let (dev, mut alloc, mut inode) = setup(64);
        assert!(matches!(
            extend_zeroed(&dev, &mut alloc, &mut inode, 100 * BLOCK_SIZE),
            Err(Error::NoSpace)
        ));
    }

    #[test]
    fn truncate_frees_whole_blocks_and_zeroes_the_tail() {
        let (dev, mut alloc, mut inode) = setup(64);
        let free0 = alloc.free_blocks();
        write_data(&dev, &mut alloc, &mut inode, 0, &[0xFF; 3 * 4096]).unwrap();
        assert_eq!(alloc.free_blocks(), free0 - 3);
        truncate_inode(&dev, &mut alloc, &mut inode, 4097).unwrap();
        assert_eq!(alloc.free_blocks(), free0 - 2);
        assert_eq!(inode.nbytes, 2 * BLOCK_SIZE);
        assert!(inode.extents_dirty);
        let (p, _) = inode.extents.lookup(1).unwrap();
        let blk = dev.writes_to(p);
        assert_eq!(blk[0], 0xFF);
        assert!(blk[1..].iter().all(|&b| b == 0));
        // Growing is sparse.
        truncate_inode(&dev, &mut alloc, &mut inode, 1 << 20).unwrap();
        assert_eq!(alloc.free_blocks(), free0 - 2);
        assert_eq!(inode.size, 1 << 20);
        truncate_inode(&dev, &mut alloc, &mut inode, 0).unwrap();
        assert_eq!(alloc.free_blocks(), free0);
        assert_eq!(inode.nbytes, 0);
    }

    fn fragmented(n: u64) -> (MemDev, BlockAllocator, Inode) {
        let (dev, mut alloc, mut inode) = setup(4 * n + 64);
        for i in 0..n {
            write_data(&dev, &mut alloc, &mut inode, 2 * i * BLOCK_SIZE, &[i as u8]).unwrap();
        }
        assert_eq!(inode.extents.extents().len() as u64, n);
        (dev, alloc, inode)
    }

    #[test]
    fn chain_is_empty_while_extents_fit_inline() {
        let (dev, mut alloc, mut inode) = fragmented(INLINE_EXTENTS as u64);
        write_chain(&dev, &mut alloc, 7, &mut inode).unwrap();
        assert!(inode.chain.is_empty());
        assert!(!inode.extents_dirty);
    }

    #[test]
    fn chain_round_trips_and_replaces_old_blocks() {
        let n = (INLINE_EXTENTS + 2 * EXTENTS_PER_BLOCK + 1) as u64;
        let (dev, mut alloc, mut inode) = fragmented(n);
        let free = alloc.free_blocks();
        write_chain(&dev, &mut alloc, 7, &mut inode).unwrap();
        assert_eq!(inode.chain.len(), 3);
        assert_eq!(alloc.free_blocks(), free - 3);

        let rec = inode.encode();
        let (mut back, next, count) = Inode::decode(&rec).unwrap().unwrap();
        assert_eq!(next, inode.chain[0]);
        assert_eq!(count as u64, n);
        load_chain(&dev, 7, alloc.total_blocks(), next, count, &mut back).unwrap();
        assert_eq!(back.extents.extents(), inode.extents.extents());
        assert_eq!(back.chain, inode.chain);

        // Wrong owner is rejected.
        let (mut other, _, _) = Inode::decode(&rec).unwrap().unwrap();
        assert!(load_chain(&dev, 8, alloc.total_blocks(), next, count, &mut other).is_err());
        // Out-of-range pointer is rejected.
        let (mut other, _, _) = Inode::decode(&rec).unwrap().unwrap();
        assert!(load_chain(&dev, 7, next, next, count, &mut other).is_err());
        // Count mismatch is rejected.
        let (mut other, _, _) = Inode::decode(&rec).unwrap().unwrap();
        assert!(load_chain(&dev, 7, alloc.total_blocks(), next, count - 1, &mut other).is_err());

        // Shrinking the file shrinks the chain and frees the old blocks.
        truncate_inode(&dev, &mut alloc, &mut inode, 2 * BLOCK_SIZE * 10).unwrap();
        write_chain(&dev, &mut alloc, 7, &mut inode).unwrap();
        assert_eq!(inode.chain.len(), 1);
    }

    #[test]
    fn chain_allocation_failure_keeps_the_old_chain() {
        let n = (INLINE_EXTENTS + EXTENTS_PER_BLOCK) as u64;
        let (dev, mut alloc, mut inode) = fragmented(n);
        write_chain(&dev, &mut alloc, 7, &mut inode).unwrap();
        let old = inode.chain.clone();
        // Fill the device and add one more extent so a second block is needed.
        while alloc.allocate(None, MAX_EXTENT_BLOCKS).is_some() {}
        inode
            .extents
            .add_extent(Extent::new(1 << 20, old[0] + 2, 1))
            .unwrap();
        let free = alloc.free_blocks();
        assert!(matches!(
            write_chain(&dev, &mut alloc, 7, &mut inode),
            Err(Error::NoSpace)
        ));
        assert_eq!(inode.chain, old);
        assert_eq!(alloc.free_blocks(), free);
    }

    #[test]
    fn names() {
        assert!(check_name("a").is_ok());
        assert!(check_name(&"n".repeat(MAX_NAME_LEN)).is_ok());
        assert!(check_name(&"n".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(check_name("ñandú").is_ok());
        for bad in ["", ".", "..", "a/b", "/", "a\0b"] {
            assert!(check_name(bad).is_err(), "{:?}", bad);
        }
        assert!(check_name("...").is_ok());
        assert!(check_name(".hidden").is_ok());
    }
}
