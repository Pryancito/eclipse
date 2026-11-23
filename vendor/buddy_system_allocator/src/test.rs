use crate::linked_list;
use crate::FrameAllocator;
use crate::Heap;
use crate::LockedHeapWithRescue;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;

#[test]
fn test_linked_list() {
    let mut value1: usize = 0;
    let mut value2: usize = 0;
    let mut value3: usize = 0;
    let mut list = linked_list::LinkedList::new();
    unsafe {
        list.push(&mut value1 as *mut usize);
        list.push(&mut value2 as *mut usize);
        list.push(&mut value3 as *mut usize);
    }

    // Test links
    assert_eq!(value3, &value2 as *const usize as usize);
    assert_eq!(value2, &value1 as *const usize as usize);
    assert_eq!(value1, 0);

    // Test iter
    let mut iter = list.iter();
    assert_eq!(iter.next(), Some(&mut value3 as *mut usize));
    assert_eq!(iter.next(), Some(&mut value2 as *mut usize));
    assert_eq!(iter.next(), Some(&mut value1 as *mut usize));
    assert_eq!(iter.next(), None);

    // Test iter_mut

    let mut iter_mut = list.iter_mut();
    assert_eq!(iter_mut.next().unwrap().pop(), &mut value3 as *mut usize);

    // Test pop
    assert_eq!(list.pop(), Some(&mut value2 as *mut usize));
    assert_eq!(list.pop(), Some(&mut value1 as *mut usize));
    assert_eq!(list.pop(), None);
}

#[test]
fn test_empty_heap() {
    let mut heap = Heap::<32>::new();
    assert!(heap.alloc(Layout::from_size_align(1, 1).unwrap()).is_err());
}

#[test]
fn test_heap_add() {
    let mut heap = Heap::<32>::new();
    assert!(heap.alloc(Layout::from_size_align(1, 1).unwrap()).is_err());

    let space: [usize; 100] = [0; 100];
    unsafe {
        heap.add_to_heap(space.as_ptr() as usize, space.as_ptr().add(100) as usize);
    }
    let addr = heap.alloc(Layout::from_size_align(1, 1).unwrap());
    assert!(addr.is_ok());
}

#[test]
fn test_heap_oom() {
    let mut heap = Heap::<32>::new();
    let space: [usize; 100] = [0; 100];
    unsafe {
        heap.add_to_heap(space.as_ptr() as usize, space.as_ptr().add(100) as usize);
    }

    assert!(heap
        .alloc(Layout::from_size_align(100 * size_of::<usize>(), 1).unwrap())
        .is_err());
    assert!(heap.alloc(Layout::from_size_align(1, 1).unwrap()).is_ok());
}

#[test]
fn test_heap_oom_rescue() {
    static mut SPACE: [usize; 100] = [0; 100];
    let heap = LockedHeapWithRescue::new(|heap: &mut Heap<32>, _layout: &Layout| unsafe {
        heap.add_to_heap(SPACE.as_ptr() as usize, SPACE.as_ptr().add(100) as usize);
    });

    unsafe {
        assert!(heap.alloc(Layout::from_size_align(1, 1).unwrap()) as usize != 0);
    }
}

#[test]
fn test_heap_alloc_and_free() {
    let mut heap = Heap::<32>::new();
    assert!(heap.alloc(Layout::from_size_align(1, 1).unwrap()).is_err());

    let space: [usize; 100] = [0; 100];
    unsafe {
        heap.add_to_heap(space.as_ptr() as usize, space.as_ptr().add(100) as usize);
    }
    for _ in 0..100 {
        let addr = heap.alloc(Layout::from_size_align(1, 1).unwrap()).unwrap();
        heap.dealloc(addr, Layout::from_size_align(1, 1).unwrap());
    }
}

#[test]
fn test_empty_frame_allocator() {
    let mut frame = FrameAllocator::new();
    assert!(frame.alloc(1).is_none());
}

#[test]
fn test_frame_allocator_add() {
    let mut frame = FrameAllocator::new();
    assert!(frame.alloc(1).is_none());

    frame.insert(0..3);
    let num = frame.alloc(1);
    assert_eq!(num, Some(2));
    let num = frame.alloc(2);
    assert_eq!(num, Some(0));
    assert!(frame.alloc(1).is_none());
    assert!(frame.alloc(2).is_none());
}

#[test]
fn test_frame_allocator_alloc_and_free() {
    let mut frame = FrameAllocator::new();
    assert!(frame.alloc(1).is_none());

    frame.add_frame(0, 1024);
    for _ in 0..100 {
        let addr = frame.alloc(512).unwrap();
        frame.dealloc(addr, 512);
    }
}

