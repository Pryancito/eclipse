//! Heap allocator for eclipse-relibc
//!
//! Uses chunk-based allocation to avoid mmap-per-malloc: we request large chunks
//! (64KB) via mmap and sub-allocate from them. Small allocations are served from
//! the chunk heap with a free list for reuse. Large allocations (>= 32KB) use
//! direct mmap/munmap.
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use eclipse_syscall::call::{mmap, munmap};
use eclipse_syscall::flag::*;
use crate::types::*;

/// On allocation failure, trigger the alloc_error_handler instead of returning null
/// so we get a clear message and exit instead of crashing with CR2=0.
#[inline(never)]
fn oom(layout: Layout) -> ! {
    alloc::alloc::handle_alloc_error(layout);
}

const ALIGNMENT: usize = 16;
const MIN_BLOCK: usize = 16; // header(8) + free list next(8)
const CHUNK_SIZE: usize = 64 * 1024; // 64KB per mmap chunk
const LARGE_THRESHOLD: usize = 32 * 1024; // >= 32KB: direct mmap/munmap
const PAGE_SIZE: usize = 4096;

// Block header: 8 bytes before user pointer store block size (includes header)
fn block_size_with_header(size: usize) -> usize {
    let s = size.max(1);
    (s + 8 + ALIGNMENT - 1) & !(ALIGNMENT - 1)
}

