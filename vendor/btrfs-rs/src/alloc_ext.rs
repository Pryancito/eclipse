//! In-memory extent allocator.
//!
//! At mount time the extent tree is scanned once to build a free-space map
//! (per block group) and a device-extent map. Allocations and frees update
//! the in-memory state immediately and record *pending* extent-tree edits,
//! which the filesystem layer applies after the triggering tree mutation has
//! finished (extent-tree edits may themselves split tree blocks and allocate
//! more — the queue makes that convergent instead of recursive).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::structs::*;
use crate::{Error, Result};

/// Deferred extent-tree bookkeeping.
#[derive(Debug, Clone)]
pub enum PendingOp {
    AddMeta {
        bytenr: u64,
        owner: u64,
        level: u8,
    },
    DelMeta {
        bytenr: u64,
        owner: u64,
        level: u8,
    },
    AddData {
        bytenr: u64,
        len: u64,
        root: u64,
        objectid: u64,
        offset: u64,
    },
    DelData {
        bytenr: u64,
        len: u64,
        root: u64,
        objectid: u64,
        offset: u64,
    },
}

#[derive(Debug, Clone)]
pub struct BlockGroup {
    pub start: u64,
    pub len: u64,
    pub flags: u64,
    pub used: u64,
    pub dirty: bool,
}

/// Free-range map: start → len, non-overlapping, coalesced.
#[derive(Default)]
pub struct RangeMap {
    map: BTreeMap<u64, u64>,
}

impl RangeMap {
    pub fn insert(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut start = start;
        let mut end = start + len;
        // Merge with predecessor.
        if let Some((&ps, &pl)) = self.map.range(..=start).next_back() {
            if ps + pl >= start {
                start = ps;
                end = end.max(ps + pl);
                self.map.remove(&ps);
            }
        }
        // Merge with successors.
        while let Some((&ns, &nl)) = self.map.range(start..).next() {
            if ns > end {
                break;
            }
            end = end.max(ns + nl);
            self.map.remove(&ns);
        }
        self.map.insert(start, end - start);
    }

    /// Remove `[start, start+len)` from the free map (must be fully free).
    pub fn take(&mut self, start: u64, len: u64) -> Result<()> {
        let (&rs, &rl) = self.map.range(..=start).next_back().ok_or(Error::NoSpace)?;
        if rs > start || rs + rl < start + len {
            return Err(Error::NoSpace);
        }
        self.map.remove(&rs);
        if rs < start {
            self.map.insert(rs, start - rs);
        }
        if start + len < rs + rl {
            self.map.insert(start + len, rs + rl - (start + len));
        }
        Ok(())
    }

    /// Carve a free range of exactly `len` bytes within `[lo, hi)`, aligned to
    /// `align`. Returns its start.
    ///
    /// Iterates via `ranges_overlapping`, which also yields the at-most-one
    /// free range that *starts before* `lo` and extends into the window. Free
    /// ranges are coalesced across block-group boundaries, and block groups
    /// are laid out back to back (mkfs puts SYSTEM, METADATA and DATA next to
    /// each other, and `create_chunk` places every new one at `logical_end()`),
    /// so a block group whose head is free is routinely covered by a range
    /// keyed *inside its predecessor*. Keying the scan on `range(lo..hi)`
    /// missed exactly that range, and a brand-new chunk -- entirely free, and
    /// adjacent to a predecessor with a free tail -- looked like it had no
    /// usable space at all.
    pub fn alloc_in(&mut self, lo: u64, hi: u64, len: u64, align: u64) -> Option<u64> {
        let start = self.ranges_overlapping(lo, hi).find_map(|(rs, rl)| {
            let start = rs.max(lo).checked_next_multiple_of(align)?;
            let end = (rs + rl).min(hi);
            (start.checked_add(len)? <= end).then_some(start)
        })?;
        self.take(start, len).ok()?;
        Some(start)
    }

