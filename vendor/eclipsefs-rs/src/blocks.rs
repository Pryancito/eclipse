//! Block allocation groups (XFS), ported from eclipsefs-lib v0.3.0.
//!
//! The device is cut into groups of [`GROUP_BLOCKS`] blocks. Block 0 of every
//! group holds a superblock (the primary in group 0, backups elsewhere) and
//! block 1 its free-space bitmap: one 4 KiB block, one bit per block of the
//! group, **1 = in use**. Bits past the end of a short last group are kept set
//! so they can never be handed out.
//!
//! The original bitmap was a `heapless::Vec<u64, 64>` under no_std (4096
//! blocks, 16 MiB, for the whole filesystem); here every group carries a full
//! bitmap block and the allocator holds them all in memory (8 KiB per GiB).
//!
//! Groups whose chunk keeps parity (RAID5/6, see [`crate::volume`]) hand
//! out blocks by *rows* of `row` blocks: only from rows that are wholly
//! free, or that the open transaction has already started. So no write
//! ever lands in a row that holds committed data, and a crash can never
//! leave a committed block's parity stale (the RAID write hole).

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::format::{BLOCK_SIZE, GROUP_BLOCKS};
use crate::volume::{ChunkKind, Tier};
use crate::{Error, Result};

const WORDS: usize = (GROUP_BLOCKS / 64) as usize;

/// One allocation group and its bitmap.
#[derive(Debug, Clone)]
pub struct AllocationGroup {
    pub id: u32,
    pub start_block: u64,
    pub block_count: u64,
    pub free_blocks: u64,
    /// What its blocks may hold (volumes keep data and metadata apart).
    pub kind: ChunkKind,
    bitmap: Vec<u64>,
    dirty: bool,
    /// Blocks from here on are not handed out (being emptied by a resize).
    fence: u64,
    /// Data blocks per parity row (1: no rows).
    row: u64,
    /// Rows the open transaction has handed blocks out of, as ranges
    /// (first row, rows).
    open: BTreeMap<u64, u64>,
    /// Tier of the devices behind the group.
    pub tier: Tier,
}

impl AllocationGroup {
    /// A freshly formatted group: everything free except its superblock and
    /// bitmap blocks.
    pub fn new(id: u32, block_count: u64) -> Self {
        let mut g = Self {
            id,
            start_block: id as u64 * GROUP_BLOCKS,
            block_count,
            free_blocks: block_count,
            kind: ChunkKind::Mixed,
            bitmap: vec![0; WORDS],
            dirty: true,
            fence: u64::MAX,
            row: 1,
            open: BTreeMap::new(),
            tier: Tier::Any,
        };
        g.mark_tail_used();
        g.set_range(0, 2, true);
        g
    }

