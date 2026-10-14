//! Copy-on-write B+tree (EclipseFS v0.4).
//!
//! Every tree (the root tree, the refcount tree, each subvolume) is a B+tree
//! of 4 KiB nodes keyed by `(objectid, type, offset)`, as in btrfs. A node is
//! never modified once committed: changing it writes a copy to a new block
//! and updates the parent to point there, up to the root ("shadowing"). A
//! transaction ends by writing the new nodes and switching the superblock to
//! the new root tree, so a crash always leaves one whole tree on disk.
//!
//! Nodes can be shared between trees (snapshots). Sharing is tracked with
//! lazy reference counts (Rodeh, "B-trees, Shadowing, and Clones", 2008):
//! copying a shared node adds a reference to each of its children, and
//! freeing the last reference to a node drops one from each child. The
//! counts themselves live outside this module, behind [`Space`].
//!
//! Node layout (little-endian):
//!
//! ```text
//! 0   magic "EFSN"     4  crc32c of bytes 8..4096
//! 8   bytenr (block)   16 generation      24 owner tree
//! 32  nritems u32      36 level u8        40 fsid [16]
//! 64  leaf:     items [key 17 | data offset u32 | data size u32], data
//!               packed from the end of the block
//!     internal: pointers [key 17 | child block u64 | child generation u64]
//! ```

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::crc::crc32c;
use crate::device::BlockDevice;
use crate::format::{le32, le64, put32, put64, BLOCK_SIZE};
use crate::{Error, Result};

const BS: usize = BLOCK_SIZE as usize;
pub const NODE_MAGIC: u32 = u32::from_le_bytes(*b"EFSN");
pub const NODE_HDR: usize = 64;
const KEY_SIZE: usize = 17;
const LEAF_ITEM: usize = KEY_SIZE + 8;
const PTR_SIZE: usize = KEY_SIZE + 16;
/// Bytes available for items in a leaf.
pub const LEAF_SPACE: usize = BS - NODE_HDR;
/// Pointers in an internal node.
pub const MAX_PTRS: usize = (BS - NODE_HDR) / PTR_SIZE;
/// Largest item: any two fit in one leaf, so a split never needs a third.
pub const MAX_ITEM: usize = LEAF_SPACE / 2 - LEAF_ITEM;
/// Deepest tree accepted (a 4 KiB tree of height 8 holds ~10^16 items).
pub const MAX_LEVEL: u8 = 8;
/// Clean nodes kept decoded in memory.
const CACHE_NODES: usize = 8192;

/// Item key, ordered by objectid, then type, then offset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub objectid: u64,
    pub ty: u8,
    pub offset: u64,
}

impl Key {
    pub const MIN: Key = Key::new(0, 0, 0);
    pub const MAX: Key = Key::new(u64::MAX, u8::MAX, u64::MAX);

    pub const fn new(objectid: u64, ty: u8, offset: u64) -> Self {
        Self {
            objectid,
            ty,
            offset,
        }
    }

    fn encode(&self, b: &mut [u8]) {
        put64(b, 0, self.objectid);
        b[8] = self.ty;
        put64(b, 9, self.offset);
    }

    fn decode(b: &[u8]) -> Self {
        Self::new(le64(b, 0), b[8], le64(b, 9))
    }
}

/// Child pointer of an internal node. `key` is the first key in the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ptr {
    pub key: Key,
    pub bytenr: u64,
    pub gen: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Leaf(Vec<(Key, Vec<u8>)>),
    Internal(Vec<Ptr>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub owner: u64,
    pub gen: u64,
    pub level: u8,
    pub body: Body,
}

fn leaf_bytes(items: &[(Key, Vec<u8>)]) -> usize {
    items.iter().map(|(_, d)| LEAF_ITEM + d.len()).sum()
}

