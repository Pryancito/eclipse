//! The address-window allocator behind PCI bus enumeration.
//!
//! One of these guards each of the four address spaces a PCI upstream node
//! hands out: 32-bit MMIO, 64-bit MMIO, prefetchable MMIO and port I/O. It
//! holds the **free** space only, as a set of disjoint half-open intervals
//! `[base, base + size)` kept in ascending order, and [`RegionAllocator::add`]
//! merges anything that touches -- so a window that is entirely free is always
//! exactly one region, never two that meet. Every query here relies on that:
//! they look for *one* region that contains the window asked about.
//!
//! `zircon-object` drives it from two directions. A device whose BAR the
//! firmware already placed calls [`RegionAllocator::allocate_by_addr`] to keep
//! that placement, and one with no address yet calls
//! [`RegionAllocator::allocate_by_size`]; a bridge does both in turn, taking
//! its window out of its parent's free space with `allocate_by_addr` and then
//! `add`ing the same window to its own list for the devices below it. A `false`
//! from `allocate_by_addr` becomes `ZxError::NO_MEMORY` and the bridge ends up
//! giving its children no BARs at all, so a query that answers "not free" about
//! free space costs a whole branch of the bus its registers.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::cmp::{max, min};

#[derive(Eq, Copy, Clone, Debug, Ord, PartialEq, PartialOrd)]
struct Region {
    base: usize,
    size: usize,
}

impl Region {
    /// A region is the half-open interval `[base, base + size)`, so its end has
    /// to be an address that exists.
    ///
    /// Every `base + size` in this file used to be unchecked, and the kernel is
    /// built `release` with no `overflow-checks`: a window whose end carried
    /// past `usize::MAX` wrapped round to a small number instead of panicking,
    /// and `base + size <= region.base + region.size` then compared a wrapped
    /// end against a real one and said yes. So `allocate_by_addr` answered
    /// `true` -- "reserved" -- while `subtract` matched nothing and removed
    /// nothing: the window stayed free and the next device was handed the same
    /// one. A 64-bit prefetchable bridge window is read straight out of config
    /// space as `limit - base + 1`, so the numbers come from the hardware.
    ///
    /// Clamped rather than refused: dropping a whole window because its last
    /// byte is unrepresentable would cost far more than the byte. The
    /// consequence is that `usize::MAX` itself is in no region, which is why
    /// [`RegionAllocator::check_point`] answers `false` for it.
    fn new(base: usize, size: usize) -> Self {
        Self {
            base,
            size: min(size, usize::MAX - base),
        }
    }

    /// One past the last address of the region. Cannot overflow, because
    /// [`Region::new`] is the only place a region is given its size.
    fn end(&self) -> usize {
        self.base + self.size
    }
}

/// An endpoint-based region allocator.
#[derive(Default)]
pub struct RegionAllocator {
    regions: BTreeSet<Region>,
}