fn round_up_page(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

pub struct Allocator {
    /// Free list head: pointer to first free block. Each free block stores [size:8][next:8].
    free_list: AtomicPtr<u8>,
}

/// Single allocator for C malloc/free and Rust's global allocator (when allocator feature).
#[cfg(all(not(any(test, feature = "host-testing")), any(not(any(target_os = "linux", unix)), eclipse_target)))]
#[cfg_attr(all(feature = "allocator", not(feature = "no-allocator")), global_allocator)]
static ALLOCATOR: Allocator = Allocator::new();

impl Allocator {
    pub const fn new() -> Self {
        Self {
            free_list: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn lock_free_list(&self) -> *mut u8 {
        self.free_list.swap(ptr::null_mut(), Ordering::Acquire)
    }

    fn unlock_free_list(&self, head: *mut u8) {
        self.free_list.store(head, Ordering::Release);
    }

    /// Allocate from free list. Free block layout: [size: 8][next: 8]
    /// Caller has taken the list via lock_free_list; caller must unlock with the remainder.
    unsafe fn alloc_from_freelist(&self, mut head: *mut u8, need: usize) -> (*mut u8, *mut u8, *mut u8) {
        let mut prev: *mut u8 = ptr::null_mut();
        while !head.is_null() {
            let block = head;
            let size = (block as *const usize).read();
            let next = (block.add(8) as *const *mut u8).read();

            if size >= need {
                let new_head = if prev.is_null() { next } else { head };
                if !prev.is_null() {
                    (prev.add(8) as *mut *mut u8).write(next);
                }
                return (block.add(8), block, new_head);
            }
            prev = head;
            head = next;
        }
        (ptr::null_mut(), ptr::null_mut(), head)
    }
}

// Free list layout: we store size at block_start (8 bytes before user area).
// For free blocks the "user" area starts with next pointer. So:
// Free block: [size: usize][next: *mut u8] - we need 16 bytes
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Defensive: if the global allocator static was not linked/relocated (e.g. address 0),
        // avoid dereferencing self to prevent CR2=0 fault.
        if (self as *const Self).is_null() {
            oom(layout);
        }
        let size = layout.size();
        if size == 0 {
            return ptr::null_mut();
        }

        let need = block_size_with_header(size);

        // Large allocation: direct mmap, store size in first 8 bytes
        if need >= LARGE_THRESHOLD {
            let map_size = round_up_page(need + 8);
            match mmap(0, map_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) {
                Ok(addr) => {
                    let block = addr as *mut u8;
                    (block as *mut usize).write(map_size);
                    return block.add(8);
                }
                Err(_) => oom(layout),
            }
        }

        loop {
            // Always allocate a new chunk instead of reusing the free list.
            // This avoids reading from free-list pointers that might be invalid if the
            // allocator state was ever shared or not properly process-local.
            let map_size = round_up_page(CHUNK_SIZE);
            let chunk = match mmap(0, map_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) {
                Ok(addr) => addr as *mut u8,
                Err(_) => oom(layout),
            };

            let first_size = need;
            let remainder = map_size - first_size;
            if remainder >= MIN_BLOCK {
                // Put remainder in free list for reuse (dealloc will still use it).
                let free_block = chunk.add(first_size);
                (free_block as *mut usize).write(remainder);
                (free_block.add(8) as *mut *mut u8).write(ptr::null_mut());
                let head = self.free_list.swap(ptr::null_mut(), Ordering::Acquire);
                (free_block.add(8) as *mut *mut u8).write(head);
                self.free_list.store(free_block, Ordering::Release);
            }

            (chunk as *mut usize).write(first_size);
            return chunk.add(8);
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let block = ptr.sub(8);
        let size = (block as *const usize).read();

        if size >= LARGE_THRESHOLD {
            // For large blocks, we store the mmap size directly
            let _ = munmap(block as usize, size);
            return;
        }

        (block.add(8) as *mut *mut u8).write(self.free_list.load(Ordering::Relaxed));
        while self.free_list.compare_exchange_weak(
            (block.add(8) as *const *mut u8).read(),
            block,
            Ordering::Release,
            Ordering::Relaxed,
        ).is_err() {
            (block.add(8) as *mut *mut u8).write(self.free_list.load(Ordering::Relaxed));
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(not(any(target_os = "linux", unix)), eclipse_target)))]
mod imp {
    use super::*;
    use crate::types::*;
    #[no_mangle]
    pub unsafe extern "C" fn malloc(size: size_t) -> *mut c_void {
        if size == 0 {
            return ptr::null_mut();
        }
        let layout = Layout::from_size_align_unchecked(size as usize, ALIGNMENT);
        ALLOCATOR.alloc(layout) as *mut c_void
    }

    #[no_mangle]
    pub unsafe extern "C" fn free(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        let layout = Layout::from_size_align_unchecked(0, ALIGNMENT);
        ALLOCATOR.dealloc(ptr as *mut u8, layout);
    }

    #[no_mangle]
    pub unsafe extern "C" fn calloc(nmemb: size_t, size: size_t) -> *mut c_void {
        let total = nmemb.saturating_mul(size);
        if total == 0 {
            return ptr::null_mut();
        }
        let ptr = malloc(total);
        if !ptr.is_null() {
            ptr::write_bytes(ptr as *mut u8, 0, total as usize);
        }
        ptr
    }

    #[no_mangle]
    pub unsafe extern "C" fn realloc(ptr: *mut c_void, new_size: size_t) -> *mut c_void {
        if ptr.is_null() {
            return malloc(new_size);
        }
        if new_size == 0 {
            free(ptr);
            return ptr::null_mut();
        }
        let block = (ptr as *mut u8).sub(8);
        let old_size = (block as *const usize).read();
        let old_user_size = old_size.saturating_sub(8);
        if new_size as usize <= old_user_size {
            return ptr;
        }
        let new_ptr = malloc(new_size);
        if !new_ptr.is_null() {
            ptr::copy_nonoverlapping(ptr as *const u8, new_ptr as *mut u8, old_user_size.min(new_size as usize));
            free(ptr);
        }
        new_ptr
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(not(any(target_os = "linux", unix)), eclipse_target)))]
pub use imp::{malloc, free, calloc, realloc};

#[cfg(any(any(test, feature = "host-testing"), all(any(target_os = "linux", unix), not(eclipse_target))))]
extern "C" {
    pub fn malloc(size: size_t) -> *mut c_void;
    pub fn free(ptr: *mut c_void);
    pub fn calloc(nmemb: size_t, size: size_t) -> *mut c_void;
    pub fn realloc(ptr: *mut c_void, size: size_t) -> *mut c_void;
}
