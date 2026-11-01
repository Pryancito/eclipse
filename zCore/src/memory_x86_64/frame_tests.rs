use super::*;

const PAGE: usize = 1 << PAGE_BITS;

/// The whole allocator lives in one `static`, and `insert` is not
/// idempotent, so two tests inserting overlapping ranges see each other's
/// frames. Serialise them.
///
/// And drain the per-CPU lists, because they are a second `static` that
/// outlives a test: a frame freed by the test before is parked in one, and
/// the next `frame_alloc` pops THAT frame -- from a region this test never
/// inserted. `the_frames_a_region_contributes_are_the_ones_it_hands_out`
/// failed on exactly that, which is the per-CPU lists working as intended
/// and the fixture not knowing about them.
#[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
fn alone_with_the_allocator() -> spin::MutexGuard<'static, ()> {
    static GUARD: spin::Mutex<()> = spin::Mutex::new(());
    let guard = GUARD.lock();
    frame_drain_percpu();
    guard
}

// --- the two conversions ---------------------------------------------

#[test]
fn a_frame_index_and_its_address_are_the_same_thing() {
    for idx in [0usize, 1, 0xff, 0x1_0000, (1 << 24) - 1] {
        assert_eq!(phys_addr_to_frame_idx(frame_idx_to_phys_addr(idx)), idx);
    }
    // Any address inside a frame names that frame.
    assert_eq!(phys_addr_to_frame_idx(0x1000), 1);
    assert_eq!(phys_addr_to_frame_idx(0x1fff), 1);
    assert_eq!(phys_addr_to_frame_idx(0x2000), 2);
}

/// The bitmap and the address cap have to agree: a `remove`/`insert` past
/// the bitmap's last bit is what `MAX_MANAGED_PADDR_EXCLUSIVE` exists to
/// prevent, and the two are written as unrelated literals.
#[test]
fn the_address_cap_is_exactly_what_the_bitmap_can_hold() {
    assert_eq!(
        phys_addr_to_frame_idx(MAX_MANAGED_PADDR_EXCLUSIVE),
        FrameAlloc::CAP,
        "MAX_MANAGED_PADDR_EXCLUSIVE and the BitAlloc type have drifted apart"
    );
}

// --- frames_inside ----------------------------------------------------

#[test]
fn an_aligned_region_is_taken_whole() {
    assert_eq!(frames_inside(&(0x10_0000..0x20_0000)), 0x100..0x200);
}

/// The bug. Both ends used to round OUTWARD -- the start implicitly (a bare
/// `addr >> PAGE_BITS`) and the end explicitly (`idx(end - 1) + 1`) -- so an
/// unaligned region handed out the page straddling each of its edges. That
/// page belongs to whatever is on the other side: the tail of the kernel
/// image, or the head of the next reserved range, now with two owners.
#[test]
fn an_unaligned_region_gives_up_the_partial_page_at_each_end() {
    // 0x1800..0x4800 covers all of frame 2 and part of frames 1 and 4.
    assert_eq!(frames_inside(&(0x1800..0x4800)), 2..4);
}

#[test]
fn a_region_too_small_to_hold_one_whole_page_yields_nothing() {
    assert!(frames_inside(&(0x1800..0x1900)).is_empty());
    assert!(frames_inside(&(0x1001..0x2000)).is_empty());
    assert!(frames_inside(&(0x1000..0x1fff)).is_empty());
    // ...and exactly one page does yield it.
    assert_eq!(frames_inside(&(0x1000..0x2000)), 1..2);
}

#[test]
fn an_empty_or_backwards_region_yields_nothing() {
    assert!(frames_inside(&(0x2000..0x2000)).is_empty());
    assert!(frames_inside(&(0x4000..0x2000)).is_empty());
}

/// Frame 0 is never handed out: a physical address of 0 is how every
/// "allocation failed" is spelled, so a real frame at 0 would be
/// indistinguishable from a failure.
#[test]
fn frame_zero_is_never_handed_out() {
    assert_eq!(frames_inside(&(0..0x4000)), 1..4);
    assert!(frames_inside(&(0..0x1000)).is_empty());
}

#[test]
fn a_region_past_the_bitmap_is_clipped_to_it() {
    let cap = MAX_MANAGED_PADDR_EXCLUSIVE;
    assert_eq!(
        frames_inside(&(cap - 2 * PAGE..cap + 0x1_0000)),
        FrameAlloc::CAP - 2..FrameAlloc::CAP,
    );
    assert!(
        frames_inside(&(cap..cap + 0x1_0000)).is_empty(),
        "a region entirely above the cap is not ours to manage"
    );
}

// --- insert_regions and the allocator --------------------------------

