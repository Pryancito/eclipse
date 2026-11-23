#![cfg_attr(feature = "const_fn", feature(const_mut_refs, const_fn_fn_ptr_basics))]
#![no_std]

#[cfg(test)]
#[macro_use]
extern crate std;

#[cfg(feature = "use_spin")]
extern crate spin;

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cmp::{max, min};
use core::fmt;
use core::mem::size_of;
#[cfg(feature = "use_spin")]
use core::ops::Deref;
use core::ptr::NonNull;
#[cfg(feature = "use_spin")]
use spin::Mutex;

mod frame;
mod free_set;
pub mod linked_list;
#[cfg(test)]
mod test;

pub use frame::*;

/// A heap that uses buddy system with configurable order.
///
/// # Usage
///
/// Create a heap and add a memory region to it:
/// ```
/// use buddy_system_allocator::*;
/// # use core::mem::size_of;
/// let mut heap = Heap::<32>::empty();
/// # let space: [usize; 100] = [0; 100];
/// # let begin: usize = space.as_ptr() as usize;
/// # let end: usize = begin + 100 * size_of::<usize>();
/// # let size: usize = 100 * size_of::<usize>();
/// unsafe {
///     heap.init(begin, size);
///     // or, the same range given by its ends (never both: a range
///     // added twice is a double free of every block in it):
///     // heap.add_to_heap(begin, end);
/// }
/// ```
pub struct Heap<const ORDER: usize> {
    // buddy system with max order of `ORDER`: one address-ordered set of free
    // blocks per class (see `free_set`)
    free_list: [free_set::FreeSet; ORDER],

    // statistics
    user: usize,
    allocated: usize,
    total: usize,
}

/// The smallest block: a free block holds its two tree links.
pub const MIN_BLOCK: usize = free_set::NODE_SIZE;

/// The block `layout` takes: a power of two, at least its size, its alignment
/// and [`MIN_BLOCK`].
pub const fn block_size(layout: Layout) -> usize {
    let mut size = layout.size().next_power_of_two();
    if layout.align() > size {
        size = layout.align();
    }
    if MIN_BLOCK > size {
        size = MIN_BLOCK;
    }
    size
}

impl<const ORDER: usize> Heap<ORDER> {
    /// Create an empty heap
    pub const fn new() -> Self {
        Heap {
            free_list: [free_set::FreeSet::new(); ORDER],
            user: 0,
            allocated: 0,
            total: 0,
        }
    }

    /// Create an empty heap
    pub const fn empty() -> Self {
        Self::new()
    }

    /// Add a range of memory [start, end) to the heap
    pub unsafe fn add_to_heap(&mut self, mut start: usize, mut end: usize) {
        // Whole smallest blocks only: each must hold its node.
        start = (start + MIN_BLOCK - 1) & !(MIN_BLOCK - 1);
        end &= !(MIN_BLOCK - 1);
        assert!(start <= end);

        let mut total = 0;
        let mut current_start = start;

        while current_start + MIN_BLOCK <= end {
            let lowbit = current_start & (!current_start + 1);
            let size = min(lowbit, prev_power_of_two(end - current_start));
            total += size;

            self.free_list[size.trailing_zeros() as usize].insert(current_start);
            current_start += size;
        }

        self.total += total;
    }

    /// Add a range of memory [start, end) to the heap
    pub unsafe fn init(&mut self, start: usize, size: usize) {
        self.add_to_heap(start, start + size);
    }

    /// Alloc a range of memory from the heap satifying `layout` requirements
    ///
    /// From the smallest class that has a free block, and of that class the
    /// block with the highest address; split, the block keeps its top half
    /// and frees the bottom one. Live blocks gather at the top of the heap
    /// and free space at the bottom, where it can merge back into large
    /// blocks. In the Eclipse kernel the static arena is the highest memory
    /// the heap has, above every region it grows into, so this also uses the
    /// arena before any growth and leaves a growth whole for as long as it
    /// can, ready to be given back.
    pub fn alloc(&mut self, layout: Layout) -> Result<NonNull<u8>, ()> {
        let size = block_size(layout);
        let class = size.trailing_zeros() as usize;
        for i in class..self.free_list.len() {
            // Find the first non-empty size class
            if let Some(mut block) = self.free_list[i].pop_highest() {
                // Split buffers
                for j in (class + 1..i + 1).rev() {
                    // SAFETY: the bottom half is free memory of the heap,
                    // aligned to its size, which is at least `MIN_BLOCK`.
                    unsafe { self.free_list[j - 1].insert(block) };
                    block += 1 << (j - 1);
                }
                self.user += layout.size();
                self.allocated += size;
                return NonNull::new(block as *mut u8).ok_or(());
            }
        }
        Err(())
    }

