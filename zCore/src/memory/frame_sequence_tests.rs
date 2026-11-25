//! Every sequence of frame-allocator calls up to a length, not a sample.
//!
//! The allocator is two layers (this CPU's list in front of the bitmap) with
//! a drain-and-retry between them, and the interesting states are the ones
//! where frames sit in the wrong layer for the request: a contiguous run
//! wanted while its frames are parked in the list, the bitmap dry while the
//! list is full, an aligned run whose partner frame is held. A hand-written
//! test reaches the few of those its author thought of. This one starts from
//! an allocator that owns exactly [`POOL`] and nothing else, and runs **every**
//! sequence of [`LEN`] operations out of [`Op`] against it -- 7^6 = 117 649
//! sequences -- checking after each step against a model that is just the set
//! of free frames:
//!
//! * nothing handed out is already held, and every run is contiguous, inside
//!   the pool and aligned as asked;
//! * a request fails only when the model has no run that fits: frames parked
//!   in a list must not make the allocator refuse what it owns;
//! * after the sequence, freeing what is held and draining leaves the
//!   allocator owning exactly the pool again: no frame lost, none invented.

use super::*;

const PAGE: usize = 1 << PAGE_BITS;

/// Far from every other test's region: `insert` is not idempotent and the
/// allocator is one `static` for the whole test binary.
const BASE: usize = 0x400_0000;

/// The frames, as offsets from `BASE` in pages, the allocator owns during the
/// walk. Holes at 3 and 6, so that only some pairs are contiguous, and only
/// one of those (0-1; 4-5 too) starts on an even frame.
const POOL: [usize; 6] = [0, 1, 2, 4, 5, 7];

/// Operations per sequence. Six is enough to hold all of `POOL` at once,
/// by every mix of single frames and pairs, from every order of getting
/// there, and to free some of it again.
const LEN: u32 = 6;

#[derive(Clone, Copy, Debug)]
enum Op {
    One,
    OneAligned,
    Two,
    TwoAligned,
    FreeNewest,
    FreeOldest,
    Drain,
}

const OPS: [Op; 7] = [
    Op::One,
    Op::OneAligned,
    Op::Two,
    Op::TwoAligned,
    Op::FreeNewest,
    Op::FreeOldest,
    Op::Drain,
];

fn idx(page: usize) -> usize {
    phys_addr_to_frame_idx(BASE) + page
}

/// Whether `free` (page offsets) has a run of `count` starting on a multiple
/// of `1 << align_log2` in frame-index terms.
fn model_has_run(free: &[bool; 8], count: usize, align_log2: usize) -> bool {
    (0..=8 - count)
        .any(|p| idx(p).is_multiple_of(1 << align_log2) && free[p..p + count].iter().all(|&f| f))
}

#[test]
fn every_short_sequence_of_allocations_and_frees_keeps_the_frame_count() {
    let _alone = super::frame_tests::alone_with_the_allocator();
    insert_regions(core::slice::from_ref(&(BASE..BASE + 8 * PAGE)));

    // Dry the allocator completely, then hand back only the pool: from here
    // on it owns those six frames and nothing else.
    let mut dried = alloc::vec::Vec::new();
    while let Some(a) = frame_alloc(1, 0) {
        dried.push(a);
        assert!(dried.len() < 1 << 20, "the allocator did not run dry");
    }
    for &p in &POOL {
        let a = BASE + p * PAGE;
        let at = dried
            .iter()
            .position(|&d| d == a)
            .expect("the pool came out of the dry-out");
        frame_dealloc(dried.swap_remove(at));
    }

    let total = 7u32.pow(LEN);
    for n in 0..total {
        let mut free = [false; 8];
        for &p in &POOL {
            free[p] = true;
        }
        // Held runs, oldest first: (first page, count).
        let mut held: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        let mut code = n;
        let mut trail = alloc::vec::Vec::new();
        for _ in 0..LEN {
            let op = OPS[(code % 7) as usize];
            code /= 7;
            trail.push(op);
            let (count, align) = match op {
                Op::One => (1, 0),
                Op::OneAligned => (1, 1),
                Op::Two => (2, 0),
                Op::TwoAligned => (2, 1),
                Op::FreeNewest | Op::FreeOldest => {
                    if held.is_empty() {
                        continue;
                    }
                    let (p, c) = if matches!(op, Op::FreeNewest) {
                        held.pop().unwrap()
                    } else {
                        held.remove(0)
                    };
                    for i in 0..c {
                        frame_dealloc(BASE + (p + i) * PAGE);
                        free[p + i] = true;
                    }
                    continue;
                }
                Op::Drain => {
                    frame_drain_percpu();
                    continue;
                }
            };
            let expect = model_has_run(&free, count, align);
            match frame_alloc(count, align) {
                Some(a) => {
                    assert!(
                        (BASE..BASE + 8 * PAGE).contains(&a),
                        "{trail:?}: {a:#x} is outside the pool"
                    );
                    let p = (a - BASE) / PAGE;
                    assert!(
                        p + count <= 8,
                        "{trail:?}: run at page {p} overruns the pool"
                    );
                    assert!(
                        phys_addr_to_frame_idx(a).is_multiple_of(1 << align),
                        "{trail:?}: {a:#x} is not aligned to 2^{align} frames"
                    );
                    for i in 0..count {
                        assert!(
                            free[p + i],
                            "{trail:?}: page {} handed out while held or not owned",
                            p + i
                        );
                        free[p + i] = false;
                    }
                    held.push((p, count));
                }
                None => assert!(
                    !expect,
                    "{trail:?}: refused count={count} align_log2={align} with {free:?} free"
                ),
            }
            assert!(
                frames_cached() <= POOL.len() * PAGE,
                "{trail:?}: more parked than owned"
            );
        }
        for (p, c) in held.drain(..) {
            for i in 0..c {
                frame_dealloc(BASE + (p + i) * PAGE);
            }
        }
        frame_drain_percpu();
        assert_eq!(
            frames_cached(),
            0,
            "{trail:?}: frames still parked after a drain"
        );
        // Conservation: the allocator owns exactly the pool again.
        let mut back = alloc::vec::Vec::new();
        while let Some(a) = frame_alloc(1, 0) {
            back.push(a);
            assert!(
                back.len() <= POOL.len(),
                "{trail:?}: the allocator owns more than the pool"
            );
        }
        back.sort_unstable();
        let want: alloc::vec::Vec<usize> = POOL.iter().map(|&p| BASE + p * PAGE).collect();
        assert_eq!(
            back, want,
            "{trail:?}: the allocator lost or invented frames"
        );
        for a in back {
            frame_dealloc(a);
        }
        frame_drain_percpu();
    }

    for a in dried {
        frame_dealloc(a);
    }
    frame_drain_percpu();
}
