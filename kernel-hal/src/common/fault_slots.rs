//! The lock-free registry that tells the page-fault handler which kernel
//! addresses belong to a stack guard band or to a quarantined stack.
//!
//! Two of these exist. One holds the guard bands that `Executor::new` installs
//! around every coroutine stack, so a fault inside one is reported as a kernel
//! stack overflow instead of being taken to the faulting thread's VMAR. The
//! other holds the freed stacks the scheduler hands over write-protected, so a
//! stale write into one faults at the writer's own rip and names the
//! use-after-free of `docs/README-crash-repro.md`.
//!
//! They were written twice, one copy per registry, inside a module that only a
//! bare x86_64 build compiles — so neither copy was ever compiled by anything
//! that could report a mistake in it, and a fix to one would not have reached
//! the other. One table, here, where every build compiles it.
//!
//! No lock and no allocation, because [`SlotTable::slot_of`] runs on the
//! page-fault path: taking a lock there turns a diagnosable overflow into a
//! deadlock, and that path is not rare — a kernel fault on a *user* address
//! (copy-to-user touching an uncommitted page) comes through it too.

use core::sync::atomic::{fence, AtomicUsize, Ordering};

/// One registry entry, published under a sequence counter.
///
/// `seq` is even while the entry is readable and odd while it is being
/// written; a claim and a release each take it through one odd value. A reader
/// that sees a different `seq` either side of its loads has straddled a write
/// and must not believe the pair it read — see [`covers`] for why that matters
/// here specifically.
pub struct Slot {
    seq: AtomicUsize,
    base: AtomicUsize,
    end: AtomicUsize,
    flags: AtomicUsize,
}

impl Slot {
    const fn new() -> Self {
        Self {
            seq: AtomicUsize::new(0),
            base: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            flags: AtomicUsize::new(0),
        }
    }
}

/// Whether one slot's snapshot puts `vaddr` inside its range.
///
/// Pure, and separate from the loads that produce it, because the case worth
/// stating is the one that cannot be reached by inspection: `base` and `end`
/// are two words, so a reader can take `base` from the entry that was there
/// and `end` from the one that replaced it. A registry slot is reused as soon
/// as an executor dies and the next one is created, so the two halves of such
/// a pair name unrelated bands — and the range they describe together covers
/// memory that never was a guard band at all. Believing it turns an ordinary
/// kernel page fault into "stack overflow", which is a diagnosis, not a
/// symptom: it stops the fault being resolved against the VMAR that could
/// actually explain it.
///
/// `seq_before != seq_after`, or an odd `seq_before`, means exactly that: the
/// pair is not a pair and the answer is no.
pub fn covers(seq_before: usize, base: usize, end: usize, seq_after: usize, vaddr: usize) -> bool {
    seq_before == seq_after
        && seq_before.is_multiple_of(2)
        && base != 0
        && vaddr >= base
        && vaddr < end
}

/// Whether `vaddr` is a kernel-half address.
///
/// Every band this table holds is kernel memory, so a user address is answered
/// without touching the table at all — which is most of the traffic on the
/// fault path.
#[inline]
pub fn is_kernel_half(vaddr: usize) -> bool {
    (vaddr as isize) < 0
}

/// A fixed table of [`Slot`]s plus the counters the boot log reports.
pub struct SlotTable<const N: usize> {
    slots: [Slot; N],
    /// One past the highest index ever claimed, so [`SlotTable::slot_of`] does
    /// not walk the whole table on every kernel page fault.
    high: AtomicUsize,
    accepted: AtomicUsize,
    refused: AtomicUsize,
}

impl<const N: usize> Default for SlotTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> SlotTable<N> {
    pub const fn new() -> Self {
        Self {
            slots: [const { Slot::new() }; N],
            high: AtomicUsize::new(0),
            accepted: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
        }
    }

