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
use crate::btree::{Body, Key, Root, Store, MAX_ITEM};
use crate::cowfmt::*;
use crate::crc::crc32c;
use crate::device::BlockDevice;
use crate::format::{
    FileKind, BLOCK_SIZE, GROUP_BLOCKS, MAX_NAME_LEN, MAX_SYMLINK_LEN, MIN_GROUP_BLOCKS,
};
use crate::fs::{CheckReport, DirEntry, FsStat, InodeStat, ScrubReport, SubvolInfo, COMMIT_OPS};
use crate::journal::{JournalArea, Overlay};
use crate::space::{metadata_reserve, BlockSpace, RangeSet, RefMap};
use crate::v3::write_home;
use crate::{Error, Result};

const BS: u64 = BLOCK_SIZE;
/// Blocks handled per step of a large write (bounds the buffer).
const WRITE_CHUNK: u64 = 2048;
/// Commit early once this many tree nodes wait to be written.
const DIRTY_NODES_LIMIT: usize = 4096;

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
}

impl Ext {
    fn item(pblk: u64, blocks: u64, prealloc: bool) -> Vec<u8> {
        if prealloc {
            FileExtent::Prealloc { disk: pblk, blocks }.encode()
        } else {
            FileExtent::Reg { disk: pblk, blocks }.encode()
        }
    }
}

/// A mounted v0.4 filesystem.
pub struct CowFs {
    /// Superblock and bitmaps, through the journal.
    dev: Arc<Overlay>,
    raw: Arc<dyn BlockDevice>,
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

/// Flags a new inode takes from its directory (as on btrfs).
const FL_INHERITED: u32 = FL_NOCOW | FL_COMPR | FL_NOCOMP;

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

fn read_groups(dev: &dyn BlockDevice, total: u64) -> Result<Vec<AllocationGroup>> {
    let n = total.div_ceil(GROUP_BLOCKS);
    let last = total - (n - 1) * GROUP_BLOCKS;
    let mut buf = vec![0u8; BS as usize];
    let mut groups = Vec::with_capacity(n as usize);
    for i in 0..n {
        let count = if i + 1 == n { last } else { GROUP_BLOCKS };
        dev.read_at((i * GROUP_BLOCKS + 1) * BS, &mut buf)?;
        groups.push(AllocationGroup::from_bytes(i as u32, count, &buf)?);
    }
    Ok(groups)
}

impl CowFs {
    // -----------------------------------------------------------------------
    // Mount and transactions
    // -----------------------------------------------------------------------

    pub fn mount(raw: Arc<dyn BlockDevice>, read_only: bool) -> Result<Self> {
        let mut buf = vec![0u8; BS as usize];
        raw.read_at(0, &mut buf)?;
        let home = SuperV4::decode(&buf)?;
        if home.total_blocks > raw.size() / BS {
            return Err(Error::Corrupt("filesystem larger than device"));
        }
        let journal = JournalArea {
            start: home.journal_start,
            blocks: home.journal_blocks as u64,
        };
        let dev = Arc::new(Overlay::new(raw.clone()));
        if let Some((seq, blocks)) = journal.read(&*raw, &home.uuid, home.total_blocks)? {
            debug!(
                "eclipsefs: replaying transaction {} ({} blocks)",
                seq,
                blocks.len()
            );
            for (b, data) in blocks {
                dev.insert(b, data);
            }
            if !read_only {
                write_home(&*raw, &dev.take())?;
                raw.sync()?;
            }
        }
        dev.read_at(0, &mut buf)?;
        let sb = SuperV4::decode(&buf)?;
        if sb.total_blocks > raw.size() / BS {
            return Err(Error::Corrupt("filesystem larger than device"));
        }
        let mut alloc = BlockAllocator::new(read_groups(&*dev, sb.total_blocks)?);
        alloc.set_deferred_frees(true);
        let mut st = Store::new(
            raw.clone(),
            sb.uuid,
            sb.generation + 1,
            sb.total_blocks,
            extent_of,
        );
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
        let free = alloc.free_blocks();
        let mut fs = Self {
            dev,
            raw,
            journal,
            sp: BlockSpace::new(alloc, refs.clone()),
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
            sb,
        };
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
        Ok(fs)
    }

