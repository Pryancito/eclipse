//! Block ownership for the copy-on-write format: allocation, reference
//! counts of shared blocks, and what the open transaction may reuse.
//!
//! A block in use has one owner unless the reference map says otherwise
//! (only shared blocks have an entry, as ranges with a count of 2 or more).
//! Dropping the last owner frees the block: at once if the open transaction
//! allocated it (nothing committed points there), otherwise when the
//! transaction commits, so the committed trees stay intact until the new
//! ones are on disk.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::blocks::{AllocationGroup, BlockAllocator};
use crate::btree::Space;
use crate::format::GROUP_BLOCKS;
use crate::volume::{Chunk, ChunkKind, Profile, Tier, Volume, MIN_CHUNK};
use crate::{Error, Result};

/// Set of block ranges (start -> length), merged when adjacent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeSet(BTreeMap<u64, u64>);

impl RangeSet {
    pub fn insert(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut s = start;
        let mut e = start + len;
        if let Some((&ps, &pl)) = self.0.range(..=s).next_back() {
            if ps + pl >= s {
                s = ps;
                e = e.max(ps + pl);
                self.0.remove(&ps);
            }
        }
        while let Some((&ns, &nl)) = self.0.range(s..=e).next() {
            e = e.max(ns + nl);
            self.0.remove(&ns);
        }
        self.0.insert(s, e - s);
    }

    pub fn remove(&mut self, start: u64, len: u64) {
        // An empty range removes nothing; without this the run holding
        // `start` would be split in two touching runs.
        if len == 0 {
            return;
        }
        let end = start + len;
        let mut cut = Vec::new();
        if let Some((&ps, &pl)) = self.0.range(..start).next_back() {
            if ps + pl > start {
                cut.push((ps, pl));
            }
        }
        cut.extend(self.0.range(start..end).map(|(&s, &l)| (s, l)));
        for (s, l) in cut {
            self.0.remove(&s);
            if s < start {
                self.0.insert(s, start - s);
            }
            if s + l > end {
                self.0.insert(end, s + l - end);
            }
        }
    }

    pub fn contains(&self, b: u64) -> bool {
        self.0
            .range(..=b)
            .next_back()
            .is_some_and(|(&s, &l)| b < s + l)
    }

    /// Whether every block of the range is in the set.
    pub fn covers(&self, start: u64, len: u64) -> bool {
        len == 0
            || self
                .0
                .range(..=start)
                .next_back()
                .is_some_and(|(&s, &l)| start + len <= s + l)
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Whether every block of `self` is in `other`.
    pub fn is_subset(&self, other: &RangeSet) -> bool {
        self.iter().all(|(s, l)| other.covers(s, l))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.0.iter().map(|(&s, &l)| (s, l))
    }

    /// The parts of the set inside `start..end`, in order.
    pub fn within(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        let before = self
            .0
            .range(..start)
            .next_back()
            .filter(|(&s, &l)| s + l > start);
        before
            .into_iter()
            .chain(self.0.range(start..end))
            .map(move |(&s, &l)| (s.max(start), (s + l).min(end)))
            .filter(|&(s, e)| s < e)
            .map(|(s, e)| (s, e - s))
    }
}

/// Reference counts of shared ranges: start -> (length, count >= 2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefMap(BTreeMap<u64, (u64, u32)>);

impl RefMap {
    pub fn from_entries(entries: impl IntoIterator<Item = (u64, u64, u32)>) -> Result<Self> {
        let mut m = BTreeMap::new();
        let mut end = 0;
        for (s, l, c) in entries {
            if l == 0 || c < 2 || s < end {
                return Err(Error::Corrupt("reference count entry"));
            }
            end = s + l;
            m.insert(s, (l, c));
        }
        Ok(Self(m))
    }

    pub fn entries(&self) -> impl Iterator<Item = (u64, u64, u32)> + '_ {
        self.0.iter().map(|(&s, &(l, c))| (s, l, c))
    }

