//! The registry that keeps a freed DMA block out of the frame pool while
//! userspace still maps it.
//!
//! A `VmObject::new_physical` over a DMA/GEM block does not own the frames:
//! the driver frees them through `drivers_dma_dealloc` when the GEM handle
//! closes. If userspace still has the range mapped — or a GPU ring still
//! points at it — returning those frames to the pool recycles them into a live
//! coroutine stack or another client's buffer, which is the residual DMA
//! use-after-free of `docs/README-crash-repro.md`. So every physical VMO
//! *pins* its range for its own lifetime, and a free that overlaps a pin is
//! *held* until the last pin drops.
//!
//! The bookkeeping lives here, apart from the bare-metal wrappers that call
//! it, because it is ordinary arithmetic over two lists and needs no machine
//! at all — while every build this project runs compiles either the bare
//! version (x86 hardware only) or a set of no-op stubs. The guard against the
//! crash it exists for was therefore never executed by anything that could
//! report a mistake in it.
//!
//! Pins are refcounted per exact range, because that is the granularity the
//! caller pairs them at: one `new_physical` and one `Drop` per VMO object, not
//! per mapping. Overlap, not equality, decides whether a *free* is held: the
//! block being freed is the driver's, the pins are userspace's, and the two
//! describe the same memory in different units.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use lock::Mutex;

use crate::PAGE_SIZE;

struct UserPin {
    base: usize,
    pages: usize,
    refs: usize,
}

struct HeldFree {
    base: usize,
    pages: usize,
}

/// Live userspace pins, and the DMA frees parked behind them.
pub struct PinRegistry {
    pins: Vec<UserPin>,
    held: Vec<HeldFree>,
}

/// Unpins that named a range no pin covered. Every pin is taken by a
/// `new_physical` and dropped by exactly one `Drop`, so a non-zero count means
/// the two have drifted apart — and a pin that is never dropped holds its
/// block out of the pool for the rest of the boot.
static UNBALANCED_UNPINS: AtomicU64 = AtomicU64::new(0);

/// Whether two page ranges share a page.
///
/// The end of a range is computed without wrapping, and a range that would run
/// off the top of the address space is treated as covering the rest of it
/// rather than wrapping round to zero — an over-broad answer holds a free
/// longer than it must, where a wrapped one would release it early, and only
/// one of those two mistakes recycles memory a device is still writing.
fn ranges_overlap(a: usize, an: usize, b: usize, bn: usize) -> bool {
    if an == 0 || bn == 0 {
        return false;
    }
    let a1 = a.saturating_add(an.saturating_mul(PAGE_SIZE));
    let b1 = b.saturating_add(bn.saturating_mul(PAGE_SIZE));
    a < b1 && b < a1
}

impl Default for PinRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PinRegistry {
    pub const fn new() -> Self {
        PinRegistry {
            pins: Vec::new(),
            held: Vec::new(),
        }
    }

    /// Pin `[base, base + pages * PAGE)` against returning to the frame pool.
    fn pin(&mut self, base: usize, pages: usize) {
        if pages == 0 || base == 0 {
            return;
        }
        if let Some(e) = self
            .pins
            .iter_mut()
            .find(|e| e.base == base && e.pages == pages)
        {
            e.refs = e.refs.saturating_add(1);
            return;
        }
        self.pins.push(UserPin {
            base,
            pages,
            refs: 1,
        });
    }

    /// Drop one pin on `[base, base + pages * PAGE)` and return every held
    /// block that is now free to go, oldest first.
    fn unpin(&mut self, base: usize, pages: usize) -> Vec<(usize, usize)> {
        if pages == 0 || base == 0 {
            return Vec::new();
        }
        match self
            .pins
            .iter()
            .position(|e| e.base == base && e.pages == pages)
        {
            Some(pos) => {
                let e = &mut self.pins[pos];
                e.refs = e.refs.saturating_sub(1);
                if e.refs == 0 {
                    self.pins.swap_remove(pos);
                }
            }
            // Nothing to drop. Counted rather than ignored: it means a pin
            // somewhere will never be dropped either, and that one is holding
            // a block out of the pool until the machine reboots.
            None => {
                UNBALANCED_UNPINS.fetch_add(1, Relaxed);
            }
        }
        self.sweep()
    }

    /// True if any live pin overlaps `[base, base + pages * PAGE)`.
    fn pinned(&self, base: usize, pages: usize) -> bool {
        self.holds_over(base, pages)
    }

    fn holds_over(&self, base: usize, pages: usize) -> bool {
        pages != 0
            && self
                .pins
                .iter()
                .any(|p| ranges_overlap(base, pages, p.base, p.pages))
    }

    /// Park this free if a pin still overlaps it, and say whether it was
    /// parked.
    ///
    /// The check and the park are one step on purpose. They used to be two
    /// calls, each taking the lock for itself, and the last unpin could land
    /// between them: it found nothing to release — this block was not parked
    /// yet — and then the block was parked behind a pin that no longer
    /// existed, where nothing would ever look at it again. The frames were
    /// lost for the rest of the boot, and the window is exactly a client being
    /// killed while it still has a GEM buffer mapped, which is the every-time
    /// case at teardown rather than a rare race.
    fn hold_if_pinned(&mut self, base: usize, pages: usize) -> bool {
        if !self.holds_over(base, pages) {
            return false;
        }
        if !self.held.iter().any(|h| h.base == base && h.pages == pages) {
            self.held.push(HeldFree { base, pages });
        }
        true
    }

