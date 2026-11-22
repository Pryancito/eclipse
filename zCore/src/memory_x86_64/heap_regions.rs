//! Which addresses the kernel heap owns: see the module docs in
//! `memory_x86_64.rs`, where it is declared.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Regions the registry can hold: the static arena, plus one per elastic
/// growth.
///
/// This has to cover the allocator's *worst* case, not its typical one. A
/// growth takes 32 MiB when it can, but under memory pressure it falls
/// back to 8 MiB and then 2 MiB, so the heap can reach its 2 GiB ceiling
/// from a 512 MiB arena in 768 small steps. An overflow does not merely
/// lose a region: `check` then stops answering `Outside` at all, so the
/// whole guard would switch itself off exactly on the machine that is
/// short of memory. A `const` assertion next to the growth policy ties
/// this number to those chunk sizes, so it cannot silently fall behind.
///
/// At three words a region this is 24 KiB of `.bss`, which is the
/// cheapest part of the bargain.
///
/// 4608 since the ceiling follows RAM up to 8 GiB: (8 GiB - 512 MiB) / 2 MiB
/// is 3840 growths, plus the arena, plus room. 108 KiB of `.bss`.
pub const MAX_REGIONS: usize = 4608;

static STARTS: [AtomicUsize; MAX_REGIONS] = [const { AtomicUsize::new(0) }; MAX_REGIONS];
static ENDS: [AtomicUsize; MAX_REGIONS] = [const { AtomicUsize::new(0) }; MAX_REGIONS];
/// Slots handed out. Always `>= COUNT`: a slot is reserved here, written,
/// and only then published through `COUNT`.
static RESERVED: AtomicUsize = AtomicUsize::new(0);
/// Slots that are fully written. Readers never look past this.
static COUNT: AtomicUsize = AtomicUsize::new(0);
static UNREGISTERED: AtomicUsize = AtomicUsize::new(0);
/// Bytes of each region the heap has given back to the frame allocator.
/// See [`note_released`].
static RELEASED: [AtomicUsize; MAX_REGIONS] = [const { AtomicUsize::new(0) }; MAX_REGIONS];
/// The region that answered last. Every free runs through [`contains`], so
/// the common case -- block after block out of the same region -- is two
/// comparisons instead of a walk over every region the heap has grown.
static HINT: AtomicUsize = AtomicUsize::new(0);

/// Why a block must not be handed to the allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFault {
    /// A null pointer.
    Null,
    /// Not aligned to what its own layout asked for.
    Unaligned,
    /// `ptr + size` wraps the address space.
    Wraps,
    /// Inside no region the heap owns.
    Outside,
}

impl BlockFault {
    /// One phrase for a report.
    pub fn why(self) -> &'static str {
        match self {
            BlockFault::Null => "null pointer",
            BlockFault::Unaligned => "not aligned to its own layout",
            BlockFault::Wraps => "ptr + size wraps the address space",
            BlockFault::Outside => "outside every region the kernel heap owns",
        }
    }
}