    /// Largest free range within `[lo, hi)`, if any: (start, len).
    pub fn largest_in(&self, lo: u64, hi: u64) -> Option<(u64, u64)> {
        let mut best: Option<(u64, u64)> = None;
        for (rs, rl) in self.ranges_overlapping(lo, hi) {
            let s = rs.max(lo);
            let e = (rs + rl).min(hi);
            if e > s && best.is_none_or(|(_, bl)| e - s > bl) {
                best = Some((s, e - s));
            }
        }
        best
    }

    pub fn total_free_in(&self, lo: u64, hi: u64) -> u64 {
        self.ranges_overlapping(lo, hi)
            .map(|(rs, rl)| {
                let s = rs.max(lo);
                let e = (rs + rl).min(hi);
                e.saturating_sub(s)
            })
            .sum()
    }

    /// Iterate only the free ranges that can overlap `[lo, hi)`: the at-most-one
    /// range that starts before `lo` but extends into it, followed by every
    /// range starting within `[lo, hi)`. This is `O(log n + k)` in the number
    /// of overlapping ranges `k`, instead of scanning the whole map up to `hi`
    /// (which made per-block-group queries like `meta_free` cost `O(n)` and the
    /// surrounding per-mutation checks `O(n^2)` as free space fragmented).
    fn ranges_overlapping(&self, lo: u64, hi: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        let straddler = self
            .map
            .range(..lo)
            .next_back()
            .and_then(|(&rs, &rl)| (rs + rl > lo).then_some((rs, rl)));
        straddler
            .into_iter()
            .chain(self.map.range(lo..hi).map(|(&rs, &rl)| (rs, rl)))
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.map.iter().map(|(&s, &l)| (s, l))
    }
}

#[derive(Default)]
pub struct FreeSpace {
    pub nodesize: u64,
    pub sectorsize: u64,
    /// Logical free space (within block groups).
    pub free: RangeMap,
    /// Block groups keyed by logical start.
    pub bgs: BTreeMap<u64, BlockGroup>,
    /// Physical free space on the single device.
    pub dev_free: RangeMap,
    pub pending: Vec<PendingOp>,
    /// Net change to superblock `bytes_used` not yet flushed.
    pub bytes_used_delta: i64,
    /// Net change to dev_item `bytes_used` (chunk allocation) not yet flushed.
    pub dev_used_delta: i64,
}

impl FreeSpace {
    fn bg_for(&mut self, bytenr: u64) -> Result<&mut BlockGroup> {
        let (_, bg) = self
            .bgs
            .range_mut(..=bytenr)
            .next_back()
            .ok_or(Error::Corrupt("no block group"))?;
        if bytenr >= bg.start + bg.len {
            return Err(Error::Corrupt("address outside block groups"));
        }
        Ok(bg)
    }

    /// Allocate one tree block for `owner` from a block group matching
    /// `flags` (METADATA, SYSTEM, or mixed).
    pub fn alloc_tree_block(&mut self, owner: u64, level: u8, flags: u64) -> Result<u64> {
        let nodesize = self.nodesize;
        let bytenr = self
            .alloc_range(flags, nodesize, nodesize)
            .ok_or(Error::NoSpace)?;
        self.account(bytenr, nodesize, 1)?;
        self.pending.push(PendingOp::AddMeta {
            bytenr,
            owner,
            level,
        });
        Ok(bytenr)
    }

    pub fn free_tree_block(
        &mut self,
        bytenr: u64,
        owner: u64,
        level: u8,
        _flags: u64,
    ) -> Result<()> {
        let nodesize = self.nodesize;
        self.free.insert(bytenr, nodesize);
        self.account(bytenr, nodesize, -1)?;
        self.pending.push(PendingOp::DelMeta {
            bytenr,
            owner,
            level,
        });
        Ok(())
    }