impl RegionAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish `[base, base + size)` as free, merging it with whatever it
    /// touches.
    pub fn add(&mut self, base: usize, size: usize) {
        let mut new_region = Region::new(base, size);
        if new_region.size == 0 {
            // Not a region: it holds no address. Inserting one used to leave a
            // zero-size entry in the set for `allocate_by_size` to "satisfy" a
            // zero-size request from, and for `subtract` to split a window on.
            return;
        }
        for region in self.intersection_all(&new_region) {
            // `merge_internal` cannot answer `Some` here, and putting the
            // region back is the only safe thing to do if it ever does.
            // `intersection_all` keeps a candidate exactly when it overlaps or
            // touches `new_region`, which is the negation of the test
            // `merge_internal` refuses on, and `new_region` only grows as the
            // loop merges -- so the branch is dead by construction, not by
            // luck. Deleting it would turn a loosened predicate upstream into
            // free space that silently disappears from the set.
            if let Some(region) = Self::merge_internal(&mut new_region, region) {
                self.regions.insert(region);
            }
        }
        self.regions.insert(new_region);
    }

    /// Take `[base, base + size)` out of the free space, whether or not all of
    /// it was free.
    pub fn subtract(&mut self, base: usize, size: usize) {
        let new_region = Region::new(base, size);
        if new_region.size == 0 {
            // Nothing to remove -- and this was the worst line in the file.
            // `intersection_all` extracts every region that *touches* the
            // argument, and `subtract_internal` hands back the part below it
            // and the part above it; for a zero-size argument inside a free
            // window those two parts are halves that meet, so the window came
            // back as two regions with nothing between them. Every query here
            // asks for **one** region containing the window, because `add`
            // keeps touching regions out of the set -- so from that moment on a
            // window with every byte free was refused, and the bridge that
            // asked for it got `NO_MEMORY`.
            return;
        }
        for region in self.intersection_all(&new_region) {
            let (left, right) = Self::subtract_internal(region, &new_region);
            self.regions.extend(left);
            self.regions.extend(right);
        }
    }

    pub fn add_or_subtract(&mut self, base: usize, size: usize, is_add: bool) {
        if is_add {
            self.add(base, size);
        } else {
            self.subtract(base, size);
        }
    }

    /// Reserve the window the firmware already programmed, or answer `false`
    /// and reserve nothing.
    pub fn allocate_by_addr(&mut self, base: usize, size: usize) -> bool {
        if !self.check_region(base, size) {
            return false;
        }
        self.subtract(base, size);
        true
    }

    /// Reserve `size` bytes at an `alignment`-aligned address, lowest first.
    pub fn allocate_by_size(&mut self, size: usize, alignment: usize) -> Option<(usize, usize)> {
        if size == 0 || !alignment.is_power_of_two() {
            return None;
        }
        let align = alignment - 1;
        let base = self.regions.iter().find_map(|region| {
            if size > region.size {
                return None;
            }
            // Rounding up, then the end, both checked: a `?` here skips this
            // region, where the wrapped sum used to pass the fit test and hand
            // out a window that runs off the end of the address space. The
            // rounded-up base can never be *below* `region.base`, so the guard
            // that used to stand in for these checks is gone with them.
            let base = region.base.checked_add(align)? & !align;
            let end = base.checked_add(size)?;
            (end <= region.end()).then_some(base)
        })?;
        self.subtract(base, size);
        Some((base, size))
    }

    /// Whether every address in `[base, base + size)` is free.
    ///
    /// The one predicate in this file, so the answers cannot disagree:
    /// `allocate_by_addr` used to carry its own copy of this test while
    /// `check_region` answered the same question with `BTreeSet::contains`,
    /// which is an **exact** match on `(base, size)`. For a free window of
    /// 0x2000 bytes that said its first 0x1000 bytes were not free -- the one
    /// answer a caller sizing a BAR would act on.
    ///
    /// A zero-size window is not free, it is nothing: the callers treat `false`
    /// as "not ours", which is what a size read as zero out of a device
    /// register means, while a vacuous `true` had them record and program a
    /// window this allocator never reserved.
    pub fn check_region(&self, base: usize, size: usize) -> bool {
        if size == 0 {
            return false;
        }
        let Some(end) = base.checked_add(size) else {
            return false;
        };
        // One region, not a walk across several: `add` merges everything that
        // touches, so free space is never split between two entries that meet.
        // `twenty_thousand_random_sequences_agree_with_a_byte_by_byte_model`
        // is what holds that invariant up.
        self.regions
            .iter()
            .any(|region| region.base <= base && end <= region.end())
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Whether `addr` is free. The end used to be inclusive here and half-open
    /// everywhere else, so the first address *past* a free window read as
    /// inside it.
    pub fn check_point(&self, addr: usize) -> bool {
        self.check_region(addr, 1)
    }

    fn intersection_all(&mut self, region: &Region) -> Vec<Region> {
        self.regions
            .extract_if(.., |candidate| {
                !(candidate.base > region.end() || candidate.end() < region.base)
            })
            .collect()
    }

    fn merge_internal(target: &mut Region, other: Region) -> Option<Region> {
        let target_end = target.end();
        let other_end = other.end();
        if target_end < other.base || other_end < target.base {
            return Some(other);
        }
        let new_base = min(target.base, other.base);
        let new_end = max(target_end, other_end);
        target.base = new_base;
        target.size = new_end - new_base;
        None
    }

    /// The part of `target` below `source` and the part above it. `source` is
    /// read, never narrowed: it took a `&mut` it did not use, which reads as if
    /// each candidate consumed part of it.
    /// Both `min`s are unreachable clamps, kept for the same reason as the
    /// dead branch in [`RegionAllocator::add`]. `target` reached here through
    /// `intersection_all`, which means `target.base <= source.end()` and
    /// `source.base <= target.end()`; the first gives
    /// `target_end - source_end <= target.size` and the second
    /// `source.base - target.base <= target.size`, so neither `min` ever picks
    /// its second argument. Without them, a candidate that did not really
    /// overlap would come back as a region reaching past its own end -- over
    /// space this allocator has already handed to a device.
    fn subtract_internal(target: Region, source: &Region) -> (Option<Region>, Option<Region>) {
        let target_end = target.end();
        let source_end = source.end();
        let left = (source.base > target.base).then(|| Region {
            base: target.base,
            size: min(target.size, source.base - target.base),
        });
        let right = (source_end < target_end).then(|| {
            let size = min(target.size, target_end - source_end);
            Region {
                base: target_end - size,
                size,
            }
        });
        (left, right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    /// The free list as `(base, size)` pairs, lowest address first.
    fn free_list(alloc: &RegionAllocator) -> Vec<(usize, usize)> {
        alloc.regions.iter().map(|r| (r.base, r.size)).collect()
    }

    fn with(regions: &[(usize, usize)]) -> RegionAllocator {
        let mut alloc = RegionAllocator::new();
        for &(base, size) in regions {
            alloc.add(base, size);
        }
        alloc
    }

    #[test]
    fn two_windows_that_touch_are_stored_as_one() {
        let alloc = with(&[(0x1000, 0x1000), (0x2000, 0x1000)]);
        assert_eq!(free_list(&alloc), [(0x1000, 0x2000)]);
        assert_eq!(alloc.len(), 1);
        assert!(!alloc.is_empty());
    }

    #[test]
    fn two_windows_with_a_gap_between_them_stay_apart() {
        let alloc = with(&[(0x1000, 0x1000), (0x2001, 0x1000)]);
        assert_eq!(free_list(&alloc), [(0x1000, 0x1000), (0x2001, 0x1000)]);
    }

    #[test]
    fn an_empty_allocator_has_nothing_to_give() {
        let mut alloc = RegionAllocator::new();
        assert!(alloc.is_empty());
        assert!(!alloc.check_region(0, 1));
        assert!(!alloc.check_point(0));
        assert!(!alloc.allocate_by_addr(0, 0x1000));
        assert_eq!(alloc.allocate_by_size(0x1000, 0x1000), None);
    }

    #[test]
    fn a_window_with_every_byte_free_is_reserved_at_the_address_asked_for() {
        let mut alloc = with(&[(0, 0x2000)]);
        assert!(alloc.allocate_by_addr(0x800, 0x400));
        assert_eq!(free_list(&alloc), [(0, 0x800), (0xc00, 0x1400)]);
        // And now it is not free any more, at either end of it.
        assert!(!alloc.check_point(0x800));
        assert!(!alloc.check_point(0xbff));
        assert!(alloc.check_point(0x7ff));
        assert!(alloc.check_point(0xc00));
    }

    #[test]
    fn a_window_that_runs_past_the_end_of_the_free_space_is_refused() {
        let mut alloc = with(&[(0x1000, 0x1000)]);
        assert!(!alloc.allocate_by_addr(0x1800, 0x1000));
        assert!(!alloc.allocate_by_addr(0xf00, 0x200));
        assert_eq!(free_list(&alloc), [(0x1000, 0x1000)]);
    }

    #[test]
    fn a_zero_size_subtract_leaves_the_free_list_exactly_as_it_was() {
        let mut alloc = with(&[(0, 100)]);
        let before = free_list(&alloc);
        alloc.subtract(50, 0);
        assert_eq!(free_list(&alloc), before);
        alloc.subtract(0, 0);
        alloc.subtract(100, 0);
        assert_eq!(free_list(&alloc), before);
    }

    #[test]
    fn a_free_window_is_still_reservable_after_a_zero_size_subtract_inside_it() {
        // The whole failure, in four lines: the split left every byte free and
        // the window unallocatable, and the bridge that asked for it got
        // `NO_MEMORY` and gave its devices no BARs.
        let mut alloc = with(&[(0, 100)]);
        alloc.subtract(50, 0);
        assert!(alloc.check_region(40, 20));
        assert!(alloc.allocate_by_addr(40, 20));
        assert_eq!(free_list(&alloc), [(0, 40), (60, 40)]);
    }

    #[test]
    fn a_free_window_can_still_be_sized_out_after_a_zero_size_subtract_inside_it() {
        let mut alloc = with(&[(0, 100)]);
        alloc.subtract(50, 0);
        assert_eq!(alloc.allocate_by_size(100, 1), Some((0, 100)));
    }

    #[test]
    fn a_zero_size_request_is_refused_instead_of_reported_as_reserved() {
        let mut alloc = with(&[(0x1000, 0x1000)]);
        assert!(!alloc.allocate_by_addr(0x1800, 0));
        assert!(!alloc.check_region(0x1800, 0));
        assert_eq!(alloc.allocate_by_size(0, 0x100), None);
        assert_eq!(free_list(&alloc), [(0x1000, 0x1000)]);
    }

    #[test]
    fn a_zero_size_window_is_not_published_as_free() {
        let alloc = with(&[(0x1000, 0)]);
        assert!(alloc.is_empty());
        let alloc = with(&[(0x1000, 0x1000), (0x2000, 0)]);
        assert_eq!(free_list(&alloc), [(0x1000, 0x1000)]);
    }

    #[test]
    fn a_window_whose_end_wraps_round_the_address_space_is_refused_and_stays_free() {
        // It used to answer `true` and remove nothing, so the caller recorded
        // the window as its own and the next caller was given the same one.
        let mut alloc = with(&[(usize::MAX - 0x1fff, 0x1000)]);
        assert!(!alloc.allocate_by_addr(usize::MAX - 0x1fff, 0x2000));
        assert_eq!(free_list(&alloc), [(usize::MAX - 0x1fff, 0x1000)]);
        // Still there for the caller it really belongs to.
        assert!(alloc.allocate_by_addr(usize::MAX - 0x1fff, 0x1000));
        assert!(alloc.is_empty());
    }

    #[test]
    fn a_window_published_past_the_end_of_the_address_space_is_clamped_not_wrapped() {
        let alloc = with(&[(usize::MAX - 0xff, 0x1000)]);
        assert_eq!(free_list(&alloc), [(usize::MAX - 0xff, 0xff)]);
        assert!(alloc.check_point(usize::MAX - 1));
        assert!(!alloc.check_point(usize::MAX));
    }

    #[test]
    fn a_subtract_past_the_end_of_the_address_space_removes_only_addresses_that_exist() {
        let mut alloc = with(&[(usize::MAX - 0xfff, 0x1000)]);
        alloc.subtract(usize::MAX - 0x7ff, 0x1000);
        assert_eq!(free_list(&alloc), [(usize::MAX - 0xfff, 0x800)]);
    }

    #[test]
    fn the_last_address_of_a_window_is_in_it_and_the_first_one_past_it_is_not() {
        let alloc = with(&[(0, 100)]);
        assert!(alloc.check_point(0));
        assert!(alloc.check_point(99));
        assert!(!alloc.check_point(100));
        assert!(!alloc.check_point(101));
    }

    #[test]
    fn a_sub_range_of_a_free_window_is_free() {
        let alloc = with(&[(0, 0x2000)]);
        assert!(alloc.check_region(0, 0x1000));
        assert!(alloc.check_region(0x1800, 0x800));
        assert!(alloc.check_region(0, 0x2000));
        assert!(!alloc.check_region(0, 0x2001));
        assert!(!alloc.check_region(0x1fff, 2));
    }

    #[test]
    fn the_two_ways_to_ask_whether_a_window_is_free_agree() {
        let shape = [
            (0x1000, 0x1000),
            (0x4000, 0x2000),
            (usize::MAX - 0xff, 0x100),
        ];
        for base in [
            0,
            0xfff,
            0x1000,
            0x1800,
            0x1fff,
            0x2000,
            0x4000,
            0x5fff,
            usize::MAX - 0x100,
            usize::MAX - 0xff,
            usize::MAX - 1,
            usize::MAX,
        ] {
            for size in [0usize, 1, 2, 0x800, 0x1000, 0x2001] {
                let asked = with(&shape).check_region(base, size);
                let mut taken = with(&shape);
                let reserved = taken.allocate_by_addr(base, size);
                assert_eq!(
                    asked, reserved,
                    "check_region({base:#x}, {size:#x}) = {asked} but allocate_by_addr said {reserved}"
                );
                assert_eq!(
                    reserved,
                    free_list(&taken) != free_list(&with(&shape)),
                    "allocate_by_addr({base:#x}, {size:#x}) = {reserved} does not match what it removed"
                );
            }
        }
    }

    #[test]
    fn an_alignment_that_is_not_a_power_of_two_is_refused() {
        let mut alloc = with(&[(0, 0x4000)]);
        assert_eq!(alloc.allocate_by_size(0x100, 0), None);
        assert_eq!(alloc.allocate_by_size(0x100, 3), None);
        assert_eq!(alloc.allocate_by_size(0x100, 0x1800), None);
        assert_eq!(free_list(&alloc), [(0, 0x4000)]);
        assert_eq!(alloc.allocate_by_size(0x100, 1), Some((0, 0x100)));
    }

    #[test]
    fn a_block_is_reserved_at_the_alignment_asked_for_and_the_gap_below_it_stays_free() {
        let mut alloc = with(&[(0x1001, 0x3000)]);
        assert_eq!(
            alloc.allocate_by_size(0x1000, 0x1000),
            Some((0x2000, 0x1000))
        );
        assert_eq!(free_list(&alloc), [(0x1001, 0xfff), (0x3000, 0x1001)]);
    }

    #[test]
    fn a_region_too_small_for_the_request_is_skipped_for_one_further_up() {
        let mut alloc = with(&[(0x1000, 0x400), (0x8000, 0x4000)]);
        assert_eq!(
            alloc.allocate_by_size(0x1000, 0x1000),
            Some((0x8000, 0x1000))
        );
        assert_eq!(free_list(&alloc), [(0x1000, 0x400), (0x9000, 0x3000)]);
    }

    #[test]
    fn a_region_big_enough_only_before_alignment_is_skipped() {
        // 0x800 bytes free but not one 0x1000-aligned address in it.
        let mut alloc = with(&[(0x1800, 0x800), (0x8000, 0x1000)]);
        assert_eq!(
            alloc.allocate_by_size(0x1000, 0x1000),
            Some((0x8000, 0x1000))
        );
    }

    #[test]
    fn a_block_handed_out_by_size_is_never_handed_out_again() {
        let mut alloc = with(&[(0, 0x4000)]);
        let first = alloc.allocate_by_size(0x1000, 0x1000).unwrap();
        let second = alloc.allocate_by_size(0x1000, 0x1000).unwrap();
        assert_ne!(first, second);
        assert!(first.0 + first.1 <= second.0 || second.0 + second.1 <= first.0);
        assert!(!alloc.check_region(first.0, first.1));
        assert!(!alloc.check_region(second.0, second.1));
    }

    #[test]
    fn a_window_at_the_very_top_of_the_address_space_can_still_be_sized_out() {
        let mut alloc = with(&[(usize::MAX - 0xfff, 0x1000)]);
        // Clamped to 0xfff bytes, so the aligned 0x800 block fits and a full
        // 0x1000 one does not.
        assert_eq!(
            alloc.allocate_by_size(0x800, 0x800),
            Some((usize::MAX - 0xfff, 0x800))
        );
        assert_eq!(alloc.allocate_by_size(0x1000, 0x1000), None);
    }

    #[test]
    fn an_alignment_the_window_cannot_reach_is_skipped_not_rounded_off_the_end() {
        // Rounding `usize::MAX - 0xff` up to 0x1000 carries past the end of the
        // address space. Wrapped, it lands near zero and then fits inside a
        // window that starts at the very top, so the caller is handed a block
        // that is nowhere near its own region.
        let mut alloc = with(&[(usize::MAX - 0xff, 0x100)]);
        assert_eq!(alloc.allocate_by_size(0x10, 0x1000), None);
        assert_eq!(free_list(&alloc), [(usize::MAX - 0xff, 0xff)]);
    }

    #[test]
    fn a_block_that_would_run_off_the_end_of_the_address_space_is_not_handed_out() {
        // 0x1000 bytes free at the top and a 0x800-aligned address inside them,
        // but no 0x1000 bytes above that address: the block's own end is what
        // does not exist.
        let mut alloc = with(&[(usize::MAX - 0x1000, 0x1000)]);
        assert_eq!(alloc.allocate_by_size(0x1000, 0x800), None);
        assert_eq!(
            alloc.allocate_by_size(0x800, 0x800),
            Some((usize::MAX - 0xfff, 0x800))
        );
    }

    #[test]
    fn a_bridge_takes_its_window_from_its_parent_and_republishes_it_to_its_children() {
        // `PciBridge::allocate_bars`: the parent's space, the bridge's own
        // window out of it, then the same window offered to the devices below.
        let mut parent = with(&[(0xe000_0000, 0x1000_0000)]);
        assert!(parent.allocate_by_addr(0xe400_0000, 0x0400_0000));
        assert_eq!(
            free_list(&parent),
            [(0xe000_0000, 0x0400_0000), (0xe800_0000, 0x0800_0000)]
        );
        let mut bridge = RegionAllocator::new();
        bridge.add(0xe400_0000, 0x0400_0000);
        // A device under the bridge keeps the address the firmware gave it.
        assert!(bridge.allocate_by_addr(0xe400_0000, 0x0100_0000));
        // The parent must not be able to give that window away again.
        assert!(!parent.check_region(0xe400_0000, 0x0100_0000));
        assert_eq!(
            bridge.allocate_by_size(0x0100_0000, 0x0100_0000),
            Some((0xe500_0000, 0x0100_0000))
        );
    }

    #[test]
    fn two_devices_are_never_given_the_same_window() {
        let mut alloc = with(&[(0xf000_0000, 0x1000_0000)]);
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for _ in 0..16 {
            let block = alloc.allocate_by_size(0x0100_0000, 0x0100_0000).unwrap();
            for other in &taken {
                assert!(
                    block.0 + block.1 <= other.0 || other.0 + other.1 <= block.0,
                    "{block:?} overlaps {other:?}"
                );
            }
            taken.push(block);
        }
        assert!(alloc.is_empty());
        assert_eq!(alloc.allocate_by_size(0x0100_0000, 0x0100_0000), None);
    }

    /// Every operation replayed against a set of free bytes, which is the
    /// definition the callers work to. It also holds up the invariant every
    /// query in this file leans on: no two regions in the set touch.
    #[test]
    fn twenty_thousand_random_sequences_agree_with_a_byte_by_byte_model() {
        use alloc::collections::BTreeSet as Bytes;
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut rng = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut alloc = RegionAllocator::new();
            let mut free: Bytes<usize> = Bytes::new();
            let mut ops: Vec<(u8, usize, usize)> = Vec::new();
            for _ in 0..(rng() % 5) + 1 {
                let op = (rng() % 3) as u8;
                let base = (rng() % 20) as usize;
                let size = (rng() % 8) as usize;
                ops.push((op, base, size));
                match op {
                    0 => {
                        alloc.add(base, size);
                        free.extend(base..base + size);
                    }
                    1 => {
                        alloc.subtract(base, size);
                        for addr in base..base + size {
                            free.remove(&addr);
                        }
                    }
                    _ => {
                        let all_free = size > 0 && (base..base + size).all(|a| free.contains(&a));
                        assert_eq!(
                            alloc.allocate_by_addr(base, size),
                            all_free,
                            "allocate_by_addr({base}, {size}) disagrees with the byte model after {ops:?}"
                        );
                        if all_free {
                            for addr in base..base + size {
                                free.remove(&addr);
                            }
                        }
                    }
                }
                let mut previous_end = None;
                for region in alloc.regions.iter() {
                    assert!(region.size > 0, "a zero-size region after {ops:?}");
                    if let Some(end) = previous_end {
                        assert!(
                            end < region.base,
                            "regions that touch or overlap at {end} after {ops:?}"
                        );
                    }
                    previous_end = Some(region.end());
                }
                let bytes: Bytes<usize> =
                    alloc.regions.iter().flat_map(|r| r.base..r.end()).collect();
                assert_eq!(bytes, free, "free space disagrees after {ops:?}");
            }
        }
    }
}