/// Record a region the heap owns, `[start, end)`.
///
/// Called from `init` for the static arena and from the elastic growth
/// right where it hands the frames to the buddy. A region that does not
/// fit is counted, not dropped: see [`check`].
///
/// A slot that [`note_released`] emptied is used again before a new one
/// is taken: the heap can grow, give the growth back and grow again for
/// as long as the machine runs, and taking a fresh slot each time would
/// fill the registry and turn the guard off. Callers are serialised (the
/// heap's growth and its release both run under `HEAP_GROWING`, and
/// `init` runs before either), so two of them never pick the same empty
/// slot.
pub fn register(start: usize, end: usize) {
    if end <= start {
        return;
    }
    if absorb(start, end) {
        return;
    }
    for i in 0..registered() {
        if STARTS[i].load(Ordering::SeqCst) == 0 {
            RELEASED[i].store(0, Ordering::SeqCst);
            // End first, as below: the slot reads as empty until the
            // start lands, and then as the complete new pair.
            ENDS[i].store(end, Ordering::SeqCst);
            STARTS[i].store(start, Ordering::SeqCst);
            return;
        }
    }
    let slot = RESERVED.fetch_add(1, Ordering::SeqCst);
    if slot >= MAX_REGIONS {
        RESERVED.store(MAX_REGIONS, Ordering::SeqCst);
        UNREGISTERED.fetch_add(1, Ordering::SeqCst);
        return;
    }
    // End first: a reader that sees the start must see a complete pair.
    ENDS[slot].store(end, Ordering::SeqCst);
    STARTS[slot].store(start, Ordering::SeqCst);
    // Publish last, and in slot order. A reader that saw `COUNT` cover a
    // slot still being written would find it empty and call a perfectly
    // sound block wild -- which leaks it and prints a false accusation.
    // Growth is serialised behind the heap lock, so this never spins in
    // practice; it is here so that the invariant does not rest on that.
    while COUNT
        .compare_exchange(slot, slot + 1, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

/// Fold `[start, end)` into the regions on record that it overlaps or
/// touches, so that the registry never holds two regions that share or
/// abut an address. Answers whether it did; `false` leaves the range for
/// [`register`] to put in a slot of its own.
///
/// Both cases are real, and each turned sound blocks into wild ones:
///
/// - **Touching.** Two growths that are neighbours in physical memory are
///   neighbours in the linear map, and the buddy merges free blocks across
///   them (its buddy is `ptr ^ size`, whatever region either half came
///   from). A block of the merged size handed out and freed again lay in
///   no ONE region, so [`contains`] said no, `dealloc` refused it as wild
///   and the block was leaked -- still counted live, for good. Two 2 MiB
///   fallback chunks side by side make exactly the 4 MiB block a
///   coroutine stack used to be.
/// - **Overlapping.** A region given back in part stays on record with its
///   holes (see [`note_released`]); a later growth can take the very
///   frames of a hole. Recorded as a second region, every release of it
///   was credited to both, so the first one's count ran ahead of what it
///   had really given back until it was forgotten with blocks still live
///   in it -- and from then on every free in it was refused and leaked.
///
/// The frames a growth gets are free frames, so where it overlaps a
/// region on record it overlaps only that region's holes: those bytes are
/// the heap's again and come off the region's released count.
///
/// Widening only: a reader of [`contains`] racing this sees the old pair
/// or a wider one, never a narrower, and the range is not the buddy's yet
/// (registering comes first, see the growth). Serialised with
/// [`note_released`] like [`register`].
fn absorb(start: usize, end: usize) -> bool {
    let (mut lo, mut hi) = (start, end);
    let mut keep: Option<usize> = None;
    let mut released = 0usize;
    // Bytes of `[start, end)` already inside a region on record.
    let mut overlap = 0usize;
    // A range can bridge two regions, and widening to one can make it
    // touch another it was compared with earlier: go round until a pass
    // folds nothing in.
    loop {
        let mut folded = false;
        for i in 0..registered() {
            if keep == Some(i) {
                continue;
            }
            let s = STARTS[i].load(Ordering::SeqCst);
            if s == 0 {
                continue;
            }
            let e = ENDS[i].load(Ordering::SeqCst);
            if e < lo || s > hi {
                continue;
            }
            overlap += end.min(e).saturating_sub(start.max(s));
            released += RELEASED[i].load(Ordering::SeqCst);
            lo = lo.min(s);
            hi = hi.max(e);
            match keep {
                None => keep = Some(i),
                Some(k) => {
                    // Widen the kept slot over this one before this one
                    // goes, so its addresses are never in neither.
                    ENDS[k].store(hi, Ordering::SeqCst);
                    STARTS[k].store(lo, Ordering::SeqCst);
                    STARTS[i].store(0, Ordering::SeqCst);
                    RELEASED[i].store(0, Ordering::SeqCst);
                }
            }
            folded = true;
        }
        if !folded {
            break;
        }
    }
    let Some(k) = keep else {
        return false;
    };
    RELEASED[k].store(released.saturating_sub(overlap), Ordering::SeqCst);
    // End first, then start: each store only widens.
    ENDS[k].store(hi, Ordering::SeqCst);
    STARTS[k].store(lo, Ordering::SeqCst);
    true
}

/// Record that `[start, end)` went back to the frame allocator, and
/// forget every region that has now been given back whole. Answers how
/// many regions were forgotten.
///
/// A region given back only in part stays on record, holes and all: the
/// registry has one range per region, and splitting it would take slots
/// a growth may need. The cost is that a wild free into a hole is not
/// called wild, which is the same blind spot the guard has for a free
/// into a part of the heap that is merely unused.
///
/// Regions on record never overlap or touch ([`absorb`] folds them
/// together), so a range given back is credited to one region only.
/// Serialised with [`register`], for the same reason.
pub fn note_released(start: usize, end: usize) -> usize {
    let mut forgotten = 0;
    for i in 0..registered() {
        let s = STARTS[i].load(Ordering::SeqCst);
        if s == 0 {
            continue;
        }
        let e = ENDS[i].load(Ordering::SeqCst);
        let (lo, hi) = (start.max(s), end.min(e));
        if lo >= hi {
            continue;
        }
        let gone = RELEASED[i].fetch_add(hi - lo, Ordering::SeqCst) + (hi - lo);
        if gone >= e - s {
            STARTS[i].store(0, Ordering::SeqCst);
            RELEASED[i].store(0, Ordering::SeqCst);
            forgotten += 1;
        }
    }
    forgotten
}

/// Whether `[addr, addr + size)` lies inside one region.
///
/// One region is enough: regions that touch are recorded as one (see
/// [`absorb`]), so a block the buddy merged across two growths still
/// lies in a single region.
pub fn contains(addr: usize, size: usize) -> bool {
    let Some(end) = addr.checked_add(size) else {
        return false;
    };
    let n = COUNT.load(Ordering::SeqCst).min(MAX_REGIONS);
    let hint = HINT.load(Ordering::Relaxed);
    if hint < n && in_region(hint, addr, end) {
        return true;
    }
    for i in 0..n {
        if i != hint && in_region(i, addr, end) {
            HINT.store(i, Ordering::Relaxed);
            return true;
        }
    }
    false
}

fn in_region(i: usize, addr: usize, end: usize) -> bool {
    let start = STARTS[i].load(Ordering::SeqCst);
    start != 0 && addr >= start && end <= ENDS[i].load(Ordering::SeqCst)
}

/// One registered region, as `(start, end)`.
pub type Region = (usize, usize);

/// The region `ptr` STARTS in, ignoring the block's size: `(index,
/// start, end)`.
///
/// [`contains`] asks the question a free has to answer -- does the whole
/// block lie in one region -- and answers "no" the same way for a pointer
/// that is nowhere near the heap and for one that is a sound block's
/// address with a corrupt size on it. A refusal that cannot tell those
/// apart sends the reader hunting for the wrong bug: the first is memory
/// the heap never owned, the second is a `Layout` that stopped matching
/// its allocation. So the report asks this as well.
pub fn region_of(ptr: usize) -> Option<(usize, usize, usize)> {
    let n = registered();
    (0..n).find_map(|i| {
        let start = STARTS[i].load(Ordering::SeqCst);
        let end = ENDS[i].load(Ordering::SeqCst);
        (start != 0 && ptr >= start && ptr < end).then_some((i, start, end))
    })
}

/// The registered regions nearest `ptr` on each side: the one with the
/// greatest `end` at or below it, and the one with the smallest `start`
/// above it.
///
/// What a refusal is missing is a yardstick. "Outside every region" from a
/// registry whose regions are a page away from the pointer says the block
/// is sound and the registry lost a growth; the same words with the
/// nearest region a gigabyte away say the pointer was never the heap's.
/// Returned rather than printed so the pure part can be tested on the
/// host, where the allocator itself is not compiled at all.
pub fn neighbours(ptr: usize) -> (Option<Region>, Option<Region>) {
    let n = registered();
    let mut below: Option<Region> = None;
    let mut above: Option<Region> = None;
    for i in 0..n {
        let start = STARTS[i].load(Ordering::SeqCst);
        let end = ENDS[i].load(Ordering::SeqCst);
        if start == 0 {
            continue;
        }
        if end <= ptr {
            if below.is_none_or(|(_, e)| end > e) {
                below = Some((start, end));
            }
        } else if start > ptr && above.is_none_or(|(s, _)| start < s) {
            above = Some((start, end));
        }
    }
    (below, above)
}

/// Regions on record.
pub fn registered() -> usize {
    COUNT.load(Ordering::SeqCst).min(MAX_REGIONS)
}

/// Regions the heap owns that did not fit in the registry.
pub fn unregistered() -> usize {
    UNREGISTERED.load(Ordering::SeqCst)
}

/// Judge a block the allocator is about to take back, or has just handed
/// out. `None` means "nothing to say" — which is not the same as "sound".
///
/// Two deliberate silences, both so that a healthy kernel is never
/// slandered. Before `init` there are no regions and no basis for an
/// opinion; and once a growth has failed to fit in the registry, the
/// registry no longer knows the whole heap, so `Outside` stops being
/// evidence. The cheap checks — null, alignment, wrap — hold either way,
/// because they need no map of the heap at all.
pub fn check(ptr: usize, size: usize, align: usize) -> Option<BlockFault> {
    if ptr == 0 {
        return Some(BlockFault::Null);
    }
    if align != 0 && !ptr.is_multiple_of(align) {
        return Some(BlockFault::Unaligned);
    }
    if ptr.checked_add(size).is_none() {
        return Some(BlockFault::Wraps);
    }
    if registered() == 0 || unregistered() > 0 {
        return None;
    }
    if contains(ptr, size) {
        return None;
    }
    Some(BlockFault::Outside)
}

/// The first [`SNAPSHOT`] regions, in the order they were taken, for a
/// reader that wants the map rather than a verdict (`/proc/kheap`).
/// Empty slots come back as `(0, 0)`.
pub fn snapshot() -> [Region; SNAPSHOT] {
    let n = registered().min(SNAPSHOT);
    core::array::from_fn(|i| {
        if i < n {
            (
                STARTS[i].load(Ordering::SeqCst),
                ENDS[i].load(Ordering::SeqCst),
            )
        } else {
            (0, 0)
        }
    })
}

/// Regions one [`snapshot`] carries. Enough for the static arena plus the
/// growths a 2 GiB heap needs at the usual 32 MiB chunk, which is the
/// shape every reading so far has had; a heap that fell back to smaller
/// chunks prints the first of them and says how many it did not.
pub const SNAPSHOT: usize = 64;

/// Forget every region. Tests only: while the kernel runs, a region is
/// forgotten only once [`note_released`] has seen all of it given back.
#[cfg(test)]
pub(super) fn reset() {
    for i in 0..MAX_REGIONS {
        STARTS[i].store(0, Ordering::SeqCst);
        ENDS[i].store(0, Ordering::SeqCst);
        RELEASED[i].store(0, Ordering::SeqCst);
    }
    COUNT.store(0, Ordering::SeqCst);
    RESERVED.store(0, Ordering::SeqCst);
    UNREGISTERED.store(0, Ordering::SeqCst);
    HINT.store(0, Ordering::SeqCst);
}