    /// Allocate up to `want` bytes of contiguous DATA space (at least
    /// `sectorsize`). Returns (bytenr, got).
    pub fn alloc_data(&mut self, want: u64) -> Result<(u64, u64)> {
        let want = want.max(self.sectorsize);
        // Try a contiguous allocation first, then fall back to the largest
        // available range in any data block group.
        if let Some(bytenr) = self.alloc_range(BLOCK_GROUP_DATA, want, self.sectorsize) {
            self.account(bytenr, want, 1)?;
            return Ok((bytenr, want));
        }
        let mut best: Option<(u64, u64)> = None;
        for bg in self.bgs.values() {
            if bg.flags & BLOCK_GROUP_DATA == 0 {
                continue;
            }
            if let Some((s, l)) = self.free.largest_in(bg.start, bg.start + bg.len) {
                if best.is_none_or(|(_, bl)| l > bl) {
                    best = Some((s, l));
                }
            }
        }
        let (start, len) = best.ok_or(Error::NoSpace)?;
        let len = len.min(want) / self.sectorsize * self.sectorsize;
        if len == 0 {
            return Err(Error::NoSpace);
        }
        self.free.take(start, len)?;
        self.account(start, len, 1)?;
        Ok((start, len))
    }

    /// Return a just-allocated (but not yet recorded) data range to the free
    /// pool — used to back out of multi-extent reservations on ENOSPC.
    pub fn unreserve_data(&mut self, bytenr: u64, len: u64) -> Result<()> {
        self.free.insert(bytenr, len);
        self.account(bytenr, len, -1)
    }

    pub fn note_data_extent(
        &mut self,
        bytenr: u64,
        len: u64,
        root: u64,
        objectid: u64,
        offset: u64,
    ) {
        self.pending.push(PendingOp::AddData {
            bytenr,
            len,
            root,
            objectid,
            offset,
        });
    }

    pub fn free_data(
        &mut self,
        bytenr: u64,
        len: u64,
        root: u64,
        objectid: u64,
        offset: u64,
    ) -> Result<()> {
        self.free.insert(bytenr, len);
        self.account(bytenr, len, -1)?;
        self.pending.push(PendingOp::DelData {
            bytenr,
            len,
            root,
            objectid,
            offset,
        });
        Ok(())
    }

    fn alloc_range(&mut self, flags: u64, len: u64, align: u64) -> Option<u64> {
        let bgs: Vec<(u64, u64)> = self
            .bgs
            .values()
            .filter(|bg| bg.flags & flags != 0)
            .map(|bg| (bg.start, bg.len))
            .collect();
        for (start, bg_len) in bgs {
            if let Some(b) = self.free.alloc_in(start, start + bg_len, len, align) {
                return Some(b);
            }
        }
        None
    }

    fn account(&mut self, bytenr: u64, len: u64, sign: i64) -> Result<()> {
        let bg = self.bg_for(bytenr)?;
        if sign > 0 {
            bg.used += len;
        } else {
            bg.used = bg.used.saturating_sub(len);
        }
        bg.dirty = true;
        self.bytes_used_delta += sign * len as i64;
        Ok(())
    }

    /// Free METADATA (or mixed) bytes still available.
    pub fn meta_free(&self) -> u64 {
        self.free_in_groups(BLOCK_GROUP_METADATA)
    }

    pub fn data_free(&self) -> u64 {
        self.free_in_groups(BLOCK_GROUP_DATA)
    }

    /// Total free bytes across every block group whose flags intersect `flags`.
    ///
    /// Uses each block group's accounted `used` counter (`len - used`) instead
    /// of summing the free-range fragments inside it. Both are kept in lock-step
    /// by `account()`, but the fragment sum is `O(fragments)` while this is
    /// `O(1)` per group. The fragment form turned every `prepare_mutation`
    /// (which calls `meta_free`/system-free on *each* write) into `O(n^2)` as
    /// the metadata block group's free list shattered while extracting a large
    /// file — the "hang" partway through writing `libLLVM.so`, with `df`
    /// blocking behind the held filesystem lock.
    pub fn free_in_groups(&self, flags: u64) -> u64 {
        self.bgs
            .values()
            .filter(|bg| bg.flags & flags != 0)
            .map(|bg| bg.len.saturating_sub(bg.used))
            .sum()
    }