    /// Dealloc a range of memory from the heap
    pub fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let size = block_size(layout);
        let class = size.trailing_zeros() as usize;

        // Merge with the buddy while it is free: one search of its class's
        // set per level, not a walk of the class.
        let mut current_ptr = ptr.as_ptr() as usize;
        let mut current_class = class;
        while current_class + 1 < self.free_list.len() {
            let buddy = current_ptr ^ (1 << current_class);
            if !self.free_list[current_class].remove(buddy) {
                break;
            }
            current_ptr = min(current_ptr, buddy);
            current_class += 1;
        }
        // SAFETY: the caller frees a block this heap handed out, so its bytes,
        // and its free buddies' that were merged into it, are the heap's.
        unsafe { self.free_list[current_class].insert(current_ptr) };

        self.user -= layout.size();
        self.allocated -= size;
    }

    /// Take one free block of at least `min_size` bytes that `accept` agrees
    /// to, out of the heap for good, and answer it as `(address, size)`.
    ///
    /// Not in upstream 0.8. Added in Eclipse so that the kernel heap, which
    /// grows by taking chunks of physical RAM, can give a free block back to
    /// the frame allocator once nothing uses it. Without this, `add_to_heap`
    /// only ever goes one way.
    ///
    /// The block leaves the free lists and stops counting in
    /// [`stats_total_bytes`](Self::stats_total_bytes), so the heap never hands
    /// it out again or merges another block into it: a merge only looks for a
    /// buddy in the free lists. The caller now owns the block's memory.
    ///
    /// Searches the largest classes first, so the call gives back as much as
    /// it can, and visits only classes of `min_size` and up. Within a class,
    /// the lowest address first: the end of the heap `alloc` keeps away from.
    pub fn take_free_block(
        &mut self,
        min_size: usize,
        mut accept: impl FnMut(usize, usize) -> bool,
    ) -> Option<(usize, usize)> {
        let min_class = max(min_size, MIN_BLOCK)
            .next_power_of_two()
            .trailing_zeros() as usize;
        for class in (min_class..self.free_list.len()).rev() {
            let size = 1usize << class;
            if let Some(addr) = self.free_list[class].take_lowest_where(|a| accept(a, size)) {
                self.total -= size;
                return Some((addr, size));
            }
        }
        None
    }

    /// Free blocks of each class: entry `i` counts the blocks of `1 << i`
    /// bytes. Allocates nothing and walks nothing.
    pub fn free_blocks(&self) -> [usize; ORDER] {
        let mut out = [0; ORDER];
        for (o, set) in out.iter_mut().zip(self.free_list.iter()) {
            *o = set.len();
        }
        out
    }

    /// Bytes in the largest free block, 0 for none.
    pub fn largest_free(&self) -> usize {
        self.free_list
            .iter()
            .rposition(|s| !s.is_empty())
            .map_or(0, |i| 1 << i)
    }

    /// Return the number of bytes that user requests
    pub fn stats_alloc_user(&self) -> usize {
        self.user
    }

    /// Return the number of bytes that are actually allocated
    pub fn stats_alloc_actual(&self) -> usize {
        self.allocated
    }

    /// Return the total number of bytes in the heap
    pub fn stats_total_bytes(&self) -> usize {
        self.total
    }

    /// Whether the block of `size` bytes at `addr` is free, for the tests.
    #[cfg(test)]
    pub(crate) fn is_free(&self, addr: usize, size: usize) -> bool {
        self.free_list[size.trailing_zeros() as usize].contains(addr)
    }

    /// The deepest set's depth, for the tests.
    #[cfg(test)]
    pub(crate) fn max_depth(&self) -> usize {
        self.free_list.iter().map(|s| s.depth()).max().unwrap_or(0)
    }
}

