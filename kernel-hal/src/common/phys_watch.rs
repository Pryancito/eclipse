//! The physical-frame bookkeeping the coroutine-stack hunt runs on.
//!
//! [`FrameSet`] records which physical frames currently back a live coroutine
//! stack, so `pmem_zero`/`pmem_write` can refuse to scribble over one through
//! the physmap alias — the write lands at `0xffff_8000 + paddr`, a different
//! virtual address than the stack's own, so no VA-based tripwire can see it.
//!
//! [`FreedRing`] records the last few hundred DMA blocks returned to the frame
//! pool, so when a stack *is* found corrupt the fault path can say whether its
//! frames were one of them. That is the difference between "corruption
//! happened" and "a freed DMA buffer wrote here, this many frees ago".
//!
//! [`FramePool`] holds the page-table frames reserved at boot for splitting a
//! huge mapping, because the guard installer runs inside `Executor::new` with
//! the runtime lock held and cannot allocate one.
//!
//! All three used to live in `bare/stack_guard.rs`, which only an x86_64 bare
//! build compiles: they are the instruments of the hunt, and no build that
//! could report a mistake in them ever compiled a line. All three are plain
//! bookkeeping over an address and a count and need no machine at all.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::PAGE_SIZE;

/// Mask of the bits for frames `lo..=hi` within one 64-frame word.
///
/// `lo` and `hi` are bit positions inside the word, `lo <= hi <= 63`.
fn word_mask(lo: usize, hi: usize) -> u64 {
    debug_assert!(lo <= hi && hi < 64);
    let width = hi - lo + 1;
    if width >= 64 {
        u64::MAX
    } else {
        ((1u64 << width) - 1) << lo
    }
}

/// A bitset over physical frame numbers.
///
/// `WORDS` words cover `WORDS * 64` frames; frames past that are not tracked
/// and are counted instead, because a machine whose kernel heap sits above the
/// covered range is not a configuration this hunt targets and saying so once
/// beats growing a 512 KiB table into a multi-megabyte one.
pub struct FrameSet<const WORDS: usize> {
    bits: [AtomicU64; WORDS],
    over_cap: AtomicUsize,
}

impl<const WORDS: usize> Default for FrameSet<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const WORDS: usize> FrameSet<WORDS> {
    pub const fn new() -> Self {
        Self {
            bits: [const { AtomicU64::new(0) }; WORDS],
            over_cap: AtomicUsize::new(0),
        }
    }

    /// Highest frame number this set can hold, plus one.
    pub const fn tracked_frames(&self) -> usize {
        WORDS * 64
    }

    /// Start tracking `frame`. A frame past the cap is counted, not tracked.
    pub fn mark(&self, frame: usize) {
        if frame >= self.tracked_frames() {
            self.over_cap.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.bits[frame / 64].fetch_or(1u64 << (frame % 64), Ordering::Relaxed);
    }

    /// Stop tracking `frame`.
    pub fn clear(&self, frame: usize) {
        if frame >= self.tracked_frames() {
            return;
        }
        self.bits[frame / 64].fetch_and(!(1u64 << (frame % 64)), Ordering::Relaxed);
    }

    /// Whether `frame` is tracked.
    pub fn contains(&self, frame: usize) -> bool {
        if frame >= self.tracked_frames() {
            return false;
        }
        self.bits[frame / 64].load(Ordering::Relaxed) & (1u64 << (frame % 64)) != 0
    }

    /// Whether any frame in `[paddr, paddr + len)` is tracked.
    ///
    /// Walked a word at a time rather than a frame at a time. The callers are
    /// `pmem_zero` and `pmem_write`, whose `len` is a whole VMO range: at one
    /// atomic load per frame a gigabyte costs a quarter of a million loads on
    /// the write path, and the overwhelmingly common answer — a word of zeros —
    /// is reached sixty-four frames at a time.
    pub fn aliases(&self, paddr: usize, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let first = paddr / PAGE_SIZE;
        if first >= self.tracked_frames() {
            return false;
        }
        // Saturating: `paddr + len` past the end of the address space is
        // nonsense, but wrapping it would make `last < first` and answer "no
        // alias", and a guard must not go quiet on a malformed request.
        let last = (paddr.saturating_add(len - 1) / PAGE_SIZE).min(self.tracked_frames() - 1);
        let (fw, lw) = (first / 64, last / 64);
        for (w, cell) in self.bits.iter().enumerate().take(lw + 1).skip(fw) {
            let word = cell.load(Ordering::Relaxed);
            if word == 0 {
                continue;
            }
            let lo = if w == fw { first % 64 } else { 0 };
            let hi = if w == lw { last % 64 } else { 63 };
            if word & word_mask(lo, hi) != 0 {
                return true;
            }
        }
        false
    }

    /// How many frames were offered to [`Self::mark`] from past the cap.
    pub fn over_cap(&self) -> usize {
        self.over_cap.load(Ordering::Relaxed)
    }
}

/// Whether `paddr` falls inside the block `[base, base + pages * PAGE_SIZE)`.
///
/// Saturating on purpose: a torn or corrupt entry must not wrap its end below
/// its base and start matching every address below it.
fn block_holds(base: usize, pages: usize, paddr: usize) -> bool {
    if pages == 0 {
        return false;
    }
    let end = base.saturating_add(pages.saturating_mul(PAGE_SIZE));
    paddr >= base && paddr < end
}

/// A ring of the most recently freed DMA blocks.
pub struct FreedRing<const N: usize> {
    base: [AtomicUsize; N],
    pages: [AtomicUsize; N],
    /// The `seq + 1` of the write that owns each slot, `0` while one is in
    /// progress. Monotonic, which is what makes it a usable witness.
    stamp: [AtomicU64; N],
    /// Monotonic count of blocks freed since boot; also the write cursor.
    seq: AtomicU64,
}

impl<const N: usize> Default for FreedRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FreedRing<N> {
    pub const fn new() -> Self {
        Self {
            base: [const { AtomicUsize::new(0) }; N],
            pages: [const { AtomicUsize::new(0) }; N],
            stamp: [const { AtomicU64::new(0) }; N],
            seq: AtomicU64::new(0),
        }
    }

