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
use crate::journal::Spin;
use crate::volume::Volume;
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

/// Nodes the shared lookups ([`Store::get_shared`]) keep for themselves.
const SHARED_NODES: usize = 512;

/// Most nodes one [`Store::trim`] may drop.
///
/// `trim` used to take the cache from its high-water mark straight down to
/// `CACHE_NODES * 3 / 4`, which from the trigger in [`Store::load`] is 10 240
/// nodes in one call. A node is a decoded leaf whose every item owns its own
/// `Vec<u8>`, so that sweep is on the order of a *million* `dealloc`s -- and
/// `EclipseMountFs` holds its mount lock, a ticket spinlock taken with
/// interrupts off, across the whole of it. A capture from real hardware
/// (16-oct-2026, build `d5b176be0f`) caught exactly that: the holder of
/// `eclipsefs_mount.rs:512` spinning in
/// `TicketMutex<Heap<32>>::lock` inside `dealloc`, called from
/// `trim` <- `load` <- `range_rec`, with three other cores piled up in
/// `stat`, and the kernel's >8 s deadlock detector firing on it. The same
/// boot's `[heap-pressure]` line read `32B x 1064172` live -- the burst and
/// the blocks it frees are the same million.
///
/// Bounding the sweep makes the cache converge over successive loads instead
/// of in one stall. A load inserts at most one node and may drop this many,
/// so the cache still cannot run away past [`CACHE_NODES`].
const TRIM_BATCH: usize = 64;

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

impl Body {
    fn ptrs(&self) -> Option<&Vec<Ptr>> {
        match self {
            Body::Internal(p) => Some(p),
            Body::Leaf(_) => None,
        }
    }

    fn ptrs_mut(&mut self) -> Option<&mut Vec<Ptr>> {
        match self {
            Body::Internal(p) => Some(p),
            Body::Leaf(_) => None,
        }
    }

