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

use alloc::vec;
use alloc::vec::Vec;

use crate::format::{BLOCK_SIZE, GROUP_BLOCKS};
use crate::{Error, Result};

const WORDS: usize = (GROUP_BLOCKS / 64) as usize;

/// One allocation group and its bitmap.
#[derive(Debug, Clone)]
pub struct AllocationGroup {
    pub id: u32,
    pub start_block: u64,
    pub block_count: u64,
    pub free_blocks: u64,
    bitmap: Vec<u64>,
    dirty: bool,
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
            bitmap: vec![0; WORDS],
            dirty: true,
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
            bitmap,
            dirty: false,
        };
        g.mark_tail_used();
        if !g.is_used(0) || !g.is_used(1) {
            return Err(Error::Corrupt("group header blocks marked free"));
        }
        g.free_blocks = g.count_free();
        Ok(g)
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

    /// First free block at or after `from` (group-relative).
    fn next_free(&self, from: u64) -> Option<u64> {
        let mut off = from;
        while off < self.block_count {
            let w = self.bitmap[(off / 64) as usize] | ((1u64 << (off % 64)) - 1);
            if w == u64::MAX {
                off = (off / 64 + 1) * 64;
                continue;
            }
            let found = (off / 64) * 64 + (!w).trailing_zeros() as u64;
            return (found < self.block_count).then_some(found);
        }
        None
    }

    /// Length of the free run starting at `off`, capped at `max`.
    fn run_len(&self, off: u64, max: u64) -> u64 {
        let mut len = 0;
        while len < max && off + len < self.block_count && !self.is_used(off + len) {
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
            .next_free(from.min(self.block_count))
            .or_else(|| self.next_free(0))?;
        let len = self.run_len(start, max);
        self.set_range(start, len, true);
        Some((start, len))
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
    deferred: Vec<(u64, u64)>,
    deferred_blocks: u64,
}

impl BlockAllocator {
    pub fn new(groups: Vec<AllocationGroup>) -> Self {
        Self {
            groups,
            cursor: 0,
            defer: false,
            deferred: Vec::new(),
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
            let from = if i == 0 { from } else { 0 };
            if let Some((off, len)) = g.allocate(from, max) {
                self.cursor = gi;
                return Some((g.start_block + off, len));
            }
        }
        None
    }

    /// Free `len` blocks starting at absolute block `start` (or queue them,
    /// see [`set_deferred_frees`](Self::set_deferred_frees)).
    pub fn free(&mut self, start: u64, len: u64) -> Result<()> {
        if !self.defer {
            return self.free_now(start, len);
        }
        let end = start.checked_add(len).ok_or(Error::Corrupt("free range"))?;
        if (start..end).any(|b| !self.is_used(b)) || end > self.total_blocks() {
            return Err(Error::Corrupt("double free"));
        }
        if self.deferred.iter().any(|&(s, l)| start < s + l && s < end) {
            return Err(Error::Corrupt("double free"));
        }
        self.deferred.push((start, len));
        self.deferred_blocks += len;
        Ok(())
    }

    fn free_now(&mut self, start: u64, len: u64) -> Result<()> {
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
}