    /// Claim a free slot for `[base, base + size)`, returning its index.
    ///
    /// `None` when the table is full, which is not fatal anywhere it is used:
    /// the caller refuses that band and the scheduler keeps its soft canary.
    /// A zero `base` cannot be registered — it is the marker for a free slot,
    /// and no guard band ever lives at virtual address 0.
    pub fn claim(&self, base: usize, size: usize, flags: usize) -> Option<usize> {
        if base == 0 || size == 0 {
            return None;
        }
        for i in 0..N {
            let s = self.slots[i].seq.load(Ordering::Acquire);
            if !s.is_multiple_of(2) || self.slots[i].base.load(Ordering::Relaxed) != 0 {
                continue;
            }
            // Winning this take the slot: a competing claim, or a release,
            // would have moved `seq` past `s`.
            if self.slots[i]
                .seq
                .compare_exchange(s, s + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            self.slots[i].base.store(base, Ordering::Relaxed);
            self.slots[i].end.store(base + size, Ordering::Relaxed);
            self.slots[i].flags.store(flags, Ordering::Relaxed);
            self.slots[i].seq.store(s + 2, Ordering::Release);
            self.high.fetch_max(i + 1, Ordering::Release);
            return Some(i);
        }
        None
    }

    /// Release a slot claimed by [`Self::claim`]. Out-of-range indices are
    /// ignored rather than panicking: this runs on the teardown path of a
    /// coroutine stack, where a panic would take the kernel with it.
    pub fn free(&self, i: usize) {
        if i >= N {
            return;
        }
        let s = self.slots[i].seq.fetch_add(1, Ordering::AcqRel);
        self.slots[i].base.store(0, Ordering::Relaxed);
        self.slots[i].end.store(0, Ordering::Relaxed);
        self.slots[i].flags.store(0, Ordering::Relaxed);
        self.slots[i].seq.store(s + 2, Ordering::Release);
    }

    /// The flags recorded with a live slot, or `None` if it is free.
    pub fn flags(&self, i: usize) -> Option<usize> {
        let (_, _, flags) = self.read(i)?;
        Some(flags)
    }

    /// The range recorded with a live slot, or `None` if it is free.
    pub fn range(&self, i: usize) -> Option<(usize, usize)> {
        let (base, end, _) = self.read(i)?;
        Some((base, end))
    }

    fn read(&self, i: usize) -> Option<(usize, usize, usize)> {
        if i >= N {
            return None;
        }
        let s1 = self.slots[i].seq.load(Ordering::Acquire);
        let base = self.slots[i].base.load(Ordering::Relaxed);
        let end = self.slots[i].end.load(Ordering::Relaxed);
        let flags = self.slots[i].flags.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        let s2 = self.slots[i].seq.load(Ordering::Acquire);
        // `end` is exclusive, so `base` itself is the cheapest address that is
        // inside any live range.
        if covers(s1, base, end, s2, base) {
            Some((base, end, flags))
        } else {
            None
        }
    }

    /// The slot covering `vaddr`, if any.
    pub fn slot_of(&self, vaddr: usize) -> Option<usize> {
        if !is_kernel_half(vaddr) {
            return None;
        }
        // `take` rather than a clamp on the mark: the walk is bounded by the
        // array itself, so a mark that somehow read too high costs time and
        // not an access past the end — which matters in a module whose whole
        // premise is that this kernel's memory gets overwritten. A clamp would
        // have been a branch no test could ever take.
        let high = self.high.load(Ordering::Acquire);
        self.slots
            .iter()
            .take(high)
            .enumerate()
            .find(|(_, slot)| {
                let s1 = slot.seq.load(Ordering::Acquire);
                let base = slot.base.load(Ordering::Relaxed);
                let end = slot.end.load(Ordering::Relaxed);
                fence(Ordering::Acquire);
                let s2 = slot.seq.load(Ordering::Acquire);
                covers(s1, base, end, s2, vaddr)
            })
            .map(|(i, _)| i)
    }

    /// Count one registration that went all the way through, returning how
    /// many there had been before it.
    ///
    /// Counted by the caller rather than by [`Self::claim`] because claiming
    /// the slot is not the end of the job: both callers publish the slot
    /// *before* editing the page table, so that a fault inside the band from
    /// then on is already reported as a guard hit, and both roll the whole
    /// thing back and release the slot if the edit does not verify. A count
    /// taken at the claim would report those as installed.
    pub fn note_accepted(&self) -> usize {
        self.accepted.fetch_add(1, Ordering::Relaxed)
    }

    /// Count one refused registration, returning how many there had been
    /// before it — which is what lets a caller log the first few and go quiet,
    /// on a path that runs on every executor creation.
    pub fn note_refused(&self) -> usize {
        self.refused.fetch_add(1, Ordering::Relaxed)
    }

    /// `(registrations accepted, registrations refused)` since boot.
    pub fn stats(&self) -> (usize, usize) {
        (
            self.accepted.load(Ordering::Relaxed),
            self.refused.load(Ordering::Relaxed),
        )
    }

    /// How many slots are claimed right now.
    ///
    /// A scan, bounded by the high-water mark, so it is a diagnostic rather
    /// than something to call from the fault path. It exists because
    /// "installed since boot" and "installed now" are different questions and
    /// a monotonic counter can only answer the first.
    pub fn live(&self) -> usize {
        let high = self.high.load(Ordering::Acquire);
        (0..high.min(N)).filter(|&i| self.read(i).is_some()).count()
    }

    /// One past the highest index ever claimed. Diagnostic: it is what bounds
    /// the walk on the fault path, and it never shrinks.
    pub fn high_water(&self) -> usize {
        self.high.load(Ordering::Relaxed).min(N)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: usize = 0xffff_ff00_0000_0000;

    // ── the decision a reader makes from one snapshot ───────────────────────

    #[test]
    fn a_live_slot_covers_its_own_range_and_stops_at_the_end() {
        assert!(covers(2, K, K + 0x1000, 2, K));
        assert!(covers(2, K, K + 0x1000, 2, K + 0xfff));
        assert!(!covers(2, K, K + 0x1000, 2, K + 0x1000));
        assert!(!covers(2, K, K + 0x1000, 2, K - 1));
    }

    #[test]
    fn a_free_slot_covers_nothing_even_at_address_zero() {
        // `base == 0` is what marks a slot free, so address 0 must not match
        // a slot that has been released — otherwise every null dereference in
        // the kernel would be reported as a stack overflow.
        assert!(!covers(2, 0, 0, 2, 0));
        assert!(!covers(2, 0, 0x1000, 2, 0x800));
    }

    #[test]
    fn a_pair_read_across_a_write_is_not_a_pair() {
        // The whole reason the sequence counter exists. A reader can take
        // `base` from the band that was in the slot and `end` from the band
        // that replaced it — the slot is reused as soon as one executor dies
        // and the next is made — and the two together describe memory that was
        // never a guard band. Believing it answers "stack overflow" for an
        // ordinary kernel fault, which stops it being resolved against the
        // VMAR that could have explained it.
        let (old_base, new_end) = (K, K + 0x100_0000);
        assert!(
            covers(2, old_base, new_end, 2, K + 0x8000),
            "the torn pair does cover the address; only the counter says no"
        );
        assert!(!covers(2, old_base, new_end, 4, K + 0x8000));
    }

    #[test]
    fn a_slot_being_written_covers_nothing() {
        assert!(!covers(1, K, K + 0x1000, 1, K));
        assert!(!covers(3, K, K + 0x1000, 3, K));
    }

    #[test]
    fn only_kernel_half_addresses_reach_the_table() {
        assert!(!is_kernel_half(0));
        assert!(!is_kernel_half(0x7fff_ffff_ffff));
        assert!(is_kernel_half(K));
        assert!(is_kernel_half(usize::MAX));
    }

    // ── the table ───────────────────────────────────────────────────────────

    #[test]
    fn a_claimed_band_is_found_and_a_freed_one_is_not() {
        let t: SlotTable<3> = SlotTable::new();
        let i = t.claim(K, 0x1000, 0b101).unwrap();
        assert_eq!(t.slot_of(K + 0x800), Some(i));
        assert_eq!(t.range(i), Some((K, K + 0x1000)));
        assert_eq!(t.flags(i), Some(0b101));
        t.free(i);
        assert_eq!(t.slot_of(K + 0x800), None);
        assert_eq!(t.range(i), None);
        assert_eq!(t.flags(i), None);
    }

    #[test]
    fn a_reused_slot_answers_for_its_new_band_and_not_its_old_one() {
        let t: SlotTable<3> = SlotTable::new();
        let i = t.claim(K, 0x1000, 1).unwrap();
        t.free(i);
        let j = t.claim(K + 0x10_0000, 0x2000, 2).unwrap();
        assert_eq!(i, j, "the freed slot is the one reused");
        assert_eq!(t.slot_of(K + 0x800), None);
        assert_eq!(t.slot_of(K + 0x10_0800), Some(j));
        assert_eq!(t.flags(j), Some(2));
    }

    #[test]
    fn a_user_address_never_walks_the_table() {
        let t: SlotTable<2> = SlotTable::new();
        // A band can only be kernel memory, and a kernel fault on a *user*
        // address — copy-to-user touching an uncommitted page — is ordinary
        // traffic on this path.
        t.claim(0x1000, 0x1000, 0).unwrap();
        assert_eq!(t.slot_of(0x1800), None);
    }

    #[test]
    fn a_full_table_refuses_instead_of_overwriting() {
        let t: SlotTable<2> = SlotTable::new();
        assert_eq!(t.claim(K, 0x1000, 0), Some(0));
        assert_eq!(t.claim(K + 0x1000, 0x1000, 0), Some(1));
        assert_eq!(t.claim(K + 0x2000, 0x1000, 0), None);
        // The two that got in are still intact.
        assert_eq!(t.slot_of(K), Some(0));
        assert_eq!(t.slot_of(K + 0x1000), Some(1));
    }

    #[test]
    fn a_band_at_zero_or_of_no_size_is_refused() {
        let t: SlotTable<2> = SlotTable::new();
        // Address 0 is the free marker, and an empty range would be a slot
        // that can never match anything while still occupying the table.
        assert_eq!(t.claim(0, 0x1000, 0), None);
        assert_eq!(t.claim(K, 0, 0), None);
        assert_eq!(t.live(), 0);
    }

    #[test]
    fn freeing_a_slot_that_is_not_there_is_ignored() {
        // Runs on the teardown path of a coroutine stack: a panic here takes
        // the kernel down over a bookkeeping slip.
        let t: SlotTable<2> = SlotTable::new();
        t.free(2);
        t.free(usize::MAX);
        assert_eq!(t.live(), 0);
    }

    #[test]
    fn the_high_water_mark_bounds_the_walk_and_never_shrinks() {
        let t: SlotTable<4> = SlotTable::new();
        assert_eq!(t.high_water(), 0);
        let a = t.claim(K, 0x1000, 0).unwrap();
        let b = t.claim(K + 0x1000, 0x1000, 0).unwrap();
        assert_eq!(t.high_water(), 2);
        t.free(a);
        t.free(b);
        assert_eq!(t.high_water(), 2, "slots are reused, so it cannot shrink");
        assert_eq!(t.slot_of(K), None);
    }

    #[test]
    fn a_band_past_the_high_water_mark_would_be_missed() {
        // States the coupling: `slot_of` only walks up to the mark, so a claim
        // that did not raise it would be invisible.
        let t: SlotTable<4> = SlotTable::new();
        t.claim(K, 0x1000, 0).unwrap();
        t.claim(K + 0x1000, 0x1000, 0).unwrap();
        t.claim(K + 0x2000, 0x1000, 0).unwrap();
        assert_eq!(t.high_water(), 3);
        assert_eq!(t.slot_of(K + 0x2000), Some(2));
    }

    #[test]
    fn live_counts_now_and_stats_count_ever() {
        let t: SlotTable<3> = SlotTable::new();
        let a = t.claim(K, 0x1000, 0).unwrap();
        t.note_accepted();
        let b = t.claim(K + 0x1000, 0x1000, 0).unwrap();
        t.note_accepted();
        assert_eq!(t.live(), 2);
        assert_eq!(t.stats(), (2, 0));
        t.free(a);
        t.free(b);
        assert_eq!(t.live(), 0, "nothing is protected now");
        assert_eq!(t.stats(), (2, 0), "two were, and that is a different fact");
    }

    #[test]
    fn a_claim_that_is_rolled_back_is_not_an_install() {
        // Both callers publish the slot before editing the page table, then
        // release it if the edit does not verify. Counting at the claim would
        // report those as installed bands.
        let t: SlotTable<2> = SlotTable::new();
        let i = t.claim(K, 0x1000, 0).unwrap();
        t.free(i);
        t.note_refused();
        assert_eq!(t.stats(), (0, 1));
    }

    #[test]
    fn the_refusal_counter_reports_what_came_before_it() {
        // The caller logs the first few and goes quiet; it needs the count
        // before its own, on a path that runs on every executor creation.
        let t: SlotTable<2> = SlotTable::new();
        assert_eq!(t.note_refused(), 0);
        assert_eq!(t.note_refused(), 1);
        assert_eq!(t.note_refused(), 2);
        assert_eq!(t.stats(), (0, 3));
        assert_eq!(t.note_accepted(), 0);
        assert_eq!(t.note_accepted(), 1);
        assert_eq!(t.stats(), (2, 3));
    }

    #[test]
    fn two_registries_do_not_share_their_tables() {
        // The guards and the quarantine are two instances, and a hit in one
        // must not be reported as a hit in the other: one says "stack
        // overflow", the other says "use after free".
        let guards: SlotTable<2> = SlotTable::new();
        let quar: SlotTable<2> = SlotTable::new();
        guards.claim(K, 0x1000, 0).unwrap();
        assert_eq!(guards.slot_of(K), Some(0));
        assert_eq!(quar.slot_of(K), None);
    }

    #[test]
    fn a_band_is_found_from_any_address_inside_it() {
        let t: SlotTable<2> = SlotTable::new();
        let i = t.claim(K + 0x1000, 0x8_0000, 0).unwrap();
        assert_eq!(t.slot_of(K + 0x1000), Some(i));
        assert_eq!(t.slot_of(K + 0x4_0000), Some(i));
        assert_eq!(t.slot_of(K + 0x8_0fff), Some(i));
        assert_eq!(t.slot_of(K + 0x8_1000), None);
        assert_eq!(t.slot_of(K + 0xfff), None);
    }
}
