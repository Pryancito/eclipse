//! A wild `dealloc` is not a failed free: the buddy's lists are intrusive,
//! so it is a wild WRITE into the allocator's own bookkeeping, and what
//! surfaces afterwards is unrelated code faulting on pointers that make no
//! sense. These tests are about never being wrong in the direction that
//! would make the allocator refuse a sound block.

use super::heap_regions::{self, BlockFault};

/// The registry is one `static`; serialise.
#[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
fn alone() -> spin::MutexGuard<'static, ()> {
    static GUARD: spin::Mutex<()> = spin::Mutex::new(());
    GUARD.lock()
}

const ARENA: usize = 0xffff_ff00_1000_0000;
const ARENA_LEN: usize = 512 * 1024 * 1024;

fn with_arena() {
    heap_regions::reset();
    heap_regions::register(ARENA, ARENA + ARENA_LEN);
}

/// The whole point: a pointer the heap never handed out is named, instead
/// of being spliced into a free list.
#[test]
fn a_pointer_from_outside_the_heap_is_refused() {
    let _alone = alone();
    with_arena();
    assert_eq!(
        heap_regions::check(0xffff_8000_0000_0000, 64, 8),
        Some(BlockFault::Outside)
    );
}

/// ... and every ordinary block inside it is not. A check that cried wolf
/// would leak the whole heap one block at a time.
#[test]
fn every_block_inside_the_arena_passes() {
    let _alone = alone();
    with_arena();
    for off in [0usize, 8, 4096, ARENA_LEN / 2, ARENA_LEN - 64] {
        assert_eq!(
            heap_regions::check(ARENA + off, 64, 8),
            None,
            "off={off:#x}"
        );
    }
}

/// A block that starts inside and ends past the end is outside: it is the
/// shape a size that no longer matches its allocation takes.
#[test]
fn a_block_running_past_the_end_of_its_region_is_refused() {
    let _alone = alone();
    with_arena();
    assert_eq!(
        heap_regions::check(ARENA + ARENA_LEN - 32, 64, 8),
        Some(BlockFault::Outside)
    );
}

/// A grown region is as much the heap as the static arena.
#[test]
fn a_block_in_a_grown_region_passes() {
    let _alone = alone();
    with_arena();
    const GROWN: usize = 0xffff_8002_0000_0000;
    assert_eq!(
        heap_regions::check(GROWN + 128, 64, 8),
        Some(BlockFault::Outside),
        "not registered yet"
    );
    heap_regions::register(GROWN, GROWN + 32 * 1024 * 1024);
    assert_eq!(heap_regions::check(GROWN + 128, 64, 8), None);
}

/// The hint is a fast path, never an answer. Blocks arriving from two
/// regions in turn must all pass: a hint that shortcut the walk without
/// re-checking would refuse every other free.
#[test]
fn alternating_between_two_regions_passes_every_time() {
    let _alone = alone();
    heap_regions::reset();
    const A: usize = 0x1000_0000;
    const B: usize = 0x9000_0000;
    heap_regions::register(A, A + 0x10_0000);
    heap_regions::register(B, B + 0x10_0000);
    for i in 0..8usize {
        assert_eq!(heap_regions::check(A + i * 64, 64, 8), None, "A round {i}");
        assert_eq!(heap_regions::check(B + i * 64, 64, 8), None, "B round {i}");
    }
    assert_eq!(
        heap_regions::check(A + 0x20_0000, 64, 8),
        Some(heap_regions::BlockFault::Outside),
        "the gap between the two regions is not the heap"
    );
}

/// A block never straddles two regions, because they are separate
/// allocations -- so one that appears to is a corrupt size, not a merge.
#[test]
fn a_block_straddling_two_regions_is_refused() {
    let _alone = alone();
    heap_regions::reset();
    heap_regions::register(0x1_0000, 0x2_0000);
    heap_regions::register(0x2_0000, 0x3_0000);
    assert_eq!(
        heap_regions::check(0x1_fff0, 0x40, 8),
        Some(BlockFault::Outside)
    );
}

/// A refusal that says only "outside every region" reads the same for a
/// pointer the heap never owned and for a sound block whose `Layout` has
/// stopped matching it. These two are what the report uses to tell them
/// apart, and they are the whole reason a second photograph was needed.
#[test]
fn a_block_that_runs_past_its_region_is_still_inside_it_by_its_pointer() {
    let _alone = alone();
    with_arena();
    // The block starts in the arena; only `ptr + size` leaves it.
    assert_eq!(
        heap_regions::check(ARENA + ARENA_LEN - 0x1000, 0x4000, 1),
        Some(BlockFault::Outside),
        "the free itself is still refused"
    );
    assert_eq!(
        heap_regions::region_of(ARENA + ARENA_LEN - 0x1000),
        Some((0, ARENA, ARENA + ARENA_LEN)),
        "but the pointer is the heap's, which is a different bug"
    );
}

#[test]
fn a_pointer_nowhere_near_the_heap_is_in_no_region() {
    let _alone = alone();
    with_arena();
    assert_eq!(heap_regions::region_of(0xffff_8001_3dd4_0000), None);
    assert_eq!(heap_regions::region_of(ARENA - 1), None);
    assert_eq!(
        heap_regions::region_of(ARENA + ARENA_LEN),
        None,
        "one past the end is past the end"
    );
}