    /// Append a right sibling's contents.
    fn append(&mut self, other: Body) -> Result<()> {
        match (self, other) {
            (Body::Leaf(x), Body::Leaf(y)) => x.extend(y),
            (Body::Internal(x), Body::Internal(y)) => x.extend(y),
            _ => return Err(Error::Corrupt("tree sibling levels")),
        }
        Ok(())
    }
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
    /// Whether `n` more nodes can be allocated now (making room for them
    /// if that is possible). A modification checks this first, so that it
    /// fails on a full device before it changes anything.
    fn node_room(&mut self, _n: u64) -> bool {
        true
    }
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
    /// Where the next bounded [`Store::trim`] resumes its sweep.
    ///
    /// Without it a bounded trim would take the first `TRIM_BATCH` clean keys
    /// of the map every time, i.e. always the lowest block numbers -- which in
    /// a tree are the oldest-allocated nodes, the ones nearest the root and
    /// the likeliest to be wanted on the very next descent. Evicting them on
    /// every load and reading them straight back is a thrash the unbounded
    /// sweep never had, because it ran rarely. Resuming where the last sweep
    /// stopped, and wrapping, spreads eviction over the whole cache instead.
    trim_cursor: u64,
    /// Nodes created in this transaction (written at commit).
    dirty: BTreeSet<u64>,
    extent_of: ExtentOf,
    /// Copies to fall back on when a node fails to verify (volumes), and
    /// whether to write the good copy back over the bad ones.
    pub mirrors: Option<Arc<Volume>>,
    pub repair: bool,
    /// Nodes read by the shared lookups, which take `&self` and so cannot
    /// fill `cache`. Only committed nodes land here (a node of the open
    /// transaction is dirty, and dirty nodes never leave `cache`), and each
    /// is checked against the generation its parent points at before use:
    /// a block freed and reused since holds a node of a later generation.
    shared: Spin<BTreeMap<u64, Node>>,
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
            trim_cursor: 0,
            dirty: BTreeSet::new(),
            extent_of,
            mirrors: None,
            repair: false,
            shared: Spin::new(BTreeMap::new()),
        }
    }

    /// Read nodes through `dev` from now on.
    pub fn set_device(&mut self, dev: Arc<dyn BlockDevice>) {
        self.dev = dev;
    }

    /// The node at `bytenr` from the device, checked against what points at
    /// it; on a volume, from another copy if this one is bad (repairing
    /// the bad ones if allowed).
    fn read_node(&self, bytenr: u64, level: u8, gen: Option<u64>) -> Result<Node> {
        self.read_node_with(bytenr, level, gen, self.repair)
    }

    fn read_node_with(
        &self,
        bytenr: u64,
        level: u8,
        gen: Option<u64>,
        repair: bool,
    ) -> Result<Node> {
        let check = |buf: &[u8]| -> Result<Node> {
            let node = Node::decode(buf, bytenr, &self.fsid)?;
            if node.level != level || gen.is_some_and(|g| g != node.gen) {
                return Err(Error::Corrupt("tree node level or generation"));
            }
            Ok(node)
        };
        let mut buf = vec![0u8; BS];
        let first = self
            .dev
            .read_at(bytenr * BLOCK_SIZE, &mut buf)
            .and_then(|_| check(&buf));
        let (err, vol) = match (first, &self.mirrors) {
            (Ok(n), _) => return Ok(n),
            (Err(e), None) => return Err(e),
            (Err(e), Some(v)) => (e, v),
        };
        let mut good = None;
        let mut bad = Vec::new();
        for k in 0..vol.copies(bytenr) {
            let mut img = vec![0u8; BS];
            match vol
                .read_copy(bytenr * BLOCK_SIZE, &mut img, k)
                .and_then(|_| check(&img))
            {
                Ok(n) if good.is_none() => good = Some((n, img)),
                Ok(_) => {}
                Err(_) => bad.push(k),
            }
        }
        let (node, img) = good.ok_or(err)?;
        warn!(
            "eclipsefs: tree node {} bad in {} cop{}, read from another",
            bytenr,
            bad.len(),
            if bad.len() == 1 { "y" } else { "ies" }
        );
        if repair {
            for k in bad {
                let _ = vol.write_copy(bytenr * BLOCK_SIZE, &img, k);
            }
        }
        Ok(node)
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

    /// Drop up to [`TRIM_BATCH`] clean nodes, resuming the sweep where the
    /// last call left off.
    ///
    /// Cheap enough to call on every load: under [`CACHE_NODES`] it is one
    /// length comparison. Over it, the work -- and the `dealloc`s it causes
    /// under the mount's interrupts-off spinlock -- is bounded, which is the
    /// whole point (see [`TRIM_BATCH`]).
    fn trim(&mut self) {
        if self.cache.len() <= CACHE_NODES {
            return;
        }
        let want = (self.cache.len() - CACHE_NODES * 3 / 4).min(TRIM_BATCH);
        let cursor = self.trim_cursor;
        let dirty = &self.dirty;
        let mut clean: Vec<u64> = self
            .cache
            .range(cursor..)
            .map(|(&b, _)| b)
            .filter(|b| !dirty.contains(b))
            .take(want)
            .collect();
        // Wrap: the tail from the cursor on may hold fewer than `want` clean
        // nodes (or none, once the cursor has passed the last key), and a
        // sweep that stopped there would leave the cache over its budget for
        // ever with the cursor never advancing.
        if clean.len() < want {
            let rest = want - clean.len();
            clean.extend(
                self.cache
                    .range(..cursor)
                    .map(|(&b, _)| b)
                    .filter(|b| !dirty.contains(b))
                    .take(rest),
            );
        }
        // Past the last key dropped, so the next sweep carries on rather than
        // re-walking what is already gone. Nothing dropped means every node
        // from here on is dirty: start over from the beginning next time.
        self.trim_cursor = clean.last().map_or(0, |b| b.saturating_add(1));
        for b in clean {
            self.cache.remove(&b);
        }
    }

    /// Drop every cached node that is not waiting to be written, and the
    /// shared side cache.
    pub fn drop_clean(&mut self) {
        let dirty = &self.dirty;
        self.cache.retain(|b, _| dirty.contains(b));
        self.trim_cursor = 0;
        self.shared.with(|m| m.clear());
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
            // Unconditional, and no longer only past twice the budget: the
            // sweep is bounded now, so it has to run often enough to keep up
            // with the inserts below. It costs one comparison while the cache
            // is within budget.
            self.trim();
            let node = self.read_node(bytenr, level, gen)?;
            self.cache.insert(bytenr, node);
        }
        let node = &self.cache[&bytenr];
        if node.level != level || gen.is_some_and(|g| g != node.gen) {
            return Err(Error::Corrupt("tree node level or generation"));
        }
        Ok(())
    }

    /// Decode the image of the node at `bytenr` and check it against what
    /// points at it.
    pub fn check_node(&self, img: &[u8], bytenr: u64, level: u8, gen: Option<u64>) -> Result<Node> {
        if bytenr < 2 || bytenr >= self.total_blocks {
            return Err(Error::Corrupt("tree pointer out of range"));
        }
        let node = Node::decode(img, bytenr, &self.fsid)?;
        if node.level != level || gen.is_some_and(|g| g != node.gen) {
            return Err(Error::Corrupt("tree node level or generation"));
        }
        Ok(node)
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

    /// How many new blocks copying every node of the tree would take (the
    /// most a transaction can spend on it: a node is copied once), counting
    /// up to `cap`.
    pub fn cow_cost(&mut self, sp: &dyn Space, root: &Root, cap: u64) -> Result<u64> {
        let mut n = 0;
        let mut todo = vec![(root.bytenr, root.level)];
        while let Some((b, level)) = todo.pop() {
            if n >= cap {
                break;
            }
            self.load(b, level, None)?;
            if !self.dirty.contains(&b) || sp.refs(b) > 1 {
                n += 1;
            }
            if let Body::Internal(ptrs) = &self.cache[&b].body {
                let level = level.checked_sub(1).ok_or(Error::Corrupt("tree level"))?;
                todo.extend(ptrs.iter().map(|p| (p.bytenr, level)));
            }
        }
        Ok(n.min(cap))
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

    /// Copy every node of the tree that `moving` picks, with the path
    /// above it, as a change under it would: top-down, so a shared node is
    /// copied for this tree only and stays shared below the copy. Returns
    /// whether anything was copied.
    pub fn relocate(
        &mut self,
        sp: &mut dyn Space,
        root: &mut Root,
        moving: &dyn Fn(u64) -> bool,
    ) -> Result<bool> {
        let mut need = BTreeSet::new();
        if !self.mark_moving(root.bytenr, root.level, None, moving, &mut need)? {
            return Ok(false);
        }
        let new = self.cow(sp, root.bytenr, root.level)?;
        root.bytenr = new;
        root.gen = self.gen;
        self.relocate_below(sp, new, root.level, &need)?;
        Ok(true)
    }

    /// Put in `need` every node at or below `b` that moves or has one that
    /// does below it.
    fn mark_moving(
        &mut self,
        b: u64,
        level: u8,
        gen: Option<u64>,
        moving: &dyn Fn(u64) -> bool,
        need: &mut BTreeSet<u64>,
    ) -> Result<bool> {
        self.load(b, level, gen)?;
        let mut any = moving(b);
        if let Body::Internal(ptrs) = &self.cache[&b].body {
            if level == 0 {
                return Err(Error::Corrupt("tree level"));
            }
            for p in ptrs.clone() {
                any |= self.mark_moving(p.bytenr, level - 1, Some(p.gen), moving, need)?;
            }
        }
        if any {
            need.insert(b);
        }
        Ok(any)
    }

    fn relocate_below(
        &mut self,
        sp: &mut dyn Space,
        b: u64,
        level: u8,
        need: &BTreeSet<u64>,
    ) -> Result<()> {
        let ptrs = match &self.cache[&b].body {
            Body::Internal(ptrs) => ptrs.clone(),
            Body::Leaf(_) => return Ok(()),
        };
        for (i, p) in ptrs.iter().enumerate() {
            if !need.contains(&p.bytenr) {
                continue;
            }
            let child = self.cow(sp, p.bytenr, level - 1)?;
            let gen = self.gen;
            if let Body::Internal(ptrs) = &mut self.cache.get_mut(&b).unwrap().body {
                ptrs[i].bytenr = child;
                ptrs[i].gen = gen;
            }
            self.relocate_below(sp, child, level - 1, need)?;
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

    // -------------------------------------------------------------------
    // Shared lookups: `&self`, for readers that run side by side
    // -------------------------------------------------------------------

    /// Run `f` on the node at `bytenr` without touching `cache`: from it if
    /// it is there, else from the shared side cache or the device. Never
    /// writes: a bad copy is read around, not repaired.
    fn with_node_shared<R>(
        &self,
        bytenr: u64,
        level: u8,
        gen: Option<u64>,
        f: impl FnOnce(&Node) -> Result<R>,
    ) -> Result<R> {
        let fits = |n: &Node| n.level == level && gen.is_none_or(|g| g == n.gen);
        if let Some(n) = self.cache.get(&bytenr) {
            if !fits(n) {
                return Err(Error::Corrupt("tree node level or generation"));
            }
            return f(n);
        }
        if bytenr < 2 || bytenr >= self.total_blocks {
            return Err(Error::Corrupt("tree pointer out of range"));
        }
        // A root has no parent to say its generation; it is read afresh.
        if gen.is_some() {
            let hit = self
                .shared
                .with(|m| m.get(&bytenr).filter(|n| fits(n)).cloned());
            if let Some(n) = hit {
                return f(&n);
            }
        }
        let n = self.read_node_with(bytenr, level, gen, false)?;
        let r = f(&n);
        if gen.is_some() {
            self.shared.with(|m| {
                if m.len() >= SHARED_NODES {
                    m.pop_first();
                }
                m.insert(bytenr, n);
            });
        }
        r
    }

    /// [`get`](Self::get) through `&self`.
    pub fn get_shared(&self, root: &Root, key: &Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .seek_le_shared(root, key)?
            .filter(|(k, _)| k == key)
            .map(|(_, d)| d))
    }

    /// [`seek_le`](Self::seek_le) through `&self`.
    pub fn seek_le_shared(&self, root: &Root, key: &Key) -> Result<Option<(Key, Vec<u8>)>> {
        enum Step {
            Found(Option<(Key, Vec<u8>)>),
            Down(u64, u64),
        }
        let mut b = root.bytenr;
        let mut level = root.level;
        let mut gen = None;
        loop {
            let step = self.with_node_shared(b, level, gen, |n| {
                Ok(match &n.body {
                    Body::Leaf(items) => {
                        let i = items.partition_point(|(k, _)| k <= key);
                        Step::Found((i > 0).then(|| items[i - 1].clone()))
                    }
                    Body::Internal(ptrs) => {
                        let i = ptrs.partition_point(|p| p.key <= *key);
                        if i == 0 {
                            Step::Found(None)
                        } else {
                            Step::Down(ptrs[i - 1].bytenr, ptrs[i - 1].gen)
                        }
                    }
                })
            })?;
            match step {
                Step::Found(r) => return Ok(r),
                Step::Down(nb, ng) => {
                    if level == 0 {
                        return Err(Error::Corrupt("tree node level or generation"));
                    }
                    b = nb;
                    gen = Some(ng);
                    level -= 1;
                }
            }
        }
    }

    /// [`range`](Self::range) through `&self`.
    pub fn range_shared(
        &self,
        root: &Root,
        lo: &Key,
        hi: &Key,
        limit: usize,
    ) -> Result<Vec<(Key, Vec<u8>)>> {
        let mut out = Vec::new();
        if lo <= hi && limit > 0 {
            self.range_shared_rec(root.bytenr, root.level, None, lo, hi, limit, &mut out)?;
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn range_shared_rec(
        &self,
        b: u64,
        level: u8,
        gen: Option<u64>,
        lo: &Key,
        hi: &Key,
        limit: usize,
        out: &mut Vec<(Key, Vec<u8>)>,
    ) -> Result<bool> {
        let ptrs = self.with_node_shared(b, level, gen, |n| match &n.body {
            Body::Leaf(items) => {
                let start = items.partition_point(|(k, _)| k < lo);
                for (k, d) in &items[start..] {
                    if k > hi {
                        return Ok(None);
                    }
                    out.push((*k, d.clone()));
                    if out.len() >= limit {
                        return Ok(None);
                    }
                }
                Ok(Some(Vec::new()))
            }
            Body::Internal(ptrs) => {
                let first = ptrs.partition_point(|p| p.key <= *lo).saturating_sub(1);
                Ok(Some(ptrs[first..].to_vec()))
            }
        })?;
        // `None`: done; an empty list: a leaf that ran out before `hi`.
        let Some(ptrs) = ptrs else {
            return Ok(true);
        };
        for p in ptrs {
            if p.key > *hi {
                return Ok(true);
            }
            if level == 0 {
                return Err(Error::Corrupt("tree node level or generation"));
            }
            if self.range_shared_rec(p.bytenr, level - 1, Some(p.gen), lo, hi, limit, out)? {
                return Ok(true);
            }
        }
        Ok(false)
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
        let old_len = self.get(root, &key)?.map(|d| d.len());
        match (&op, old_len) {
            (Op::Insert(_), Some(_)) => return Err(Error::Exists),
            (Op::Update(_) | Op::Delete, None) => return Err(Error::NotFound),
            _ => {}
        }
        // Or when the device cannot hold the nodes it may take: a split
        // that fails half-way would leave the change in an overfull node.
        let grows = match (&op, old_len) {
            (Op::Delete, _) => false,
            (Op::Insert(d) | Op::Update(d) | Op::Upsert(d), old) => old.is_none_or(|l| d.len() > l),
        };
        let need = self.nodes_needed(sp, root, &key, grows)?;
        if !sp.node_room(need) {
            return Err(Error::NoSpace);
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

    /// Most nodes a modification at `key` allocates: a copy of each node on
    /// the path that is not already a private copy of this transaction,
    /// plus, when the item grows, a split at every level and a new root.
    /// (Merges after a shrink are skipped when there is no room for them.)
    fn nodes_needed(&mut self, sp: &dyn Space, root: &Root, key: &Key, grows: bool) -> Result<u64> {
        let mut need = 0;
        let (mut b, mut level, mut gen) = (root.bytenr, root.level, None);
        loop {
            self.load(b, level, gen)?;
            if !self.dirty.contains(&b) || sp.refs(b) > 1 {
                need += 1;
            }
            match &self.cache[&b].body {
                Body::Leaf(_) => break,
                Body::Internal(ptrs) => {
                    let i = ptrs.partition_point(|p| p.key <= *key).saturating_sub(1);
                    b = ptrs[i].bytenr;
                    gen = Some(ptrs[i].gen);
                    level = level.checked_sub(1).ok_or(Error::Corrupt("tree level"))?;
                }
            }
        }
        let levels = root.level as u64 + 1;
        if grows {
            need += levels + 1;
        }
        Ok(need)
    }

    /// The pointers of the cached internal node `b`, which the caller has
    /// already been through as one.
    fn internal_mut(&mut self, b: u64) -> &mut Vec<Ptr> {
        self.cache
            .get_mut(&b)
            .and_then(|n| n.body.ptrs_mut())
            .expect("cached internal node")
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
            let ptrs = self.internal_mut(b);
            ptrs[idx].bytenr = child;
            ptrs[idx].gen = gen;
        }
        let out = r?;
        {
            let ptrs = self.internal_mut(b);
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
            // Merging copies the sibling first, before it changes anything:
            // on a full device the node just stays underfull.
            match self.merge_child(sp, b, level, idx) {
                Err(Error::NoSpace) => {}
                r => r?,
            }
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
        let ptrs = self.cache[&parent]
            .body
            .ptrs()
            .expect("merge under a leaf")
            .clone();
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
        self.cache.get_mut(&lb).unwrap().body.append(right.body)?;
        let first = self.cache[&lb].first_key();
        let gen = self.gen;
        let p = self.internal_mut(parent);
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
#[cfg_attr(coverage_nightly, coverage(off))]
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

    /// A store whose cache holds `n` synthetic clean nodes, keyed 1..=n.
    fn store_with_cached_nodes(n: u64) -> Store {
        let dev = Arc::new(Ram(UnsafeCell::new(vec![0u8; BS])));
        let mut st = Store::new(dev, [9; 16], 1, n + 1, |_, _| None);
        for b in 1..=n {
            st.cache.insert(b, Node::empty_leaf(5, 1));
        }
        st
    }

    /// The sweep is bounded, because the whole of it runs under the mount's
    /// interrupts-off spinlock. An unbounded one took the cache from twice
    /// the budget down to three quarters of it in one call -- 10 240 nodes,
    /// a million `dealloc`s -- and wedged the machine for longer than the
    /// kernel's 8 s deadlock detector will wait.
    #[test]
    fn one_trim_drops_at_most_a_batch() {
        let n = (CACHE_NODES * 2) as u64;
        let mut st = store_with_cached_nodes(n);
        st.trim();
        assert_eq!(
            st.cache.len(),
            n as usize - TRIM_BATCH,
            "un solo barrido no puede soltar mas de un lote"
        );
        // Under the budget it is a comparison and nothing else.
        let mut small = store_with_cached_nodes(CACHE_NODES as u64);
        small.trim();
        assert_eq!(small.cache.len(), CACHE_NODES);
        assert_eq!(small.trim_cursor, 0, "no barrio nada, no movio el cursor");
    }

    /// Successive sweeps move on instead of re-taking the lowest keys, which
    /// in a tree are the oldest nodes -- nearest the root, and the first
    /// wanted again on the next descent.
    #[test]
    fn successive_trims_sweep_forward_instead_of_eating_the_lowest_keys() {
        let n = (CACHE_NODES * 2) as u64;
        let mut st = store_with_cached_nodes(n);
        st.trim();
        let first = TRIM_BATCH as u64 + 1;
        assert_eq!(
            st.cache.keys().next().copied(),
            Some(first),
            "el primer barrido se lleva el primer lote de claves"
        );
        assert_eq!(st.trim_cursor, first);
        st.trim();
        // The second sweep took the NEXT batch, so the keys just below the
        // first cursor are still gone and the ones above it have now gone too.
        assert_eq!(st.cache.len(), n as usize - 2 * TRIM_BATCH);
        assert_eq!(st.trim_cursor, 2 * TRIM_BATCH as u64 + 1);
        assert!(st.cache.contains_key(&(2 * TRIM_BATCH as u64 + 1)));
    }

    /// Bounding the sweep must not stop it getting the cache back inside its
    /// budget; it only spreads the same work over more calls.
    #[test]
    fn repeated_trims_bring_the_cache_back_within_budget() {
        let n = (CACHE_NODES * 2) as u64;
        let mut st = store_with_cached_nodes(n);
        let mut sweeps = 0;
        while st.cache.len() > CACHE_NODES && sweeps < 10_000 {
            st.trim();
            sweeps += 1;
        }
        assert!(
            st.cache.len() <= CACHE_NODES,
            "la cache se quedo en {} nodos tras {} barridos",
            st.cache.len(),
            sweeps
        );
    }

    /// Once the cursor is past the last key, a sweep that did not wrap would
    /// find nothing above it, drop nothing, and leave the cache over budget
    /// for ever.
    #[test]
    fn a_cursor_past_the_last_key_wraps_instead_of_sweeping_nothing() {
        let n = (CACHE_NODES * 2) as u64;
        let mut st = store_with_cached_nodes(n);
        st.trim_cursor = n + 1_000;
        st.trim();
        assert_eq!(
            st.cache.len(),
            n as usize - TRIM_BATCH,
            "con el cursor pasado del final no barrio nada"
        );
        assert_eq!(st.cache.keys().next().copied(), Some(TRIM_BATCH as u64 + 1));
        assert_eq!(st.trim_cursor, TRIM_BATCH as u64 + 1);
    }

    /// The tail from the cursor on can hold *some* clean nodes but fewer than
    /// a batch; the sweep then finishes the batch from the beginning rather
    /// than coming up short every time.
    #[test]
    fn a_short_tail_is_completed_from_the_start_of_the_cache() {
        let n = (CACHE_NODES * 2) as u64;
        let mut st = store_with_cached_nodes(n);
        let tail = (TRIM_BATCH / 4) as u64;
        st.trim_cursor = n - tail + 1;
        st.trim();
        assert_eq!(
            st.cache.len(),
            n as usize - TRIM_BATCH,
            "la cola corta no se completo desde el principio"
        );
        // The tail went, and so did the first keys, enough to make a batch.
        assert!(!st.cache.contains_key(&n));
        assert!(!st.cache.contains_key(&1));
        assert!(st.cache.contains_key(&(TRIM_BATCH as u64 - tail + 1)));
    }

    /// A dirty node is unwritten data: dropping it loses the transaction. The
    /// bound must not turn "skip the dirty ones" into "stop at the first
    /// dirty one".
    #[test]
    fn a_dirty_node_is_never_dropped_however_the_sweep_lands_on_it() {
        let n = (CACHE_NODES * 2) as u64;
        let mut st = store_with_cached_nodes(n);
        // Every key the first sweep would otherwise take.
        for b in 1..=(TRIM_BATCH as u64) {
            st.dirty.insert(b);
        }
        st.trim();
        for b in 1..=(TRIM_BATCH as u64) {
            assert!(st.cache.contains_key(&b), "solto el nodo sucio {}", b);
        }
        assert_eq!(st.cache.len(), n as usize - TRIM_BATCH);

        // All dirty: nothing may be dropped, and the sweep must not wedge.
        let mut all = store_with_cached_nodes((CACHE_NODES * 2) as u64);
        for b in 1..=(CACHE_NODES as u64 * 2) {
            all.dirty.insert(b);
        }
        all.trim();
        assert_eq!(all.cache.len(), CACHE_NODES * 2);
        assert_eq!(
            all.trim_cursor, 0,
            "sin nada que soltar, vuelve al principio"
        );
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

    fn reseal(b: &mut [u8]) {
        let c = crc32c(&b[8..]);
        put32(b, 4, c);
    }

    /// Write every dirty node out and forget the cache, as a commit would.
    fn commit(st: &mut Store) {
        for (b, img) in st.take_dirty() {
            st.dev.write_at(b * BLOCK_SIZE, &img).unwrap();
        }
        st.cache.clear();
    }

    /// Put `node` in the cache at `b` as a node of this transaction.
    fn inject(st: &mut Store, sp: &mut TestSpace, b: u64, node: Node) {
        sp.refs.insert(b, 1);
        st.cache.insert(b, node);
        st.dirty.insert(b);
    }

    fn leaf_node(gen: u64, items: Vec<(Key, Vec<u8>)>) -> Node {
        Node {
            owner: 5,
            gen,
            level: 0,
            body: Body::Leaf(items),
        }
    }

    fn internal_node(gen: u64, level: u8, ptrs: &[(Key, u64)]) -> Node {
        Node {
            owner: 5,
            gen,
            level,
            body: Body::Internal(
                ptrs.iter()
                    .map(|&(key, bytenr)| Ptr { key, bytenr, gen })
                    .collect(),
            ),
        }
    }

    #[test]
    fn an_empty_shared_range_reads_nothing() {
        let (st, _, root) = setup();
        assert!(st
            .range_shared(&root, &Key::MAX, &Key::MIN, 10)
            .unwrap()
            .is_empty());
        assert!(st
            .range_shared(&root, &Key::MIN, &Key::MAX, 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn node_len_and_body_accessors() {
        let mut n = Node::empty_leaf(1, 1);
        assert!(n.is_empty());
        assert!(n.body.ptrs().is_none());
        assert!(n.body.ptrs_mut().is_none());
        let mut i = internal_node(1, 1, &[(Key::MIN, 7)]);
        assert!(!i.is_empty());
        assert_eq!(i.body.ptrs_mut().unwrap().len(), 1);
        assert_eq!(
            n.body.append(i.body.clone()).unwrap_err(),
            Error::Corrupt("tree sibling levels")
        );
    }

    #[test]
    fn decode_rejects_every_kind_of_damage() {
        let fsid = [0; 16];
        let n = leaf_node(1, vec![(Key::new(1, 0, 0), vec![1, 2, 3])]);
        let good = n.encode(5, &fsid);

        let mut b = good.clone();
        b[36] = MAX_LEVEL + 1;
        reseal(&mut b);
        assert_eq!(
            Node::decode(&b, 5, &fsid).unwrap_err(),
            Error::Corrupt("tree node level")
        );

        let mut b = good.clone();
        put32(&mut b, 32, (LEAF_SPACE / LEAF_ITEM + 1) as u32);
        reseal(&mut b);
        assert_eq!(
            Node::decode(&b, 5, &fsid).unwrap_err(),
            Error::Corrupt("tree leaf item count")
        );

        // Data inside the item table, then running off the end of the block.
        for (off, len) in [(0u32, 3u32), ((BS - 2) as u32, 3)] {
            let mut b = good.clone();
            put32(&mut b, NODE_HDR + KEY_SIZE, off);
            put32(&mut b, NODE_HDR + KEY_SIZE + 4, len);
            reseal(&mut b);
            assert_eq!(
                Node::decode(&b, 5, &fsid).unwrap_err(),
                Error::Corrupt("tree leaf item bounds")
            );
        }

        let i = internal_node(1, 1, &[(Key::new(2, 0, 0), 7), (Key::new(1, 0, 0), 8)]);
        assert_eq!(
            Node::decode(&i.encode(5, &fsid), 5, &fsid).unwrap_err(),
            Error::Corrupt("tree node key order")
        );
    }

    /// Every way into the store checks the block number, level and
    /// generation a pointer gives against the node it finds.
    #[test]
    fn the_store_checks_what_pointers_say() {
        let (mut st, mut sp, mut root) = setup();
        st.insert(&mut sp, &mut root, Key::new(1, 0, 0), vec![1])
            .unwrap();
        let img = st.cache[&root.bytenr].encode(root.bytenr, &st.fsid);
        commit(&mut st);
        let range = Error::Corrupt("tree pointer out of range");
        let mismatch = Error::Corrupt("tree node level or generation");

        assert_eq!(st.check_node(&img, 1, 0, None).unwrap_err(), range);
        assert_eq!(st.check_node(&img, 40_000, 0, None).unwrap_err(), range);
        assert_eq!(
            st.check_node(&img, root.bytenr, 1, None).unwrap_err(),
            mismatch
        );
        assert!(st.check_node(&img, root.bytenr, 0, Some(1)).is_ok());
        assert_eq!(st.read_disk(0, 0, None).unwrap_err(), range);
        assert_eq!(st.read_disk(root.bytenr, 0, Some(7)).unwrap_err(), mismatch);
        assert!(st.read_disk(root.bytenr, 0, None).is_ok());

        // From the device, then from the cache.
        assert_eq!(st.node(1, 0, None).unwrap_err(), range);
        assert_eq!(st.node(root.bytenr, 1, None).unwrap_err(), mismatch);
        st.node(root.bytenr, 0, None).unwrap();
        assert_eq!(st.node(root.bytenr, 1, None).unwrap_err(), mismatch);

        // The shared readers, from the cache and from the device.
        let wrong = Root { level: 1, ..root };
        assert_eq!(st.get_shared(&wrong, &Key::MIN).unwrap_err(), mismatch);
        let outside = Root { bytenr: 1, ..root };
        assert_eq!(st.get_shared(&outside, &Key::MIN).unwrap_err(), range);
    }

    /// A key below everything in a tree of more than one level stops at the
    /// first internal node.
    #[test]
    fn shared_seek_below_the_first_key() {
        let (mut st, mut sp, mut root) = setup();
        for i in 1..2000u64 {
            st.insert(&mut sp, &mut root, Key::new(i, 1, 0), val(i, 30))
                .unwrap();
        }
        assert!(root.level >= 1);
        assert_eq!(st.seek_le_shared(&root, &Key::MIN).unwrap(), None);
        commit(&mut st);
        assert_eq!(st.seek_le_shared(&root, &Key::MIN).unwrap(), None);
        assert_eq!(
            st.seek_le_shared(&root, &Key::new(5, 1, 0))
                .unwrap()
                .unwrap()
                .0,
            Key::new(5, 1, 0)
        );
    }

    /// An internal node that says it is a leaf (or the other way round)
    /// stops every walk instead of sending it below level 0.
    #[test]
    fn a_node_at_the_wrong_level_is_corrupt_everywhere() {
        let (mut st, mut sp, root) = setup();
        let gen = st.gen;
        let level = Error::Corrupt("tree level");
        let shared = Error::Corrupt("tree node level or generation");
        // A level-0 node with pointers.
        inject(
            &mut st,
            &mut sp,
            30_000,
            internal_node(gen, 0, &[(Key::MIN, root.bytenr)]),
        );
        let bad0 = Root {
            bytenr: 30_000,
            level: 0,
            gen,
        };
        // A level-1 node with items.
        let mut l = leaf_node(gen, vec![(Key::MIN, vec![1])]);
        l.level = 1;
        inject(&mut st, &mut sp, 30_001, l);
        let bad1 = Root {
            bytenr: 30_001,
            level: 1,
            gen,
        };

        assert_eq!(st.seek_le_shared(&bad0, &Key::MAX).unwrap_err(), shared);
        assert_eq!(
            st.range_shared(&bad0, &Key::MIN, &Key::MAX, 10)
                .unwrap_err(),
            shared
        );
        let mut r = bad0;
        assert_eq!(st.relocate(&mut sp, &mut r, &|_| true).unwrap_err(), level);
        assert_eq!(st.diff(&bad0, &root).unwrap_err(), level);
        assert_eq!(st.diff(&root, &bad1).unwrap_err(), level);
        let mut r = bad1;
        assert_eq!(
            st.upsert(&mut sp, &mut r, Key::new(2, 0, 0), vec![2])
                .unwrap_err(),
            level
        );
        assert_eq!(
            st.modify_leaf(&mut sp, 30_000, Key::MIN, Op::Delete)
                .err()
                .unwrap(),
            level
        );
    }

    /// `modify` checks the key before copying anything; the leaf checks it
    /// again on its own.
    #[test]
    fn the_leaf_checks_presence_too() {
        let (mut st, mut sp, mut root) = setup();
        st.insert(&mut sp, &mut root, Key::new(1, 0, 0), vec![1])
            .unwrap();
        let b = root.bytenr;
        assert_eq!(
            st.modify_leaf(&mut sp, b, Key::new(1, 0, 0), Op::Insert(vec![2]))
                .err()
                .unwrap(),
            Error::Exists
        );
        assert_eq!(
            st.modify_leaf(&mut sp, b, Key::new(2, 0, 0), Op::Delete)
                .err()
                .unwrap(),
            Error::NotFound
        );
        assert_eq!(
            st.modify_leaf(&mut sp, b, Key::new(2, 0, 0), Op::Update(vec![]))
                .err()
                .unwrap(),
            Error::NotFound
        );
    }

    /// The split point moves so that neither half is over a node, however
    /// lopsided the items.
    #[test]
    fn split_keeps_the_left_half_within_a_node() {
        let (mut st, mut sp, _) = setup();
        let gen = st.gen;
        let items = (0..10u64)
            .map(|i| (Key::new(i, 0, 0), val(i, MAX_ITEM)))
            .collect();
        inject(&mut st, &mut sp, 30_000, leaf_node(gen, items));
        let (k, _) = st.split(&mut sp, 30_000).unwrap();
        let Body::Leaf(left) = &st.cache[&30_000].body else {
            panic!()
        };
        assert_eq!(left.len(), 4);
        assert_eq!(k, Key::new(4, 0, 0));
    }

    #[test]
    fn siblings_at_different_levels_do_not_merge() {
        let (mut st, mut sp, _) = setup();
        let gen = st.gen;
        inject(&mut st, &mut sp, 30_000, leaf_node(gen, vec![]));
        inject(
            &mut st,
            &mut sp,
            30_001,
            internal_node(gen, 0, &[(Key::MIN, 3)]),
        );
        inject(
            &mut st,
            &mut sp,
            30_002,
            internal_node(gen, 1, &[(Key::MIN, 30_000), (Key::new(5, 0, 0), 30_001)]),
        );
        assert_eq!(
            st.merge_child(&mut sp, 30_002, 1, 0).unwrap_err(),
            Error::Corrupt("tree sibling levels")
        );
    }

    /// Two empty children merge into nothing: both go, unless they were the
    /// only child left.
    #[test]
    fn empty_siblings_are_dropped_together() {
        for (empties, left) in [(2u64, 1usize), (1, 1)] {
            let (mut st, mut sp, _) = setup();
            let gen = st.gen;
            let mut ptrs = Vec::new();
            for i in 0..empties {
                inject(&mut st, &mut sp, 30_000 + i, leaf_node(gen, vec![]));
                ptrs.push((Key::new(i, 0, 0), 30_000 + i));
            }
            let k = Key::new(10, 0, 0);
            inject(&mut st, &mut sp, 30_010, leaf_node(gen, vec![(k, vec![1])]));
            ptrs.push((k, 30_010));
            inject(&mut st, &mut sp, 30_020, internal_node(gen, 1, &ptrs));
            let mut root = Root {
                bytenr: 30_020,
                level: 1,
                gen,
            };
            assert_eq!(st.delete(&mut sp, &mut root, k).unwrap(), vec![1]);
            // One empty leaf is left, and the root collapsed onto it.
            assert_eq!(root.level, 0);
            assert_eq!(root.bytenr, 30_000);
            // The tree `setup` made is still there too.
            let live = sp.refs.values().filter(|&&c| c > 0).count();
            assert_eq!(live, left + 1);
        }
    }

    /// A tree already MAX_LEVEL high cannot grow another level.
    #[test]
    fn a_tree_at_the_height_limit_refuses_to_grow() {
        let (mut st, mut sp, _) = setup();
        let gen = st.gen;
        let half = vec![0u8; MAX_ITEM];
        inject(
            &mut st,
            &mut sp,
            30_000,
            leaf_node(
                gen,
                vec![(Key::new(1, 0, 0), half.clone()), (Key::new(3, 0, 0), half)],
            ),
        );
        for level in 1..=MAX_LEVEL {
            let b = 30_000 + level as u64;
            let mut ptrs = vec![(Key::MIN, b - 1)];
            for i in 1..MAX_PTRS as u64 {
                ptrs.push((Key::new(1000 + i, 0, 0), 39_000));
            }
            inject(&mut st, &mut sp, b, internal_node(gen, level, &ptrs));
        }
        let mut root = Root {
            bytenr: 30_000 + MAX_LEVEL as u64,
            level: MAX_LEVEL,
            gen,
        };
        assert_eq!(
            st.insert(&mut sp, &mut root, Key::new(2, 0, 0), vec![1])
                .unwrap_err(),
            Error::NoSpace
        );
    }

    /// Items only in the first tree, past the end of the second.
    #[test]
    fn diff_lists_what_only_the_first_tree_has() {
        let (mut st, mut sp, mut a) = setup();
        let mut b = st.create(&mut sp, 5, 0).unwrap();
        for i in 0..10u64 {
            st.insert(&mut sp, &mut a, Key::new(i, 0, 0), vec![i as u8])
                .unwrap();
            if i < 5 {
                st.insert(&mut sp, &mut b, Key::new(i, 0, 0), vec![i as u8])
                    .unwrap();
            }
        }
        let d = st.diff(&a, &b).unwrap();
        assert_eq!(d.len(), 5);
        assert!(d.iter().all(|(_, va, vb)| va.is_some() && vb.is_none()));
    }

    /// A bad leaf in the second tree fails the diff: its internal nodes are
    /// read for the first pass, its leaves only for the second.
    #[test]
    fn diff_fails_on_a_bad_leaf_of_the_second_tree() {
        let (mut st, mut sp, mut a) = setup();
        let mut b = st.create(&mut sp, 5, 0).unwrap();
        st.insert(&mut sp, &mut a, Key::MIN, vec![1]).unwrap();
        for i in 0..500u64 {
            st.insert(&mut sp, &mut b, Key::new(i, 1, 0), val(i, 60))
                .unwrap();
        }
        assert!(b.level >= 1);
        commit(&mut st);
        let Body::Internal(ptrs) = st.node(b.bytenr, b.level, None).unwrap().body.clone() else {
            panic!()
        };
        let leaf = ptrs[1].bytenr;
        st.dev.write_at(leaf * BLOCK_SIZE, &[0xff; 8]).unwrap();
        st.cache.clear();
        assert!(matches!(st.diff(&a, &b), Err(Error::Corrupt(_))));
    }

    /// Merging into a copy while the right sibling is still shared with a
    /// snapshot: what moves gains an owner, so the snapshot keeps it.
    #[test]
    fn merges_with_a_shared_right_sibling_keep_the_snapshot_whole() {
        let (mut st, mut sp, mut a) = setup();
        let mut model = BTreeMap::new();
        for i in 0..12_000u64 {
            let mut v = val(i, 24);
            let k = if i % 4 == 0 {
                v[..8].copy_from_slice(&(100_000 + i).to_le_bytes());
                v[8..16].copy_from_slice(&1u64.to_le_bytes());
                Key::new(i, 108, 0)
            } else {
                Key::new(i, 1, 0)
            };
            st.insert(&mut sp, &mut a, k, v.clone()).unwrap();
            model.insert(k, v);
        }
        assert!(a.level >= 2, "level {}", a.level);
        st.take_dirty();
        st.share(&mut sp, a).unwrap();
        let mut b = a;
        let keys: Vec<Key> = model.keys().copied().collect();
        let mut mb = model.clone();
        // Thin out the tree from the left, a few keys from each leaf and
        // then almost everything, so merges reach the internal levels.
        for (n, k) in keys.iter().enumerate() {
            if n % 8 != 0 {
                let v = st.delete(&mut sp, &mut b, *k).unwrap();
                // The caller drops what a deleted extent item points at.
                if let Some((s, l)) = (st.extent_of)(k, &v) {
                    sp.decref_data(s, l).unwrap();
                }
                mb.remove(k);
            }
        }
        check_all(&mut st, &a, &model);
        check_all(&mut st, &b, &mb);
        st.release(&mut sp, a).unwrap();
        check_all(&mut st, &b, &mb);
        st.release(&mut sp, b).unwrap();
        assert!(sp.refs.values().all(|&c| c == 0));
        assert!(sp.data_refs.values().all(|&c| c == 0));
    }
}
