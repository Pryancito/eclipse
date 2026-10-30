//! Counters split per CPU, so the allocators' hot paths stop sharing a line.
//!
//! The heap's front cache and the frame allocator's per-CPU lists took the
//! global locks off their hot paths, and the statistics beside them put a
//! shared cache line right back: every heap `alloc`/`dealloc` did a
//! `fetch_add` on `HEAP_USED`, on a `HEAP_LIVE` bucket and on a `HOT_LIVE`
//! slot, and every frame on `FRAMES_USED` and `FRAMES_CACHED`. An atomic RMW
//! on a line another CPU just wrote has to fetch it from that CPU first, so
//! with several CPUs allocating each of those adds costs as much as -- or more
//! than -- the whole cached allocation it is counting. That is the same reason
//! Linux keeps `vm_stat` and `percpu_counter` per CPU.
//!
//! Each CPU gets its own row, aligned to a cache line, and adds into it; a
//! reader sums the rows. The adds stay atomic (relaxed), so a thread that
//! migrates between reading its CPU id and adding, or an interrupt that
//! allocates in the middle of an add, still lands the delta *somewhere*; only
//! the sum is meaningful, and the sum is exact. A row can go negative (a block
//! allocated on one CPU and freed on another), which is why the cells are
//! signed and only the sum is clamped.

use core::sync::atomic::{AtomicIsize, Ordering};
use kernel_hal::cpu_topology::{percpu_slot, PERCPU_SLOTS};

/// One CPU's row: `N` cells on lines no other CPU writes to.
#[repr(align(64))]
struct Row<const N: usize>([AtomicIsize; N]);

/// `N` counters, each split across every CPU slot.
pub struct PerCpuCounters<const N: usize> {
    rows: [Row<N>; PERCPU_SLOTS],
}

/// This CPU's row index.
#[inline]
pub fn this_slot() -> usize {
    percpu_slot(kernel_hal::cpu::cpu_id() as usize)
}

impl<const N: usize> PerCpuCounters<N> {
    pub const fn new() -> Self {
        PerCpuCounters {
            rows: [const { Row([const { AtomicIsize::new(0) }; N]) }; PERCPU_SLOTS],
        }
    }

    /// Add `delta` to counter `i` on this CPU's row, and answer the row's new
    /// value -- a local figure, not the counter's.
    #[inline]
    pub fn add(&self, i: usize, delta: isize) -> isize {
        self.add_on(this_slot(), i, delta)
    }

    /// [`add`](Self::add) on an explicit row. For the tests, which have no
    /// CPUs to stand on.
    #[inline]
    pub fn add_on(&self, slot: usize, i: usize, delta: isize) -> isize {
        self.rows[slot].0[i]
            .fetch_add(delta, Ordering::Relaxed)
            .wrapping_add(delta)
    }

    /// Counter `i`: every row added up. Never below zero -- a read racing a
    /// free that lands before its alloc's add is not a negative count.
    pub fn sum(&self, i: usize) -> usize {
        let total = self.rows.iter().fold(0isize, |acc, r| {
            acc.wrapping_add(r.0[i].load(Ordering::Relaxed))
        });
        total.max(0) as usize
    }
}

/// One counter split per CPU.
pub struct PerCpuCounter(PerCpuCounters<1>);

impl PerCpuCounter {
    pub const fn new() -> Self {
        PerCpuCounter(PerCpuCounters::new())
    }

    #[inline]
    pub fn add(&self, delta: usize) {
        self.0.add(0, delta as isize);
    }

    #[inline]
    pub fn sub(&self, delta: usize) {
        self.0.add(0, (delta as isize).wrapping_neg());
    }

    pub fn get(&self) -> usize {
        self.0.sum(0)
    }
}

/// A per-CPU counter with a cheap, slightly stale global reading: Linux's
/// `percpu_counter` with its `batch`.
///
/// For a figure that is *read* on the hot path as well as written. The heap
/// compares `HEAP_USED` to its growth watermark on every allocation, and
/// summing 65 rows there would trade the contended add for 65 loads. Instead
/// each CPU folds its row into the shared [`approx`](Self::approx) once the row
/// has drifted `BATCH` away from zero, so the shared line is written once per
/// `BATCH` bytes of net allocation on a CPU rather than once per call -- and a
/// CPU that allocates and frees in pairs, which is what the front cache
/// serves, never writes it at all.
///
/// `approx` is off by at most `BATCH` per CPU slot; [`get`](Self::get) is
/// exact.
pub struct BatchedCounter<const BATCH: isize> {
    global: AtomicIsize,
    local: PerCpuCounters<1>,
}

impl<const BATCH: isize> BatchedCounter<BATCH> {
    pub const fn new() -> Self {
        BatchedCounter {
            global: AtomicIsize::new(0),
            local: PerCpuCounters::new(),
        }
    }

