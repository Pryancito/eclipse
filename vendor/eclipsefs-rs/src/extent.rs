//! Extent-based block mapping (ext4/XFS), ported from eclipsefs-lib v0.3.0.
//!
//! The original kept extents in a `heapless::Vec<_, 16>` under no_std, which
//! capped every file at 16 extents in the kernel. Here the tree is an
//! `alloc::vec::Vec` kept sorted by logical block, looked up by binary search,
//! and has no per-file limit.

use alloc::vec::Vec;

use crate::{Error, Result};

/// Extent flags (on-disk values carried over from eclipsefs-lib).
pub const EXTENT_FLAG_UNWRITTEN: u16 = 0x0001;
pub const EXTENT_FLAG_COMPRESSED: u16 = 0x0002;
pub const EXTENT_FLAG_ENCRYPTED: u16 = 0x0004;

/// Bytes of one extent on disk.
pub const EXTENT_DISK_SIZE: usize = 24;

/// Longest run one extent can describe.
pub const MAX_EXTENT_BLOCKS: u64 = u32::MAX as u64;

/// A contiguous run of blocks: `length` logical blocks starting at
/// `logical_block` live at `physical_block..physical_block + length`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical_block: u64,
    pub physical_block: u64,
    pub length: u32,
    pub flags: u16,
}

impl Extent {
    pub fn new(logical_block: u64, physical_block: u64, length: u32) -> Self {
        Self {
            logical_block,
            physical_block,
            length,
            flags: 0,
        }
    }

    pub fn is_unwritten(&self) -> bool {
        self.flags & EXTENT_FLAG_UNWRITTEN != 0
    }

    /// End logical block (exclusive).
    pub fn logical_end(&self) -> u64 {
        self.logical_block + self.length as u64
    }

    /// End physical block (exclusive).
    pub fn physical_end(&self) -> u64 {
        self.physical_block + self.length as u64
    }

    /// `other` directly follows `self` in both logical and physical space.
    pub fn is_followed_by(&self, other: &Extent) -> bool {
        self.flags == other.flags
            && self.logical_end() == other.logical_block
            && self.physical_end() == other.physical_block
            && self.length as u64 + other.length as u64 <= MAX_EXTENT_BLOCKS
    }

    pub fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.logical_block.to_le_bytes());
        out[8..16].copy_from_slice(&self.physical_block.to_le_bytes());
        out[16..20].copy_from_slice(&self.length.to_le_bytes());
        out[20..22].copy_from_slice(&self.flags.to_le_bytes());
        out[22..24].fill(0);
    }

    pub fn decode(b: &[u8]) -> Self {
        Self {
            logical_block: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            physical_block: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            length: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            flags: u16::from_le_bytes(b[20..22].try_into().unwrap()),
        }
    }
}

/// Fragmentation figures for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtentStats {
    pub extent_count: usize,
    pub total_blocks: u64,
    pub largest_extent: u32,
}

/// Sorted, non-overlapping list of a file's extents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtentTree {
    extents: Vec<Extent>,
}

