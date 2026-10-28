//! The front cache's lists used to follow whatever a freed block's first
//! word said. These tests write into freed blocks the way a stale owner
//! does -- a clock value, a small integer, a pointer to a live block, a
//! zero -- and require that the cache names the block and never hands out
//! anything it was not given.

use super::free_lists::{
    budget_per_cpu, cap_of, class_size, tag_of, Broken, LinkFault, SlabCache, CAP, CLASS_BUDGET,
    MIN_CAP, NUM_CLASSES,
};
use super::free_ring::{FreeRing, DEPTH};
use std::alloc::Layout;

/// 64-byte blocks: a tagged class.
const C64: usize = 3;
/// Fewest blocks a class has to keep for the cache to be worth its lookup.
///
/// Its own number rather than `MIN_CAP`, because `cap >= MIN_CAP` is the one
/// assertion that cannot fail: lowering `MIN_CAP` to zero lowers the bar with
/// it, and a mutation that did exactly that survived the version of this test
/// that quoted the constant.
const FLOOR: u32 = 8;

/// And `MIN_CAP` is held to the same floor, so lowering it is a build failure
/// rather than a silently weaker test. A `const` assertion because both sides
/// are constants: an `assert!` in the test body is a line clippy is right to
/// call pointless, and this one is checked whether the test runs or not.
const _: () = assert!(
    MIN_CAP >= FLOOR,
    "MIN_CAP is below the floor the cap test holds every class to"
);

/// 4 KiB blocks: the top class, and the only one whose cap is NOT `CAP`.
/// `C64`'s cap works out to 1024, which *is* `CAP`, so a cap test written on
/// `C64` alone passes just as well with `push` asking the old flat ceiling --
/// a mutation that puts `CAP` back in `push` survives it. This class is where
/// the two numbers differ.
const CTOP: usize = NUM_CLASSES - 1;
/// 8-byte blocks: one word, no room for the free mark.
const C8: usize = 0;

/// Real memory for the lists to write their links and marks into.
struct Arena {
    base: usize,
    layout: Layout,
}

impl Arena {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 4096).unwrap();
        let base = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
        assert_ne!(base, 0);
        Arena { base, layout }
    }

    /// The `n`th block of class `i`.
    fn block(&self, i: usize, n: usize) -> usize {
        assert!((n + 1) * class_size(i) <= self.layout.size());
        self.base + n * class_size(i)
    }

    fn owns(&self) -> impl Fn(usize, usize) -> bool + '_ {
        move |addr, size| {
            addr >= self.base
                && addr + size <= self.base + self.layout.size()
                && addr.is_multiple_of(size)
        }
    }

    fn write(&self, addr: usize, value: usize) {
        assert!(addr >= self.base && addr + 8 <= self.base + self.layout.size());
        unsafe { core::ptr::write(addr as *mut usize, value) }
    }

    fn read(&self, addr: usize) -> usize {
        assert!(addr >= self.base && addr + 8 <= self.base + self.layout.size());
        unsafe { core::ptr::read(addr as *const usize) }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.base as *mut u8, self.layout) }
    }
}

/// The word the ninth capture found where a `BTreeMap` node pointer
/// belonged: an instant, not an address.
const CLOCK: usize = 0x0000_0012_a05f_2000;

#[test]
fn blocks_come_back_last_in_first_out_without_their_mark() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, b, d) = (
        arena.block(C64, 0),
        arena.block(C64, 1),
        arena.block(C64, 2),
    );
    for x in [a, b, d] {
        assert_eq!(unsafe { c.push(C64, x) }, Ok(true));
    }
    assert_eq!(c.count(C64), 3);
    for x in [d, b, a] {
        assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(x)));
        // A live block carrying the mark would read as "already free" to
        // the next `push` of it.
        assert_eq!(arena.read(x + 8), 0);
    }
    assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(None));
    assert_eq!(c.count(C64), 0);
}

/// The ninth capture's shape: a clock value where a free block's link
/// was. The block that holds it is named, and the clock value is never
/// handed out as an allocation.
#[test]
fn a_link_overwritten_with_a_clock_value_is_caught_at_that_block() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, b, d) = (
        arena.block(C64, 0),
        arena.block(C64, 1),
        arena.block(C64, 2),
    );
    for x in [a, b, d] {
        unsafe { c.push(C64, x) }.unwrap();
    }
    arena.write(b, CLOCK);
    assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(d)));
    assert_eq!(
        unsafe { c.pop(C64, arena.owns()) },
        Err(Broken::Link {
            block: b,
            link: CLOCK,
            lost: 1,
            why: LinkFault::Outside,
        })
    );
    // The rest of the list is gone, not followed.
    assert_eq!(c.count(C64), 0);
    assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(None));
}