    pub fn set_clock(&mut self, clock: fn() -> (u64, u32)) {
        self.clock = Some(clock);
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
        let total = self.sp.alloc.total_blocks();
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
        if self.pending_ops >= COMMIT_OPS || self.st.dirty_count() >= DIRTY_NODES_LIMIT {
            self.flush()?;
        }
        Ok(())
    }

    /// Commit the open transaction.
    fn flush(&mut self) -> Result<()> {
        self.pending_ops = 0;
        let changed = self.st.dirty_count() > 0
            || self.sp.refs_dirty
            || self.sp.alloc.deferred_blocks() > 0
            || self.sp.alloc.groups().iter().any(|g| g.is_dirty())
            || !self.dev.is_empty();
        if !changed {
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
        // Superblock and bitmaps, atomically through the journal.
        self.sp.alloc.release_deferred()?;
        self.sp.committed();
        self.sb.root = self.root_tree;
        self.sb.generation = gen;
        self.sb.free_blocks = self.sp.alloc.free_blocks();
        for g in self
            .sp
            .alloc
            .groups_mut()
            .iter_mut()
            .filter(|g| g.is_dirty())
        {
            self.dev.write_at(g.bitmap_block() * BS, &g.to_bytes())?;
            g.clear_dirty();
        }
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
        }
        r
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
    /// per bucket of blocks.
    fn store_csums(&mut self) -> Result<()> {
        if self.sp.csums.is_empty() {
            return Ok(());
        }
        let pending: Vec<(u64, Option<u32>)> =
            self.sp.csums.iter().map(|(&b, &c)| (b, c)).collect();
        let mut i = 0;
        while i < pending.len() {
            let bucket = pending[i].0 / CSUM_BUCKET * CSUM_BUCKET;
            let key = Key::new(bucket, CSUM_ITEM, 0);
            let old = self.st.get(&self.csum_root, &key)?;
            let mut cb = match &old {
                Some(d) => CsumBucket::decode(d)?,
                None => CsumBucket::default(),
            };
            while i < pending.len() && pending[i].0 < bucket + CSUM_BUCKET {
                cb.0[(pending[i].0 - bucket) as usize] = pending[i].1;
                i += 1;
            }
            if cb.is_empty() {
                if old.is_some() {
                    self.st.delete(&mut self.sp, &mut self.csum_root, key)?;
                }
            } else {
                self.st
                    .upsert(&mut self.sp, &mut self.csum_root, key, cb.encode())?;
            }
        }
        self.sp.csums.clear();
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

    pub fn grow_to_device(&mut self) -> Result<bool> {
        self.writable()?;
        let new_total = self.raw.size() / BS;
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
        self.flush()?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Items
    // -----------------------------------------------------------------------

    fn get(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        self.st.get(&self.fs_root, &key)
    }

    fn insert(&mut self, key: Key, v: Vec<u8>) -> Result<()> {
        self.st.insert(&mut self.sp, &mut self.fs_root, key, v)
    }

    fn upsert(&mut self, key: Key, v: Vec<u8>) -> Result<()> {
        self.st.upsert(&mut self.sp, &mut self.fs_root, key, v)
    }

    fn delete(&mut self, key: Key) -> Result<Vec<u8>> {
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
        i.seq += 1;
        self.st.update(
            &mut self.sp,
            &mut self.fs_root,
            Key::new(ino, INODE_ITEM, 0),
            i.encode(),
        )
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
                FileExtent::Reg { .. } | FileExtent::Prealloc { .. } => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Regular extents overlapping file blocks `from..to`.
    fn extents(&mut self, ino: u64, from: u64, to: u64) -> Result<Vec<Ext>> {
        let mut out = Vec::new();
        if from >= to {
            return Ok(out);
        }
        let first = self.st.seek_le(&self.fs_root, &Self::ext_key(ino, from))?;
        let mut items = Vec::new();
        if let Some((k, d)) = first {
            if k.objectid == ino && k.ty == EXTENT_DATA {
                items.push((k, d));
            }
        }
        let lo = Key::new(ino, EXTENT_DATA, from * BS + 1);
        let hi = Key::new(ino, EXTENT_DATA, to * BS - 1);
        items.extend(self.st.range(&self.fs_root, &lo, &hi, usize::MAX)?);
        for (k, d) in items {
            let (disk, blocks, prealloc) = match FileExtent::decode(&d)? {
                FileExtent::Reg { disk, blocks } => (disk, blocks, false),
                FileExtent::Prealloc { disk, blocks } => (disk, blocks, true),
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
        // Extend the previous extent when the new blocks continue it.
        if lblk > 0 {
            if let Some(prev) = self.extents(ino, lblk - 1, lblk)?.first().copied() {
                if prev.lblk + prev.blocks == lblk
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
    fn unmap(&mut self, ino: u64, inode: &mut InodeItem, from: u64, to: u64) -> Result<()> {
        for e in self.extents(ino, from, to)? {
            let key = Self::ext_key(ino, e.lblk);
            let end = e.lblk + e.blocks;
            if e.lblk < from {
                self.upsert(key, Ext::item(e.pblk, from - e.lblk, e.prealloc))?;
            } else {
                self.delete(key)?;
            }
            if end > to {
                self.insert(
                    Self::ext_key(ino, to),
                    Ext::item(e.pblk + (to - e.lblk), end - to, e.prealloc),
                )?;
            }
            let s = e.lblk.max(from);
            let t = end.min(to);
            self.sp_decref(e.pblk + (s - e.lblk), t - s)?;
            inode.nbytes = inode.nbytes.saturating_sub((t - s) * BS);
        }
        Ok(())
    }

    fn sp_decref(&mut self, start: u64, len: u64) -> Result<()> {
        crate::btree::Space::decref_data(&mut self.sp, start, len)
    }

    /// Read file bytes `off..off+buf.len()` (no size check; holes are 0),
    /// verifying the checksum of every block read.
    fn read_blocks(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> Result<()> {
        buf.fill(0);
        if buf.is_empty() {
            return Ok(());
        }
        let end = off + buf.len() as u64;
        // No-COW files have no checksums (as on btrfs).
        let sums = self.inode(ino)?.flags & FL_NOCOW as u64 == 0;
        for e in self.extents(ino, off / BS, end.div_ceil(BS))? {
            if e.prealloc {
                continue;
            }
            let s = (e.lblk * BS).max(off);
            let t = ((e.lblk + e.blocks) * BS).min(end);
            let mut pos = s;
            while pos < t {
                // Whole blocks, a bounded number at a time.
                let b0 = pos / BS;
                let b1 = t.div_ceil(BS).min(b0 + WRITE_CHUNK);
                let upto = (b1 * BS).min(t);
                let p0 = e.pblk + (b0 - e.lblk);
                let dst = &mut buf[(pos - off) as usize..(upto - off) as usize];
                if pos.is_multiple_of(BS) && upto.is_multiple_of(BS) {
                    self.raw.read_at(p0 * BS, dst)?;
                    if sums {
                        self.verify(p0, dst)?;
                    }
                } else {
                    let mut tmp = vec![0u8; ((b1 - b0) * BS) as usize];
                    self.raw.read_at(p0 * BS, &mut tmp)?;
                    if sums {
                        self.verify(p0, &tmp)?;
                    }
                    let o = (pos - b0 * BS) as usize;
                    let len = dst.len();
                    dst.copy_from_slice(&tmp[o..o + len]);
                }
                pos = upto;
            }
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
        let end = off + data.len() as u64;
        let mut lb = off / BS;
        let last = end.div_ceil(BS);
        let mut written_to = off;
        // No-COW files rewrite their own unshared blocks in place and keep
        // no checksums (btrfs's nodatacow).
        let nocow = inode.flags & FL_NOCOW as u64 != 0;
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
                    let goal = if lb > 0 {
                        self.extents(ino, lb - 1, lb)?
                            .first()
                            .map(|e| e.pblk + (lb - e.lblk))
                    } else {
                        None
                    };
                    match self.sp.alloc_data(goal, n) {
                        Some(r) => r,
                        None => break,
                    }
                }
            };
            // Contents of the blocks: data where the write covers them, the
            // old contents (or zeros) at the edges. If the old contents
            // cannot be read, the new blocks go back.
            let buf = match self.fill_blocks(ino, lb, got, off, data) {
                Ok(b) => b,
                Err(e) => {
                    if in_place.is_none() {
                        self.sp_decref(pblk, got)?;
                    }
                    return Err(e);
                }
            };
            let t = end.min((lb + got) * BS);
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
        let small = end <= INLINE_MAX as u64 && inode.size <= INLINE_MAX as u64;
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
        let mut d = self.inode(dir)?;
        if d.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        let index = d.dir_next;
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
        d.dir_next += 1;
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
    fn purge(&mut self, ino: u64) -> Result<()> {
        let items = self.st.range(
            &self.fs_root,
            &Key::new(ino, 0, 0),
            &Key::new(ino, u8::MAX, u64::MAX),
            usize::MAX,
        )?;
        for (k, d) in items {
            // The item goes first: copying a leaf shared with a snapshot
            // adds the owners its extents are then released from.
            self.delete(k)?;
            if let Some((s, l)) = extent_of(&k, &d) {
                self.sp_decref(s, l)?;
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
        Ok(self
            .dir_lookup(dir, name.as_bytes())?
            .ok_or(Error::NotFound)?
            .ino)
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
        let mut out = Vec::with_capacity(items.len());
        for (_, d) in items {
            let e = decode_pair(&d)?;
            out.push(DirEntry {
                ino: e.ino,
                name: String::from_utf8(e.name).map_err(|_| Error::Corrupt("entry name"))?,
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
        let name = check_name(name)?;
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
            i.flags = d.flags & FL_INHERITED as u64;
        }
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
        let name = check_name(name)?;
        self.writable()?;
        let mut i = self.inode(ino)?;
        if i.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
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
        let name = check_name(name)?;
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
        let old_name = check_name(old_name)?;
        let new_name = check_name(new_name)?;
        self.writable()?;
        let e = self.dir_lookup(old_dir, old_name)?.ok_or(Error::NotFound)?;
        if old_dir == new_dir && old_name == new_name {
            return Ok(());
        }
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
        if let Some(v) = self.inline_data(ino)? {
            let o = offset as usize;
            let avail = v.len().saturating_sub(o).min(n);
            buf[..avail].copy_from_slice(&v[o..o + avail]);
            buf[avail..n].fill(0);
            return Ok(n);
        }
        self.read_blocks(ino, offset, &mut buf[..n])?;
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

    pub fn list_subvolumes(&mut self) -> Result<Vec<SubvolInfo>> {
        // Items of the subvolumes touched since the last commit are newer
        // than the root tree's.
        let mut items: BTreeMap<u64, RootItem> = self.root_items()?.into_iter().collect();
        items.insert(self.subvol, self.fs_item.clone());
        for (&id, (_, it)) in &self.subvols {
            items.insert(id, it.clone());
        }
        let mut back: BTreeMap<u64, (u64, DirEnt)> = BTreeMap::new();
        for (id, p, e) in self.backrefs()? {
            back.insert(id, (p, e));
        }
        let mut out = Vec::new();
        for (id, it) in items {
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
        let exts = match inline {
            Some(_) => Vec::new(),
            None => self.extents(s_ino, src_off / BS, src_off / BS + blocks)?,
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
        // Checksummed and no-COW files cannot share blocks (as on btrfs).
        if (di.flags ^ si.flags) & FL_NOCOW as u64 != 0 {
            return Err(Error::Invalid);
        }
        // A partial last block only at the end of both files.
        if len % BS != 0 && (s_end != si.size || d_end < di.size) {
            return Err(Error::Invalid);
        }
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
            None => self.share_extents(d_ino, &mut di, &exts, src_off / BS, dst_off / BS, blocks),
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

    /// Map file blocks `d0..d0+blocks` of `ino` to the blocks `exts` map
    /// at `s0..`, as one more owner of them (holes stay holes).
    fn share_extents(
        &mut self,
        ino: u64,
        inode: &mut InodeItem,
        exts: &[Ext],
        s0: u64,
        d0: u64,
        blocks: u64,
    ) -> Result<()> {
        if self.inline_data(ino)?.is_some() {
            self.uninline(ino, inode)?;
        }
        self.unmap(ino, inode, d0, d0 + blocks)?;
        // Preallocated ranges stay holes: they read as zeros either way.
        for e in exts.iter().filter(|e| !e.prealloc) {
            let from = e.lblk.max(s0);
            let to = (e.lblk + e.blocks).min(s0 + blocks);
            if from >= to {
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
        Ok(self.inode(ino)?.flags as u32 & FL_SETTABLE)
    }

    pub fn set_flags(&mut self, h: u64, flags: u32) -> Result<()> {
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
        i.flags = (i.flags & !(FL_SETTABLE as u64)) | flags as u64;
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
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
            let goal = if lb > 0 {
                self.extents(ino, lb - 1, lb)?
                    .first()
                    .map(|e| e.pblk + (lb - e.lblk))
            } else {
                None
            };
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
        for g in self.sp.alloc.groups() {
            own(&mut owners, total, g.start_block, 2)?;
        }
        own(&mut owners, total, self.journal.start, self.journal.blocks)?;
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
                            if matches!(FileExtent::decode(d)?, FileExtent::Reg { .. }) {
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
        for b in 0..total {
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
        for (id, _) in &subvols {
            let named = backs.iter().filter(|b| b.0 == *id).count();
            if named != usize::from(*id != FS_TREE) {
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
        let items = self
            .st
            .range(&item.root, &Key::MIN, &Key::MAX, usize::MAX)?;
        let mut links = Vec::new();
        let mut regs = Vec::new();
        let mut inodes: BTreeMap<u64, InodeItem> = BTreeMap::new();
        let mut names: BTreeMap<u64, u32> = BTreeMap::new();
        let mut refs: BTreeMap<u64, u32> = BTreeMap::new();
        let mut subdirs: BTreeMap<u64, u32> = BTreeMap::new();
        let mut bytes: BTreeMap<u64, u64> = BTreeMap::new();
        let mut index_names: BTreeSet<(u64, u64, Vec<u8>, u64)> = BTreeSet::new();
        let mut item_names: BTreeSet<(u64, u64, Vec<u8>, u64)> = BTreeSet::new();
        let mut last_ext_end: BTreeMap<u64, u64> = BTreeMap::new();
        let mut xattr_owners: BTreeSet<u64> = BTreeSet::new();
        for (k, d) in &items {
            match k.ty {
                INODE_ITEM => {
                    inodes.insert(k.objectid, InodeItem::decode(d)?);
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
                        regs.push((k.objectid, disk, blocks));
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
                        *bytes.entry(k.objectid).or_default() += v.len() as u64;
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
        if !inodes.contains_key(&root) {
            return Err(Error::Corrupt("fsck: root"));
        }
        let mut need = RangeSet::default();
        for (ino, disk, blocks) in regs {
            if inodes[&ino].flags & FL_NOCOW as u64 == 0 {
                need.insert(disk, blocks);
            }
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
            let node = match self.st.read_disk(b, level, gen) {
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
                            if matches!(FileExtent::decode(d), Ok(FileExtent::Reg { .. })) {
                                data.insert(s, l);
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
                self.raw.read_at(b * BS, chunk)?;
                r.data_blocks += n;
                match self.csums(b, n) {
                    Ok(sums) => {
                        for (i, c) in sums.into_iter().enumerate() {
                            let block = &chunk[i * BS as usize..(i + 1) * BS as usize];
                            // No-COW files keep no checksums.
                            if c.is_some_and(|c| c != crc32c(block)) {
                                r.bad_data_blocks.push(b + i as u64);
                            }
                        }
                    }
                    Err(Error::Io) => return Err(Error::Io),
                    // The checksums themselves are unreadable.
                    Err(_) => r.bad_data_blocks.extend(b..b + n),
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
        item(fs_root, FIRST_INO + 1, 1).encode(),
    )?;
    let nodes = st.take_dirty();
    write_home(dev, &nodes)?;
    let alloc = sp.alloc;
    for g in alloc.groups() {
        dev.write_at(g.bitmap_block() * BS, &g.to_bytes())?;
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