    /// Release every held block no live pin covers any more.
    ///
    /// Run on every change to either list, not only on the unpin that happens
    /// to match: the invariant worth keeping is "a held block always has a
    /// live pin", and a sweep that only ever looks at the range just unpinned
    /// cannot restore it once anything else has dropped a pin quietly.
    fn sweep(&mut self) -> Vec<(usize, usize)> {
        let mut released = Vec::new();
        let mut i = 0;
        while i < self.held.len() {
            let h = &self.held[i];
            if self
                .pins
                .iter()
                .any(|p| ranges_overlap(h.base, h.pages, p.base, p.pages))
            {
                i += 1;
            } else {
                let h = self.held.swap_remove(i);
                released.push((h.base, h.pages));
            }
        }
        released
    }
}

static REGISTRY: Mutex<PinRegistry> = Mutex::new(PinRegistry::new());

/// Pin `[paddr, paddr + pages * PAGE)`. Called from `VmObject::new_physical`.
pub fn pin(paddr: usize, pages: usize) {
    REGISTRY.lock().pin(paddr, pages);
}

/// Drop one pin, returning the held blocks that may now go back through the
/// quarantine. The caller releases them with the lock already dropped:
/// `frame_dealloc` and the quarantine's poison scan must not run under it.
pub fn unpin(paddr: usize, pages: usize) -> Vec<(usize, usize)> {
    REGISTRY.lock().unpin(paddr, pages)
}

/// True if any userspace pin overlaps `[paddr, paddr + pages * PAGE)`.
pub fn pinned(paddr: usize, pages: usize) -> bool {
    REGISTRY.lock().pinned(paddr, pages)
}

/// Park a DMA free behind its pins, and say whether it was parked. `false`
/// means nothing holds these frames and the caller owns them.
pub fn hold_if_pinned(paddr: usize, pages: usize) -> bool {
    REGISTRY.lock().hold_if_pinned(paddr, pages)
}