    /// Record that `[base, base + pages * PAGE_SIZE)` was just returned to the
    /// frame pool.
    ///
    /// The slot is **retired before it is overwritten**: its stamp goes to
    /// zero first, then the new base and length, then the stamp of the write
    /// that owns it. A reader that catches the slot mid-write sees the zero
    /// and skips it; one that straddles a whole write sees a stamp that has
    /// moved on. Without that, a reader could take the *old* base with the
    /// *new* length and match an address that was never in either block, on
    /// the one path where a false positive costs the most: the already-fatal
    /// fault report, whose whole job is to name the writer.
    ///
    /// The stamp has to be the witness rather than the length, because the
    /// length is not one. Re-reading it and finding it unchanged says nothing
    /// when the ring is short and the traffic repeats: two blocks freed in
    /// turn put the same length back in the same slot, so a reader whose two
    /// loads straddle a full cycle sees its own value again and pairs it with
    /// the base it read in between. The stamp only ever counts up.
    pub fn note(&self, base: usize, pages: usize) {
        if pages == 0 {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::AcqRel);
        let slot = (seq % N as u64) as usize;
        self.stamp[slot].store(0, Ordering::Release);
        self.base[slot].store(base, Ordering::Release);
        self.pages[slot].store(pages, Ordering::Release);
        self.stamp[slot].store(seq + 1, Ordering::Release);
    }

    /// If `paddr` falls inside a block still in the ring, how many frees have
    /// happened since it was freed — `0` for the most recent. `None` when it
    /// was never a DMA block, or has aged out.
    pub fn since(&self, paddr: usize) -> Option<u64> {
        let now = self.seq.load(Ordering::Acquire);
        for back in 0..(N as u64) {
            if back >= now {
                break;
            }
            let want = now - 1 - back;
            let slot = (want % N as u64) as usize;
            // A slot still holding the write this position names. Zero is a
            // write in flight, and a never-written slot never matches.
            if self.stamp[slot].load(Ordering::Acquire) != want + 1 {
                continue;
            }
            let base = self.base[slot].load(Ordering::Acquire);
            let pages = self.pages[slot].load(Ordering::Acquire);
            // And still holding it afterwards, which is what says the two
            // words belong together.
            if self.stamp[slot].load(Ordering::Acquire) != want + 1 {
                continue;
            }
            if block_holds(base, pages, paddr) {
                return Some(back);
            }
        }
        None
    }

