//! The per-CPU frame lists sit in front of the one global `Mutex<FrameAlloc>`
//! and change two things a caller could have been relying on: an allocation no
//! longer comes back with the lowest free frame, and RAM can sit parked where
//! the bitmap cannot see it. These tests are about the second one -- a frame
//! parked in a list and never given back is RAM lost until reboot, and a
//! contiguous request refused with 32 MiB parked is an ENOMEM with free RAM on
//! the meter.

use super::*;

const PAGE: usize = 1 << PAGE_BITS;

/// The bitmap and the lists are both `static`s; serialise, and start from
/// empty lists.
#[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
fn alone() -> spin::MutexGuard<'static, ()> {
    static GUARD: spin::Mutex<()> = spin::Mutex::new(());
    let guard = GUARD.lock();
    frame_drain_percpu();
    guard
}

// --- the list itself -----------------------------------------------------

#[test]
fn a_list_hands_frames_back_in_reverse_and_stops_at_its_capacity() {
    let mut p = pcp::Pcp::new();
    assert_eq!(p.pop(), None, "an empty list has nothing to hand out");
    for i in 0..pcp::CAPACITY {
        assert!(
            p.push(i + 1),
            "the list refused frame {i} below its capacity"
        );
    }
    assert_eq!(p.len(), pcp::CAPACITY);
    assert!(
        !p.push(0xdead),
        "the list took a frame past its capacity: the array behind it is fixed, \
         so one more would be a write past the end"
    );
    // LIFO, which is the point: the frame just freed is the one still warm in
    // this CPU's cache.
    for i in (0..pcp::CAPACITY).rev() {
        assert_eq!(p.pop(), Some(i + 1));
    }
    assert_eq!(p.len(), 0);
}

/// A frame index wider than the `u32` the list stores is refused rather than
/// truncated. `BitAlloc16M` cannot produce one today, so this is about the
/// day the bitmap grows: a truncated index is a frame handed to two owners.
#[test]
fn an_index_too_wide_for_the_list_is_refused_not_truncated() {
    let mut p = pcp::Pcp::new();
    assert!(!p.push(u32::MAX as usize + 1));
    assert_eq!(p.len(), 0, "a refused push must not be recorded");
    assert!(p.push(u32::MAX as usize), "the widest index still fits");
}

// --- the accounting ------------------------------------------------------

/// `frame_stats` answers what CALLERS hold, so parking must not move it; and
/// `frames_cached` answers what the lists hold, so parking must move that.
/// Two separate numbers, because a reader of a failed allocation needs both.
#[test]
fn parking_a_frame_moves_the_cached_figure_and_not_the_used_one() {
    let _alone = alone();
    let base = 0x70_0000;
    // `from_ref` rather than `&[..]`: a one-element array of a `Range` is what
    // `insert_regions` takes, and clippy's `single_range_in_vec_init` reads it
    // as a mistyped `Vec<usize>` and suggests collecting the range into one.
    let region = base..base + 8 * PAGE;
    insert_regions(core::slice::from_ref(&region));
    assert_eq!(frames_cached(), 0, "the fixture drained the lists");

    let (used_before, _) = frame_stats();
    let addr = frame_alloc(1, 0).expect("a frame went in");
    assert_eq!(
        frame_stats().0 - used_before,
        PAGE,
        "the caller holds exactly the page it asked for"
    );
    // The refill took a batch, so most of what it took is parked.
    let parked_after_alloc = frames_cached();
    assert!(
        parked_after_alloc > 0,
        "the refill parked nothing: the list is not in front of the bitmap"
    );

    frame_dealloc(addr);
    assert_eq!(
        frame_stats().0,
        used_before,
        "the freed page is no longer held by the caller"
    );
    assert_eq!(
        frames_cached(),
        parked_after_alloc + PAGE,
        "the freed page was not parked"
    );

    // And every parked byte comes back.
    let parked = frames_cached();
    let moved = frame_drain_percpu();
    assert_eq!(
        moved * PAGE,
        parked,
        "the drain lost track of parked frames"
    );
    assert_eq!(frames_cached(), 0);
}

/// Draining with nothing parked moves nothing and says so, which is what
/// stops `frame_alloc` from retrying an allocation that cannot change.
#[test]
fn draining_empty_lists_moves_nothing() {
    let _alone = alone();
    assert_eq!(frame_drain_percpu(), 0);
    assert_eq!(frames_cached(), 0);
}

// --- what the drain is for -----------------------------------------------

/// The failure this exists to prevent: an allocation refused while the frames
/// it wanted are parked, one at a time, in a per-CPU list.
///
/// Built by draining the bitmap dry -- the only way to be sure of it, because
/// the bitmap is one `static` that holds every region every test inserted, so
/// "ask for a run bigger than my region" is answered from somebody else's.
/// Then free a handful, which PARKS them rather than returning them, and ask
/// for a contiguous run: the contiguous path never looks at a list, so the
/// only thing that can serve it is the drain-and-retry in `frame_alloc`.
///
/// A mutation that removed that retry survived an earlier version of this test
/// that simply asked for four pages: another test's region had four free.
#[test]
fn an_allocation_is_served_from_what_the_lists_had_parked() {
    let _alone = alone();
    let base = 0x80_0000;
    const N: usize = 16;
    let region = base..base + N * PAGE;
    insert_regions(core::slice::from_ref(&region));

    // Empty the bitmap AND the lists: a single-frame request comes back `None`
    // only when both are dry.
    let mut held = alloc::vec::Vec::new();
    for _ in 0..4096 {
        let Some(addr) = frame_alloc(1, 0) else { break };
        held.push(addr);
    }
    assert!(
        frame_alloc(1, 0).is_none(),
        "4096 frames was not enough to empty the allocator; raise the bound"
    );
    assert!(held.len() >= 8, "only {} frames came out", held.len());

    // Park a few. Freeing goes to this CPU's list, not to the bitmap, so the
    // bitmap stays dry and these frames are invisible to it.
    let parked: alloc::vec::Vec<usize> = held.drain(..8).collect();
    for addr in &parked {
        frame_dealloc(*addr);
    }
    assert_eq!(
        frames_cached(),
        8 * PAGE,
        "the eight freed frames were not parked: this test is testing nothing"
    );

    // Two pages, contiguous. The bitmap is empty; the eight parked frames are
    // the only RAM in the system. Served only by drain-and-retry.
    let run = frame_alloc(2, 0).expect(
        "a contiguous run was refused with frames parked in the per-CPU lists: \
         the drain-and-retry in frame_alloc is not running",
    );
    frame_dealloc(run);
    frame_dealloc(run + PAGE);

    // And give everything back, bitmap included: the next test inherits this
    // allocator.
    for addr in held {
        frame_dealloc(addr);
    }
    frame_drain_percpu();
}