#[test]
fn test_frame_allocator_alloc_and_free_complex() {
    let mut frame = FrameAllocator::new();
    frame.add_frame(100, 1024);
    for _ in 0..10 {
        let addr = frame.alloc(1).unwrap();
        frame.dealloc(addr, 1);
    }
    let addr1 = frame.alloc(1).unwrap();
    let addr2 = frame.alloc(1).unwrap();
    assert_ne!(addr1, addr2);
}

/// An aligned arena for the `take_free_block` tests: 64 KiB, aligned to it,
/// so the whole of it merges back into one block.
#[repr(align(65536))]
struct Arena([u8; 65536]);

#[test]
fn a_taken_block_leaves_the_heap_for_good() {
    let mut arena = std::boxed::Box::new(Arena([0; 65536]));
    let begin = arena.0.as_mut_ptr() as usize;
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(begin, begin + 65536) };
    assert_eq!(
        heap.take_free_block(4096, |a, s| a == begin && s == 65536),
        Some((begin, 65536))
    );
    assert_eq!(heap.stats_total_bytes(), 0);
    assert!(heap.alloc(Layout::from_size_align(8, 8).unwrap()).is_err());
}

#[test]
fn a_block_in_use_is_never_taken_and_its_free_neighbours_are() {
    let mut arena = std::boxed::Box::new(Arena([0; 65536]));
    let begin = arena.0.as_mut_ptr() as usize;
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(begin, begin + 65536) };
    let layout = Layout::from_size_align(4096, 4096).unwrap();
    let used = heap.alloc(layout).unwrap();
    // 64 KiB split down to one 4 KiB block in use: its free buddies are one
    // block each of 4, 8, 16 and 32 KiB, and nothing of 64 KiB is left.
    assert!(heap.take_free_block(65536, |_, _| true).is_none());
    let mut taken = 0;
    while let Some((a, s)) = heap.take_free_block(4096, |_, _| true) {
        let u = used.as_ptr() as usize;
        assert!(a + s <= u || a >= u + 4096, "took the block in use");
        taken += s;
    }
    assert_eq!(taken, 65536 - 4096);
    assert_eq!(heap.stats_total_bytes(), 4096);
    // The block in use still frees cleanly: its buddy is gone, so it merges
    // with nothing and sits alone in its class.
    heap.dealloc(used, layout);
    assert_eq!(
        heap.take_free_block(4096, |_, _| true),
        Some((used.as_ptr() as usize, 4096))
    );
}

#[test]
fn take_free_block_asks_before_it_takes() {
    let mut arena = std::boxed::Box::new(Arena([0; 65536]));
    let begin = arena.0.as_mut_ptr() as usize;
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(begin, begin + 65536) };
    assert!(heap.take_free_block(4096, |_, _| false).is_none());
    assert_eq!(heap.stats_total_bytes(), 65536);
    assert!(heap
        .alloc(Layout::from_size_align(65536, 8).unwrap())
        .is_ok());
}

/// A 1 MiB arena aligned to itself, leaked, for the tree tests.
fn arena(size: usize) -> usize {
    let layout = Layout::from_size_align(size, size).unwrap();
    unsafe { std::alloc::alloc(layout) as usize }
}

/// The bug the address-ordered sets fix: freeing a block whose buddy is in
/// use walked every free block of its class. Here 16 384 free 32-byte
/// blocks whose buddies are live; each free must stay cheap and right.
#[test]
fn a_free_finds_its_buddy_without_walking_the_class() {
    const SIZE: usize = 1 << 20;
    let base = arena(SIZE);
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(base, base + SIZE) };
    let l32 = Layout::from_size_align(32, 8).unwrap();
    let blocks: std::vec::Vec<_> = (0..SIZE / 32).map(|_| heap.alloc(l32).unwrap()).collect();
    assert!(heap.alloc(l32).is_err(), "the arena is full");
    // Free every other block: none of them can merge.
    for b in blocks.iter().step_by(2) {
        heap.dealloc(*b, l32);
    }
    assert_eq!(heap.free_blocks()[5], SIZE / 64);
    assert!(heap.max_depth() < 64, "depth {}", heap.max_depth());
    // Now the rest: every free merges, all the way up.
    for b in blocks.iter().skip(1).step_by(2) {
        heap.dealloc(*b, l32);
    }
    assert_eq!(heap.largest_free(), SIZE);
    assert_eq!(heap.free_blocks().iter().sum::<usize>(), 1);
    assert_eq!(heap.stats_alloc_actual(), 0);
}