    /// How many DMA blocks have been freed since boot.
    pub fn freed(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }
}

/// A fixed reserve of physical frames, handed out and never given back.
///
/// Filled once at boot, where allocating is ordinary, and drawn on from a path
/// that cannot allocate. Running out is not a failure — the caller refuses
/// that one band and the scheduler keeps its soft canary — but it is the
/// difference between a machine whose coroutine stacks are guarded and one
/// whose are not, so it is counted rather than merely returned.
pub struct FramePool<const N: usize> {
    frames: [AtomicUsize; N],
    len: AtomicUsize,
    taken: AtomicUsize,
    refused: AtomicUsize,
}

impl<const N: usize> Default for FramePool<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FramePool<N> {
    pub const fn new() -> Self {
        Self {
            frames: [const { AtomicUsize::new(0) }; N],
            len: AtomicUsize::new(0),
            taken: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
        }
    }

    /// Add one reserved frame. `false` once the reserve is full.
    pub fn push(&self, paddr: usize) -> bool {
        let i = self.len.load(Ordering::Relaxed);
        if i >= N {
            return false;
        }
        self.frames[i].store(paddr, Ordering::Relaxed);
        self.len.store(i + 1, Ordering::Release);
        true
    }

    /// Hand out one frame, or `None` once the reserve is spent.
    ///
    /// The cursor advances only when a frame actually comes out. Advancing it
    /// on the way past the end — which is what a bare `fetch_add` did — made
    /// "spent" and "spent, and then refused four thousand more" the same
    /// number once it was clamped for display, so the boot log could report a
    /// full reserve and an exhausted one identically. On the architectures
    /// where every band needs a split, that line is the only thing that says
    /// why the stacks ended up with no hard guard.
    pub fn take(&self) -> Option<usize> {
        let len = self.len.load(Ordering::Acquire);
        loop {
            let i = self.taken.load(Ordering::Acquire);
            if i >= len {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            if self
                .taken
                .compare_exchange_weak(i, i + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(self.frames[i].load(Ordering::Relaxed));
            }
        }
    }

    /// `(frames reserved, frames handed out, requests refused)`.
    pub fn stats(&self) -> (usize, usize, usize) {
        (
            self.len.load(Ordering::Relaxed),
            self.taken.load(Ordering::Relaxed),
            self.refused.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2 words = 128 frames, so both word boundaries and the cap are reachable.
    type Small = FrameSet<2>;

    fn at(frame: usize) -> usize {
        frame * PAGE_SIZE
    }

    // ── the per-word mask ───────────────────────────────────────────────────

    #[test]
    fn the_word_mask_covers_exactly_the_frames_asked_for() {
        assert_eq!(word_mask(0, 0), 1);
        assert_eq!(word_mask(63, 63), 1 << 63);
        assert_eq!(word_mask(0, 63), u64::MAX);
        assert_eq!(word_mask(4, 7), 0b1111_0000);
        assert_eq!(word_mask(60, 63), 0xf000_0000_0000_0000);
    }

    // ── the stack-frame bitset ──────────────────────────────────────────────

    #[test]
    fn a_marked_frame_is_tracked_until_it_is_cleared() {
        let s = Small::new();
        assert!(!s.contains(7));
        s.mark(7);
        assert!(s.contains(7));
        assert!(!s.contains(6));
        assert!(!s.contains(8));
        s.clear(7);
        assert!(!s.contains(7));
    }

    #[test]
    fn a_frame_past_the_cap_is_counted_and_not_tracked() {
        let s = Small::new();
        assert_eq!(s.tracked_frames(), 128);
        s.mark(128);
        s.mark(1_000_000);
        assert_eq!(s.over_cap(), 2);
        assert!(!s.contains(128));
        // And clearing one is not an out-of-bounds write.
        s.clear(128);
        assert_eq!(s.over_cap(), 2);
    }

    #[test]
    fn a_write_that_straddles_a_live_stack_frame_is_caught() {
        let s = Small::new();
        s.mark(40);
        // Starts before it and ends after it.
        assert!(s.aliases(at(38), 5 * PAGE_SIZE));
        // Ends in the middle of it.
        assert!(s.aliases(at(38), 2 * PAGE_SIZE + 1));
        // Starts in the middle of it.
        assert!(s.aliases(at(40) + PAGE_SIZE - 1, 2));
        // Stops one byte short of it.
        assert!(!s.aliases(at(38), 2 * PAGE_SIZE));
        // Starts one byte past it.
        assert!(!s.aliases(at(41), PAGE_SIZE));
    }

    #[test]
    fn a_neighbour_in_the_same_word_is_not_a_hit() {
        // The walk is a word at a time, sixty-four frames per load, so the
        // masking of the two edge words is the whole of its precision: a
        // marked frame that shares a word with the range but sits outside it
        // must not answer.
        let s = Small::new();
        s.mark(0);
        s.mark(63);
        assert!(!s.aliases(at(1), 62 * PAGE_SIZE));
        assert!(s.aliases(at(0), PAGE_SIZE));
        assert!(s.aliases(at(63), PAGE_SIZE));
    }

    #[test]
    fn a_range_spanning_several_words_finds_a_frame_in_any_of_them() {
        let s = Small::new();
        s.mark(70);
        assert!(s.aliases(at(10), 100 * PAGE_SIZE));
        assert!(!s.aliases(at(10), 50 * PAGE_SIZE));
        s.clear(70);
        s.mark(10);
        assert!(s.aliases(at(10), 100 * PAGE_SIZE));
    }

    #[test]
    fn a_set_that_tracks_nothing_answers_for_nothing() {
        // The degenerate size has to answer rather than reach the arithmetic
        // below, where "the last frame there is" is one less than none.
        let s: FrameSet<0> = FrameSet::new();
        assert_eq!(s.tracked_frames(), 0);
        assert!(!s.aliases(0, PAGE_SIZE));
        assert!(!s.contains(0));
        s.mark(0);
        s.clear(0);
        assert_eq!(s.over_cap(), 1);
        assert!(!s.aliases(0, usize::MAX));
    }

    #[test]
    fn a_write_of_no_length_is_not_a_write() {
        let s = Small::new();
        s.mark(3);
        assert!(!s.aliases(at(3), 0));
    }

    #[test]
    fn a_range_running_past_the_cap_still_answers_for_the_part_it_covers() {
        let s = Small::new();
        s.mark(127);
        assert!(s.aliases(at(120), 1_000 * PAGE_SIZE));
        assert!(!s.aliases(at(128), 1_000 * PAGE_SIZE));
    }

    #[test]
    fn a_length_that_runs_off_the_end_of_the_address_space_does_not_go_quiet() {
        // `paddr + len` wrapping would put the last frame below the first and
        // leave the loop with nothing to do — a guard answering "no alias" to
        // the most malformed request it will ever see.
        let s = Small::new();
        s.mark(1);
        assert!(s.aliases(at(0), usize::MAX));
        assert!(s.aliases(PAGE_SIZE, usize::MAX));
    }

    // ── the recently-freed DMA ring ─────────────────────────────────────────

    #[test]
    fn the_most_recent_free_is_zero_frees_ago() {
        let r: FreedRing<4> = FreedRing::new();
        r.note(0x1000, 2);
        assert_eq!(r.since(0x1000), Some(0));
        assert_eq!(r.since(0x1fff), Some(0));
        assert_eq!(r.since(0x2fff), Some(0));
        assert_eq!(r.since(0x3000), None, "one page past the end");
        assert_eq!(r.since(0xfff), None, "one byte before the start");
    }

    #[test]
    fn distance_is_counted_in_frees_since() {
        let r: FreedRing<4> = FreedRing::new();
        r.note(0x1000, 1);
        r.note(0x5000, 1);
        r.note(0x9000, 1);
        assert_eq!(r.since(0x9000), Some(0));
        assert_eq!(r.since(0x5000), Some(1));
        assert_eq!(r.since(0x1000), Some(2));
        assert_eq!(r.freed(), 3);
    }

    #[test]
    fn a_block_ages_out_once_the_ring_has_turned_over() {
        let r: FreedRing<4> = FreedRing::new();
        r.note(0x1000, 1);
        for i in 0..4 {
            r.note(0x10_0000 + i * 0x1000, 1);
        }
        assert_eq!(r.since(0x1000), None);
        assert_eq!(r.since(0x10_3000), Some(0));
        assert_eq!(r.since(0x10_0000), Some(3));
    }

    #[test]
    fn a_free_of_no_pages_is_not_recorded_at_all() {
        // It would otherwise burn a ring slot and push a real block out of it.
        let r: FreedRing<4> = FreedRing::new();
        r.note(0x1000, 1);
        r.note(0x5000, 0);
        assert_eq!(r.freed(), 1);
        assert_eq!(r.since(0x1000), Some(0));
        assert_eq!(r.since(0x5000), None);
    }

    #[test]
    fn an_empty_ring_answers_for_nothing() {
        let r: FreedRing<4> = FreedRing::new();
        assert_eq!(r.since(0), None);
        assert_eq!(r.since(0x1000), None);
        assert_eq!(r.freed(), 0);
    }

    #[test]
    fn a_block_whose_length_runs_off_the_end_matches_only_above_its_base() {
        // A torn or corrupt entry must not wrap its end below its base and
        // start claiming every address under it — on the fault path, that
        // reads as "a freed DMA buffer wrote here", which is the conclusion
        // the whole ring exists to make trustworthy.
        // A length whose product with the page size lands exactly on a
        // multiple of 2^64 wraps to *zero*, which puts the end at the base and
        // makes the block answer for nothing at all.
        assert!(block_holds(0x1000, 1 << 52, 0x2000));
        let base = usize::MAX - PAGE_SIZE;
        // Wrapping the length would put the end *below* the base, and the
        // block would answer for nothing at all.
        assert!(block_holds(base, usize::MAX, base));
        assert!(block_holds(base, usize::MAX, usize::MAX - 1));
        // Saturating to the top of the address space is not the same as
        // wrapping round to the bottom of it.
        assert!(!block_holds(base, usize::MAX, 0));
        assert!(!block_holds(base, usize::MAX, base - 1));
        assert!(!block_holds(0x1000, 0, 0x1000));
    }

    #[test]
    fn a_reader_never_pairs_one_blocks_base_with_anothers_length() {
        // The one finding here that a single thread cannot show. A slot is two
        // words, so a reader can take the base of the block that was in it and
        // the length of the block that replaced it. The length used to be
        // published *first*, which is the order that makes that pair possible;
        // the comment above it claimed the opposite, that a reader would see
        // the old pair or nothing.
        //
        // These two blocks share no address, but A's base with B's length
        // spans sixteen megabytes from A — and `PROBE` sits inside that span
        // and inside neither block. A hit on it is a range that never existed,
        // reported on the already-fatal fault path whose whole job is to name
        // the writer.
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;

        const A_BASE: usize = 0x1000_0000;
        const B_BASE: usize = 0x2000_0000;
        const B_PAGES: usize = 0x1000;
        const PROBE: usize = 0x1080_0000;
        assert!(!block_holds(A_BASE, 1, PROBE));
        assert!(!block_holds(B_BASE, B_PAGES, PROBE));
        assert!(
            block_holds(A_BASE, B_PAGES, PROBE),
            "the pair that must not form"
        );

        // One slot, so every write lands on the same two words.
        let ring: Arc<FreedRing<1>> = Arc::new(FreedRing::new());
        let done = Arc::new(AtomicBool::new(false));
        let (w_ring, w_done) = (ring.clone(), done.clone());
        let writer = std::thread::spawn(move || {
            for _ in 0..50_000 {
                w_ring.note(A_BASE, 1);
                w_ring.note(B_BASE, B_PAGES);
            }
            w_done.store(true, Ordering::Release);
        });
        while !done.load(Ordering::Acquire) {
            assert_eq!(
                ring.since(PROBE),
                None,
                "read a base and a length that belong to different blocks"
            );
        }
        writer.join().unwrap();
    }

    // ── the reserve of page-table frames ────────────────────────────────────

    #[test]
    fn frames_come_back_out_in_the_order_they_went_in() {
        let p: FramePool<3> = FramePool::new();
        assert!(p.push(0x1000));
        assert!(p.push(0x2000));
        assert_eq!(p.take(), Some(0x1000));
        assert_eq!(p.take(), Some(0x2000));
        assert_eq!(p.stats(), (2, 2, 0));
    }

    #[test]
    fn the_reserve_refuses_more_than_it_holds() {
        let p: FramePool<2> = FramePool::new();
        assert!(p.push(0x1000));
        assert!(p.push(0x2000));
        assert!(!p.push(0x3000), "the third does not fit");
        assert_eq!(p.stats(), (2, 0, 0));
    }

    #[test]
    fn a_spent_reserve_counts_what_it_could_not_give() {
        // The cursor used to advance whether or not a frame came out, so a
        // reserve that had been asked four thousand times too many reported
        // the same "128 of 128 spent" as one that was merely full. On the
        // architectures where every band needs a split, that line is the only
        // thing that says why the stacks ended up with no hard guard.
        let p: FramePool<2> = FramePool::new();
        p.push(0x1000);
        p.push(0x2000);
        assert_eq!(p.take(), Some(0x1000));
        assert_eq!(p.take(), Some(0x2000));
        assert_eq!(p.take(), None);
        assert_eq!(p.take(), None);
        assert_eq!(p.take(), None);
        assert_eq!(p.stats(), (2, 2, 3));
    }

    #[test]
    fn an_empty_reserve_gives_nothing_and_says_so() {
        let p: FramePool<4> = FramePool::new();
        assert_eq!(p.take(), None);
        assert_eq!(p.stats(), (0, 0, 1));
    }
}