impl<const ORDER: usize> fmt::Debug for Heap<ORDER> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Heap")
            .field("user", &self.user)
            .field("allocated", &self.allocated)
            .field("total", &self.total)
            .finish()
    }
}

/// A locked version of `Heap`
///
/// # Usage
///
/// Create a locked heap and add a memory region to it:
/// ```
/// use buddy_system_allocator::*;
/// # use core::mem::size_of;
/// let mut heap = LockedHeap::<32>::new();
/// # let space: [usize; 100] = [0; 100];
/// # let begin: usize = space.as_ptr() as usize;
/// # let end: usize = begin + 100 * size_of::<usize>();
/// # let size: usize = 100 * size_of::<usize>();
/// unsafe {
///     heap.lock().init(begin, size);
///     // or, never both:
///     // heap.lock().add_to_heap(begin, end);
/// }
/// ```
#[cfg(feature = "use_spin")]
pub struct LockedHeap<const ORDER: usize>(Mutex<Heap<ORDER>>);

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> LockedHeap<ORDER> {
    /// Creates an empty heap
    pub const fn new() -> Self {
        LockedHeap(Mutex::new(Heap::<ORDER>::new()))
    }

    /// Creates an empty heap
    pub const fn empty() -> Self {
        LockedHeap(Mutex::new(Heap::<ORDER>::new()))
    }
}

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> Deref for LockedHeap<ORDER> {
    type Target = Mutex<Heap<ORDER>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(feature = "use_spin")]
unsafe impl<const ORDER: usize> GlobalAlloc for LockedHeap<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0
            .lock()
            .alloc(layout)
            .ok()
            .map_or(0 as *mut u8, |allocation| allocation.as_ptr())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0.lock().dealloc(NonNull::new_unchecked(ptr), layout)
    }
}

/// A locked version of `Heap` with rescue before oom
///
/// # Usage
///
/// Create a locked heap:
/// ```
/// use buddy_system_allocator::*;
/// let heap = LockedHeapWithRescue::new(|heap: &mut Heap<32>, layout: &core::alloc::Layout| {});
/// ```
///
/// Before oom, the allocator will try to call rescue function and try for one more time.
#[cfg(feature = "use_spin")]
pub struct LockedHeapWithRescue<const ORDER: usize> {
    inner: Mutex<Heap<ORDER>>,
    rescue: fn(&mut Heap<ORDER>, &Layout),
}

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> LockedHeapWithRescue<ORDER> {
    /// Creates an empty heap
    #[cfg(feature = "const_fn")]
    pub const fn new(rescue: fn(&mut Heap<ORDER>, &Layout)) -> Self {
        LockedHeapWithRescue {
            inner: Mutex::new(Heap::<ORDER>::new()),
            rescue,
        }
    }

    /// Creates an empty heap
    #[cfg(not(feature = "const_fn"))]
    pub fn new(rescue: fn(&mut Heap<ORDER>, &Layout)) -> Self {
        LockedHeapWithRescue {
            inner: Mutex::new(Heap::<ORDER>::new()),
            rescue,
        }
    }
}

#[cfg(feature = "use_spin")]
impl<const ORDER: usize> Deref for LockedHeapWithRescue<ORDER> {
    type Target = Mutex<Heap<ORDER>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(feature = "use_spin")]
unsafe impl<const ORDER: usize> GlobalAlloc for LockedHeapWithRescue<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut inner = self.inner.lock();
        match inner.alloc(layout) {
            Ok(allocation) => allocation.as_ptr(),
            Err(_) => {
                (self.rescue)(&mut inner, &layout);
                inner
                    .alloc(layout)
                    .ok()
                    .map_or(0 as *mut u8, |allocation| allocation.as_ptr())
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.inner
            .lock()
            .dealloc(NonNull::new_unchecked(ptr), layout)
    }
}

pub(crate) fn prev_power_of_two(num: usize) -> usize {
    1 << (8 * (size_of::<usize>()) - num.leading_zeros() as usize - 1)
}