    /// Load a group from its on-disk bitmap block.
    pub fn from_bytes(id: u32, block_count: u64, bytes: &[u8]) -> Result<Self> {
        if bytes.len() < BLOCK_SIZE as usize {
            return Err(Error::Invalid);
        }
        let bitmap: Vec<u64> = (0..WORDS)
            .map(|i| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap()))
            .collect();
        let mut g = Self {
            id,
            start_block: id as u64 * GROUP_BLOCKS,
            block_count,
            free_blocks: 0,
            kind: ChunkKind::Mixed,
            bitmap,
            dirty: false,
            fence: u64::MAX,
            row: 1,
            open: BTreeMap::new(),
            tier: Tier::Any,
        };
        g.mark_tail_used();
        if !g.is_used(0) || !g.is_used(1) {
            return Err(Error::Corrupt("group header blocks marked free"));
        }
        g.free_blocks = g.count_free();
        Ok(g)
    }

    /// Runs of blocks in use: (group-relative start, length).
    pub fn used_runs(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off < self.block_count {
            if !self.is_used(off) {
                off += 1;
                continue;
            }
            let start = off;
            while off < self.block_count && self.is_used(off) {
                off += 1;
            }
            out.push((start, off - start));
        }
        out
    }

    /// A group of the logical space with no chunk behind it.
    pub fn hole(id: u32) -> Self {
        Self {
            id,
            start_block: id as u64 * GROUP_BLOCKS,
            block_count: 0,
            free_blocks: 0,
            kind: ChunkKind::Mixed,
            bitmap: vec![u64::MAX; WORDS],
            dirty: false,
            fence: u64::MAX,
            row: 1,
            open: BTreeMap::new(),
            tier: Tier::Any,
        }
    }

    pub fn is_hole(&self) -> bool {
        self.block_count == 0
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BLOCK_SIZE as usize);
        for w in &self.bitmap {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// Block number of this group's bitmap.
    pub fn bitmap_block(&self) -> u64 {
        self.start_block + 1
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    fn mark_tail_used(&mut self) {
        for off in self.block_count..GROUP_BLOCKS {
            self.bitmap[(off / 64) as usize] |= 1 << (off % 64);
        }
    }

    fn count_free(&self) -> u64 {
        let used: u64 = self.bitmap.iter().map(|w| w.count_ones() as u64).sum();
        GROUP_BLOCKS - used
    }

    pub fn is_used(&self, off: u64) -> bool {
        self.bitmap[(off / 64) as usize] & (1 << (off % 64)) != 0
    }

    fn set_range(&mut self, off: u64, len: u64, used: bool) {
        for o in off..off + len {
            let (w, b) = ((o / 64) as usize, o % 64);
            if used {
                self.bitmap[w] |= 1 << b;
            } else {
                self.bitmap[w] &= !(1 << b);
            }
        }
        if used {
            self.free_blocks -= len;
        } else {
            self.free_blocks += len;
        }
        self.dirty = true;
    }

    /// Blocks per parity row of the chunk behind the group (1: none).
    pub fn set_row(&mut self, row: u64) {
        self.row = row.max(1);
        self.open.clear();
    }

    pub fn row(&self) -> u64 {
        self.row
    }

    /// The open transaction committed: its rows are committed rows now.
    pub fn close_rows(&mut self) {
        self.open.clear();
    }

    fn row_open(&self, r: u64) -> bool {
        matches!(self.open.range(..=r).next_back(), Some((&s, &l)) if r < s + l)
    }

    /// Whether blocks of row `r` may be handed out: it is wholly free, or
    /// the open transaction started it.
    fn row_ok(&self, r: u64) -> bool {
        if self.row <= 1 || self.row_open(r) {
            return true;
        }
        let (s, e) = (r * self.row, (r + 1) * self.row);
        e <= self.block_count && (s..e).all(|o| !self.is_used(o))
    }

    /// Note the rows of `off..off+len` as started by the open transaction.
    fn open_rows(&mut self, off: u64, len: u64) {
        if self.row <= 1 || len == 0 {
            return;
        }
        let (mut a, mut b) = (off / self.row, (off + len - 1) / self.row + 1);
        if let Some((&s, &l)) = self.open.range(..=a).next_back() {
            if s + l >= a {
                a = s;
                b = b.max(s + l);
                self.open.remove(&s);
            }
        }
        while let Some((&s, &l)) = self.open.range(a..=b).next() {
            b = b.max(s + l);
            self.open.remove(&s);
        }
        self.open.insert(a, b - a);
    }

    /// First block at or after `from` that may be handed out.
    fn next_alloc(&self, from: u64) -> Option<u64> {
        let mut off = from;
        loop {
            let o = self.next_free(off)?;
            if self.row_ok(o / self.row) {
                return Some(o);
            }
            off = (o / self.row + 1) * self.row;
        }
    }

    /// End of the blocks allocations may take.
    fn alloc_end(&self) -> u64 {
        self.block_count.min(self.fence)
    }

    /// First free block at or after `from` (group-relative).
    fn next_free(&self, from: u64) -> Option<u64> {
        let end = self.alloc_end();
        let mut off = from;
        while off < end {
            let w = self.bitmap[(off / 64) as usize] | ((1u64 << (off % 64)) - 1);
            if w == u64::MAX {
                off = (off / 64 + 1) * 64;
                continue;
            }
            let found = (off / 64) * 64 + (!w).trailing_zeros() as u64;
            return (found < end).then_some(found);
        }
        None
    }

    /// Length of the free run starting at `off`, capped at `max`.
    fn run_len(&self, off: u64, max: u64) -> u64 {
        let end = self.alloc_end();
        let mut len = 0;
        while len < max && off + len < end && !self.is_used(off + len) {
            if self.row > 1 {
                let o = off + len;
                if len > 0 && o.is_multiple_of(self.row) && !self.row_ok(o / self.row) {
                    break;
                }
                len += 1;
                continue;
            }
            if (off + len).is_multiple_of(64)
                && max - len >= 64
                && self.bitmap[((off + len) / 64) as usize] == 0
            {
                len += 64;
            } else {
                len += 1;
            }
        }
        len.min(max)
    }

    /// Allocate up to `max` contiguous blocks, preferring the run that starts
    /// at or after `from` (group-relative). Returns `(group-relative start, len)`.
    pub fn allocate(&mut self, from: u64, max: u64) -> Option<(u64, u64)> {
        if self.free_blocks == 0 || max == 0 {
            return None;
        }
        let start = self
            .next_alloc(from.min(self.block_count))
            .or_else(|| self.next_alloc(0))?;
        let len = self.run_len(start, max);
        self.set_range(start, len, true);
        self.open_rows(start, len);
        Some((start, len))
    }

    /// `n` contiguous free blocks at or after `from` (first fit).
    pub fn allocate_run(&mut self, from: u64, n: u64) -> Option<u64> {
        if self.free_blocks < n || n == 0 {
            return None;
        }
        let mut off = from.min(self.block_count);
        while let Some(start) = self.next_alloc(off) {
            let len = self.run_len(start, n);
            if len == n {
                self.set_range(start, n, true);
                self.open_rows(start, n);
                return Some(start);
            }
            off = start + len.max(1);
        }
        None
    }

    /// Free `len` blocks at group-relative `off`, refusing a double free.
    pub fn free(&mut self, off: u64, len: u64) -> Result<()> {
        if off + len > self.block_count || off < 2 {
            return Err(Error::Corrupt("free outside allocation group"));
        }
        if (off..off + len).any(|o| !self.is_used(o)) {
            return Err(Error::Corrupt("double free"));
        }
        self.set_range(off, len, false);
        Ok(())
    }

    /// Mark `off..off+len` (group-relative) in use, whatever part of it
    /// already is.
    pub fn mark_used(&mut self, off: u64, len: u64) {
        for o in off..(off + len).min(self.block_count) {
            if !self.is_used(o) {
                self.set_range(o, 1, true);
            }
        }
    }

    /// Have the bitmap written with the next commit.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Shrink the (last) group to `new_count` blocks, all of whose blocks
    /// past that must be free.
    pub fn shrink(&mut self, new_count: u64) -> Result<()> {
        if new_count >= self.block_count {
            return Ok(());
        }
        if new_count < 2 || (new_count..self.block_count).any(|o| self.is_used(o)) {
            return Err(Error::Invalid);
        }
        let old = self.block_count;
        // Past the end reads as "in use", as for a short group.
        self.set_range(new_count, old - new_count, true);
        self.block_count = new_count;
        Ok(())
    }

    /// Grow the (last) group to `new_count` blocks.
    pub fn extend(&mut self, new_count: u64) {
        if new_count <= self.block_count {
            return;
        }
        let old = self.block_count;
        self.block_count = new_count.min(GROUP_BLOCKS);
        // Those bits were the "in use" padding past the old end.
        self.set_range(old, self.block_count - old, false);
    }
}

/// Allocator over every group of the filesystem.
#[derive(Debug, Clone, Default)]
pub struct BlockAllocator {
    groups: Vec<AllocationGroup>,
    /// Group of the last allocation, where the next goal-less search starts
    /// (round-robin as in eclipsefs-lib, without resetting to group 0).
    cursor: usize,
    /// Frees held back until the current transaction commits (journaled
    /// mounts): a block freed by an uncommitted transaction is still in use
    /// as far as the disk knows, so it must not be reused yet.
    defer: bool,
    /// Held-back ranges, keyed by their first block. A map and not a list:
    /// every `free` has to prove the range does not overlap one already in
    /// here, and scanning a list for that made deleting a large file
    /// quadratic -- a real machine wedged for eight seconds in this very
    /// function, purging a multi-GB file whose millions of extents each
    /// walked every free before it. Ordered by start, with no two ranges
    /// overlapping, the proof is the two neighbours of `start`.
    deferred: BTreeMap<u64, u64>,
    deferred_blocks: u64,
}

impl BlockAllocator {
    pub fn new(groups: Vec<AllocationGroup>) -> Self {
        Self {
            groups,
            cursor: 0,
            defer: false,
            deferred: BTreeMap::new(),
            deferred_blocks: 0,
        }
    }

    /// Hold frees back until [`release_deferred`](Self::release_deferred).
    pub fn set_deferred_frees(&mut self, on: bool) {
        self.defer = on;
    }

    /// Blocks freed but not yet reusable.
    pub fn deferred_blocks(&self) -> u64 {
        self.deferred_blocks
    }

    /// Apply the frees held back so far.
    pub fn release_deferred(&mut self) -> Result<()> {
        // The transaction is committing: rows it started are committed.
        for g in self.groups.iter_mut() {
            g.close_rows();
        }
        let list = core::mem::take(&mut self.deferred);
        self.deferred_blocks = 0;
        for (start, len) in list {
            self.free_now(start, len)?;
        }
        Ok(())
    }

    pub fn groups(&self) -> &[AllocationGroup] {
        &self.groups
    }

    pub fn groups_mut(&mut self) -> &mut Vec<AllocationGroup> {
        &mut self.groups
    }

    pub fn total_blocks(&self) -> u64 {
        self.groups.iter().map(|g| g.block_count).sum()
    }

    pub fn free_blocks(&self) -> u64 {
        self.groups.iter().map(|g| g.free_blocks).sum()
    }

    /// Allocate up to `max` contiguous blocks as close to block `goal` as
    /// possible. Returns `(first block, count)`; `None` when the device is full.
    pub fn allocate(&mut self, goal: Option<u64>, max: u64) -> Option<(u64, u64)> {
        self.allocate_for(goal, max, None)
    }

    /// [`allocate`](Self::allocate) from groups that serve `kind` (any
    /// group if `None`).
    pub fn allocate_for(
        &mut self,
        goal: Option<u64>,
        max: u64,
        kind: Option<ChunkKind>,
    ) -> Option<(u64, u64)> {
        self.allocate_tier(goal, max, kind, None)
    }

    /// [`allocate_for`](Self::allocate_for) from groups on `tier` only (any
    /// if `None`).
    pub fn allocate_tier(
        &mut self,
        goal: Option<u64>,
        max: u64,
        kind: Option<ChunkKind>,
        tier: Option<Tier>,
    ) -> Option<(u64, u64)> {
        let n = self.groups.len();
        if n == 0 {
            return None;
        }
        let (first, from) = match goal {
            Some(g) if ((g / GROUP_BLOCKS) as usize) < n => {
                ((g / GROUP_BLOCKS) as usize, g % GROUP_BLOCKS)
            }
            _ => (self.cursor.min(n - 1), 0),
        };
        for i in 0..n {
            let gi = (first + i) % n;
            let g = &mut self.groups[gi];
            if kind.is_some_and(|k| !g.kind.serves(k)) || tier.is_some_and(|t| g.tier != t) {
                continue;
            }
            let from = if i == 0 { from } else { 0 };
            if let Some((off, len)) = g.allocate(from, max) {
                self.cursor = gi;
                return Some((g.start_block + off, len));
            }
        }
        None
    }

    /// Exactly `n` contiguous blocks, after `goal` if possible.
    pub fn allocate_run(&mut self, goal: Option<u64>, n: u64) -> Option<u64> {
        self.allocate_run_for(goal, n, None)
    }

    /// [`allocate_run`](Self::allocate_run) from groups that serve `kind`.
    pub fn allocate_run_for(
        &mut self,
        goal: Option<u64>,
        n: u64,
        kind: Option<ChunkKind>,
    ) -> Option<u64> {
        self.allocate_run_tier(goal, n, kind, None)
    }

    /// [`allocate_run_for`](Self::allocate_run_for) from groups on `tier`.
    pub fn allocate_run_tier(
        &mut self,
        goal: Option<u64>,
        n: u64,
        kind: Option<ChunkKind>,
        tier: Option<Tier>,
    ) -> Option<u64> {
        let groups = self.groups.len();
        let (first, from) = match goal {
            Some(g) if ((g / GROUP_BLOCKS) as usize) < groups => {
                ((g / GROUP_BLOCKS) as usize, g % GROUP_BLOCKS)
            }
            _ => (self.cursor.min(groups.max(1) - 1), 0),
        };
        // The goal's group from the goal on, every other group, then the
        // start of the goal's group.
        for i in 0..=groups {
            let gi = (first + i) % groups.max(1);
            let g = self.groups.get_mut(gi)?;
            if kind.is_some_and(|k| !g.kind.serves(k)) || tier.is_some_and(|t| g.tier != t) {
                continue;
            }
            let from = if i == 0 { from } else { 0 };
            if let Some(off) = g.allocate_run(from, n) {
                self.cursor = gi;
                return Some(g.start_block + off);
            }
        }
        None
    }

    /// Hand out nothing in `from..to` until [`unfence`](Self::unfence).
    /// The range must reach the end of every group it starts in.
    pub fn fence(&mut self, from: u64, to: u64) {
        for g in self.groups.iter_mut() {
            let end = g.start_block + GROUP_BLOCKS;
            if end > from && g.start_block < to {
                g.fence = g.fence.min(from.saturating_sub(g.start_block));
            }
        }
    }

    pub fn unfence(&mut self) {
        for g in self.groups.iter_mut() {
            g.fence = u64::MAX;
        }
    }

    /// Free blocks in groups of exactly `kind`.
    pub fn free_in(&self, kind: ChunkKind) -> u64 {
        self.groups
            .iter()
            .filter(|g| g.kind == kind)
            .map(|g| g.free_blocks)
            .sum()
    }

    /// Put `g` at its place (`g.id`), with holes before it if needed.
    pub fn set_group(&mut self, g: AllocationGroup) {
        let i = g.id as usize;
        while self.groups.len() <= i {
            let id = self.groups.len() as u32;
            self.groups.push(AllocationGroup::hole(id));
        }
        self.groups[i] = g;
        while self.groups.last().is_some_and(|g| g.is_hole()) {
            self.groups.pop();
        }
    }

    /// End of the last group (in blocks).
    pub fn end(&self) -> u64 {
        self.groups
            .last()
            .map(|g| g.start_block + g.block_count)
            .unwrap_or(0)
    }

    /// Free `len` blocks starting at absolute block `start` (or queue them,
    /// see [`set_deferred_frees`](Self::set_deferred_frees)).
    /// Does `start..end` touch a range already held back?
    ///
    /// The held-back ranges never overlap each other -- this check is what
    /// keeps them from it -- so only two of them can reach into `start..end`:
    /// the last one starting at or before `start`, and the first one starting
    /// after it. Everything else starts too early to reach that far, or too
    /// late to start inside.
    fn overlaps_deferred(&self, start: u64, end: u64) -> bool {
        if let Some((&s, &l)) = self.deferred.range(..=start).next_back() {
            if start < s + l {
                return true;
            }
        }
        matches!(self.deferred.range(start..).next(), Some((&s, _)) if s < end)
    }

    pub fn free(&mut self, start: u64, len: u64) -> Result<()> {
        if !self.defer {
            return self.free_now(start, len);
        }
        let end = start.checked_add(len).ok_or(Error::Corrupt("free range"))?;
        if (start..end).any(|b| !self.is_used(b)) || end > self.end() {
            return Err(Error::Corrupt("double free"));
        }
        if self.overlaps_deferred(start, end) {
            return Err(Error::Corrupt("double free"));
        }
        self.deferred.insert(start, len);
        self.deferred_blocks += len;
        Ok(())
    }

    /// Free right away even when frees are held back: for blocks allocated
    /// by the open transaction, which no committed state references.
    pub fn free_now(&mut self, start: u64, len: u64) -> Result<()> {
        let mut start = start;
        let mut len = len;
        while len > 0 {
            let gi = (start / GROUP_BLOCKS) as usize;
            let g = self
                .groups
                .get_mut(gi)
                .ok_or(Error::Corrupt("free past end of filesystem"))?;
            let off = start % GROUP_BLOCKS;
            let n = len.min(g.block_count.saturating_sub(off)).max(1);
            g.free(off, n)?;
            start += n;
            len -= n;
        }
        Ok(())
    }

    /// Is absolute block `b` in use?
    pub fn is_used(&self, b: u64) -> bool {
        match self.groups.get((b / GROUP_BLOCKS) as usize) {
            Some(g) => b % GROUP_BLOCKS >= g.block_count || g.is_used(b % GROUP_BLOCKS),
            None => true,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn deferred_frees_are_not_reused_until_released() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 20)]);
        assert_eq!(a.allocate(None, 18), Some((2, 18)));
        a.set_deferred_frees(true);
        a.free(5, 3).unwrap();
        a.free(10, 1).unwrap();
        assert_eq!(a.deferred_blocks(), 4);
        assert_eq!(a.free_blocks(), 0);
        assert_eq!(a.allocate(None, 1), None);
        // Freeing a held-back block again, or a free one, is refused.
        assert!(a.free(6, 1).is_err());
        assert!(a.free(4, 2).is_err());
        a.release_deferred().unwrap();
        assert_eq!(a.deferred_blocks(), 0);
        assert_eq!(a.free_blocks(), 4);
        assert_eq!(a.allocate(Some(5), 10), Some((5, 3)));
        // Released blocks are really free now: freeing them is a double free.
        assert!(a.free(10, 1).is_err());
        // Turning deferral off frees immediately.
        assert_eq!(a.allocate(Some(10), 1), Some((10, 1)));
        a.set_deferred_frees(false);
        a.free(10, 1).unwrap();
        assert_eq!(a.free_blocks(), 1);
        assert_eq!(a.deferred_blocks(), 0);
    }

    /// The held-back ranges are checked against the two neighbours of the new
    /// one, not against all of them, so every shape an overlap can take has to
    /// be one of those two: one that starts before and reaches in, one that
    /// starts inside, one that contains the new range whole, and the same
    /// range twice.
    #[test]
    fn every_shape_of_overlap_with_a_held_back_range_is_refused() {
        for (s, l) in [(35u64, 10u64), (45, 10), (20, 40), (40, 10)] {
            let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 100)]);
            while a.allocate(None, 98).is_some() {}
            a.set_deferred_frees(true);
            a.free(40, 10).unwrap();
            assert!(
                a.free(s, l).is_err(),
                "{}..{} was allowed next to 40..50",
                s,
                s + l
            );
        }
    }

    /// And a range that only touches one end of a held-back one is not an
    /// overlap: a check written off the neighbours is as easy to make too
    /// strict as too lax, and refusing these would leak blocks.
    #[test]
    fn a_range_that_merely_abuts_a_held_back_one_is_allowed() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 100)]);
        while a.allocate(None, 98).is_some() {}
        a.set_deferred_frees(true);
        a.free(40, 10).unwrap();
        a.free(30, 10).unwrap();
        a.free(50, 10).unwrap();
        assert_eq!(a.deferred_blocks(), 30);
        a.release_deferred().unwrap();
        assert_eq!(a.free_blocks(), 30);
    }

    /// Held back in whatever order they arrive, released as one run of
    /// blocks: the map that makes the overlap check cheap also reorders them,
    /// and `free_now` must not care.
    #[test]
    fn frees_held_back_out_of_order_all_come_back() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 100)]);
        while a.allocate(None, 98).is_some() {}
        a.set_deferred_frees(true);
        for start in [80u64, 10, 50, 30, 70, 20] {
            a.free(start, 5).unwrap();
        }
        assert_eq!(a.deferred_blocks(), 30);
        assert_eq!(a.free_blocks(), 0);
        a.release_deferred().unwrap();
        assert_eq!(a.free_blocks(), 30);
        for start in [80u64, 10, 50, 30, 70, 20] {
            assert!(!a.is_used(start), "block {} did not come back", start);
        }
    }

    /// Purging a large file frees extents by the million, and every one of
    /// them used to walk the whole list of the frees before it. This is the
    /// same shape at a size a quadratic check cannot finish: with the list it
    /// is ~5e9 comparisons, which is what wedged a real machine for the eight
    /// seconds the deadlock detector waits.
    #[test]
    fn a_hundred_thousand_held_back_frees_do_not_walk_each_other() {
        const GROUPS: u64 = 13;
        let mut a = BlockAllocator::new(
            (0..GROUPS as u32)
                .map(|id| AllocationGroup::new(id, GROUP_BLOCKS))
                .collect(),
        );
        while a.allocate(None, 4096).is_some() {}
        a.set_deferred_frees(true);
        let mut freed = 0;
        // Two blocks every four, skipping the two header blocks of each
        // group, which are never free to begin with.
        for g in 0..GROUPS {
            for start in (g * GROUP_BLOCKS + 4..(g + 1) * GROUP_BLOCKS).step_by(4) {
                a.free(start, 2).unwrap();
                freed += 2;
            }
        }
        assert!(freed > 100_000, "only {} blocks freed", freed);
        assert_eq!(a.deferred_blocks(), freed);
        a.release_deferred().unwrap();
        assert_eq!(a.free_blocks(), freed);
    }

    #[test]
    fn parity_groups_never_reuse_a_committed_row() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 400)]);
        a.groups_mut()[0].set_row(4);
        a.set_deferred_frees(true);
        // Row 0 holds the header blocks: never handed out.
        assert_eq!(a.allocate(None, 1), Some((4, 1)));
        // The same transaction keeps filling the row it started.
        assert_eq!(a.allocate(None, 2), Some((5, 2)));
        a.release_deferred().unwrap();
        // Committed: block 7 shares a row with committed data.
        assert_eq!(a.allocate(None, 1), Some((8, 1)));
        assert_eq!(a.allocate(None, 10), Some((9, 10)));
        a.release_deferred().unwrap();
        // A run never crosses into a committed row.
        assert_eq!(a.allocate(Some(7), 3), Some((20, 3)));
        a.release_deferred().unwrap();
        // Freeing the whole row makes it usable again.
        a.free(4, 3).unwrap();
        a.release_deferred().unwrap();
        assert_eq!(a.allocate(Some(4), 4), Some((4, 4)));
        a.release_deferred().unwrap();
        // Exact runs start in a fresh row too.
        let r = a.allocate_run(Some(0), 6).unwrap();
        assert_eq!(r % 4, 0);
    }

    #[test]
    fn allocation_prefers_the_tier_asked_for() {
        let mut a = BlockAllocator::new(vec![
            AllocationGroup::new(0, 100),
            AllocationGroup::new(1, 100),
        ]);
        a.groups_mut()[1].tier = Tier::Fast;
        let (s, _) = a
            .allocate_tier(Some(10), 5, None, Some(Tier::Fast))
            .unwrap();
        assert!(s >= GROUP_BLOCKS);
        assert!(a.allocate_tier(None, 5, None, Some(Tier::Slow)).is_none());
        let r = a
            .allocate_run_tier(None, 5, None, Some(Tier::Fast))
            .unwrap();
        assert!(r >= GROUP_BLOCKS);
    }

    #[test]
    fn header_blocks_are_reserved() {
        let g = AllocationGroup::new(0, 100);
        assert_eq!(g.free_blocks, 98);
        assert!(g.is_used(0) && g.is_used(1) && !g.is_used(2));
    }

    #[test]
    fn contiguous_and_goal() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 1000)]);
        assert_eq!(a.allocate(None, 10), Some((2, 10)));
        assert_eq!(a.allocate(Some(12), 5), Some((12, 5)));
        a.free(5, 3).unwrap();
        // Goal points at the hole.
        assert_eq!(a.allocate(Some(5), 10), Some((5, 3)));
        // No goal: first free after the cursor's group start.
        assert_eq!(a.allocate(None, 4), Some((17, 4)));
        assert_eq!(a.free_blocks(), 998 - 19);
    }

    #[test]
    fn double_free_is_caught() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 100)]);
        let (s, n) = a.allocate(None, 4).unwrap();
        a.free(s, n).unwrap();
        assert!(a.free(s, 1).is_err());
        assert!(a.free(0, 1).is_err());
    }

    #[test]
    fn spills_into_next_group_and_fills_up() {
        let mut a = BlockAllocator::new(vec![
            AllocationGroup::new(0, GROUP_BLOCKS),
            AllocationGroup::new(1, 10),
        ]);
        let mut total = 0;
        while let Some((_, n)) = a.allocate(Some(GROUP_BLOCKS - 1), 1000) {
            total += n;
        }
        assert_eq!(total, GROUP_BLOCKS - 2 + 8);
        assert_eq!(a.free_blocks(), 0);
    }

    #[test]
    fn bitmap_roundtrip_and_extend() {
        let mut g = AllocationGroup::new(3, 64);
        g.allocate(0, 7).unwrap();
        let bytes = g.to_bytes();
        let back = AllocationGroup::from_bytes(3, 64, &bytes).unwrap();
        assert_eq!(back.free_blocks, 64 - 9);
        let mut back = back;
        back.extend(200);
        assert_eq!(back.block_count, 200);
        assert_eq!(back.free_blocks, 200 - 9);
        assert!(!back.is_used(150));
    }

    #[test]
    fn runs_cross_word_boundaries() {
        let mut g = AllocationGroup::new(0, 1000);
        // Blocks 2..60 used, so the first free run starts at 60 and must
        // continue through bit 63 into the next words.
        assert_eq!(g.allocate(0, 58), Some((2, 58)));
        assert_eq!(g.allocate(0, 200), Some((60, 200)));
        assert_eq!(g.free_blocks, 1000 - 260);
    }

    #[test]
    fn a_run_stops_at_the_first_used_block() {
        let mut g = AllocationGroup::new(0, 1000);
        g.allocate(0, 98).unwrap(); // 2..100
        g.free(50, 10).unwrap();
        assert_eq!(g.allocate(0, 100), Some((50, 10)));
    }

    #[test]
    fn search_wraps_around_to_the_start() {
        let mut g = AllocationGroup::new(0, 100);
        g.allocate(0, 98).unwrap();
        g.free(5, 1).unwrap();
        assert_eq!(g.allocate(90, 4), Some((5, 1)));
        assert_eq!(g.allocate(0, 4), None);
    }

    #[test]
    fn zero_request_allocates_nothing() {
        let mut g = AllocationGroup::new(0, 100);
        assert_eq!(g.allocate(0, 0), None);
        assert_eq!(g.free_blocks, 98);
    }

    #[test]
    fn the_tail_past_a_short_group_is_never_handed_out() {
        let mut g = AllocationGroup::new(0, 70);
        let (s, n) = g.allocate(0, 1000).unwrap();
        assert_eq!((s, n), (2, 68));
        assert_eq!(g.allocate(0, 1), None);
        assert!(g.free(70, 1).is_err());
    }

    #[test]
    fn a_full_group_is_exactly_one_bitmap_block() {
        let g = AllocationGroup::new(0, GROUP_BLOCKS);
        assert_eq!(g.to_bytes().len(), BLOCK_SIZE as usize);
        assert_eq!(g.free_blocks, GROUP_BLOCKS - 2);
        assert_eq!(g.bitmap_block(), 1);
        let g7 = AllocationGroup::new(7, 100);
        assert_eq!(g7.start_block, 7 * GROUP_BLOCKS);
        assert_eq!(g7.bitmap_block(), 7 * GROUP_BLOCKS + 1);
    }

    #[test]
    fn from_bytes_refuses_free_header_blocks_and_short_input() {
        let g = AllocationGroup::new(0, 100);
        let mut b = g.to_bytes();
        b[0] &= !0b10;
        assert!(AllocationGroup::from_bytes(0, 100, &b).is_err());
        assert!(AllocationGroup::from_bytes(0, 100, &b[..100]).is_err());
    }

    #[test]
    fn from_bytes_masks_a_tail_left_free_on_disk() {
        let mut b = vec![0u8; BLOCK_SIZE as usize];
        b[0] = 0b11;
        let g = AllocationGroup::from_bytes(0, 100, &b).unwrap();
        assert_eq!(g.free_blocks, 98);
    }

    #[test]
    fn dirty_tracking() {
        let mut g = AllocationGroup::new(0, 100);
        assert!(g.is_dirty());
        g.clear_dirty();
        g.allocate(0, 1).unwrap();
        assert!(g.is_dirty());
        g.clear_dirty();
        let back = AllocationGroup::from_bytes(0, 100, &g.to_bytes()).unwrap();
        assert!(!back.is_dirty());
    }

    #[test]
    fn extend_never_passes_a_whole_group_and_never_shrinks() {
        let mut g = AllocationGroup::new(0, 100);
        g.extend(50);
        assert_eq!(g.block_count, 100);
        g.extend(GROUP_BLOCKS * 3);
        assert_eq!(g.block_count, GROUP_BLOCKS);
        assert_eq!(g.free_blocks, GROUP_BLOCKS - 2);
    }

    #[test]
    fn freeing_a_range_that_spans_groups() {
        let mut a = BlockAllocator::new(vec![
            AllocationGroup::new(0, GROUP_BLOCKS),
            AllocationGroup::new(1, GROUP_BLOCKS),
        ]);
        // Use the tail of group 0 and the start of group 1 (after its header).
        let (s0, n0) = a.allocate(Some(GROUP_BLOCKS - 10), 10).unwrap();
        assert_eq!((s0, n0), (GROUP_BLOCKS - 10, 10));
        assert!(a.is_used(GROUP_BLOCKS));
        assert!(a.is_used(GROUP_BLOCKS + 1));
        let before = a.free_blocks();
        a.free(s0, n0).unwrap();
        assert_eq!(a.free_blocks(), before + 10);
        // Freeing over a header block is corruption.
        assert!(a.free(GROUP_BLOCKS - 1, 3).is_err());
    }

    #[test]
    fn goal_past_the_end_falls_back_to_the_cursor() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 100)]);
        assert_eq!(a.allocate(Some(1 << 40), 3), Some((2, 3)));
        assert!(a.is_used(1 << 40), "blocks past the end count as used");
        assert!(a.free(1 << 40, 1).is_err());
    }

    #[test]
    fn empty_allocator() {
        let mut a = BlockAllocator::default();
        assert_eq!(a.allocate(None, 1), None);
        assert_eq!(a.free_blocks(), 0);
        assert_eq!(a.total_blocks(), 0);
    }

    #[test]
    fn cursor_keeps_new_files_in_the_group_last_used() {
        let mut a = BlockAllocator::new(vec![
            AllocationGroup::new(0, 100),
            AllocationGroup::new(1, 100),
        ]);
        let (s, _) = a.allocate(Some(GROUP_BLOCKS + 50), 1).unwrap();
        assert_eq!(s, GROUP_BLOCKS + 50);
        let (t, _) = a.allocate(None, 1).unwrap();
        assert_eq!(t / GROUP_BLOCKS, 1, "goal-less allocation stays in group 1");
    }

    #[test]
    fn allocations_never_overlap() {
        let mut a = BlockAllocator::new(vec![AllocationGroup::new(0, 4000)]);
        let mut seen = alloc::collections::BTreeSet::new();
        let mut seed = 9u64;
        let mut live = Vec::new();
        for i in 0..2000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            if i % 3 == 2 && !live.is_empty() {
                let (s, n): (u64, u64) = live.swap_remove((seed >> 33) as usize % live.len());
                a.free(s, n).unwrap();
                for b in s..s + n {
                    seen.remove(&b);
                }
                continue;
            }
            if let Some((s, n)) = a.allocate(Some((seed >> 40) % 4000), 1 + (seed >> 50) % 8) {
                for b in s..s + n {
                    assert!(seen.insert(b), "block {} handed out twice", b);
                }
                live.push((s, n));
            }
        }
        assert_eq!(a.free_blocks(), 3998 - seen.len() as u64);
    }

    #[test]
    fn kinds_keep_data_and_metadata_apart() {
        let mut meta = AllocationGroup::new(0, 100);
        meta.kind = ChunkKind::Metadata;
        let mut data = AllocationGroup::new(2, 100);
        data.kind = ChunkKind::Data;
        let mut a = BlockAllocator::new(vec![meta]);
        a.set_group(data);
        // Group 1 is a hole: nothing there, everything in it "used".
        assert_eq!(a.groups().len(), 3);
        assert!(a.groups()[1].is_hole() && a.is_used(GROUP_BLOCKS + 5));
        assert_eq!(a.end(), 2 * GROUP_BLOCKS + 100);
        assert_eq!(a.total_blocks(), 200);
        let (d, _) = a.allocate_for(None, 10, Some(ChunkKind::Data)).unwrap();
        assert_eq!(d / GROUP_BLOCKS, 2);
        let (m, _) = a
            .allocate_for(Some(d), 10, Some(ChunkKind::Metadata))
            .unwrap();
        assert_eq!(m / GROUP_BLOCKS, 0);
        assert_eq!(a.free_in(ChunkKind::Data), 88);
        assert_eq!(a.free_in(ChunkKind::Metadata), 88);
        assert!(a
            .allocate_run_for(None, 89, Some(ChunkKind::Data))
            .is_none());
        // Frees past the old sum of group sizes are fine (logical space).
        a.set_deferred_frees(true);
        a.free(d, 10).unwrap();
        a.release_deferred().unwrap();
        assert_eq!(a.free_in(ChunkKind::Data), 98);
        assert_eq!(a.groups()[2].used_runs(), vec![(0, 2)]);
        // Dropping the last group trims the holes before it.
        a.set_group(AllocationGroup::hole(2));
        assert_eq!(a.groups().len(), 1);
    }

    /// Rows a transaction opens are kept as merged ranges: opening the rows
    /// between two open ranges joins all three into one.
    #[test]
    fn opened_rows_merge_with_both_neighbours() {
        let mut g = AllocationGroup::new(0, 256);
        g.set_row(4);
        let a = g.allocate_run(32, 4).unwrap();
        let b = g.allocate_run(48, 4).unwrap();
        assert_eq!((a, b), (32, 48));
        // Rows 9..12 sit between rows 8 and 12: one range 8..13 remains.
        assert_eq!(g.allocate_run(36, 12), Some(36));
        assert_eq!(
            g.open.iter().map(|(&s, &l)| (s, l)).collect::<Vec<_>>(),
            vec![(8, 5)]
        );
    }

    /// Enough free blocks in total but no run as long as asked: no run.
    #[test]
    fn a_fragmented_group_has_no_long_run() {
        let mut g = AllocationGroup::new(0, 64);
        let free = g.free_blocks;
        let (s, n) = g.allocate(0, free).unwrap();
        assert_eq!(n, free);
        for o in (s..s + n).step_by(2) {
            g.free(o, 1).unwrap();
        }
        assert!(g.free_blocks >= 2);
        assert_eq!(g.allocate_run(0, 2), None);
    }

    #[test]
    fn shrinking_needs_a_free_tail() {
        let mut g = AllocationGroup::new(0, 128);
        g.shrink(128).unwrap();
        g.shrink(500).unwrap();
        assert_eq!(g.block_count, 128);
        assert_eq!(g.shrink(1), Err(Error::Invalid));
        g.mark_used(100, 1);
        assert_eq!(g.shrink(64), Err(Error::Invalid));
        g.free(100, 1).unwrap();
        g.shrink(64).unwrap();
        assert_eq!(g.block_count, 64);
    }
}