/// Allocations come from the highest free block, and a split keeps the top
/// half: the bottom of the heap stays free in large blocks.
#[test]
fn allocations_pile_up_at_the_top() {
    const SIZE: usize = 1 << 16;
    let base = arena(SIZE);
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(base, base + SIZE) };
    let l = Layout::from_size_align(64, 8).unwrap();
    let a = heap.alloc(l).unwrap().as_ptr() as usize;
    assert_eq!(a, base + SIZE - 64, "the topmost 64 bytes");
    let b = heap.alloc(l).unwrap().as_ptr() as usize;
    assert_eq!(b, base + SIZE - 128);
    assert!(
        heap.is_free(base, SIZE / 2),
        "the bottom half is one free block"
    );
}

/// Two regions: the higher one (the arena, in the kernel) fills first, and
/// the lower one (a growth) is given back whole.
#[test]
fn the_higher_region_is_used_first_and_the_lower_given_back_whole() {
    const SIZE: usize = 1 << 16;
    let low = arena(SIZE);
    let high = arena(SIZE);
    let (low, high) = (low.min(high), low.max(high));
    let mut heap = Heap::<32>::new();
    unsafe {
        heap.add_to_heap(high, high + SIZE);
        heap.add_to_heap(low, low + SIZE);
    }
    let l = Layout::from_size_align(256, 8).unwrap();
    for _ in 0..SIZE / 256 / 2 {
        let p = heap.alloc(l).unwrap().as_ptr() as usize;
        assert!(p >= high, "{p:#x} came from the lower region");
    }
    assert_eq!(
        heap.take_free_block(SIZE, |a, _| a < high),
        Some((low, SIZE)),
        "the lower region is one free block"
    );
}

/// The release looks at the lowest addresses first.
#[test]
fn take_free_block_gives_the_lowest_accepted_block() {
    const SIZE: usize = 1 << 16;
    let base = arena(SIZE);
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(base, base + SIZE) };
    let l = Layout::from_size_align(4096, 4096).unwrap();
    let all: std::vec::Vec<_> = (0..16).map(|_| heap.alloc(l).unwrap()).collect();
    // Free blocks 3, 5 and 9: no two are buddies.
    for i in [9, 3, 5] {
        heap.dealloc(all[i], l);
    }
    let mut freed: std::vec::Vec<usize> = [3, 5, 9]
        .iter()
        .map(|&i| all[i].as_ptr() as usize)
        .collect();
    freed.sort();
    assert_eq!(
        heap.take_free_block(4096, |_, _| true),
        Some((freed[0], 4096))
    );
    assert_eq!(
        heap.take_free_block(4096, |a, _| a != freed[1]),
        Some((freed[2], 4096)),
        "a refused block is skipped, not taken"
    );
    assert!(heap.is_free(freed[1], 4096));
}

/// Random churn against a model: every block handed out is inside the
/// arena, aligned, and overlaps no other live block; at the end everything
/// merges back into one block.
#[test]
fn random_churn_keeps_blocks_disjoint_and_merges_back() {
    const SIZE: usize = 1 << 22;
    let base = arena(SIZE);
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(base, base + SIZE) };
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut live: std::collections::BTreeMap<usize, (usize, Layout)> = Default::default();
    for _ in 0..200_000 {
        if live.len() < 4000 && rnd() % 3 != 0 {
            let size = 1 + (rnd() as usize % (1 << (4 + rnd() % 12)));
            let l = Layout::from_size_align(size, 8).unwrap();
            if let Ok(p) = heap.alloc(l) {
                let a = p.as_ptr() as usize;
                let bs = crate::block_size(l);
                assert!(a >= base && a + bs <= base + SIZE);
                assert_eq!(a % bs, 0);
                if let Some((&pa, &(ps, _))) = live.range(..a + bs).next_back() {
                    assert!(pa + ps <= a, "overlap");
                }
                live.insert(a, (bs, l));
            }
        } else if let Some(&k) = live.keys().nth(rnd() as usize % live.len().max(1)) {
            let (_, l) = live.remove(&k).unwrap();
            heap.dealloc(NonNull::new(k as *mut u8).unwrap(), l);
        }
    }
    for (k, (_, l)) in std::mem::take(&mut live) {
        heap.dealloc(NonNull::new(k as *mut u8).unwrap(), l);
    }
    assert_eq!(heap.largest_free(), SIZE);
    assert!(heap.max_depth() < 64);
}

/// A block freed twice is caught, not linked twice.
#[test]
#[should_panic(expected = "freed twice")]
fn a_double_free_is_caught() {
    const SIZE: usize = 1 << 12;
    let base = arena(SIZE);
    let mut heap = Heap::<32>::new();
    unsafe { heap.add_to_heap(base, base + SIZE) };
    let l = Layout::from_size_align(64, 8).unwrap();
    let a = heap.alloc(l).unwrap();
    let _b = heap.alloc(l).unwrap();
    heap.dealloc(a, l);
    heap.dealloc(a, l);
}