impl ExtentTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a list read off disk, checking it is sorted and disjoint.
    pub fn from_extents(extents: Vec<Extent>) -> Result<Self> {
        for w in extents.windows(2) {
            if w[0].logical_end() > w[1].logical_block {
                return Err(Error::Corrupt("overlapping extents"));
            }
        }
        if extents.iter().any(|e| e.length == 0) {
            return Err(Error::Corrupt("empty extent"));
        }
        Ok(Self { extents })
    }

    pub fn extents(&self) -> &[Extent] {
        &self.extents
    }

    pub fn extent_count(&self) -> usize {
        self.extents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.extents.is_empty()
    }

    pub fn total_blocks(&self) -> u64 {
        self.extents.iter().map(|e| e.length as u64).sum()
    }

    pub fn stats(&self) -> ExtentStats {
        ExtentStats {
            extent_count: self.extents.len(),
            total_blocks: self.total_blocks(),
            largest_extent: self.extents.iter().map(|e| e.length).max().unwrap_or(0),
        }
    }

    /// Index of the extent containing `lb`, if any.
    fn index_of(&self, lb: u64) -> Option<usize> {
        // First extent that starts after `lb`; the candidate is the one before.
        let after = self.extents.partition_point(|e| e.logical_block <= lb);
        let i = after.checked_sub(1)?;
        (lb < self.extents[i].logical_end()).then_some(i)
    }

    pub fn find_extent(&self, lb: u64) -> Option<&Extent> {
        self.index_of(lb).map(|i| &self.extents[i])
    }

    /// Physical block of logical block `lb` and how many blocks of the same
    /// extent follow it (itself included).
    pub fn lookup(&self, lb: u64) -> Option<(u64, u64)> {
        let e = self.find_extent(lb)?;
        let off = lb - e.logical_block;
        Some((e.physical_block + off, e.length as u64 - off))
    }

    pub fn logical_to_physical(&self, lb: u64) -> Option<u64> {
        self.lookup(lb).map(|(p, _)| p)
    }

    /// Logical start of the first extent beginning after `lb`.
    pub fn next_start_after(&self, lb: u64) -> Option<u64> {
        let i = self.extents.partition_point(|e| e.logical_block <= lb);
        self.extents.get(i).map(|e| e.logical_block)
    }

    /// Where an allocation for logical block `lb` should ideally land: right
    /// after the physical end of the nearest extent before it.
    pub fn goal_for(&self, lb: u64) -> Option<u64> {
        let i = self.extents.partition_point(|e| e.logical_block <= lb);
        let prev = self.extents[..i].last()?;
        Some(prev.physical_end() + lb.saturating_sub(prev.logical_end()))
    }

    /// Insert an extent that overlaps none of the existing ones, merging with
    /// its neighbours when they are contiguous.
    pub fn add_extent(&mut self, ext: Extent) -> Result<()> {
        if ext.length == 0 {
            return Err(Error::Invalid);
        }
        let i = self
            .extents
            .partition_point(|e| e.logical_block < ext.logical_block);
        if i > 0 && self.extents[i - 1].logical_end() > ext.logical_block {
            return Err(Error::Corrupt("extent overlaps its predecessor"));
        }
        if i < self.extents.len() && ext.logical_end() > self.extents[i].logical_block {
            return Err(Error::Corrupt("extent overlaps its successor"));
        }
        let merge_prev = i > 0 && self.extents[i - 1].is_followed_by(&ext);
        let merge_next = i < self.extents.len() && ext.is_followed_by(&self.extents[i]);
        match (merge_prev, merge_next) {
            (true, true)
                if (self.extents[i - 1].length as u64
                    + ext.length as u64
                    + self.extents[i].length as u64)
                    <= MAX_EXTENT_BLOCKS =>
            {
                let next = self.extents.remove(i);
                self.extents[i - 1].length += ext.length + next.length;
            }
            (true, _) => self.extents[i - 1].length += ext.length,
            (false, true) => {
                let next = &mut self.extents[i];
                next.logical_block = ext.logical_block;
                next.physical_block = ext.physical_block;
                next.length += ext.length;
            }
            (false, false) => self.extents.insert(i, ext),
        }
        Ok(())
    }

    /// Drop every mapping at or past logical block `lb`, returning the freed
    /// physical runs as `(start, len)`.
    pub fn truncate_from(&mut self, lb: u64) -> Vec<(u64, u64)> {
        let mut freed = Vec::new();
        let first = self.extents.partition_point(|e| e.logical_end() <= lb);
        for e in self.extents.drain(first..).collect::<Vec<_>>() {
            if e.logical_block >= lb {
                freed.push((e.physical_block, e.length as u64));
            } else {
                let keep = lb - e.logical_block;
                freed.push((e.physical_block + keep, e.length as u64 - keep));
                let mut kept = e;
                kept.length = keep as u32;
                self.extents.push(kept);
            }
        }
        freed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_and_holes() {
        let mut t = ExtentTree::new();
        t.add_extent(Extent::new(0, 100, 10)).unwrap();
        t.add_extent(Extent::new(20, 500, 5)).unwrap();
        assert_eq!(t.lookup(0), Some((100, 10)));
        assert_eq!(t.lookup(9), Some((109, 1)));
        assert_eq!(t.lookup(10), None);
        assert_eq!(t.lookup(22), Some((502, 3)));
        assert_eq!(t.next_start_after(10), Some(20));
        assert_eq!(t.next_start_after(20), None);
        assert_eq!(t.goal_for(12), Some(112));
    }

    #[test]
    fn merging_both_ways() {
        let mut t = ExtentTree::new();
        t.add_extent(Extent::new(0, 100, 10)).unwrap();
        t.add_extent(Extent::new(20, 120, 10)).unwrap();
        t.add_extent(Extent::new(10, 110, 10)).unwrap();
        assert_eq!(t.extents(), &[Extent::new(0, 100, 30)]);
        // Logically contiguous but physically not: no merge.
        t.add_extent(Extent::new(30, 900, 1)).unwrap();
        assert_eq!(t.extent_count(), 2);
        assert_eq!(t.total_blocks(), 31);
    }

    #[test]
    fn overlap_is_refused() {
        let mut t = ExtentTree::new();
        t.add_extent(Extent::new(5, 100, 10)).unwrap();
        assert!(t.add_extent(Extent::new(0, 0, 6)).is_err());
        assert!(t.add_extent(Extent::new(14, 0, 1)).is_err());
        assert!(t.add_extent(Extent::new(15, 0, 1)).is_ok());
    }

    #[test]
    fn truncate_splits() {
        let mut t = ExtentTree::new();
        t.add_extent(Extent::new(0, 100, 10)).unwrap();
        t.add_extent(Extent::new(20, 300, 10)).unwrap();
        let freed = t.truncate_from(5);
        assert_eq!(freed, alloc::vec![(105, 5), (300, 10)]);
        assert_eq!(t.extents(), &[Extent::new(0, 100, 5)]);
        assert!(t.truncate_from(5).is_empty());
        assert_eq!(t.truncate_from(0), alloc::vec![(100, 5)]);
        assert!(t.is_empty());
    }

    #[test]
    fn disk_roundtrip() {
        let mut e = Extent::new(7, 1 << 40, 99);
        e.flags = EXTENT_FLAG_UNWRITTEN;
        let mut b = [0u8; EXTENT_DISK_SIZE];
        e.encode(&mut b);
        assert_eq!(Extent::decode(&b), e);
        assert!(Extent::decode(&b).is_unwritten());
    }
}