/// A heap pointer is exactly what a stale owner stores, and it passes
/// every address test. The block it names is live, so it has no mark.
#[test]
fn a_link_overwritten_with_a_live_block_is_caught() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, b, live) = (
        arena.block(C64, 0),
        arena.block(C64, 1),
        arena.block(C64, 5),
    );
    for x in [a, b] {
        unsafe { c.push(C64, x) }.unwrap();
    }
    arena.write(b, live);
    assert_eq!(
        unsafe { c.pop(C64, arena.owns()) },
        Err(Broken::Link {
            block: b,
            link: live,
            lost: 1,
            why: LinkFault::NotFree,
        })
    );
}

/// A write that misses the link lands on the mark.
#[test]
fn a_write_over_the_free_mark_is_caught() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, b) = (arena.block(C64, 0), arena.block(C64, 1));
    for x in [a, b] {
        unsafe { c.push(C64, x) }.unwrap();
    }
    arena.write(b + 8, 0x71);
    assert_eq!(
        unsafe { c.pop(C64, arena.owns()) },
        Err(Broken::Tag {
            block: b,
            found: 0x71,
            lost: 1,
        })
    );
}

/// Zero is a valid link only from the last block. Anywhere else it is the
/// zero-writer this hunt began with, and it would have leaked the rest of
/// the list without a word.
#[test]
fn a_zeroed_link_with_blocks_behind_it_is_caught() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, b) = (arena.block(C64, 0), arena.block(C64, 1));
    for x in [a, b] {
        unsafe { c.push(C64, x) }.unwrap();
    }
    arena.write(b, 0);
    assert_eq!(
        unsafe { c.pop(C64, arena.owns()) },
        Err(Broken::Link {
            block: b,
            link: 0,
            lost: 1,
            why: LinkFault::Zeroed,
        })
    );
}

/// One-word blocks have no mark, so a link to another block of the heap
/// passes the address test -- but not out of the last block.
#[test]
fn a_link_out_of_the_last_block_is_caught() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, other) = (arena.block(C8, 0), arena.block(C8, 9));
    unsafe { c.push(C8, a) }.unwrap();
    arena.write(a, other);
    assert_eq!(
        unsafe { c.pop(C8, arena.owns()) },
        Err(Broken::Link {
            block: a,
            link: other,
            lost: 0,
            why: LinkFault::PastEnd,
        })
    );
}

/// Before: the second free put the block in the list twice, and two
/// allocations later two owners shared it.
#[test]
fn a_block_freed_twice_is_refused_and_the_list_stays_sound() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, b) = (arena.block(C64, 0), arena.block(C64, 1));
    for x in [a, b] {
        unsafe { c.push(C64, x) }.unwrap();
    }
    assert_eq!(
        unsafe { c.push(C64, a) },
        Err(Broken::DoubleFree { block: a })
    );
    assert_eq!(c.count(C64), 2);
    assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(b)));
    assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(a)));
    assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(None));
}

/// Without a mark, the head is the one double free a one-word class can
/// still see -- and the one that would make the list point at itself.
#[test]
fn a_one_word_block_freed_twice_in_a_row_is_refused() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let a = arena.block(C8, 0);
    unsafe { c.push(C8, a) }.unwrap();
    assert_eq!(
        unsafe { c.push(C8, a) },
        Err(Broken::DoubleFree { block: a })
    );
    assert_eq!(c.count(C8), 1);
}

/// The everyday case must stay quiet: a block that was handed out and
/// comes back is a free, not a double free.
#[test]
fn a_block_handed_out_and_freed_again_is_not_a_double_free() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let a = arena.block(C64, 0);
    unsafe { c.push(C64, a) }.unwrap();
    assert_eq!(unsafe { c.pop(C64, arena.owns()) }, Ok(Some(a)));
    assert_eq!(unsafe { c.push(C64, a) }, Ok(true));
}

/// The mark is mixed with the block's address, so a live object holding
/// a copy of another block's mark -- a struct copied out of freed memory
/// -- is not taken for a free block.
#[test]
fn another_blocks_mark_in_a_live_block_is_not_a_double_free() {
    let arena = Arena::new(4096);
    let mut c = SlabCache::new();
    let (a, live) = (arena.block(C64, 0), arena.block(C64, 1));
    unsafe { c.push(C64, a) }.unwrap();
    arena.write(live + 8, tag_of(a));
    assert_eq!(unsafe { c.push(C64, live) }, Ok(true));
}

