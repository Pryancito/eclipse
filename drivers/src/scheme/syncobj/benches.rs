//! What looking at a GPU fence costs.
//!
//! A landing zone is pinned sysmem the GPU writes, mapped uncached, so a
//! read of it is a trip off the CPU's caches rather than a load. That is
//! the whole reason [`hw_fences_landed`] exists: it reads each distinct
//! zone once however many of the fences name it, where
//! `fences.iter().all(hw_fence_landed)` read the word once per fence.
//! These rows are what that saved, and what the shape it chose costs.
//!
//! A caveat that bounds every absolute figure here: **under `mock` the
//! landing zone is ordinary host memory**, so `read_volatile` is an L1
//! load and not the uncached round trip the hardware pays. So the reads
//! are UNDER-weighted here relative to the arithmetic around them, and
//! the rows below are a lower bound on the win from reading fewer of
//! them. What they do measure faithfully is the bookkeeping: the scan of
//! the per-call `words` list, which is CPU-side either way.
//!
//! `black_box` goes around the inputs, not only the result; see
//! `docs/README-benchmarks.md`.

use super::*;
use test::{black_box, Bencher};

/// A landing zone holding `word`, leaked so the address stays valid for
/// the whole row. A `Box` rather than a `Vec` element so each zone is its
/// own allocation, which is what distinct zones are on the hardware.
fn zone(word: u32) -> usize {
    alloc::boxed::Box::leak(alloc::boxed::Box::new(word)) as *const u32 as usize
}

/// The documented common case: one ring's semaphore word, carrying one
/// fence per submit that wrote the buffer. One read answers all of them,
/// so this family's slope is the compare and nothing else.
fn bench_one_zone(b: &mut Bencher, n: usize) {
    let va = zone(1000);
    let fences: Vec<(usize, u32)> = (0..n).map(|i| (va, i as u32)).collect();
    b.iter(|| black_box(hw_fences_landed(black_box(&fences))));
}

#[bench]
fn landed_2_fences_in_1_zone(b: &mut Bencher) {
    bench_one_zone(b, 2);
}

#[bench]
fn landed_8_fences_in_1_zone(b: &mut Bencher) {
    bench_one_zone(b, 8);
}

#[bench]
fn landed_256_fences_in_1_zone(b: &mut Bencher) {
    bench_one_zone(b, 256);
}

/// The case the code's own comment rules out -- "these lists are a
/// handful of entries long (one per ring that wrote the buffer), and a
/// `Vec` of pairs beats a map at that size" -- measured rather than
/// assumed, because the `words` list is scanned linearly for every
/// fence, so this is quadratic in the number of DISTINCT zones.
///
/// 256 is not an arbitrary top end: it is the group size NVK submits in,
/// so it is the number to check the "handful" against. If this family
/// grows faster than the one above, the comment is right about today's
/// lists and the shape is what would have to change if one ever got
/// wide.
fn bench_distinct_zones(b: &mut Bencher, n: usize) {
    let fences: Vec<(usize, u32)> = (0..n).map(|i| (zone(1000), i as u32)).collect();
    b.iter(|| black_box(hw_fences_landed(black_box(&fences))));
}

#[bench]
fn landed_2_fences_in_2_zones(b: &mut Bencher) {
    bench_distinct_zones(b, 2);
}

#[bench]
fn landed_8_fences_in_8_zones(b: &mut Bencher) {
    bench_distinct_zones(b, 8);
}

#[bench]
fn landed_256_fences_in_256_zones(b: &mut Bencher) {
    bench_distinct_zones(b, 256);
}

/// What it replaced: one read per fence, whatever zone it names. Against
/// `landed_256_fences_in_1_zone` this is the saving, and remember the
/// caveat above -- on hardware each of these reads is an uncached round
/// trip, so the real gap is wider than this row shows.
#[bench]
fn the_old_one_read_per_fence_over_256_in_1_zone(b: &mut Bencher) {
    let va = zone(1000);
    let fences: Vec<(usize, u32)> = (0..256).map(|i| (va, i as u32)).collect();
    b.iter(|| {
        black_box(
            black_box(&fences)
                .iter()
                .all(|&(va, p)| hw_fence_landed(va, p)),
        )
    });
}

/// The short circuit: the first fence still in flight ends the call, so a
/// wait that is going to park pays almost nothing to find out. Against
/// the all-landed row of the same width, the difference is what a frame
/// pays to learn it must wait.
#[bench]
fn not_landed_stops_at_the_first_of_256(b: &mut Bencher) {
    let va = zone(0);
    let fences: Vec<(usize, u32)> = (0..256).map(|i| (va, 1 + i as u32)).collect();
    b.iter(|| black_box(hw_fences_landed(black_box(&fences))));
}

/// One fence, which is the overwhelmingly common look.
#[bench]
fn landed_1_fence(b: &mut Bencher) {
    let va = zone(1000);
    b.iter(|| black_box(hw_fence_landed(black_box(va), black_box(1u32))));
}

/// The wrapping compare on its own: the word is a `u32` counter that runs
/// past its top, so this is a signed difference and not a `>=`. It is on
/// every fence of every look.
#[bench]
fn payload_reached_decision(b: &mut Bencher) {
    b.iter(|| black_box(payload_reached(black_box(1000u32), black_box(999u32))));
}

// ---- the wait policy ------------------------------------------------

/// The backoff decision the CPU-side `wait` takes between looks at the
/// table. It is pure arithmetic, and the point of measuring it is to
/// show that it is: the reason `wait` used to starve the compositor was
/// that it re-took the table on every spin turn, and if choosing a
/// backoff cost anything that fix would have traded one problem for
/// another.
#[bench]
fn wait_spin_backoff_on_the_eager_probes(b: &mut Bencher) {
    b.iter(|| black_box(wait_spin_backoff(black_box(0))));
}

/// The same once it has started doubling, which is the contended wait.
#[bench]
fn wait_spin_backoff_once_doubling(b: &mut Bencher) {
    b.iter(|| black_box(wait_spin_backoff(black_box(8))));
}

/// And at the cap, which is where a wait on a hung ring sits for ten
/// seconds.
#[bench]
fn wait_spin_backoff_at_the_cap(b: &mut Bencher) {
    b.iter(|| black_box(wait_spin_backoff(black_box(64))));
}

/// `fence_poll_step` is the same question for the fence poll loop.
#[bench]
fn fence_poll_step_decision(b: &mut Bencher) {
    b.iter(|| black_box(fence_poll_step(black_box(8))));
}
