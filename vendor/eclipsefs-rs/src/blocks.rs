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
}

impl BlockAllocator {
    pub fn new(groups: Vec<AllocationGroup>) -> Self {
        Self { groups, cursor: 0 }
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

    /// Free `len` blocks starting at absolute block `start`.
    pub fn free(&mut self, start: u64, len: u64) -> Result<()> {
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
}