/// A full class hands the block back without writing into it: the buddy
/// keeps its own links there.
#[test]
fn a_full_class_leaves_the_block_to_the_buddy_untouched() {
    // `cap_of`, not `CAP`: the cap is per class now, and writing the
    // ceiling here would fill 1024 blocks of a class whose cap is 16 and
    // then assert that the 1025th was refused -- true, but for the wrong
    // reason, and it would stop testing the boundary entirely.
    let cap = cap_of(C64) as usize;
    let arena = Arena::new((cap + 1) * class_size(C64));
    let mut c = SlabCache::new();
    for n in 0..cap {
        assert_eq!(unsafe { c.push(C64, arena.block(C64, n)) }, Ok(true));
    }
    let extra = arena.block(C64, cap);
    arena.write(extra, 0x1111);
    arena.write(extra + 8, 0x2222);
    assert_eq!(unsafe { c.push(C64, extra) }, Ok(false));
    assert_eq!(arena.read(extra), 0x1111);
    assert_eq!(arena.read(extra + 8), 0x2222);
    assert_eq!(c.count(C64), cap as u32);
}

/// The top class fills at `cap_of`, which is 16 blocks, not at `CAP`. The test
/// above cannot see the difference (`cap_of(C64) == CAP`), so this is the one
/// that holds `push` to the per-class cap.
#[test]
fn the_top_class_fills_at_its_own_cap_not_at_the_ceiling() {
    let cap = cap_of(CTOP) as usize;
    assert!(
        cap < CAP as usize,
        "CTOP's cap is {cap}, the same as the ceiling: this test proves nothing"
    );
    let arena = Arena::new((cap + 1) * class_size(CTOP));
    let mut c = SlabCache::new();
    for n in 0..cap {
        assert_eq!(unsafe { c.push(CTOP, arena.block(CTOP, n)) }, Ok(true));
    }
    assert_eq!(
        unsafe { c.push(CTOP, arena.block(CTOP, cap)) },
        Ok(false),
        "the top class took block {} past its cap of {cap}",
        cap + 1,
    );
    assert_eq!(c.count(CTOP), cap as u32);
}

/// The cap is a cap on BYTES, which is the whole point of `cap_of`: a cap
/// on blocks reserves ten times as much RAM in the 4 KiB class as in the
/// 400-byte one, and multiplied by `PERCPU_SLOTS` caches that is the heap.
#[test]
fn no_class_may_pin_more_than_its_byte_budget() {
    for i in 0..NUM_CLASSES {
        let bytes = cap_of(i) as usize * class_size(i);
        assert!(
            bytes <= CLASS_BUDGET.max(MIN_CAP as usize * class_size(i)),
            "class {i} ({} B blocks) pins {bytes} B, budget is {CLASS_BUDGET} B",
            class_size(i),
        );
    }
}

/// Every class caches something, and nothing caches more than the ceiling.
/// A cap of zero would turn the class into a plain buddy passthrough while
/// still paying for the lookup, and nothing else in the file would say so.
///
/// The floor is written here as a NUMBER rather than as `MIN_CAP`, because
/// `cap >= MIN_CAP` is the one assertion that cannot fail: lowering `MIN_CAP`
/// to zero lowers the bar with it, and a mutation that did exactly that
/// survived the version of this test that quoted the constant. Eight blocks is
/// what a class has to keep for the cache to be worth its lookup, whatever the
/// constants are set to; `MIN_CAP` is then held to the same floor on its own
/// line, so lowering it is a failure and not a silently weaker test.
#[test]
fn every_class_has_a_cap_between_the_floor_and_the_ceiling() {
    for i in 0..NUM_CLASSES {
        let cap = cap_of(i);
        assert!(cap >= FLOOR, "class {i} caps at {cap}, floor is {FLOOR}");
        assert!(cap <= CAP, "class {i} caps at {cap}, ceiling is {CAP}");
    }
}

/// A bigger block never gets a bigger block count: `cap_of` divides the
/// budget by the class size, so the sequence is non-increasing. Written
/// down because the clamps at both ends could hide an inversion.
#[test]
fn the_cap_never_grows_with_the_class() {
    for i in 1..NUM_CLASSES {
        assert!(
            cap_of(i) <= cap_of(i - 1),
            "class {i} caps at {} but class {} caps at {}",
            cap_of(i),
            i - 1,
            cap_of(i - 1),
        );
    }
}

