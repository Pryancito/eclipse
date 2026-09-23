//! The FIFO that holds freed DMA blocks out of the frame pool for a while.
//!
//! Freed DMA memory used to go straight back to the pool, so a device
//! descriptor or a userspace `VmObject::new_physical` mapping still pointing
//! at a just-freed block could write it *after* `frame_alloc` had handed the
//! recycled frames to a fresh coroutine stack or another process — the SMP
//! `[null-exec]` zero-smash and the GEM-recycle SIGSEGV that kills a `wl_shm`
//! client at desktop start on a `killall` + relaunch. Holding the block out of
//! circulation for a window lets that in-flight DMA drain onto memory nothing
//! owns yet, and the poison the caller writes into each page names a stale
//! writer instead of letting it corrupt in silence.
//!
//! This is the bookkeeping half — which block to evict, and when — kept apart
//! from the frame pool and the poison scan so it compiles, and can be tested,
//! without a machine. It used to sit inside the bare-metal deallocator, which
//! no build this project runs compiles.

/// Per-block cap (32 MiB) — covers a 4K framebuffer. A single block larger
/// than this is returned immediately: its own use-after-free window is
/// comparatively tiny and holding it would blow the budget on its own.
pub const MAX_BLOCK_PAGES: usize = 8192;
/// Total held out of the pool (64 MiB).
pub const BUDGET_PAGES: usize = 16 * 1024;
/// Entries held at once; whichever bound is hit first evicts the oldest.
pub const MAX_BLOCKS: usize = 1024;

// A per-block cap above the budget would make the budget meaningless: every
// push would evict the whole ring to make room and the quarantine would hold
// one block at a time, which is close enough to not quarantining at all.
const _: () = assert!(MAX_BLOCK_PAGES <= BUDGET_PAGES);
const _: () = assert!(MAX_BLOCKS > 0);

/// What a caller should do with a block handed to [`Quarantine::push`].
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Held. Do nothing; it will come back from a later `push` as an eviction.
    Held,
    /// Too big to hold. Return it to the frame pool now.
    TooBig,
    /// Already in the quarantine — a double free. Returning these frames to
    /// the pool twice hands one frame to two owners, which is the corruption
    /// this whole mechanism exists to catch, so the second free is dropped and
    /// the caller reports it.
    DoubleFree,
}

/// A fixed-size FIFO of `(base, pages)`.
pub struct Quarantine {
    ring: [(usize, usize); MAX_BLOCKS],
    /// Index of the oldest entry.
    head: usize,
    len: usize,
    held_pages: usize,
}

impl Default for Quarantine {
    fn default() -> Self {
        Self::new()
    }
}

impl Quarantine {
    pub const fn new() -> Self {
        Quarantine {
            ring: [(0, 0); MAX_BLOCKS],
            head: 0,
            len: 0,
            held_pages: 0,
        }
    }

    /// Blocks held right now, and the pages they cover.
    pub fn depth(&self) -> (usize, usize) {
        (self.len, self.held_pages)
    }

    /// Whether any held block covers `base`.
    fn holds(&self, base: usize, pages: usize) -> bool {
        (0..self.len).any(|i| {
            let (b, n) = self.ring[(self.head + i) % MAX_BLOCKS];
            let end = b.saturating_add(n.saturating_mul(crate::PAGE_SIZE));
            let e = base.saturating_add(pages.saturating_mul(crate::PAGE_SIZE));
            n != 0 && pages != 0 && base < end && b < e
        })
    }

