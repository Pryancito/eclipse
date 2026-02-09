//! Heap allocator for Eclipse OS applications

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

const HEAP_SIZE: usize = 2 * 1024 * 1024;
static mut HEAP_MEMORY: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

pub struct BumpAllocator {
    next: AtomicUsize,
}

impl BumpAllocator {
    const fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
        }
    }

    fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();
        let mut next = self.next.load(Ordering::Relaxed);

        loop {
            let aligned = (next + align - 1) & !(align - 1);
            let new_next = aligned + size;

            if new_next > HEAP_SIZE {
                return null_mut();
            }

            match self.next.compare_exchange_weak(
                next, new_next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => unsafe {
                    return HEAP_MEMORY.as_mut_ptr().add(aligned);
                },
                Err(n) => next = n,
            }
        }
    }
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.alloc(layout)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator::new();

pub fn init_heap() {}
