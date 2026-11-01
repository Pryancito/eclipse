use crate::linked_list;
use crate::FrameAllocator;
use crate::Heap;
use crate::LockedHeapWithRescue;
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
use core::mem::size_of;

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