    /// Take `(base, pages)` into the quarantine, appending to `evicted` the
    /// blocks pushed out to make room — oldest first, and the caller's to
    /// return to the frame pool once it has checked their poison.
    ///
    /// `poison` runs exactly when the verdict is [`Verdict::Held`], after the
    /// evictions and before the block becomes visible to anyone else. It is
    /// the caller's business what poison means; what matters here is that a
    /// block refused for any reason is never poisoned — re-poisoning one that
    /// is already quarantined would erase the stale write the poison exists to
    /// catch — and that a block in the ring is never briefly un-poisoned,
    /// which would make the next eviction report a corruption that did not
    /// happen. The eviction *scan*, which is the slow part, stays outside.
    pub fn push(
        &mut self,
        base: usize,
        pages: usize,
        evicted: &mut alloc::vec::Vec<(usize, usize)>,
        poison: impl FnOnce(),
    ) -> Verdict {
        if pages > MAX_BLOCK_PAGES {
            return Verdict::TooBig;
        }
        if self.holds(base, pages) {
            return Verdict::DoubleFree;
        }
        while self.len >= MAX_BLOCKS || (self.held_pages + pages > BUDGET_PAGES && self.len > 0) {
            let h = self.head;
            let (b, n) = self.ring[h];
            self.ring[h] = (0, 0);
            self.head = (h + 1) % MAX_BLOCKS;
            self.len -= 1;
            self.held_pages -= n;
            evicted.push((b, n));
        }
        poison();
        let tail = (self.head + self.len) % MAX_BLOCKS;
        self.ring[tail] = (base, pages);
        self.len += 1;
        self.held_pages += pages;
        Verdict::Held
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const PG: usize = crate::PAGE_SIZE;
    /// Blocks far apart enough that none of them overlaps another.
    fn at(i: usize) -> usize {
        0x1000_0000 + i * MAX_BLOCK_PAGES * PG
    }

    fn push(q: &mut Quarantine, base: usize, pages: usize) -> (Verdict, Vec<(usize, usize)>) {
        let mut ev = Vec::new();
        let v = q.push(base, pages, &mut ev, || {});
        (v, ev)
    }

    /// `(verdict, evicted, whether the block was poisoned)`.
    fn push_p(
        q: &mut Quarantine,
        base: usize,
        pages: usize,
    ) -> (Verdict, Vec<(usize, usize)>, bool) {
        let mut ev = Vec::new();
        let mut poisoned = false;
        let v = q.push(base, pages, &mut ev, || poisoned = true);
        (v, ev, poisoned)
    }

    #[test]
    fn only_a_block_that_is_actually_held_gets_poisoned() {
        let mut q = Quarantine::new();
        assert_eq!(push_p(&mut q, at(1), 4).2, true);
        // Too big to hold: it goes straight back to the pool, so poisoning it
        // is writing to memory the caller is about to give away.
        assert_eq!(push_p(&mut q, at(2), MAX_BLOCK_PAGES + 1).2, false);
        // Already quarantined: re-poisoning would erase the stale write the
        // poison is there to catch, which is the whole point of holding it.
        assert_eq!(push_p(&mut q, at(1), 4).2, false);
    }

    #[test]
    fn a_block_is_held_rather_than_returned() {
        let mut q = Quarantine::new();
        assert_eq!(push(&mut q, at(1), 4), (Verdict::Held, alloc::vec![]));
        assert_eq!(q.depth(), (1, 4));
    }

    #[test]
    fn a_block_bigger_than_the_per_block_cap_goes_straight_back() {
        // Holding it would blow the whole budget on one buffer.
        let mut q = Quarantine::new();
        assert_eq!(
            push(&mut q, at(1), MAX_BLOCK_PAGES + 1),
            (Verdict::TooBig, alloc::vec![])
        );
        assert_eq!(q.depth(), (0, 0));
        // Exactly at the cap is still held.
        assert_eq!(push(&mut q, at(1), MAX_BLOCK_PAGES).0, Verdict::Held);
    }

    #[test]
    fn freeing_a_block_that_is_already_quarantined_is_a_double_free() {
        // Returning these frames to the pool twice hands one frame to two
        // owners, which is exactly the corruption the quarantine is here to
        // catch — and it is the one place in the system that can see it.
        let mut q = Quarantine::new();
        assert_eq!(push(&mut q, at(1), 8).0, Verdict::Held);
        assert_eq!(push(&mut q, at(1), 8), (Verdict::DoubleFree, alloc::vec![]));
        // A partial overlap is the same mistake with worse aim.
        assert_eq!(push(&mut q, at(1) + PG, 2).0, Verdict::DoubleFree);
        assert_eq!(q.depth(), (1, 8));
    }

    #[test]
    fn an_address_freed_again_after_it_left_the_quarantine_is_not_a_double_free() {
        // Once evicted the frames are back in the pool and may legitimately be
        // handed out, freed and quarantined again.
        let mut q = Quarantine::new();
        assert_eq!(push(&mut q, at(1), MAX_BLOCK_PAGES).0, Verdict::Held);
        // Two of these fill the budget exactly, which is not over it.
        assert_eq!(
            push(&mut q, at(2), MAX_BLOCK_PAGES),
            (Verdict::Held, alloc::vec![])
        );
        let (v, ev) = push(&mut q, at(3), 1);
        assert_eq!(v, Verdict::Held);
        assert_eq!(ev, alloc::vec![(at(1), MAX_BLOCK_PAGES)]);
        assert_eq!(push(&mut q, at(1), 4).0, Verdict::Held);
    }

    #[test]
    fn the_oldest_block_is_the_one_evicted() {
        // Newest-first would return the frames a device is most likely to
        // still be writing.
        let mut q = Quarantine::new();
        for i in 0..MAX_BLOCKS {
            assert_eq!(push(&mut q, at(i), 1).0, Verdict::Held);
        }
        assert_eq!(q.depth(), (MAX_BLOCKS, MAX_BLOCKS));
        let (v, ev) = push(&mut q, at(MAX_BLOCKS), 1);
        assert_eq!(v, Verdict::Held);
        assert_eq!(ev, alloc::vec![(at(0), 1)]);
        assert_eq!(q.depth(), (MAX_BLOCKS, MAX_BLOCKS));
    }

    #[test]
    fn the_ring_wraps_without_losing_its_place() {
        let mut q = Quarantine::new();
        for i in 0..MAX_BLOCKS * 2 + 3 {
            push(&mut q, at(i), 1);
        }
        assert_eq!(q.depth(), (MAX_BLOCKS, MAX_BLOCKS));
        // The oldest still in the ring is the one evicted next.
        let (_, ev) = push(&mut q, at(MAX_BLOCKS * 2 + 3), 1);
        assert_eq!(ev, alloc::vec![(at(MAX_BLOCKS + 3), 1)]);
    }

    #[test]
    fn the_page_budget_evicts_before_the_entry_count_does() {
        // Two 32 MiB buffers fill the 64 MiB budget with two of the 1024
        // entries used.
        let mut q = Quarantine::new();
        assert_eq!(push(&mut q, at(1), MAX_BLOCK_PAGES).0, Verdict::Held);
        assert_eq!(push(&mut q, at(2), MAX_BLOCK_PAGES).0, Verdict::Held);
        assert_eq!(q.depth(), (2, BUDGET_PAGES));
        let (_, ev) = push(&mut q, at(3), 1);
        assert_eq!(ev, alloc::vec![(at(1), MAX_BLOCK_PAGES)]);
        assert_eq!(q.depth(), (2, MAX_BLOCK_PAGES + 1));
    }

    #[test]
    fn a_block_evicts_only_as_much_as_it_needs_room_for() {
        let mut q = Quarantine::new();
        for i in 0..8 {
            push(&mut q, at(i), BUDGET_PAGES / 8);
        }
        assert_eq!(q.depth(), (8, BUDGET_PAGES));
        let (_, ev) = push(&mut q, at(8), BUDGET_PAGES / 8);
        assert_eq!(ev.len(), 1, "evicted more than it had to");
        assert_eq!(q.depth(), (8, BUDGET_PAGES));
    }

    #[test]
    fn the_held_page_count_follows_what_is_actually_in_the_ring() {
        // It decides every eviction, so a count that drifts from the ring
        // either starves the frame pool or stops quarantining.
        let mut q = Quarantine::new();
        let mut ev = Vec::new();
        for i in 0..40 {
            q.push(at(i), 1 + i % 7, &mut ev, || {});
        }
        let by_hand: usize = (0..q.len)
            .map(|i| q.ring[(q.head + i) % MAX_BLOCKS].1)
            .sum();
        assert_eq!(q.depth(), (q.len, by_hand));
        assert!(ev.is_empty(), "nothing should have been evicted yet");
    }

    #[test]
    fn a_double_free_is_still_seen_once_the_oldest_entry_has_moved() {
        // `holds` walks from the oldest entry, which stops being slot zero the
        // first time the page budget evicts one — and that happens long before
        // the ring is full, so a double free would go unseen for most of a
        // session.
        let mut q = Quarantine::new();
        assert_eq!(push(&mut q, at(1), MAX_BLOCK_PAGES).0, Verdict::Held);
        assert_eq!(push(&mut q, at(2), MAX_BLOCK_PAGES).0, Verdict::Held);
        let (_, ev) = push(&mut q, at(3), 1);
        assert_eq!(ev, alloc::vec![(at(1), MAX_BLOCK_PAGES)]);
        assert_eq!(push(&mut q, at(3), 1).0, Verdict::DoubleFree);
        assert_eq!(push(&mut q, at(2), MAX_BLOCK_PAGES).0, Verdict::DoubleFree);
    }

    #[test]
    fn a_block_of_no_pages_answers_for_no_address() {
        // It covers nothing, so a real free of the memory around it must not
        // read as a double free of it.
        let mut q = Quarantine::new();
        assert_eq!(push(&mut q, at(1), 0).0, Verdict::Held);
        assert!(!q.holds(at(1) - PG, 4));
        assert!(!q.holds(at(1), 4));
    }

    #[test]
    fn a_zero_page_free_is_not_a_block() {
        // It would take an entry and match nothing, and `holds` must not treat
        // it as covering the address it names.
        let mut q = Quarantine::new();
        assert_eq!(push(&mut q, at(1), 4).0, Verdict::Held);
        assert!(!q.holds(at(1), 0));
        assert!(!q.holds(at(9), 4));
        assert!(q.holds(at(1) + 3 * PG, 1));
    }
}