    pub fn get(&self, b: u64) -> u32 {
        match self.0.range(..=b).next_back() {
            Some((&s, &(l, c))) if b < s + l => c,
            _ => 1,
        }
    }

    /// Split entries so `at` starts one (if inside an entry).
    fn cut(&mut self, at: u64) {
        if let Some((&s, &(l, c))) = self.0.range(..at).next_back() {
            if s + l > at {
                self.0.insert(s, (at - s, c));
                self.0.insert(at, (s + l - at, c));
            }
        }
    }

    /// Apply `delta` (+1/-1) to every block of the range. Returns the
    /// sub-ranges whose count dropped from 1 to 0.
    fn add(&mut self, start: u64, len: u64, delta: i32) -> Vec<(u64, u64)> {
        let end = start + len;
        self.cut(start);
        self.cut(end);
        let mut zero = Vec::new();
        let mut pos = start;
        let covered: Vec<(u64, u64, u32)> = self
            .0
            .range(start..end)
            .map(|(&s, &(l, c))| (s, l, c))
            .collect();
        let mut gaps = Vec::new();
        for (s, l, c) in covered {
            if s > pos {
                gaps.push((pos, s - pos));
            }
            let n = (c as i64 + delta as i64) as u32;
            if n >= 2 {
                self.0.insert(s, (l, n));
            } else {
                self.0.remove(&s);
            }
            pos = s + l;
        }
        if pos < end {
            gaps.push((pos, end - pos));
        }
        for (s, l) in gaps {
            if delta > 0 {
                self.0.insert(s, (l, 2));
            } else {
                zero.push((s, l));
            }
        }
        self.merge_around(start, end);
        zero
    }