impl Node {
    pub fn empty_leaf(owner: u64, gen: u64) -> Self {
        Self {
            owner,
            gen,
            level: 0,
            body: Body::Leaf(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        match &self.body {
            Body::Leaf(i) => i.len(),
            Body::Internal(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn first_key(&self) -> Option<Key> {
        match &self.body {
            Body::Leaf(i) => i.first().map(|(k, _)| *k),
            Body::Internal(p) => p.first().map(|p| p.key),
        }
    }

    fn fits(&self) -> bool {
        match &self.body {
            Body::Leaf(i) => leaf_bytes(i) <= LEAF_SPACE,
            Body::Internal(p) => p.len() <= MAX_PTRS,
        }
    }

    fn underfull(&self) -> bool {
        match &self.body {
            Body::Leaf(i) => leaf_bytes(i) < LEAF_SPACE / 4,
            Body::Internal(p) => p.len() < MAX_PTRS / 4,
        }
    }

    pub fn encode(&self, bytenr: u64, fsid: &[u8; 16]) -> Vec<u8> {
        let mut b = vec![0u8; BS];
        put32(&mut b, 0, NODE_MAGIC);
        put64(&mut b, 8, bytenr);
        put64(&mut b, 16, self.gen);
        put64(&mut b, 24, self.owner);
        put32(&mut b, 32, self.len() as u32);
        b[36] = self.level;
        b[40..56].copy_from_slice(fsid);
        match &self.body {
            Body::Leaf(items) => {
                let mut end = BS;
                for (i, (k, d)) in items.iter().enumerate() {
                    let at = NODE_HDR + i * LEAF_ITEM;
                    end -= d.len();
                    k.encode(&mut b[at..]);
                    put32(&mut b, at + KEY_SIZE, end as u32);
                    put32(&mut b, at + KEY_SIZE + 4, d.len() as u32);
                    b[end..end + d.len()].copy_from_slice(d);
                }
            }
            Body::Internal(ptrs) => {
                for (i, p) in ptrs.iter().enumerate() {
                    let at = NODE_HDR + i * PTR_SIZE;
                    p.key.encode(&mut b[at..]);
                    put64(&mut b, at + KEY_SIZE, p.bytenr);
                    put64(&mut b, at + KEY_SIZE + 8, p.gen);
                }
            }
        }
        let crc = crc32c(&b[8..]);
        put32(&mut b, 4, crc);
        b
    }

    /// Decode and verify the node at `bytenr`.
    pub fn decode(b: &[u8], bytenr: u64, fsid: &[u8; 16]) -> Result<Self> {
        if b.len() != BS || le32(b, 0) != NODE_MAGIC {
            return Err(Error::Corrupt("tree node magic"));
        }
        if crc32c(&b[8..]) != le32(b, 4) {
            return Err(Error::Corrupt("tree node checksum"));
        }
        if le64(b, 8) != bytenr || b[40..56] != fsid[..] {
            return Err(Error::Corrupt("tree node location"));
        }
        let n = le32(b, 32) as usize;
        let level = b[36];
        if level > MAX_LEVEL {
            return Err(Error::Corrupt("tree node level"));
        }
        let body = if level == 0 {
            if n > LEAF_SPACE / LEAF_ITEM {
                return Err(Error::Corrupt("tree leaf item count"));
            }
            let data_start = NODE_HDR + n * LEAF_ITEM;
            let mut items: Vec<(Key, Vec<u8>)> = Vec::with_capacity(n);
            for i in 0..n {
                let at = NODE_HDR + i * LEAF_ITEM;
                let k = Key::decode(&b[at..]);
                let off = le32(b, at + KEY_SIZE) as usize;
                let len = le32(b, at + KEY_SIZE + 4) as usize;
                if off < data_start || off.checked_add(len).is_none_or(|e| e > BS) {
                    return Err(Error::Corrupt("tree leaf item bounds"));
                }
                if items.last().is_some_and(|(p, _)| *p >= k) {
                    return Err(Error::Corrupt("tree leaf key order"));
                }
                items.push((k, b[off..off + len].to_vec()));
            }
            Body::Leaf(items)
        } else {
            if n == 0 || n > MAX_PTRS {
                return Err(Error::Corrupt("tree node pointer count"));
            }
            let mut ptrs: Vec<Ptr> = Vec::with_capacity(n);
            for i in 0..n {
                let at = NODE_HDR + i * PTR_SIZE;
                let p = Ptr {
                    key: Key::decode(&b[at..]),
                    bytenr: le64(b, at + KEY_SIZE),
                    gen: le64(b, at + KEY_SIZE + 8),
                };
                if ptrs.last().is_some_and(|q| q.key >= p.key) {
                    return Err(Error::Corrupt("tree node key order"));
                }
                ptrs.push(p);
            }
            Body::Internal(ptrs)
        };
        Ok(Self {
            owner: le64(b, 24),
            gen: le64(b, 16),
            level,
            body,
        })
    }
}

/// Where a tree starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Root {
    pub bytenr: u64,
    pub level: u8,
    pub gen: u64,
}

/// Block accounting the trees need: node allocation and reference counts.
/// Data extents referenced from leaf items are counted in the same way.
pub trait Space {
    /// A free block for a new node, close to `goal`.
    fn alloc_node(&mut self, goal: u64) -> Result<u64>;
    /// Owners of `bytenr` (1 for an unshared block).
    fn refs(&self, bytenr: u64) -> u32;
    /// One more owner for every block of the range.
    fn incref(&mut self, start: u64, len: u64) -> Result<()>;
    /// One owner fewer for a node; true when that was the last one and the
    /// block is now free.
    fn decref_node(&mut self, bytenr: u64) -> Result<bool>;
    /// One owner fewer for every block of a data range; blocks left without
    /// owners are freed.
    fn decref_data(&mut self, start: u64, len: u64) -> Result<()>;
}

/// The data blocks a leaf item references, if any (`(first block, count)`).
pub type ExtentOf = fn(&Key, &[u8]) -> Option<(u64, u64)>;

/// What a modification does at its key.
#[derive(Clone, Debug)]
pub enum Op {
    /// Add; `Exists` if the key is there.
    Insert(Vec<u8>),
    /// Replace; `NotFound` if the key is missing.
    Update(Vec<u8>),
    /// Add or replace.
    Upsert(Vec<u8>),
    /// Remove; `NotFound` if the key is missing.
    Delete,
}

struct Outcome {
    /// New right sibling after a split: its first key and block.
    split: Option<(Key, u64)>,
    underfull: bool,
    /// Previous value at the key.
    old: Option<Vec<u8>>,
}

/// Node cache, copy-on-write and the tree algorithms, for every tree of one
/// filesystem.
pub struct Store {
    dev: Arc<dyn BlockDevice>,
    fsid: [u8; 16],
    /// Generation of the transaction being built.
    pub gen: u64,
    /// Blocks past this are corrupt pointers.
    pub total_blocks: u64,
    cache: BTreeMap<u64, Node>,
    /// Nodes created in this transaction (written at commit).
    dirty: BTreeSet<u64>,
    extent_of: ExtentOf,
}

impl Store {
    pub fn new(
        dev: Arc<dyn BlockDevice>,
        fsid: [u8; 16],
        gen: u64,
        total_blocks: u64,
        extent_of: ExtentOf,
    ) -> Self {
        Self {
            dev,
            fsid,
            gen,
            total_blocks,
            cache: BTreeMap::new(),
            dirty: BTreeSet::new(),
            extent_of,
        }
    }

    /// Nodes waiting to be written.
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// Encode every node of this transaction (block, image), in block order,
    /// and start the next transaction.
    pub fn take_dirty(&mut self) -> Vec<(u64, Vec<u8>)> {
        let out = self
            .dirty
            .iter()
            .map(|&b| (b, self.cache[&b].encode(b, &self.fsid)))
            .collect();
        self.dirty.clear();
        self.gen += 1;
        self.trim();
        out
    }

    fn trim(&mut self) {
        if self.cache.len() <= CACHE_NODES {
            return;
        }
        let clean: Vec<u64> = self
            .cache
            .keys()
            .copied()
            .filter(|b| !self.dirty.contains(b))
            .take(self.cache.len() - CACHE_NODES * 3 / 4)
            .collect();
        for b in clean {
            self.cache.remove(&b);
        }
    }

    /// Create a tree with one empty leaf.
    pub fn create(&mut self, sp: &mut dyn Space, owner: u64, goal: u64) -> Result<Root> {
        let b = sp.alloc_node(goal)?;
        self.cache.insert(b, Node::empty_leaf(owner, self.gen));
        self.dirty.insert(b);
        Ok(Root {
            bytenr: b,
            level: 0,
            gen: self.gen,
        })
    }

    fn load(&mut self, bytenr: u64, level: u8, gen: Option<u64>) -> Result<()> {
        if !self.cache.contains_key(&bytenr) {
            if bytenr < 2 || bytenr >= self.total_blocks {
                return Err(Error::Corrupt("tree pointer out of range"));
            }
            if self.cache.len() > CACHE_NODES * 2 {
                self.trim();
            }
            let mut buf = vec![0u8; BS];
            self.dev.read_at(bytenr * BLOCK_SIZE, &mut buf)?;
            let node = Node::decode(&buf, bytenr, &self.fsid)?;
            self.cache.insert(bytenr, node);
        }
        let node = &self.cache[&bytenr];
        if node.level != level || gen.is_some_and(|g| g != node.gen) {
            return Err(Error::Corrupt("tree node level or generation"));
        }
        Ok(())
    }

    /// The node at `bytenr` as it is on the device, bypassing the cache
    /// (for scrub; the caller commits first so nothing is dirty).
    pub fn read_disk(&self, bytenr: u64, level: u8, gen: Option<u64>) -> Result<Node> {
        if bytenr < 2 || bytenr >= self.total_blocks {
            return Err(Error::Corrupt("tree pointer out of range"));
        }
        let mut buf = vec![0u8; BS];
        self.dev.read_at(bytenr * BLOCK_SIZE, &mut buf)?;
        let node = Node::decode(&buf, bytenr, &self.fsid)?;
        if node.level != level || gen.is_some_and(|g| g != node.gen) {
            return Err(Error::Corrupt("tree node level or generation"));
        }
        Ok(node)
    }

    /// The node at `bytenr` (for checks and tests).
    pub fn node(&mut self, bytenr: u64, level: u8, gen: Option<u64>) -> Result<&Node> {
        self.load(bytenr, level, gen)?;
        Ok(&self.cache[&bytenr])
    }

    /// Make `bytenr` writable in this transaction: itself if it was created
    /// in it and nobody else points at it, a copy otherwise.
    fn cow(&mut self, sp: &mut dyn Space, bytenr: u64, level: u8) -> Result<u64> {
        self.load(bytenr, level, None)?;
        let shared = sp.refs(bytenr) > 1;
        if self.dirty.contains(&bytenr) && !shared {
            return Ok(bytenr);
        }
        // Allocate first: on a full device nothing has changed yet.
        let new = sp.alloc_node(bytenr)?;
        let mut node = self.cache[&bytenr].clone();
        if shared {
            // The copy is one more owner of everything below the node.
            match &node.body {
                Body::Internal(ptrs) => {
                    for p in ptrs {
                        sp.incref(p.bytenr, 1)?;
                    }
                }
                Body::Leaf(items) => {
                    for (k, d) in items {
                        if let Some((s, l)) = (self.extent_of)(k, d) {
                            sp.incref(s, l)?;
                        }
                    }
                }
            }
        }
        // Unshared: the copy takes over the children, the old block goes.
        if sp.decref_node(bytenr)? {
            self.dirty.remove(&bytenr);
            self.cache.remove(&bytenr);
        }
        node.gen = self.gen;
        self.cache.insert(new, node);
        self.dirty.insert(new);
        Ok(new)
    }

    /// Drop one reference to a whole tree (deleting a snapshot), freeing
    /// whatever no other tree shares.
    pub fn release(&mut self, sp: &mut dyn Space, root: Root) -> Result<()> {
        self.release_node(sp, root.bytenr, root.level)
    }

    fn release_node(&mut self, sp: &mut dyn Space, bytenr: u64, level: u8) -> Result<()> {
        self.load(bytenr, level, None)?;
        let node = self.cache[&bytenr].clone();
        if !sp.decref_node(bytenr)? {
            return Ok(());
        }
        self.dirty.remove(&bytenr);
        self.cache.remove(&bytenr);
        match node.body {
            Body::Internal(ptrs) => {
                for p in ptrs {
                    self.release_node(sp, p.bytenr, level - 1)?;
                }
            }
            Body::Leaf(items) => {
                for (k, d) in &items {
                    if let Some((s, l)) = (self.extent_of)(k, d) {
                        sp.decref_data(s, l)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// One more owner of a whole tree (taking a snapshot of it).
    pub fn share(&mut self, sp: &mut dyn Space, root: Root) -> Result<()> {
        sp.incref(root.bytenr, 1)
    }

    // -------------------------------------------------------------------
    // Lookups
    // -------------------------------------------------------------------

    pub fn get(&mut self, root: &Root, key: &Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .seek_le(root, key)?
            .filter(|(k, _)| k == key)
            .map(|(_, d)| d))
    }

    /// The item with the greatest key `<= key`.
    pub fn seek_le(&mut self, root: &Root, key: &Key) -> Result<Option<(Key, Vec<u8>)>> {
        let mut b = root.bytenr;
        let mut level = root.level;
        let mut gen = None;
        loop {
            self.load(b, level, gen)?;
            match &self.cache[&b].body {
                Body::Leaf(items) => {
                    let i = items.partition_point(|(k, _)| k <= key);
                    return Ok(if i == 0 {
                        None
                    } else {
                        Some(items[i - 1].clone())
                    });
                }
                Body::Internal(ptrs) => {
                    let i = ptrs.partition_point(|p| p.key <= *key);
                    if i == 0 {
                        return Ok(None);
                    }
                    b = ptrs[i - 1].bytenr;
                    gen = Some(ptrs[i - 1].gen);
                    level -= 1;
                }
            }
        }
    }

    /// Whether any node on the way from `root` to the leaf holding `key`
    /// has more than one owner (the leaf's items are then shared with
    /// another tree, whatever their own counts say).
    pub fn path_shared(&mut self, sp: &dyn Space, root: &Root, key: &Key) -> Result<bool> {
        let mut b = root.bytenr;
        let mut level = root.level;
        let mut gen = None;
        loop {
            if sp.refs(b) > 1 {
                return Ok(true);
            }
            self.load(b, level, gen)?;
            match &self.cache[&b].body {
                Body::Leaf(_) => return Ok(false),
                Body::Internal(ptrs) => {
                    let i = ptrs.partition_point(|p| p.key <= *key).max(1);
                    b = ptrs[i - 1].bytenr;
                    gen = Some(ptrs[i - 1].gen);
                    level -= 1;
                }
            }
        }
    }

    /// Items with `lo <= key <= hi`, in order, at most `limit` of them.
    pub fn range(
        &mut self,
        root: &Root,
        lo: &Key,
        hi: &Key,
        limit: usize,
    ) -> Result<Vec<(Key, Vec<u8>)>> {
        let mut out = Vec::new();
        if lo <= hi && limit > 0 {
            self.range_rec(root.bytenr, root.level, None, lo, hi, limit, &mut out)?;
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn range_rec(
        &mut self,
        b: u64,
        level: u8,
        gen: Option<u64>,
        lo: &Key,
        hi: &Key,
        limit: usize,
        out: &mut Vec<(Key, Vec<u8>)>,
    ) -> Result<bool> {
        self.load(b, level, gen)?;
        match &self.cache[&b].body {
            Body::Leaf(items) => {
                let start = items.partition_point(|(k, _)| k < lo);
                for (k, d) in &items[start..] {
                    if k > hi {
                        return Ok(true);
                    }
                    out.push((*k, d.clone()));
                    if out.len() >= limit {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Body::Internal(ptrs) => {
                let first = ptrs.partition_point(|p| p.key <= *lo).saturating_sub(1);
                let ptrs: Vec<Ptr> = ptrs[first..].to_vec();
                for p in ptrs {
                    if p.key > *hi {
                        return Ok(true);
                    }
                    if self.range_rec(p.bytenr, level - 1, Some(p.gen), lo, hi, limit, out)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// Every node block of a tree with its level, for checking.
    pub fn walk_nodes(
        &mut self,
        root: &Root,
        visit: &mut dyn FnMut(u64, &Node) -> bool,
    ) -> Result<()> {
        self.walk_rec(root.bytenr, root.level, None, visit)
    }

    fn walk_rec(
        &mut self,
        b: u64,
        level: u8,
        gen: Option<u64>,
        visit: &mut dyn FnMut(u64, &Node) -> bool,
    ) -> Result<()> {
        self.load(b, level, gen)?;
        let node = self.cache[&b].clone();
        if !visit(b, &node) {
            return Ok(());
        }
        if let Body::Internal(ptrs) = node.body {
            for p in ptrs {
                if level == 0 {
                    return Err(Error::Corrupt("tree level"));
                }
                self.walk_rec(p.bytenr, level - 1, Some(p.gen), visit)?;
            }
        }
        Ok(())
    }

    /// Items that differ between trees `a` and `b`, in key order, as
    /// (key, value in `a`, value in `b`). Subtrees the two share (a
    /// snapshot and its source) are skipped without being read.
    #[allow(clippy::type_complexity)]
    pub fn diff(
        &mut self,
        a: &Root,
        b: &Root,
    ) -> Result<Vec<(Key, Option<Vec<u8>>, Option<Vec<u8>>)>> {
        // Every node of `b`, from its internal nodes alone.
        let mut in_b = BTreeSet::new();
        self.pointers(b.bytenr, b.level, None, &mut in_b)?;
        // `a`'s items outside the nodes it shares with `b`, and those nodes.
        let mut shared = BTreeSet::new();
        let mut items_a = Vec::new();
        self.collect(a.bytenr, a.level, None, &in_b, &mut shared, &mut items_a)?;
        let mut items_b = Vec::new();
        self.collect(
            b.bytenr,
            b.level,
            None,
            &shared,
            &mut BTreeSet::new(),
            &mut items_b,
        )?;
        // Both lists are sorted: merge them.
        let mut out = Vec::new();
        let mut ia = items_a.into_iter().peekable();
        let mut ib = items_b.into_iter().peekable();
        loop {
            match (ia.peek(), ib.peek()) {
                (None, None) => break,
                (Some(_), None) => {
                    let (k, v) = ia.next().unwrap();
                    out.push((k, Some(v), None));
                }
                (None, Some(_)) => {
                    let (k, v) = ib.next().unwrap();
                    out.push((k, None, Some(v)));
                }
                (Some((ka, _)), Some((kb, _))) => {
                    if ka < kb {
                        let (k, v) = ia.next().unwrap();
                        out.push((k, Some(v), None));
                    } else if kb < ka {
                        let (k, v) = ib.next().unwrap();
                        out.push((k, None, Some(v)));
                    } else {
                        let (k, va) = ia.next().unwrap();
                        let (_, vb) = ib.next().unwrap();
                        if va != vb {
                            out.push((k, Some(va), Some(vb)));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Blocks of the node at `b` and everything under it, reading only
    /// internal nodes.
    fn pointers(
        &mut self,
        b: u64,
        level: u8,
        gen: Option<u64>,
        out: &mut BTreeSet<u64>,
    ) -> Result<()> {
        out.insert(b);
        if level == 0 {
            return Ok(());
        }
        self.load(b, level, gen)?;
        let Body::Internal(ptrs) = self.cache[&b].body.clone() else {
            return Err(Error::Corrupt("tree level"));
        };
        for p in ptrs {
            self.pointers(p.bytenr, level - 1, Some(p.gen), out)?;
        }
        Ok(())
    }

    /// Items under the node at `b`, in key order, except under the nodes
    /// in `skip` (which go to `skipped`).
    fn collect(
        &mut self,
        b: u64,
        level: u8,
        gen: Option<u64>,
        skip: &BTreeSet<u64>,
        skipped: &mut BTreeSet<u64>,
        out: &mut Vec<(Key, Vec<u8>)>,
    ) -> Result<()> {
        if skip.contains(&b) {
            skipped.insert(b);
            return Ok(());
        }
        self.load(b, level, gen)?;
        match self.cache[&b].body.clone() {
            Body::Leaf(items) => out.extend(items),
            Body::Internal(ptrs) => {
                if level == 0 {
                    return Err(Error::Corrupt("tree level"));
                }
                for p in ptrs {
                    self.collect(p.bytenr, level - 1, Some(p.gen), skip, skipped, out)?;
                }
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------
    // Modifications
    // -------------------------------------------------------------------

    pub fn insert(
        &mut self,
        sp: &mut dyn Space,
        root: &mut Root,
        key: Key,
        data: Vec<u8>,
    ) -> Result<()> {
        self.modify(sp, root, key, Op::Insert(data)).map(|_| ())
    }

    pub fn update(
        &mut self,
        sp: &mut dyn Space,
        root: &mut Root,
        key: Key,
        data: Vec<u8>,
    ) -> Result<()> {
        self.modify(sp, root, key, Op::Update(data)).map(|_| ())
    }

    pub fn upsert(
        &mut self,
        sp: &mut dyn Space,
        root: &mut Root,
        key: Key,
        data: Vec<u8>,
    ) -> Result<()> {
        self.modify(sp, root, key, Op::Upsert(data)).map(|_| ())
    }

    /// Remove `key`, returning its value.
    pub fn delete(&mut self, sp: &mut dyn Space, root: &mut Root, key: Key) -> Result<Vec<u8>> {
        self.modify(sp, root, key, Op::Delete)?
            .ok_or(Error::Corrupt("delete returned nothing"))
    }

    /// Apply one modification, copying the path from the root. Returns the
    /// previous value at the key.
    pub fn modify(
        &mut self,
        sp: &mut dyn Space,
        root: &mut Root,
        key: Key,
        op: Op,
    ) -> Result<Option<Vec<u8>>> {
        if let Op::Insert(d) | Op::Update(d) | Op::Upsert(d) = &op {
            if d.len() > MAX_ITEM {
                return Err(Error::Invalid);
            }
        }
        // Fail before copying anything when the key says so.
        let present = self.get(root, &key)?.is_some();
        match (&op, present) {
            (Op::Insert(_), true) => return Err(Error::Exists),
            (Op::Update(_) | Op::Delete, false) => return Err(Error::NotFound),
            _ => {}
        }
        let new_root = self.cow(sp, root.bytenr, root.level)?;
        // The copied path is a valid tree even if the change below fails.
        let r = self.modify_rec(sp, new_root, root.level, key, op);
        root.bytenr = new_root;
        root.gen = self.gen;
        let out = r?;
        if let Some((sk, sb)) = out.split {
            let first = self.cache[&new_root].first_key().unwrap_or(Key::MIN);
            let owner = self.cache[&new_root].owner;
            let top = sp.alloc_node(new_root)?;
            self.cache.insert(
                top,
                Node {
                    owner,
                    gen: self.gen,
                    level: root.level + 1,
                    body: Body::Internal(vec![
                        Ptr {
                            key: first,
                            bytenr: new_root,
                            gen: self.gen,
                        },
                        Ptr {
                            key: sk,
                            bytenr: sb,
                            gen: self.gen,
                        },
                    ]),
                },
            );
            self.dirty.insert(top);
            root.bytenr = top;
            root.level += 1;
            if root.level > MAX_LEVEL {
                return Err(Error::NoSpace);
            }
        } else {
            // An internal root with one child is replaced by that child.
            while root.level > 0 {
                let only = match &self.cache[&root.bytenr].body {
                    Body::Internal(p) if p.len() == 1 => p[0].bytenr,
                    _ => break,
                };
                let old = root.bytenr;
                if sp.decref_node(old)? {
                    self.dirty.remove(&old);
                    self.cache.remove(&old);
                }
                root.bytenr = only;
                root.level -= 1;
            }
        }
        Ok(out.old)
    }

    fn modify_rec(
        &mut self,
        sp: &mut dyn Space,
        b: u64,
        level: u8,
        key: Key,
        op: Op,
    ) -> Result<Outcome> {
        if level == 0 {
            return self.modify_leaf(sp, b, key, op);
        }
        let (idx, child, child_gen) = {
            let Body::Internal(ptrs) = &self.cache[&b].body else {
                return Err(Error::Corrupt("tree level"));
            };
            let i = ptrs.partition_point(|p| p.key <= key).saturating_sub(1);
            (i, ptrs[i].bytenr, ptrs[i].gen)
        };
        self.load(child, level - 1, Some(child_gen))?;
        let child = self.cow(sp, child, level - 1)?;
        let r = self.modify_rec(sp, child, level - 1, key, op);
        let gen = self.gen;
        let child_first = self.cache[&child].first_key();
        {
            let Body::Internal(ptrs) = &mut self.cache.get_mut(&b).unwrap().body else {
                unreachable!()
            };
            ptrs[idx].bytenr = child;
            ptrs[idx].gen = gen;
        }
        let out = r?;
        {
            let Body::Internal(ptrs) = &mut self.cache.get_mut(&b).unwrap().body else {
                unreachable!()
            };
            ptrs[idx].bytenr = child;
            ptrs[idx].gen = gen;
            if let Some(k) = child_first {
                ptrs[idx].key = k;
            }
            if let Some((sk, sb)) = out.split {
                ptrs.insert(
                    idx + 1,
                    Ptr {
                        key: sk,
                        bytenr: sb,
                        gen,
                    },
                );
            }
        }
        if out.underfull {
            self.merge_child(sp, b, level, idx)?;
        }
        let node = &self.cache[&b];
        let split = if node.fits() {
            None
        } else {
            Some(self.split(sp, b)?)
        };
        let node = &self.cache[&b];
        Ok(Outcome {
            split,
            underfull: node.underfull(),
            old: out.old,
        })
    }

    fn modify_leaf(&mut self, sp: &mut dyn Space, b: u64, key: Key, op: Op) -> Result<Outcome> {
        let old;
        {
            let Body::Leaf(items) = &mut self.cache.get_mut(&b).unwrap().body else {
                return Err(Error::Corrupt("tree level"));
            };
            let pos = items.binary_search_by(|(k, _)| k.cmp(&key));
            old = match (op, pos) {
                (Op::Insert(_), Ok(_)) => return Err(Error::Exists),
                (Op::Insert(d) | Op::Upsert(d), Err(i)) => {
                    items.insert(i, (key, d));
                    None
                }
                (Op::Update(d) | Op::Upsert(d), Ok(i)) => {
                    Some(core::mem::replace(&mut items[i].1, d))
                }
                (Op::Update(_) | Op::Delete, Err(_)) => return Err(Error::NotFound),
                (Op::Delete, Ok(i)) => Some(items.remove(i).1),
            };
        }
        let split = if self.cache[&b].fits() {
            None
        } else {
            Some(self.split(sp, b)?)
        };
        Ok(Outcome {
            split,
            underfull: self.cache[&b].underfull(),
            old,
        })
    }

    /// Move the upper half of an overfull node to a new right sibling.
    fn split(&mut self, sp: &mut dyn Space, b: u64) -> Result<(Key, u64)> {
        let node = self.cache.get_mut(&b).unwrap();
        let right_body = match &mut node.body {
            Body::Leaf(items) => {
                let total = leaf_bytes(items);
                let mut acc = 0;
                let mut at = items.len();
                for (i, (_, d)) in items.iter().enumerate() {
                    acc += LEAF_ITEM + d.len();
                    if acc > total / 2 {
                        at = i;
                        break;
                    }
                }
                // Both halves must fit (items are at most MAX_ITEM).
                let at = at.clamp(1, items.len() - 1);
                let at = if leaf_bytes(&items[..at]) > LEAF_SPACE {
                    at - 1
                } else if leaf_bytes(&items[at..]) > LEAF_SPACE {
                    at + 1
                } else {
                    at
                };
                Body::Leaf(items.split_off(at))
            }
            Body::Internal(ptrs) => {
                let at = ptrs.len() / 2;
                Body::Internal(ptrs.split_off(at))
            }
        };
        let right = Node {
            owner: node.owner,
            gen: self.gen,
            level: node.level,
            body: right_body,
        };
        let key = right.first_key().ok_or(Error::Corrupt("empty split"))?;
        let rb = sp.alloc_node(b)?;
        self.cache.insert(rb, right);
        self.dirty.insert(rb);
        Ok((key, rb))
    }

    /// Merge the child at `idx` of `parent` with a neighbour when both fit
    /// in one node; an empty child is removed outright.
    fn merge_child(
        &mut self,
        sp: &mut dyn Space,
        parent: u64,
        level: u8,
        idx: usize,
    ) -> Result<()> {
        let ptrs = match &self.cache[&parent].body {
            Body::Internal(p) => p.clone(),
            _ => unreachable!(),
        };
        if ptrs.len() < 2 {
            return Ok(());
        }
        let (l, r) = if idx + 1 < ptrs.len() {
            (idx, idx + 1)
        } else {
            (idx - 1, idx)
        };
        self.load(ptrs[l].bytenr, level - 1, Some(ptrs[l].gen))?;
        self.load(ptrs[r].bytenr, level - 1, Some(ptrs[r].gen))?;
        let fits = {
            let a = &self.cache[&ptrs[l].bytenr];
            let c = &self.cache[&ptrs[r].bytenr];
            match (&a.body, &c.body) {
                (Body::Leaf(x), Body::Leaf(y)) => leaf_bytes(x) + leaf_bytes(y) <= LEAF_SPACE,
                (Body::Internal(x), Body::Internal(y)) => x.len() + y.len() <= MAX_PTRS,
                _ => return Err(Error::Corrupt("tree sibling levels")),
            }
        };
        if !fits {
            return Ok(());
        }
        let lb = self.cow(sp, ptrs[l].bytenr, level - 1)?;
        let rb = ptrs[r].bytenr;
        let right = self.cache[&rb].clone();
        // The right node's contents move into the left copy; the right block
        // loses this parent as an owner (its children keep theirs).
        if sp.refs(rb) > 1 {
            match &right.body {
                Body::Internal(ps) => {
                    for p in ps {
                        sp.incref(p.bytenr, 1)?;
                    }
                }
                Body::Leaf(items) => {
                    for (k, d) in items {
                        if let Some((s, len)) = (self.extent_of)(k, d) {
                            sp.incref(s, len)?;
                        }
                    }
                }
            }
        }
        if sp.decref_node(rb)? {
            self.dirty.remove(&rb);
            self.cache.remove(&rb);
        }
        {
            let left = self.cache.get_mut(&lb).unwrap();
            match (&mut left.body, right.body) {
                (Body::Leaf(x), Body::Leaf(y)) => x.extend(y),
                (Body::Internal(x), Body::Internal(y)) => x.extend(y),
                _ => unreachable!(),
            }
        }
        let first = self.cache[&lb].first_key();
        let gen = self.gen;
        let Body::Internal(p) = &mut self.cache.get_mut(&parent).unwrap().body else {
            unreachable!()
        };
        p.remove(r);
        p[l].bytenr = lb;
        p[l].gen = gen;
        match first {
            Some(k) => p[l].key = k,
            None if p.len() > 1 => {
                // Both halves were empty: drop the left one too.
                let gone = p.remove(l);
                if sp.decref_node(gone.bytenr)? {
                    self.dirty.remove(&gone.bytenr);
                    self.cache.remove(&gone.bytenr);
                }
            }
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::GROUP_BLOCKS;
    use alloc::collections::BTreeMap;
    use core::cell::UnsafeCell;

    struct Ram(UnsafeCell<Vec<u8>>);
    unsafe impl Sync for Ram {}
    unsafe impl Send for Ram {}
    impl BlockDevice for Ram {
        fn read_at(&self, o: u64, buf: &mut [u8]) -> Result<()> {
            let d = unsafe { &*self.0.get() };
            buf.copy_from_slice(&d[o as usize..o as usize + buf.len()]);
            Ok(())
        }
        fn write_at(&self, o: u64, buf: &[u8]) -> Result<()> {
            let d = unsafe { &mut *self.0.get() };
            d[o as usize..o as usize + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn sync(&self) -> Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            unsafe { &*self.0.get() }.len() as u64
        }
    }

    /// Bump allocator with explicit refcounts, enough to test sharing.
    #[derive(Default)]
    struct TestSpace {
        next: u64,
        refs: BTreeMap<u64, u32>,
        freed: Vec<u64>,
        data_refs: BTreeMap<u64, i64>,
    }

    impl Space for TestSpace {
        fn alloc_node(&mut self, _goal: u64) -> Result<u64> {
            self.next += 1;
            assert!(self.next < GROUP_BLOCKS);
            self.refs.insert(self.next + 2, 1);
            Ok(self.next + 2)
        }
        fn refs(&self, b: u64) -> u32 {
            self.refs[&b]
        }
        fn incref(&mut self, s: u64, l: u64) -> Result<()> {
            for b in s..s + l {
                match self.refs.get_mut(&b) {
                    Some(c) => *c += 1,
                    None => *self.data_refs.entry(b).or_insert(1) += 1,
                }
            }
            Ok(())
        }
        fn decref_node(&mut self, b: u64) -> Result<bool> {
            let c = self.refs.get_mut(&b).unwrap();
            assert!(*c > 0, "double free of {}", b);
            *c -= 1;
            if *c == 0 {
                self.freed.push(b);
            }
            Ok(*c == 0)
        }
        fn decref_data(&mut self, s: u64, l: u64) -> Result<()> {
            for b in s..s + l {
                *self.data_refs.entry(b).or_insert(1) -= 1;
            }
            Ok(())
        }
    }

    fn setup() -> (Store, TestSpace, Root) {
        let dev = Arc::new(Ram(UnsafeCell::new(vec![0u8; 40000 * BS])));
        let mut st = Store::new(dev, [9; 16], 1, 40000, |k, d| {
            (k.ty == 108).then(|| (le64(d, 0), le64(d, 8)))
        });
        let mut sp = TestSpace::default();
        let root = st.create(&mut sp, 5, 0).unwrap();
        (st, sp, root)
    }

    fn val(i: u64, len: usize) -> Vec<u8> {
        (0..len).map(|j| (i as usize * 7 + j) as u8).collect()
    }

    fn key(i: u64) -> Key {
        Key::new(i / 10, (i % 3) as u8, i * 31)
    }

    fn check_all(st: &mut Store, root: &Root, model: &BTreeMap<Key, Vec<u8>>) {
        let all = st.range(root, &Key::MIN, &Key::MAX, usize::MAX).unwrap();
        let want: Vec<(Key, Vec<u8>)> = model.iter().map(|(k, v)| (*k, v.clone())).collect();
        assert_eq!(all, want);
    }

    #[test]
    fn node_roundtrip_and_checks() {
        let n = Node {
            owner: 5,
            gen: 9,
            level: 0,
            body: Body::Leaf(vec![
                (Key::new(1, 1, 0), vec![1, 2, 3]),
                (Key::new(1, 2, 0), vec![]),
                (Key::new(2, 0, 7), vec![9; 100]),
            ]),
        };
        let b = n.encode(77, &[3; 16]);
        assert_eq!(Node::decode(&b, 77, &[3; 16]).unwrap(), n);
        assert!(Node::decode(&b, 78, &[3; 16]).is_err());
        assert!(Node::decode(&b, 77, &[4; 16]).is_err());
        for at in [0, 5, 20, 40, 70, 4095] {
            let mut c = b.clone();
            c[at] ^= 1;
            assert!(Node::decode(&c, 77, &[3; 16]).is_err(), "byte {}", at);
        }
        let i = Node {
            owner: 5,
            gen: 9,
            level: 2,
            body: Body::Internal(vec![
                Ptr {
                    key: Key::new(1, 0, 0),
                    bytenr: 10,
                    gen: 8,
                },
                Ptr {
                    key: Key::new(5, 0, 0),
                    bytenr: 11,
                    gen: 9,
                },
            ]),
        };
        let b = i.encode(78, &[3; 16]);
        assert_eq!(Node::decode(&b, 78, &[3; 16]).unwrap(), i);
    }

    #[test]
    fn decode_rejects_bad_structure() {
        let mut n = Node::empty_leaf(1, 1);
        n.body = Body::Leaf(vec![
            (Key::new(2, 0, 0), vec![1]),
            (Key::new(1, 0, 0), vec![2]),
        ]);
        let b = n.encode(5, &[0; 16]);
        assert_eq!(
            Node::decode(&b, 5, &[0; 16]).unwrap_err(),
            Error::Corrupt("tree leaf key order")
        );
        let mut i = Node::empty_leaf(1, 1);
        i.level = 1;
        i.body = Body::Internal(vec![]);
        let b = i.encode(5, &[0; 16]);
        assert!(Node::decode(&b, 5, &[0; 16]).is_err());
    }

    #[test]
    fn many_inserts_split_and_stay_sorted() {
        let (mut st, mut sp, mut root) = setup();
        let mut model = BTreeMap::new();
        for i in 0..5000u64 {
            let k = key((i * 7919) % 5000);
            let v = val(i, (i % 50) as usize);
            st.insert(&mut sp, &mut root, k, v.clone()).unwrap();
            model.insert(k, v);
        }
        assert!(root.level >= 1);
        check_all(&mut st, &root, &model);
        for (k, v) in model.iter().step_by(37) {
            assert_eq!(st.get(&root, k).unwrap().as_ref(), Some(v));
        }
        assert_eq!(
            st.insert(&mut sp, &mut root, key(3), vec![]).unwrap_err(),
            Error::Exists
        );
    }

    #[test]
    fn seek_and_range_bounds() {
        let (mut st, mut sp, mut root) = setup();
        for i in (0..2000u64).step_by(2) {
            st.insert(&mut sp, &mut root, Key::new(1, 1, i), val(i, 30))
                .unwrap();
        }
        assert_eq!(
            st.seek_le(&root, &Key::new(1, 1, 0))
                .unwrap()
                .unwrap()
                .0
                .offset,
            0
        );
        assert_eq!(
            st.seek_le(&root, &Key::new(1, 1, 999))
                .unwrap()
                .unwrap()
                .0
                .offset,
            998
        );
        assert_eq!(st.seek_le(&root, &Key::new(1, 0, 5)).unwrap(), None);
        assert_eq!(
            st.seek_le(&root, &Key::MAX).unwrap().unwrap().0.offset,
            1998
        );
        let r = st
            .range(
                &root,
                &Key::new(1, 1, 501),
                &Key::new(1, 1, 600),
                usize::MAX,
            )
            .unwrap();
        assert_eq!(r.len(), 50);
        assert_eq!(r[0].0.offset, 502);
        assert_eq!(r[49].0.offset, 600);
        let r = st.range(&root, &Key::MIN, &Key::MAX, 7).unwrap();
        assert_eq!(r.len(), 7);
        assert!(st
            .range(&root, &Key::MAX, &Key::MIN, 10)
            .unwrap()
            .is_empty());
        assert!(st.get(&root, &Key::new(1, 1, 3)).unwrap().is_none());
    }

    #[test]
    fn random_ops_against_a_model_with_commits() {
        let (mut st, mut sp, mut root) = setup();
        let mut model: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        let mut x: u64 = 12345;
        let mut rnd = |n: u64| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) % n
        };
        for step in 0..20000u64 {
            let k = key(rnd(3000));
            match rnd(10) {
                0..=3 => {
                    let v = val(step, rnd(300) as usize);
                    let r = st.insert(&mut sp, &mut root, k, v.clone());
                    if let alloc::collections::btree_map::Entry::Vacant(e) = model.entry(k) {
                        r.unwrap();
                        e.insert(v);
                    } else {
                        assert_eq!(r.unwrap_err(), Error::Exists);
                    }
                }
                4 | 5 => {
                    let v = val(step, rnd(MAX_ITEM as u64) as usize);
                    st.upsert(&mut sp, &mut root, k, v.clone()).unwrap();
                    model.insert(k, v);
                }
                _ => {
                    let r = st.delete(&mut sp, &mut root, k);
                    match model.remove(&k) {
                        Some(v) => assert_eq!(r.unwrap(), v),
                        None => assert_eq!(r.unwrap_err(), Error::NotFound),
                    }
                }
            }
            if step % 997 == 0 {
                // Commit: everything written, then read back from "disk".
                for (b, img) in st.take_dirty() {
                    st.dev.write_at(b * BLOCK_SIZE, &img).unwrap();
                }
                st.cache.clear();
                check_all(&mut st, &root, &model);
            }
        }
        check_all(&mut st, &root, &model);
        // Delete everything: the tree shrinks back to one leaf.
        let keys: Vec<Key> = model.keys().copied().collect();
        for k in keys {
            st.delete(&mut sp, &mut root, k).unwrap();
        }
        assert_eq!(root.level, 0);
        assert!(st
            .range(&root, &Key::MIN, &Key::MAX, 10)
            .unwrap()
            .is_empty());
        // Only the root is still owned.
        let live: Vec<u64> = sp
            .refs
            .iter()
            .filter(|(_, &c)| c > 0)
            .map(|(&b, _)| b)
            .collect();
        assert_eq!(live, vec![root.bytenr]);
    }

    #[test]
    fn committed_nodes_are_never_written_again() {
        let (mut st, mut sp, mut root) = setup();
        for i in 0..3000u64 {
            st.insert(&mut sp, &mut root, key(i), val(i, 40)).unwrap();
        }
        let committed: BTreeSet<u64> = st.take_dirty().into_iter().map(|(b, _)| b).collect();
        for i in 0..3000u64 {
            st.upsert(&mut sp, &mut root, key(i), val(i + 1, 40))
                .unwrap();
        }
        let next: BTreeSet<u64> = st.take_dirty().into_iter().map(|(b, _)| b).collect();
        assert!(committed.is_disjoint(&next));
        // Every committed node was released when it was copied.
        for b in &committed {
            assert_eq!(sp.refs[b], 0);
        }
    }

    #[test]
    fn shared_tree_copies_on_write_and_release() {
        let (mut st, mut sp, mut a) = setup();
        let mut model = BTreeMap::new();
        for i in 0..3000u64 {
            let mut v = val(i, 16);
            if i % 10 == 0 {
                // An item pointing at data blocks.
                v[..8].copy_from_slice(&(100_000 + i).to_le_bytes());
                v[8..16].copy_from_slice(&1u64.to_le_bytes());
                st.insert(&mut sp, &mut a, Key::new(i, 108, 0), v.clone())
                    .unwrap();
                model.insert(Key::new(i, 108, 0), v);
            } else {
                st.insert(&mut sp, &mut a, key(i * 10), v.clone()).unwrap();
                model.insert(key(i * 10), v);
            }
        }
        st.take_dirty();
        // Snapshot: b shares a's root.
        st.share(&mut sp, a).unwrap();
        let mut b = a;
        // Change b; a must not see it.
        st.upsert(&mut sp, &mut b, Key::new(5, 108, 0), {
            let mut v = vec![0u8; 16];
            v[..8].copy_from_slice(&200_000u64.to_le_bytes());
            v[8..16].copy_from_slice(&1u64.to_le_bytes());
            v
        })
        .unwrap();
        st.delete(&mut sp, &mut b, key(10)).unwrap();
        assert_ne!(a.bytenr, b.bytenr);
        check_all(&mut st, &a, &model);
        let mut mb = model.clone();
        mb.remove(&key(10));
        mb.insert(
            Key::new(5, 108, 0),
            st.get(&b, &Key::new(5, 108, 0)).unwrap().unwrap(),
        );
        check_all(&mut st, &b, &mb);
        // Data blocks referenced by the copied leaf gained an owner.
        assert!(sp.data_refs.values().any(|&c| c == 2));
        // Dropping a frees only what b does not share.
        st.release(&mut sp, a).unwrap();
        check_all(&mut st, &b, &mb);
        // Dropping b too frees every node and every data reference.
        st.release(&mut sp, b).unwrap();
        assert!(sp.refs.values().all(|&c| c == 0));
        assert!(sp.data_refs.values().all(|&c| c == 0), "{:?}", sp.data_refs);
    }

    #[test]
    fn oversized_items_are_refused() {
        let (mut st, mut sp, mut root) = setup();
        assert_eq!(
            st.insert(&mut sp, &mut root, Key::MIN, vec![0; MAX_ITEM + 1])
                .unwrap_err(),
            Error::Invalid
        );
        // Big items at the limit still split cleanly.
        for i in 0..200u64 {
            st.insert(&mut sp, &mut root, key(i), val(i, MAX_ITEM))
                .unwrap();
        }
        let all = st.range(&root, &Key::MIN, &Key::MAX, usize::MAX).unwrap();
        assert_eq!(all.len(), 200);
    }
}
