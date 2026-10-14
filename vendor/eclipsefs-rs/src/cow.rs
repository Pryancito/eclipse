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
use crate::fs::{CheckReport, DirEntry, FsStat, InodeStat, ScrubReport, COMMIT_OPS};
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
    subvol: u64,
    fs_root: Root,
    fs_item: RootItem,
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
        self.fs_item.root_dir
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
        if self.fs_root.gen == gen || self.fs_item.root != self.fs_root {
            self.fs_item.root = self.fs_root;
            let v = self.fs_item.encode();
            self.st.update(
                &mut self.sp,
                &mut self.root_tree,
                Key::new(self.subvol, ROOT_ITEM, 0),
                v,
            )?;
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
                FileExtent::Reg { .. } => Ok(None),
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
            if let FileExtent::Reg { disk, blocks } = FileExtent::decode(&d)? {
                if k.offset % BS != 0 {
                    return Err(Error::Corrupt("unaligned file extent"));
                }
                let e = Ext {
                    lblk: k.offset / BS,
                    pblk: disk,
                    blocks,
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
        self.unmap(ino, inode, lblk, lblk + n)?;
        // Extend the previous extent when the new blocks continue it.
        if lblk > 0 {
            if let Some(prev) = self.extents(ino, lblk - 1, lblk)?.first().copied() {
                if prev.lblk + prev.blocks == lblk
                    && prev.pblk + prev.blocks == pblk
                    && prev.blocks + n <= MAX_EXTENT_BLOCKS
                {
                    self.upsert(
                        Self::ext_key(ino, prev.lblk),
                        FileExtent::Reg {
                            disk: prev.pblk,
                            blocks: prev.blocks + n,
                        }
                        .encode(),
                    )?;
                    inode.nbytes += n * BS;
                    return Ok(());
                }
            }
        }
        self.insert(
            Self::ext_key(ino, lblk),
            FileExtent::Reg {
                disk: pblk,
                blocks: n,
            }
            .encode(),
        )?;
        inode.nbytes += n * BS;
        Ok(())
    }

    /// Remove the mapping of file blocks `from..to`.
    fn unmap(&mut self, ino: u64, inode: &mut InodeItem, from: u64, to: u64) -> Result<()> {
        for e in self.extents(ino, from, to)? {
            let key = Self::ext_key(ino, e.lblk);
            let end = e.lblk + e.blocks;
            if e.lblk < from {
                self.upsert(
                    key,
                    FileExtent::Reg {
                        disk: e.pblk,
                        blocks: from - e.lblk,
                    }
                    .encode(),
                )?;
            } else {
                self.delete(key)?;
            }
            if end > to {
                self.insert(
                    Self::ext_key(ino, to),
                    FileExtent::Reg {
                        disk: e.pblk + (to - e.lblk),
                        blocks: end - to,
                    }
                    .encode(),
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
        for e in self.extents(ino, off / BS, end.div_ceil(BS))? {
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
                    self.verify(p0, dst)?;
                } else {
                    let mut tmp = vec![0u8; ((b1 - b0) * BS) as usize];
                    self.raw.read_at(p0 * BS, &mut tmp)?;
                    self.verify(p0, &tmp)?;
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
                    (run_end, self.sp.private_fresh(p, run_end - lb).then_some(p))
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
                self.sp.csums.insert(pblk + i as u64, Some(crc32c(block)));
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

    fn add_entry(&mut self, dir: u64, name: &[u8], ino: u64, kind: FileKind) -> Result<()> {
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
        self.insert(
            Key::new(ino, INODE_REF, ref_hash(dir, index)),
            encode_pair(dir, index, kind, name),
        )?;
        let now = self.now();
        d.dir_next += 1;
        d.size += 2 * name.len() as u64;
        d.mtime = now;
        d.ctime = now;
        self.put_inode(dir, d)
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
        self.delete(Key::new(e.ino, INODE_REF, ref_hash(dir, e.index)))?;
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
            if let Some((s, l)) = extent_of(&k, &d) {
                self.sp_decref(s, l)?;
            }
            self.delete(k)?;
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
    // Public API (same as `btrfs::Btrfs`)
    // -----------------------------------------------------------------------

    pub fn stat(&mut self, ino: u64) -> Result<InodeStat> {
        let i = self.inode(ino)?;
        Ok(stat_of(ino, &i))
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
        let mut i = self.inode(ino)?;
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

    pub fn lookup(&mut self, dir: u64, name: &str) -> Result<u64> {
        Ok(self
            .dir_lookup(dir, name.as_bytes())?
            .ok_or(Error::NotFound)?
            .ino)
    }

    pub fn readdir(&mut self, dir: u64) -> Result<Vec<DirEntry>> {
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

    pub fn create(
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
        let ino = self.fs_item.next_ino;
        let mut i = InodeItem::new(kind, mode, self.now(), self.st.gen);
        i.rdev = rdev;
        i.parent = dir;
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

    pub fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<u64> {
        if target.is_empty() || target.len() > MAX_SYMLINK_LEN {
            return Err(Error::Invalid);
        }
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

    pub fn write_symlink(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        self.writable()?;
        if self.inode(ino)?.kind() != FileKind::Symlink {
            return Err(Error::Invalid);
        }
        let end = (offset as usize)
            .checked_add(data.len())
            .filter(|&e| e <= MAX_SYMLINK_LEN)
            .ok_or(Error::Invalid)?;
        let mut target = self.read_link(ino)?;
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

    pub fn read_link(&mut self, ino: u64) -> Result<Vec<u8>> {
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

    pub fn parent(&mut self, ino: u64) -> Result<u64> {
        if ino == self.fs_item.root_dir {
            return Ok(ino);
        }
        match self.inode(ino)?.parent {
            0 => Err(Error::NotFound),
            p => Ok(p),
        }
    }

    pub fn link(&mut self, dir: u64, name: &str, ino: u64) -> Result<()> {
        let name = check_name(name)?;
        self.writable()?;
        let mut i = self.inode(ino)?;
        if i.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        if self.dir_lookup(dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        self.add_entry(dir, name, ino, i.kind())?;
        i.nlink += 1;
        i.ctime = self.now();
        self.put_inode(ino, i)?;
        self.commit()
    }

    pub fn unlink(&mut self, dir: u64, name: &str) -> Result<()> {
        let name = check_name(name)?;
        self.writable()?;
        let e = self.dir_lookup(dir, name)?.ok_or(Error::NotFound)?;
        if e.kind == FileKind::Dir && !self.dir_is_empty(e.ino)? {
            return Err(Error::NotEmpty);
        }
        self.remove_entry(dir, name)?;
        self.drop_link(dir, e.ino)?;
        self.commit()
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
        self.writable()?;
        let e = self.dir_lookup(old_dir, old_name)?.ok_or(Error::NotFound)?;
        if old_dir == new_dir && old_name == new_name {
            return Ok(());
        }
        let is_dir = e.kind == FileKind::Dir;
        if self.inode(new_dir)?.kind() != FileKind::Dir {
            return Err(Error::NotDir);
        }
        if is_dir && old_dir != new_dir {
            let mut cur = new_dir;
            for _ in 0..4096 {
                if cur == e.ino {
                    return Err(Error::Invalid);
                }
                if cur == self.fs_item.root_dir {
                    break;
                }
                cur = self.parent(cur)?;
            }
        }
        if let Some(existing) = self.dir_lookup(new_dir, new_name)? {
            if existing.ino == e.ino {
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
        self.remove_entry(old_dir, old_name)?;
        self.add_entry(new_dir, new_name, e.ino, e.kind)?;
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

    pub fn read(&mut self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let i = self.inode(ino)?;
        if i.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        self.read_inode_data(ino, &i, offset, buf)
    }

    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
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

    pub fn truncate(&mut self, ino: u64, new_size: u64) -> Result<()> {
        self.writable()?;
        let mut i = self.inode(ino)?;
        match i.kind() {
            FileKind::Dir => return Err(Error::IsDir),
            FileKind::Regular => {}
            _ => return Err(Error::Invalid),
        }
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
                            data.insert(s, l);
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
        // Every data block in use has a checksum, and nothing else does.
        if summed != data {
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

        // Namespace of the subvolume.
        let items = self
            .st
            .range(&self.fs_root, &Key::MIN, &Key::MAX, usize::MAX)?;
        let mut inodes: BTreeMap<u64, InodeItem> = BTreeMap::new();
        let mut names: BTreeMap<u64, u32> = BTreeMap::new();
        let mut refs: BTreeMap<u64, u32> = BTreeMap::new();
        let mut subdirs: BTreeMap<u64, u32> = BTreeMap::new();
        let mut bytes: BTreeMap<u64, u64> = BTreeMap::new();
        let mut index_names: BTreeSet<(u64, u64, Vec<u8>, u64)> = BTreeSet::new();
        let mut item_names: BTreeSet<(u64, u64, Vec<u8>, u64)> = BTreeSet::new();
        let mut last_ext_end: BTreeMap<u64, u64> = BTreeMap::new();
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
                    *names.entry(e.ino).or_default() += 1;
                    if e.kind == FileKind::Dir {
                        *subdirs.entry(k.objectid).or_default() += 1;
                    }
                    index_names.insert((k.objectid, k.offset, e.name, e.ino));
                }
                EXTENT_DATA => match FileExtent::decode(d)? {
                    FileExtent::Inline(v) => {
                        if k.offset != 0 {
                            return Err(Error::Corrupt("fsck: inline extent offset"));
                        }
                        *bytes.entry(k.objectid).or_default() += v.len() as u64;
                    }
                    FileExtent::Reg { blocks, .. } => {
                        let end = last_ext_end.entry(k.objectid).or_default();
                        if k.offset % BS != 0 || k.offset / BS < *end {
                            return Err(Error::Corrupt("fsck: overlapping extents"));
                        }
                        *end = k.offset / BS + blocks;
                        *bytes.entry(k.objectid).or_default() += blocks * BS;
                    }
                },
                _ => return Err(Error::Corrupt("fsck: unknown item")),
            }
        }
        if index_names != item_names {
            return Err(Error::Corrupt("fsck: name indexes differ"));
        }
        let root = self.fs_item.root_dir;
        for (&ino, i) in &inodes {
            if ino >= self.fs_item.next_ino {
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
            if !inodes.contains_key(ino) {
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
        for (dir, _, _, ino) in &index_names {
            let i = &inodes[ino];
            if i.kind() == FileKind::Dir && i.parent != *dir {
                return Err(Error::Corrupt("fsck: directory parent"));
            }
        }
        if inodes.len() as u64 != self.fs_item.inode_count {
            return Err(Error::Corrupt("fsck: inode count"));
        }
        if !inodes.contains_key(&root) {
            return Err(Error::Corrupt("fsck: root"));
        }
        Ok(CheckReport {
            inodes: inodes.len() as u64,
            directories: dirs.len() as u64,
            blocks_used,
        })
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
                            data.insert(s, l);
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
                            if c != Some(crc32c(block)) {
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