    fn merge_around(&mut self, start: u64, end: u64) {
        let keys: Vec<u64> = self
            .0
            .range(..=end)
            .rev()
            .take_while(|(&s, &(l, _))| s + l >= start)
            .map(|(&s, _)| s)
            .collect();
        for s in keys.into_iter().rev() {
            let Some(&(l, c)) = self.0.get(&s) else {
                continue;
            };
            if let Some(&(nl, nc)) = self.0.get(&(s + l)) {
                if nc == c {
                    self.0.remove(&(s + l));
                    self.0.insert(s, (l + nl, c));
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Everything about block ownership for one mounted filesystem.
pub struct BlockSpace {
    pub alloc: BlockAllocator,
    pub refs: RefMap,
    /// The reference map changed since the last commit.
    pub refs_dirty: bool,
    /// Blocks allocated by the open transaction.
    pub fresh: RangeSet,
    /// Where node allocation looks first.
    pub meta_goal: u64,
    /// Free blocks file data may not take, so that tree updates (which
    /// allocate as they copy) still find room on a full device.
    pub reserve: u64,
    /// Data checksum changes not yet in the checksum tree: the CRC32C of a
    /// block just written (`None`: written without one, no-COW).
    pub csums: BTreeMap<u64, Option<u32>>,
    /// Data blocks freed by the open transaction, whose checksums go away
    /// when it commits; [`csums`](Self::csums) entries written after the
    /// free win. Kept as ranges: deleting or truncating a file of tens of
    /// GB frees millions of blocks in one call, and an entry per block was
    /// hundreds of MiB of kernel heap.
    pub csums_freed: RangeSet,
    /// The volume, when there is one: data and metadata come from chunks
    /// of their own kind, and new chunks are made when those fill up.
    pub vol: Option<Arc<Volume>>,
    /// Profiles of new data and metadata chunks.
    pub profiles: (Profile, Profile),
    /// Devices new chunks must stay off (being removed).
    pub avoid: Vec<u32>,
    /// The chunk map changed since the last commit.
    pub chunks_dirty: bool,
    /// Repair is running: a reference to blocks that are free or past the
    /// end (a damaged extent in a leaf being copied) is ignored instead of
    /// refused; repair drops such extents and recounts everything after.
    pub lenient: bool,
    /// The volume has fast and slow devices: metadata and new data go to
    /// the fast ones first.
    pub tiered: bool,
}

/// Blocks kept back for metadata on a filesystem of `total` blocks: 1/64 of
/// it, between 16 and 8192 (32 MiB). Small filesystems keep at least 64
/// (or an eighth of themselves): a quarter of the reserve is what a commit
/// has left on a full device, and a commit copies a few nodes of every
/// tree it touches.
pub fn metadata_reserve(total: u64) -> u64 {
    (total / 64).max(64.min(total / 8)).clamp(16, 8192)
}

impl BlockSpace {
    pub fn new(alloc: BlockAllocator, refs: RefMap) -> Self {
        Self {
            alloc,
            refs,
            refs_dirty: false,
            fresh: RangeSet::default(),
            meta_goal: 0,
            reserve: 0,
            csums: BTreeMap::new(),
            csums_freed: RangeSet::default(),
            vol: None,
            profiles: (Profile::Single, Profile::Single),
            avoid: Vec::new(),
            chunks_dirty: false,
            lenient: false,
            tiered: false,
        }
    }

    /// Whether `n` data blocks can be allocated now, making a data chunk
    /// for them on a volume if needed.
    pub fn ensure_data_room(&mut self, n: u64) -> bool {
        loop {
            if self.data_room() >= n {
                return true;
            }
            if !self.grow(ChunkKind::Data) {
                return false;
            }
        }
    }

    /// Free blocks file data may take now.
    pub(crate) fn data_room(&self) -> u64 {
        if self.vol.is_none() {
            return self.alloc.free_blocks().saturating_sub(self.reserve);
        }
        // Metadata chunks hold the reserve first; mixed groups the rest.
        let need = self
            .reserve
            .saturating_sub(self.alloc.free_in(ChunkKind::Metadata));
        self.alloc.free_in(ChunkKind::Data)
            + self.alloc.free_in(ChunkKind::Mixed).saturating_sub(need)
    }

    fn data_kind(&self) -> Option<ChunkKind> {
        self.vol.as_ref().map(|_| ChunkKind::Data)
    }

    /// Where metadata and new data go first: the fast tier, when there is
    /// one (bcachefs's metadata and foreground targets).
    fn target(&self) -> Option<Tier> {
        self.tiered.then_some(Tier::Fast)
    }

    /// Up to `max` contiguous data blocks near `goal`.
    pub fn alloc_data(&mut self, goal: Option<u64>, max: u64) -> Option<(u64, u64)> {
        if let Some(t) = self.target() {
            for attempt in 0..2 {
                let room = self.data_room();
                if room > 0 {
                    let kind = self.data_kind();
                    if let Some(r) = self.alloc.allocate_tier(goal, max.min(room), kind, Some(t)) {
                        self.fresh.insert(r.0, r.1);
                        return Some(r);
                    }
                }
                if attempt == 0 && !self.grow_on(ChunkKind::Data, Some(t)) {
                    break;
                }
            }
        }
        for attempt in 0..2 {
            let room = self.data_room();
            if room > 0 {
                let kind = self.data_kind();
                if let Some(r) = self.alloc.allocate_for(goal, max.min(room), kind) {
                    self.fresh.insert(r.0, r.1);
                    return Some(r);
                }
            }
            if attempt == 0 && !self.grow(ChunkKind::Data) {
                break;
            }
        }
        None
    }

    /// Exactly `n` contiguous data blocks, near `goal` if possible.
    pub fn alloc_data_run(&mut self, goal: Option<u64>, n: u64) -> Option<u64> {
        if let Some(t) = self.target() {
            for attempt in 0..2 {
                if self.data_room() >= n {
                    let kind = self.data_kind();
                    if let Some(r) = self.alloc.allocate_run_tier(goal, n, kind, Some(t)) {
                        self.fresh.insert(r, n);
                        return Some(r);
                    }
                }
                if attempt == 0 && (n > GROUP_BLOCKS / 2 || !self.grow_on(ChunkKind::Data, Some(t)))
                {
                    break;
                }
            }
        }
        for attempt in 0..2 {
            if self.data_room() >= n {
                let kind = self.data_kind();
                if let Some(r) = self.alloc.allocate_run_for(goal, n, kind) {
                    self.fresh.insert(r, n);
                    return Some(r);
                }
            }
            if attempt == 0 && (n > GROUP_BLOCKS / 2 || !self.grow(ChunkKind::Data)) {
                break;
            }
        }
        None
    }

    /// Whether tree changes may still take blocks without eating into the
    /// half of the reserve kept for the commit (and for deletes, which
    /// copy nodes too). On a volume a metadata chunk is made if that is
    /// what it takes.
    pub fn meta_headroom(&mut self) -> bool {
        let need = self.reserve / 2;
        loop {
            let free = match self.vol {
                None => self.alloc.free_blocks(),
                Some(_) => self.meta_free(),
            };
            if free >= need {
                return true;
            }
            if !self.grow_on(ChunkKind::Metadata, self.target()) && !self.grow(ChunkKind::Metadata)
            {
                return false;
            }
        }
    }

    /// Free blocks a new tree node may take now (any free block on one
    /// device, the metadata chunks' on a volume).
    pub fn node_free(&self) -> u64 {
        match self.vol {
            None => self.alloc.free_blocks(),
            Some(_) => self.meta_free(),
        }
    }

    /// Free blocks metadata may take.
    pub fn meta_free(&self) -> u64 {
        self.alloc.free_in(ChunkKind::Metadata) + self.alloc.free_in(ChunkKind::Mixed)
    }

    /// Make a new chunk for `kind` (volumes only). False if there is no
    /// room on the devices, or (for data) if that would leave metadata
    /// without its reserve.
    pub fn grow(&mut self, kind: ChunkKind) -> bool {
        self.grow_on(kind, None)
    }

    /// [`grow`](Self::grow) with the chunk on devices of `tier` only.
    pub fn grow_on(&mut self, kind: ChunkKind, tier: Option<Tier>) -> bool {
        let Some(vol) = self.vol.clone() else {
            return false;
        };
        if kind == ChunkKind::Data
            && self.meta_free() < self.reserve
            && !self.grow_on(ChunkKind::Metadata, self.target())
            && !self.grow(ChunkKind::Metadata)
        {
            return false;
        }
        let profile = match kind {
            ChunkKind::Data => self.profiles.0,
            _ => self.profiles.1,
        };
        let held = vol.held();
        let mut avoid = self.avoid.clone();
        let made = vol.with_map(|m| {
            if let Some(t) = tier {
                avoid.extend(m.off_tier(t));
            }
            // Chunks of an eighth of the smallest device (at most a group).
            let small = m.devices.values().copied().min().unwrap_or(0);
            let want = (small / 8).clamp(MIN_CHUNK, GROUP_BLOCKS);
            let (blocks, stripes) = m.place(profile, want, &avoid, &held)?;
            let g = m.free_group();
            let c = Chunk {
                kind,
                profile,
                blocks,
                stripes,
            };
            let row = if profile.parity() > 0 {
                c.data_cols() as u64
            } else {
                1
            };
            let t = m.chunk_tier(&c);
            m.chunks.insert(g, c);
            Some((g, blocks, row, t))
        });
        let Some((g, blocks, row, t)) = made else {
            return false;
        };
        let mut grp = AllocationGroup::new(g as u32, blocks);
        grp.kind = kind;
        grp.tier = t;
        grp.set_row(row);
        self.alloc.set_group(grp);
        self.chunks_dirty = true;
        true
    }

    /// Whether `start..start+len` was allocated by this transaction and has
    /// a single owner, so it can be rewritten in place.
    pub fn private_fresh(&self, start: u64, len: u64) -> bool {
        self.fresh.covers(start, len) && (start..start + len).all(|b| self.refs.get(b) == 1)
    }

    fn free(&mut self, start: u64, len: u64) -> Result<()> {
        // Split by "fresh or not": fresh blocks go back at once.
        let mut pos = start;
        let end = start + len;
        while pos < end {
            let fresh = self.fresh.contains(pos);
            let mut run = pos + 1;
            while run < end && self.fresh.contains(run) == fresh {
                run += 1;
            }
            if fresh {
                self.fresh.remove(pos, run - pos);
                self.alloc.free_now(pos, run - pos)?;
            } else {
                self.alloc.free(pos, run - pos)?;
            }
            pos = run;
        }
        Ok(())
    }

    /// The transaction committed.
    pub fn committed(&mut self) {
        self.fresh.clear();
        self.refs_dirty = false;
    }
}

impl Space for BlockSpace {
    fn alloc_node(&mut self, goal: u64) -> Result<u64> {
        let goal = if self.meta_goal != 0 {
            self.meta_goal
        } else {
            goal
        };
        let kind = self.vol.as_ref().map(|_| ChunkKind::Metadata);
        if let Some(t) = self.target() {
            let mut got = self.alloc.allocate_tier(Some(goal), 1, kind, Some(t));
            if got.is_none() && self.grow_on(ChunkKind::Metadata, Some(t)) {
                got = self.alloc.allocate_tier(Some(goal), 1, kind, Some(t));
            }
            if let Some((b, _)) = got {
                self.fresh.insert(b, 1);
                self.meta_goal = b + 1;
                return Ok(b);
            }
        }
        let b = match self.alloc.allocate_for(Some(goal), 1, kind) {
            Some((b, _)) => b,
            None if self.grow(ChunkKind::Metadata) => {
                self.alloc
                    .allocate_for(Some(goal), 1, kind)
                    .ok_or(Error::NoSpace)?
                    .0
            }
            None => return Err(Error::NoSpace),
        };
        self.fresh.insert(b, 1);
        self.meta_goal = b + 1;
        Ok(b)
    }

    fn refs(&self, bytenr: u64) -> u32 {
        self.refs.get(bytenr)
    }

    fn node_room(&mut self, n: u64) -> bool {
        loop {
            if self.node_free() >= n {
                return true;
            }
            if !self.grow_on(ChunkKind::Metadata, self.target()) && !self.grow(ChunkKind::Metadata)
            {
                return false;
            }
        }
    }

    fn incref(&mut self, start: u64, len: u64) -> Result<()> {
        // Past the end first: a damaged length must not make this walk
        // billions of blocks.
        let bad = match start.checked_add(len) {
            Some(end) if end <= self.alloc.end() => (start..end).any(|b| !self.alloc.is_used(b)),
            _ => true,
        };
        if bad {
            if self.lenient {
                return Ok(());
            }
            return Err(Error::Corrupt("reference to a free block"));
        }
        self.refs.add(start, len, 1);
        self.refs_dirty = true;
        Ok(())
    }

    fn decref_node(&mut self, bytenr: u64) -> Result<bool> {
        if self.refs.get(bytenr) > 1 {
            self.refs_dirty = true;
        }
        let zero = self.refs.add(bytenr, 1, -1);
        if zero.is_empty() {
            return Ok(false);
        }
        self.free(bytenr, 1)?;
        Ok(true)
    }

    fn decref_data(&mut self, start: u64, len: u64) -> Result<()> {
        if (start..start + len).any(|b| self.refs.get(b) > 1) {
            self.refs_dirty = true;
        }
        for (s, l) in self.refs.add(start, len, -1) {
            self.free(s, l)?;
            while let Some((&b, _)) = self.csums.range(s..s + l).next() {
                self.csums.remove(&b);
            }
            self.csums_freed.insert(s, l);
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::blocks::AllocationGroup;
    use alloc::vec;

    #[test]
    fn range_set_merges_and_splits() {
        let mut r = RangeSet::default();
        r.insert(10, 5);
        r.insert(15, 5);
        r.insert(30, 2);
        r.insert(12, 20);
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![(10, 22)]);
        r.remove(14, 3);
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![(10, 4), (17, 15)]);
        assert!(r.covers(17, 15) && !r.covers(13, 2) && r.covers(5, 0));
        assert!(r.contains(10) && !r.contains(14) && r.contains(31) && !r.contains(32));
        r.remove(0, 100);
        assert!(r.is_empty());
    }

    #[test]
    fn ref_map_counts_by_block() {
        let mut m = RefMap::default();
        assert_eq!(m.get(5), 1);
        assert!(m.add(10, 10, 1).is_empty());
        assert!(m.add(15, 10, 1).is_empty());
        assert_eq!(
            (m.get(9), m.get(10), m.get(15), m.get(24), m.get(25)),
            (1, 2, 3, 2, 1)
        );
        // Dropping one owner of 12..22: 12..15 back to one owner (no entry).
        assert!(m.add(12, 10, -1).is_empty());
        assert_eq!((m.get(11), m.get(12), m.get(15), m.get(22)), (2, 1, 2, 2));
        let zero = m.add(0, 30, -1);
        assert_eq!(zero, vec![(0, 10), (12, 3), (20, 2), (25, 5)]);
        assert!(m.is_empty());
    }

    #[test]
    fn ref_map_merges_equal_neighbours() {
        let mut m = RefMap::default();
        m.add(0, 4, 1);
        m.add(4, 4, 1);
        assert_eq!(m.entries().collect::<Vec<_>>(), vec![(0, 8, 2)]);
        assert!(RefMap::from_entries([(0, 4, 2), (2, 4, 2)]).is_err());
        assert!(RefMap::from_entries([(0, 4, 1)]).is_err());
        assert_eq!(
            RefMap::from_entries([(0, 4, 2), (4, 1, 3)]).unwrap().get(4),
            3
        );
    }

    #[test]
    fn fresh_blocks_are_freed_at_once_and_old_ones_at_commit() {
        let mut alloc = BlockAllocator::new(vec![AllocationGroup::new(0, 64)]);
        let (old, _) = alloc.allocate(None, 4).unwrap();
        alloc.set_deferred_frees(true);
        let mut sp = BlockSpace::new(alloc, RefMap::default());
        let (new, n) = sp.alloc_data(None, 4).unwrap();
        assert_eq!(n, 4);
        assert!(sp.private_fresh(new, 4) && !sp.private_fresh(old, 1));
        let free = sp.alloc.free_blocks();
        sp.decref_data(new, 4).unwrap();
        assert_eq!(sp.alloc.free_blocks(), free + 4);
        sp.decref_data(old, 4).unwrap();
        assert_eq!(sp.alloc.free_blocks(), free + 4);
        assert_eq!(sp.alloc.deferred_blocks(), 4);
        // A shared block survives one decref and is not rewritable in place.
        let (s, _) = sp.alloc_data(None, 1).unwrap();
        sp.incref(s, 1).unwrap();
        assert!(!sp.private_fresh(s, 1));
        sp.decref_data(s, 1).unwrap();
        assert!(sp.alloc.is_used(s));
        assert!(sp.private_fresh(s, 1));
        assert!(sp.incref(60, 1).is_err());
    }

    /// Free blocks in pieces too small for a run.
    #[test]
    fn a_run_needs_contiguous_room() {
        let mut g = AllocationGroup::new(0, 64);
        g.mark_used(0, 64);
        for b in (8..64).step_by(2) {
            g.free(b, 1).unwrap();
        }
        let mut sp = BlockSpace::new(BlockAllocator::new(vec![g]), RefMap::default());
        assert!(sp.data_room() >= 4);
        assert_eq!(sp.alloc_data_run(None, 4), None);
        assert!(sp.alloc_data_run(None, 1).is_some());
    }

    /// `RangeSet` against the dumbest model there is, a bit per block, over
    /// thousands of random inserts and removes: after every step the two
    /// agree block by block, and the set stays canonical (sorted, merged,
    /// nothing empty), which is what `iter` users rely on.
    #[test]
    fn range_set_matches_a_bitmap_model() {
        use crate::testdisk::Rng;
        const N: u64 = 512;
        let mut rng = Rng::new(0xEC11);
        for _round in 0..20 {
            let mut set = RangeSet::default();
            let mut model = [false; N as usize];
            for _ in 0..300 {
                let s = rng.below(N);
                let l = rng.below(40).min(N - s);
                if rng.below(3) == 0 {
                    set.remove(s, l);
                    model[s as usize..(s + l) as usize].fill(false);
                } else {
                    set.insert(s, l);
                    model[s as usize..(s + l) as usize].fill(true);
                }
                let mut prev_end = None;
                for (s, l) in set.iter() {
                    assert!(l > 0, "empty run at {}", s);
                    if let Some(e) = prev_end {
                        assert!(s > e, "runs not merged or not sorted at {}", s);
                    }
                    prev_end = Some(s + l);
                }
                for b in 0..N {
                    assert_eq!(set.contains(b), model[b as usize], "block {}", b);
                }
                let (a, b) = (rng.below(N), rng.below(N));
                let (a, b) = (a.min(b), a.max(b));
                let inside: u64 = set.within(a, b).map(|(_, l)| l).sum();
                let want = model[a as usize..b as usize].iter().filter(|&&x| x).count() as u64;
                assert_eq!(inside, want, "within({}, {})", a, b);
                assert_eq!(
                    set.covers(a, b - a),
                    model[a as usize..b as usize].iter().all(|&x| x)
                );
            }
        }
    }

    #[test]
    fn range_set_subset_and_clear() {
        let mut a = RangeSet::default();
        let mut b = RangeSet::default();
        a.insert(5, 5);
        b.insert(0, 20);
        assert!(a.is_subset(&b) && !b.is_subset(&a));
        assert!(RangeSet::default().is_subset(&a));
        a.insert(19, 2);
        assert!(!a.is_subset(&b));
        a.clear();
        assert!(a.is_empty() && a.is_subset(&b));
        // Zero-length operations change nothing.
        b.insert(100, 0);
        b.remove(3, 0);
        assert_eq!(b.iter().collect::<Vec<_>>(), vec![(0, 20)]);
    }

    /// `RefMap` against an array of counts: whatever the sequence of
    /// increments and decrements, every block's count matches, only shared
    /// blocks (count >= 2) have entries, and what `add` reports as dropped to
    /// zero is exactly the blocks whose count did.
    #[test]
    fn ref_map_matches_a_count_model() {
        use crate::testdisk::Rng;
        const N: u64 = 256;
        let mut rng = Rng::new(77);
        let mut m = RefMap::default();
        let mut model = [1i64; N as usize];
        for _ in 0..3000 {
            let s = rng.below(N);
            let l = 1 + rng.below(24).min(N - s - 1);
            let range = s as usize..(s + l) as usize;
            let delta = if rng.below(2) == 0 { 1 } else { -1 };
            let zero = m.add(s, l, delta);
            let mut want_zero = RangeSet::default();
            for b in range {
                model[b] += delta as i64;
                if model[b] == 0 {
                    want_zero.insert(b as u64, 1);
                    model[b] = 1; // freed and handed out again
                }
            }
            let mut got = RangeSet::default();
            for (zs, zl) in zero {
                got.insert(zs, zl);
            }
            assert_eq!(got, want_zero);
            for b in 0..N {
                assert_eq!(m.get(b) as i64, model[b as usize], "block {}", b);
            }
            assert!(m.entries().all(|(_, l, c)| l > 0 && c >= 2));
        }
    }
}