/// The yardstick: how far the nearest region is on each side. A refusal
/// whose neighbours are a page away is a registry that lost a growth; one
/// whose neighbours are gigabytes away is memory the heap never owned.
#[test]
fn the_neighbours_of_a_refused_pointer_are_the_nearest_region_each_side() {
    let _alone = alone();
    heap_regions::reset();
    heap_regions::register(0x2000_0000, 0x3000_0000);
    heap_regions::register(0x1000_0000, 0x1100_0000);
    heap_regions::register(0x5000_0000, 0x5100_0000);
    let (below, above) = heap_regions::neighbours(0x1800_0000);
    assert_eq!(below, Some((0x1000_0000, 0x1100_0000)));
    assert_eq!(above, Some((0x2000_0000, 0x3000_0000)));
}

#[test]
fn a_pointer_below_every_region_has_no_neighbour_below_it() {
    let _alone = alone();
    with_arena();
    let (below, above) = heap_regions::neighbours(ARENA - 0x1000);
    assert_eq!(below, None);
    assert_eq!(above, Some((ARENA, ARENA + ARENA_LEN)));
    let (below, above) = heap_regions::neighbours(ARENA + ARENA_LEN + 0x1000);
    assert_eq!(below, Some((ARENA, ARENA + ARENA_LEN)));
    assert_eq!(above, None);
}

/// `/proc/kheap` prints this, and a reader counts regions in it. An empty
/// slot must look empty rather than like a region at address zero.
#[test]
fn the_snapshot_carries_the_regions_in_the_order_they_were_taken() {
    let _alone = alone();
    heap_regions::reset();
    heap_regions::register(ARENA, ARENA + ARENA_LEN);
    heap_regions::register(0x4000_0000, 0x4200_0000);
    let snap = heap_regions::snapshot();
    assert_eq!(snap[0], (ARENA, ARENA + ARENA_LEN));
    assert_eq!(snap[1], (0x4000_0000, 0x4200_0000));
    assert!(
        snap[2..].iter().all(|r| *r == (0, 0)),
        "unused slots must be empty, not a region at 0"
    );
}

/// Null, misalignment and a wrapping end need no map of the heap, so they
/// are answered even before `init` has registered anything.
#[test]
fn the_cheap_faults_are_answered_with_no_regions_on_record() {
    let _alone = alone();
    heap_regions::reset();
    assert_eq!(heap_regions::check(0, 64, 8), Some(BlockFault::Null));
    assert_eq!(
        heap_regions::check(0x1004, 64, 8),
        Some(BlockFault::Unaligned)
    );
    assert_eq!(
        heap_regions::check(usize::MAX - 8, 64, 1),
        Some(BlockFault::Wraps)
    );
}

/// With nothing registered there is no basis for `Outside`, and guessing
/// would make the allocator refuse every block the kernel frees before
/// `memory::init` runs.
#[test]
fn before_init_nothing_is_called_outside_the_heap() {
    let _alone = alone();
    heap_regions::reset();
    assert_eq!(heap_regions::check(0xdead_0000, 64, 8), None);
}

/// Once a region has not fitted, the registry no longer knows the whole
/// heap -- so it stops claiming a block is outside it. Getting this wrong
/// turns a machine that merely grew its heap a lot into one that leaks
/// every free.
#[test]
fn a_registry_that_overflowed_stops_claiming_blocks_are_outside() {
    let _alone = alone();
    heap_regions::reset();
    for i in 0..heap_regions::MAX_REGIONS + 1 {
        let base = 0x10_0000 + i * 0x10_0000;
        heap_regions::register(base, base + 0x1000);
    }
    assert_eq!(heap_regions::unregistered(), 1);
    assert_eq!(heap_regions::registered(), heap_regions::MAX_REGIONS);
    assert_eq!(heap_regions::check(0xffff_8000_0000_0000, 64, 8), None);
    // ... and the cheap ones still hold.
    assert_eq!(heap_regions::check(0, 64, 8), Some(BlockFault::Null));
}

/// An empty or inverted region is not recorded: it would otherwise take a
/// slot and push a real region into the overflow count.
/// The capacity is not a round number picked by feel: a 512 MiB arena
/// growing to the 2 GiB ceiling in 2 MiB fallback steps is 768 growths,
/// and if the registry cannot hold them it silently stops answering
/// `Outside` on the machine that is short of memory. The `const`
/// assertion beside the growth policy enforces this for the real
/// constants; this pins the arithmetic so the number cannot be lowered
/// here without a test saying why.
#[test]
fn the_registry_holds_every_region_the_growth_policy_can_create() {
    let worst_case = 1 + (2 * 1024 * 1024 * 1024 - 512 * 1024 * 1024) / (2 * 1024 * 1024);
    assert_eq!(worst_case, 769);
    assert!(
        heap_regions::MAX_REGIONS >= worst_case,
        "{} slots cannot hold {} regions",
        heap_regions::MAX_REGIONS,
        worst_case
    );
}

/// And filling it to the brim keeps the verdicts sound: the last region
/// registered is still recognised, and nothing has been declared wild.
#[test]
fn a_full_registry_still_answers_for_its_last_region() {
    let _alone = alone();
    heap_regions::reset();
    for i in 0..heap_regions::MAX_REGIONS {
        let base = ARENA + i * 0x10_0000;
        heap_regions::register(base, base + 0x1000);
    }
    assert_eq!(heap_regions::registered(), heap_regions::MAX_REGIONS);
    assert_eq!(heap_regions::unregistered(), 0);
    let last = ARENA + (heap_regions::MAX_REGIONS - 1) * 0x10_0000;
    assert_eq!(heap_regions::check(last, 64, 8), None);
    assert_eq!(
        heap_regions::check(last + 0x8000, 64, 8),
        Some(BlockFault::Outside)
    );
}

#[test]
fn an_empty_region_is_not_recorded() {
    let _alone = alone();
    heap_regions::reset();
    heap_regions::register(0x1000, 0x1000);
    heap_regions::register(0x2000, 0x1000);
    assert_eq!(heap_regions::registered(), 0);
}