/// Unpins that named a range no pin covered, since boot.
pub fn unbalanced_unpins() -> u64 {
    UNBALANCED_UNPINS.load(Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: usize = PAGE_SIZE;
    /// A base well clear of zero, which the registry treats as "no address".
    const A: usize = 0x10_0000;

    fn reg() -> PinRegistry {
        PinRegistry::new()
    }

    #[test]
    fn a_free_with_nothing_mapped_over_it_is_not_held() {
        let mut r = reg();
        assert!(!r.hold_if_pinned(A, 4));
        assert!(r.held.is_empty());
    }

    #[test]
    fn a_free_under_a_pin_is_held_until_the_pin_goes() {
        let mut r = reg();
        r.pin(A, 4);
        assert!(r.pinned(A, 4));
        assert!(r.hold_if_pinned(A, 4));
        assert_eq!(r.unpin(A, 4), alloc::vec![(A, 4)]);
        assert!(r.held.is_empty());
        assert!(!r.pinned(A, 4));
    }

    #[test]
    fn the_free_waits_for_the_last_pin_not_the_first() {
        // One GEM buffer, two physical VMOs over it: two `new_physical` calls
        // and two Drops. Releasing on the first would hand the frames back
        // while the second mapping is still live.
        let mut r = reg();
        r.pin(A, 4);
        r.pin(A, 4);
        assert!(r.hold_if_pinned(A, 4));
        assert!(r.unpin(A, 4).is_empty());
        assert!(r.pinned(A, 4));
        assert_eq!(r.unpin(A, 4), alloc::vec![(A, 4)]);
    }

    #[test]
    fn a_pin_anywhere_inside_the_block_holds_the_whole_free() {
        // The block is the driver's and the pin is userspace's; they describe
        // the same memory in different units, so overlap decides, not equality.
        let mut r = reg();
        r.pin(A + PG, 1);
        assert!(r.hold_if_pinned(A, 4));
        assert_eq!(r.unpin(A + PG, 1), alloc::vec![(A, 4)]);
    }

    #[test]
    fn a_pin_that_merely_touches_the_next_page_does_not_hold_the_block() {
        let mut r = reg();
        r.pin(A + 4 * PG, 1); // starts exactly where the block ends
        assert!(!r.hold_if_pinned(A, 4));
        let mut r2 = reg();
        r2.pin(A.saturating_sub(PG), 1); // ends exactly where the block starts
        assert!(!r2.hold_if_pinned(A, 4));
    }

    #[test]
    fn an_empty_range_overlaps_nothing() {
        // Zero pages is not a range; treating it as one would hold every free
        // in the system behind it.
        assert!(!ranges_overlap(A, 0, A, 4));
        assert!(!ranges_overlap(A, 4, A, 0));
        // Including one that sits strictly inside the other, where the
        // arithmetic alone would say yes.
        assert!(!ranges_overlap(A + PG, 0, A, 4));
        assert!(!ranges_overlap(A, 4, A + PG, 0));
        let mut r = reg();
        r.pin(A, 0);
        assert!(!r.pinned(A, 4));
        assert!(!r.hold_if_pinned(A, 0));
        // And a free of no pages is not held by a pin that covers its address.
        r.pin(A, 4);
        assert!(!r.pinned(A, 0));
        assert!(!r.hold_if_pinned(A, 0));
        assert!(r.held.is_empty());
    }

    #[test]
    fn a_range_at_the_top_of_memory_does_not_wrap_round_to_zero() {
        // Saturating, not wrapping: a wrapped end makes the range look empty
        // and releases a block userspace is still mapping.
        let top = usize::MAX - PG;
        assert!(ranges_overlap(top, 16, top, 1));
        assert!(!ranges_overlap(top, 16, 0x1000, 1));
    }

    #[test]
    fn the_last_unpin_cannot_slip_in_between_the_check_and_the_park() {
        // The two used to be separate calls, each taking the lock: an unpin
        // landing between them found nothing to release (the block was not
        // parked yet) and then the block was parked behind a pin that no
        // longer existed, where nothing would ever look at it again. Here the
        // same order, one step at a time, must not lose the block.
        let mut r = reg();
        r.pin(A, 4);
        assert!(r.pinned(A, 4));
        // ... the client dies here, before the free is parked.
        assert!(r.unpin(A, 4).is_empty());
        // The free arrives now. Nothing holds these frames any more, so the
        // caller owns them — it must not be told they were parked.
        assert!(!r.hold_if_pinned(A, 4));
        assert!(r.held.is_empty());
    }

    #[test]
    fn a_held_block_is_released_by_whichever_pin_is_last_to_go() {
        // Not only by an unpin naming its own range: the sweep restores the
        // invariant that a held block always has a live pin, whoever moved.
        let mut r = reg();
        r.pin(A, 8);
        r.pin(A + 2 * PG, 1);
        assert!(r.hold_if_pinned(A + PG, 2));
        assert!(r.unpin(A + 2 * PG, 1).is_empty());
        assert_eq!(r.unpin(A, 8), alloc::vec![(A + PG, 2)]);
    }

    #[test]
    fn parking_the_same_block_twice_parks_it_once() {
        let mut r = reg();
        r.pin(A, 4);
        assert!(r.hold_if_pinned(A, 4));
        assert!(r.hold_if_pinned(A, 4));
        assert_eq!(r.unpin(A, 4), alloc::vec![(A, 4)]);
    }

    #[test]
    fn every_held_block_behind_one_pin_is_released_together() {
        let mut r = reg();
        r.pin(A, 16);
        assert!(r.hold_if_pinned(A, 2));
        assert!(r.hold_if_pinned(A + 4 * PG, 2));
        assert!(r.hold_if_pinned(A + 8 * PG, 2));
        let mut got = r.unpin(A, 16);
        got.sort_unstable();
        assert_eq!(got, alloc::vec![(A, 2), (A + 4 * PG, 2), (A + 8 * PG, 2)]);
        assert!(r.held.is_empty());
    }

    #[test]
    fn address_zero_is_the_absence_of_an_address() {
        // The frame pool never hands out physical zero, and the callers use it
        // as "nothing here": pinning it would park frees against a pin that
        // can never be dropped.
        let mut r = reg();
        r.pin(0, 4);
        assert!(r.pins.is_empty());
        assert!(r.unpin(0, 4).is_empty());
    }

    #[test]
    fn an_unpin_with_no_pin_behind_it_is_counted() {
        // Pins and Drops are paired one to one, so a mismatch means some other
        // pin will never be dropped either — and that one holds its block out
        // of the pool until the machine reboots.
        let before = unbalanced_unpins();
        let mut r = reg();
        r.pin(A, 4);
        assert!(r.unpin(A + 0x1000_0000, 4).is_empty());
        assert_eq!(unbalanced_unpins(), before + 1);
        // The pin that does match is untouched by the stray one.
        assert!(r.pinned(A, 4));
    }

    #[test]
    fn a_double_unpin_does_not_take_the_count_below_zero() {
        let mut r = reg();
        r.pin(A, 4);
        assert!(r.unpin(A, 4).is_empty());
        // Second drop of a pin that is already gone: counted as unbalanced,
        // never a wrapped refcount that pins the range forever.
        let before = unbalanced_unpins();
        assert!(r.unpin(A, 4).is_empty());
        assert_eq!(unbalanced_unpins(), before + 1);
        assert!(!r.pinned(A, 4));
    }

    #[test]
    fn pins_of_different_shapes_over_one_buffer_are_counted_apart() {
        // Two VMOs over overlapping but different ranges are two pins, each
        // with its own refcount: collapsing them would release the block on
        // the first Drop.
        let mut r = reg();
        r.pin(A, 4);
        r.pin(A, 2);
        assert!(r.hold_if_pinned(A, 4));
        assert!(r.unpin(A, 2).is_empty());
        assert_eq!(r.unpin(A, 4), alloc::vec![(A, 4)]);
    }
}