/// What the whole table may pin, stated as a number so that raising
/// `CLASS_BUDGET` or `MAX_CLASS` has to come past this test.
///
/// The heap is 512 MiB (`KERNEL_HEAP_SIZE`, not reachable from the host
/// build, so the number is repeated here); 64 MiB is an eighth of it, and
/// this is the worst case -- every class of every CPU full at the same
/// moment, which no boot has ever reached.
#[test]
fn the_whole_percpu_table_cannot_pin_an_eighth_of_the_heap() {
    const PERCPU_SLOTS: usize = 64 + 1;
    let per_cpu = budget_per_cpu();
    let whole = per_cpu * PERCPU_SLOTS;
    assert!(
        whole <= 64 * 1024 * 1024,
        "{per_cpu} B per CPU x {PERCPU_SLOTS} CPUs = {whole} B",
    );
    // And it is not accidentally nothing, which is how a cap of zero
    // everywhere would pass the line above.
    assert!(per_cpu >= CLASS_BUDGET, "{per_cpu} B per CPU");
}

/// Whatever is written into free blocks, and wherever, `pop` hands out
/// only blocks that were freed and are still free: never a written value,
/// never a block twice.
#[test]
fn whatever_is_written_into_free_blocks_pop_hands_out_only_free_blocks() {
    const BLOCKS: usize = 64;
    let arena = Arena::new(BLOCKS * class_size(C64));
    let mut c = SlabCache::new();
    let mut free: Vec<usize> = Vec::new();
    let mut live: Vec<usize> = (0..BLOCKS).map(|n| arena.block(C64, n)).collect();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed as usize
    };
    let (mut caught, mut handed) = (0, 0);
    for _ in 0..20_000 {
        match next() % 8 {
            0..=2 if !live.is_empty() => {
                let b = live.swap_remove(next() % live.len());
                if unsafe { c.push(C64, b) } == Ok(true) {
                    free.push(b);
                }
            }
            3..=5 => match unsafe { c.pop(C64, arena.owns()) } {
                Ok(Some(b)) => {
                    let at = free.iter().position(|&f| f == b);
                    assert!(at.is_some(), "handed out {b:#x}, which is not free");
                    free.swap_remove(at.unwrap());
                    live.push(b);
                    handed += 1;
                }
                Ok(None) => assert_eq!(c.count(C64), 0),
                Err(_) => {
                    // Leaked with its list: neither free nor ever handed
                    // out again.
                    caught += 1;
                    assert_eq!(c.count(C64), 0);
                    free.clear();
                }
            },
            6 if !free.is_empty() => {
                // A stale owner writes into a free block.
                let b = free[next() % free.len()];
                let value = match next() % 5 {
                    0 => 0,
                    1 => 0x71,
                    2 => CLOCK + next() % 1000,
                    3 if !live.is_empty() => live[next() % live.len()],
                    _ => free[next() % free.len()],
                };
                arena.write(b + 8 * (next() % 2), value);
            }
            _ => {}
        }
    }
    assert!(caught > 0 && handed > 0, "caught={caught} handed={handed}");
}

#[test]
fn the_ring_names_the_latest_free_of_a_block_and_how_long_ago() {
    let ring: FreeRing<8> = FreeRing::new();
    let rets = |x| [x; DEPTH];
    ring.note(0xa0, rets(1));
    ring.note(0xb0, rets(2));
    ring.note(0xa0, rets(3));
    assert_eq!(ring.find(0xa0), Some((rets(3), 0)));
    assert_eq!(ring.find(0xb0), Some((rets(2), 1)));
    assert_eq!(ring.find(0xc0), None);
    assert_eq!(ring.find(0), None);
}

/// A free the ring has overwritten is "no longer in the ring", never the
/// free that took its slot.
#[test]
fn a_free_older_than_the_ring_is_not_attributed_to_anyone() {
    let ring: FreeRing<4> = FreeRing::new();
    ring.note(0xa0, [1; DEPTH]);
    for n in 1..=4 {
        ring.note(0xa0 + n * 0x10, [2; DEPTH]);
    }
    assert_eq!(ring.find(0xa0), None);
    assert_eq!(ring.find(0xe0), Some(([2; DEPTH], 0)));
    assert_eq!(ring.len(), 4);
}