/// What `insert_regions` promises: every frame it reports as managed can be
/// allocated, and none outside the region can.
#[test]
fn the_frames_a_region_contributes_are_the_ones_it_hands_out() {
    let _alone = alone_with_the_allocator();
    let base = 0x40_0000; // frame 0x400, well away from the other tests
    let region = base..base + 3 * PAGE;
    insert_regions(&[region.clone()]);
    // Not "the next three allocations are this region's three frames".
    // The bitmap is one `static` that keeps every region every test ever
    // inserted, and the per-CPU lists in front of it refill a batch at a
    // time, so an allocation can legitimately come back with a frame from
    // somebody else's region -- and in LIFO order, not lowest-first. The
    // property this test is about survives all of that: the region
    // contributed exactly its three frames, each is handed out, and none
    // is handed out twice.
    let mut got = alloc::vec::Vec::new();
    let mut mine = alloc::vec::Vec::new();
    // Bounded: the whole free pool of the other tests' regions is a few
    // dozen frames, so three hundred is slack and still terminates if the
    // region's frames are never handed out, which is the failure.
    for _ in 0..300 {
        let Some(addr) = frame_alloc(1, 0) else { break };
        got.push(addr);
        if region.contains(&addr) {
            mine.push(addr);
            if mine.len() == 3 {
                break;
            }
        }
    }
    let handed_out = got.len();
    for addr in &got {
        frame_dealloc(*addr);
    }
    got.sort_unstable();
    got.dedup();
    assert_eq!(got.len(), handed_out, "the same frame was handed out twice");
    mine.sort_unstable();
    assert_eq!(
        mine,
        alloc::vec![base, base + PAGE, base + 2 * PAGE],
        "the three frames of the region were not the three it handed out"
    );
}

/// The accounting `/proc/meminfo` reads: bytes in, bytes out, and back to
/// where it started.
#[test]
fn allocating_and_freeing_leaves_the_counters_where_they_were() {
    let _alone = alone_with_the_allocator();
    let base = 0x50_0000;
    let (used_before, total_before) = frame_stats();
    insert_regions(&[base..base + 4 * PAGE]);
    assert_eq!(
        frame_stats().1 - total_before,
        4 * PAGE,
        "four pages of managed memory"
    );
    let addr = frame_alloc(2, 0).expect("two contiguous frames");
    assert_eq!(frame_stats().0 - used_before, 2 * PAGE);
    frame_dealloc(addr);
    frame_dealloc(addr + PAGE);
    assert_eq!(frame_stats().0, used_before);
}

#[test]
fn a_contiguous_request_comes_back_contiguous_and_aligned() {
    let _alone = alone_with_the_allocator();
    let base = 0x60_0000;
    insert_regions(&[base..base + 16 * PAGE]);
    let addr = frame_alloc(4, 2).expect("four frames aligned to four");
    assert_eq!(addr % (4 * PAGE), 0, "{addr:#x} is not 4-page aligned");
    assert!((base..base + 16 * PAGE).contains(&addr));
    for i in 0..4 {
        frame_dealloc(addr + i * PAGE);
    }
}

/// A heap chunk taken from RAM is heap: counted once, and its unused part
/// free. Counted as frames AND as heap, a 64 MiB chunk that is empty again
/// read as 64 MiB used after the program that grew it had exited.
#[test]
fn a_grown_heap_chunk_is_counted_once_and_its_free_part_reads_free() {
    const MIB: usize = 1 << 20;
    // 512 MiB arena + one 64 MiB chunk, 10 MiB of heap in use; 4 GiB of
    // RAM of which 100 MiB are frames, the chunk among them.
    let (used, total) = combined_stats(10 * MIB, 576 * MIB, 64 * MIB, 100 * MIB, 4096 * MIB);
    assert_eq!(used, 10 * MIB + 36 * MIB, "the chunk was counted twice");
    assert_eq!(total, 576 * MIB + 4032 * MIB, "and so was the total");
    // Without growth it is the plain sum it always was.
    assert_eq!(
        combined_stats(10 * MIB, 512 * MIB, 0, 100 * MIB, 4096 * MIB),
        (110 * MIB, 4608 * MIB)
    );
}

/// The managed total is the bytes that went INTO the allocator, not the
/// span of the descriptor they came from: reporting more than was inserted
/// is how `/proc/meminfo` ends up with free RAM nobody can allocate.
#[test]
fn only_the_whole_pages_of_a_region_are_counted_as_managed() {
    let _alone = alone_with_the_allocator();
    let total_before = frame_stats().1;
    // Four pages of span, three whole pages inside it.
    insert_regions(&[0x80_0800..0x80_4800]);
    assert_eq!(frame_stats().1 - total_before, 3 * PAGE);
}

/// Inserting nothing is not an error, and it must not move the counters --
/// this is the path an all-reserved memory map takes.
#[test]
fn a_region_that_contributes_no_whole_page_is_not_counted() {
    let _alone = alone_with_the_allocator();
    let before = frame_stats();
    insert_regions(&[0x7000_0001..0x7000_0fff, 0x8000..0x8000]);
    assert_eq!(frame_stats(), before);
}

/// libos has no paging context, so this is the branch that must do nothing
/// rather than walk a page table that is not there.
#[test]
fn reserving_the_boot_page_tables_is_a_no_op_without_paging() {
    let _alone = alone_with_the_allocator();
    assert_eq!(
        kernel_hal::vm::current_vmtoken(),
        0,
        "libos reports no page-table root; this test would walk a null tree"
    );
    reserve_active_page_table_frames();
}