    /// Highest logical address covered by any block group.
    pub fn logical_end(&self) -> u64 {
        self.bgs
            .values()
            .map(|bg| bg.start + bg.len)
            .max()
            .unwrap_or(0)
    }

    pub fn take_pending(&mut self) -> Vec<PendingOp> {
        core::mem::take(&mut self.pending)
    }

    /// Dirty block groups (start, len, item) — clears the dirty flags.
    pub fn take_dirty_bgs(&mut self) -> Vec<(u64, u64, BlockGroupItem)> {
        let mut out = Vec::new();
        for bg in self.bgs.values_mut() {
            if bg.dirty {
                bg.dirty = false;
                out.push((
                    bg.start,
                    bg.len,
                    BlockGroupItem {
                        used: bg.used,
                        flags: bg.flags,
                    },
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Block groups sit back to back and free ranges coalesce across their
    /// boundaries, so the free space at the head of a block group is very
    /// often described by a range *keyed inside its predecessor*. Every
    /// accessor has to look at that straddling range, not just at the ranges
    /// whose key falls inside the window.
    #[test]
    fn alloc_in_sees_a_range_that_starts_before_the_window() {
        // bg A = [0, 0x1000), bg B = [0x1000, 0x2000). A's tail is free and
        // all of B is free, so both are one coalesced range keyed at 0x800.
        let mut m = RangeMap::default();
        m.insert(0x800, 0x1800);
        assert_eq!(m.iter().collect::<Vec<_>>(), alloc::vec![(0x800, 0x1800)]);
        // The three accessors must agree that B is entirely free.
        assert_eq!(m.total_free_in(0x1000, 0x2000), 0x1000);
        assert_eq!(m.largest_in(0x1000, 0x2000), Some((0x1000, 0x1000)));
        assert_eq!(m.alloc_in(0x1000, 0x2000, 0x400, 0x400), Some(0x1000));
        assert_eq!(m.total_free_in(0x1000, 0x2000), 0xc00);
    }

    /// The whole window may be covered by the straddler, with nothing keyed
    /// inside it at all -- a brand-new chunk placed right after a block group
    /// with a free tail.
    #[test]
    fn alloc_in_can_fill_a_window_covered_only_by_the_straddler() {
        let mut m = RangeMap::default();
        m.insert(0, 0x4000);
        // Consume all of [0x1000, 0x2000) one aligned block at a time.
        for i in 0..4 {
            assert_eq!(
                m.alloc_in(0x1000, 0x2000, 0x400, 0x400),
                Some(0x1000 + i * 0x400),
                "block {} of the window",
                i
            );
        }
        assert_eq!(m.alloc_in(0x1000, 0x2000, 0x400, 0x400), None);
        // The rest of the coalesced range is untouched.
        assert_eq!(m.total_free_in(0, 0x1000), 0x1000);
        assert_eq!(m.total_free_in(0x2000, 0x4000), 0x2000);
    }

    /// Alignment is applied to the clipped start, and a range that reaches
    /// into the window but leaves too little aligned room is skipped in
    /// favour of a later one.
    #[test]
    fn alloc_in_aligns_the_clipped_start_and_skips_short_ranges() {
        let mut m = RangeMap::default();
        // Straddles into the window but only by 0x80 bytes.
        m.insert(0xf80, 0x100);
        m.insert(0x1800, 0x800);
        assert_eq!(m.alloc_in(0x1000, 0x2000, 0x400, 0x400), Some(0x1800));
        // An unaligned range start is rounded up inside its own extent.
        let mut m = RangeMap::default();
        m.insert(0x1100, 0x900);
        assert_eq!(m.alloc_in(0x1000, 0x2000, 0x400, 0x400), Some(0x1400));
    }

    /// A range must not be handed out beyond `hi`, even though it continues
    /// past it in the map.
    #[test]
    fn alloc_in_never_crosses_the_upper_bound() {
        let mut m = RangeMap::default();
        m.insert(0, 0x8000);
        assert_eq!(m.alloc_in(0x1000, 0x1400, 0x800, 0x400), None);
        assert_eq!(m.alloc_in(0x1000, 0x1400, 0x400, 0x400), Some(0x1000));
    }

    /// `dev_free.alloc_in(0, u64::MAX, ..)` is a real call site and `want`
    /// reaches `create_chunk` straight from the caller, so an absurd length
    /// must come back as "no space" rather than wrap around into a hit.
    #[test]
    fn alloc_in_refuses_an_absurd_length_instead_of_wrapping() {
        let mut m = RangeMap::default();
        m.insert(0x1000, 0x1000);
        assert_eq!(m.alloc_in(0, u64::MAX, u64::MAX, 0x1000), None);
        assert_eq!(m.alloc_in(0, u64::MAX, u64::MAX - 0xfff, 0x1000), None);
        assert_eq!(m.total_free_in(0, u64::MAX), 0x1000);
        assert_eq!(m.alloc_in(0, u64::MAX, 0x1000, 0x1000), Some(0x1000));
    }

    fn bgs(groups: &[(u64, u64, u64)]) -> FreeSpace {
        let mut fs = FreeSpace {
            nodesize: 0x4000,
            sectorsize: 0x1000,
            ..Default::default()
        };
        for &(start, len, flags) in groups {
            fs.bgs.insert(
                start,
                BlockGroup {
                    start,
                    len,
                    flags,
                    used: 0,
                    dirty: false,
                },
            );
            fs.free.insert(start, len);
        }
        fs
    }

    /// The bug as the filesystem hits it: a fresh metadata chunk laid down
    /// immediately after a data block group with a free tail. Every byte of
    /// it is free and `meta_free()` says so, yet allocating one tree block
    /// out of it used to fail -- ENOSPC on a nearly empty filesystem.
    #[test]
    fn a_fresh_chunk_after_a_half_used_neighbour_is_usable() {
        let mut fs = bgs(&[
            (0x10_0000, 0x40_0000, BLOCK_GROUP_DATA),
            (0x50_0000, 0x40_0000, BLOCK_GROUP_METADATA),
        ]);
        // Half the data group is in use; its tail coalesces with the new
        // metadata group into a single free range keyed inside the data one.
        fs.free.take(0x10_0000, 0x20_0000).unwrap();
        fs.account(0x10_0000, 0x20_0000, 1).unwrap();
        assert_eq!(fs.free.iter().count(), 1);
        assert_eq!(fs.meta_free(), 0x40_0000);

        let bytenr = fs.alloc_tree_block(FS_TREE, 0, BLOCK_GROUP_METADATA).unwrap();
        assert_eq!(bytenr, 0x50_0000);
        assert_eq!(fs.meta_free(), 0x40_0000 - 0x4000);
    }

    /// `meta_free()` (`len - used`, per block group) and what the free-range
    /// map will actually hand out must not disagree: the whole metadata group
    /// has to be allocatable, one node at a time, until `meta_free()` is zero.
    #[test]
    fn metadata_can_be_allocated_down_to_the_last_node() {
        let mut fs = bgs(&[
            (0x10_0000, 0x10_0000, BLOCK_GROUP_DATA),
            (0x20_0000, 0x4_0000, BLOCK_GROUP_METADATA),
        ]);
        let mut n = 0;
        while fs.meta_free() > 0 {
            fs.alloc_tree_block(FS_TREE, 0, BLOCK_GROUP_METADATA)
                .unwrap_or_else(|e| panic!("node {} of a group reporting free space: {:?}", n, e));
            n += 1;
        }
        assert_eq!(n, 0x4_0000 / 0x4000);
        assert!(fs.alloc_tree_block(FS_TREE, 0, BLOCK_GROUP_METADATA).is_err());
        // The neighbouring data group is untouched.
        assert_eq!(fs.data_free(), 0x10_0000);
    }
}