    #[inline]
    pub fn add(&self, delta: usize) {
        self.add_on(this_slot(), delta as isize);
    }

    #[inline]
    pub fn sub(&self, delta: usize) {
        self.add_on(this_slot(), (delta as isize).wrapping_neg());
    }

    /// Add on an explicit row; see [`PerCpuCounters::add_on`].
    #[inline]
    pub fn add_on(&self, slot: usize, delta: isize) {
        let v = self.local.add_on(slot, 0, delta);
        if v >= BATCH || v <= -BATCH {
            // Move what this row holds into the global figure. A `fetch_sub`
            // of `v` rather than a `swap(0)`: an interrupt that added in
            // between keeps its delta in the row instead of losing it.
            self.local.add_on(slot, 0, v.wrapping_neg());
            self.global.fetch_add(v, Ordering::Relaxed);
        }
    }

    /// The shared figure: one load, behind the truth by less than `BATCH` per
    /// CPU slot.
    #[inline]
    pub fn approx(&self) -> usize {
        self.global.load(Ordering::Relaxed).max(0) as usize
    }

    /// The exact figure: the shared one plus every row.
    pub fn get(&self) -> usize {
        let rows = self.local.rows.iter().fold(0isize, |acc, r| {
            acc.wrapping_add(r.0[0].load(Ordering::Relaxed))
        });
        self.global
            .load(Ordering::Relaxed)
            .wrapping_add(rows)
            .max(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sum_is_every_row_added_up_even_when_a_row_is_negative() {
        let c: PerCpuCounters<2> = PerCpuCounters::new();
        // Allocated on CPU 0, freed on CPU 3: CPU 3's row goes negative, and
        // the counter still says nothing is live.
        c.add_on(0, 1, 5);
        c.add_on(3, 1, -5);
        assert_eq!(c.sum(1), 0);
        c.add_on(PERCPU_SLOTS - 1, 1, 7);
        assert_eq!(c.sum(1), 7);
        assert_eq!(c.sum(0), 0, "counters do not bleed into each other");
    }

    #[test]
    fn a_sum_that_would_be_negative_reads_zero() {
        let c: PerCpuCounters<1> = PerCpuCounters::new();
        c.add_on(1, 0, -3);
        assert_eq!(c.sum(0), 0);
    }

    #[test]
    fn add_answers_the_rows_value_not_the_counters() {
        let c: PerCpuCounters<1> = PerCpuCounters::new();
        c.add_on(0, 0, 10);
        assert_eq!(c.add_on(1, 0, 1), 1);
        assert_eq!(c.add_on(0, 0, 1), 11);
    }

    #[test]
    fn rows_do_not_share_a_cache_line() {
        assert_eq!(core::mem::align_of::<Row<1>>(), 64);
        assert_eq!(core::mem::size_of::<Row<1>>(), 64);
        assert_eq!(core::mem::size_of::<Row<32>>(), 256);
    }

    #[test]
    fn a_batched_counter_is_exact_and_its_shared_figure_lags_by_less_than_a_batch() {
        let c: BatchedCounter<100> = BatchedCounter::new();
        c.add_on(0, 60);
        assert_eq!(c.get(), 60);
        assert_eq!(
            c.approx(),
            0,
            "under a batch, nothing reached the shared figure"
        );
        c.add_on(0, 60);
        assert_eq!(c.get(), 120);
        assert_eq!(c.approx(), 120, "past a batch, the whole row moved over");
        c.add_on(2, 99);
        assert_eq!(c.get(), 219);
        assert!(c.get() - c.approx() < 100);
        c.add_on(2, -99);
        c.add_on(0, -120);
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn a_batched_counter_folds_frees_too() {
        let c: BatchedCounter<100> = BatchedCounter::new();
        c.add_on(0, 500);
        assert_eq!(c.approx(), 500);
        // Freed on another CPU: its row runs negative and folds as well.
        c.add_on(1, -150);
        assert_eq!(c.approx(), 350);
        assert_eq!(c.get(), 350);
    }

    #[test]
    fn concurrent_adds_lose_nothing() {
        use std::sync::Arc;
        let c: Arc<BatchedCounter<64>> = Arc::new(BatchedCounter::new());
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let c = c.clone();
                // Two threads per row: the row is shared on purpose, which is
                // what a migration or an interrupt does to it.
                std::thread::spawn(move || {
                    for i in 0..10_000isize {
                        c.add_on(t % 4, if i % 3 == 0 { -1 } else { 2 });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // Per thread: 3334 frees of 1, 6666 allocs of 2.
        assert_eq!(c.get(), 8 * (6666 * 2 - 3334));
    }
}
