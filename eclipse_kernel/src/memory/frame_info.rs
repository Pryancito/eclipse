use core::sync::atomic::{AtomicU8, Ordering};

/// RefCount table for all physical frames.
/// Each byte represents one 4KB frame.
/// 0 = Free, 1 = Owned by one process, >1 = Shared (CoW).
static mut FRAME_REFCOUNTS: Option<&'static mut [AtomicU8]> = None;
static mut MAX_FRAMES: usize = 0;

pub unsafe fn init(max_phys: u64) {
    let total_frames = (max_phys / 4096) as usize;
    MAX_FRAMES = total_frames;
    
    let size_bytes = total_frames; // 1 byte per frame
    
    // Allocate space for refcounts from the global allocator or a reserved region.
    // For now, we use alloc_dma_buffer which uses the kernel heap.
    if let Some((ptr, _phys)) = crate::memory::alloc_dma_buffer(size_bytes, 4096) {
        let slice = core::slice::from_raw_parts_mut(ptr as *mut AtomicU8, total_frames);
        for r in slice.iter_mut() {
            *r = AtomicU8::new(0);
        }
        FRAME_REFCOUNTS = Some(slice);
        crate::serial::serial_printf(format_args!("[MEM] Frame refcounts initialized for {} frames ({} KB)\n", total_frames, size_bytes / 1024));
    } else {
        panic!("Failed to allocate frame refcounts table!");
    }
}

pub fn get_refcount(phys: u64) -> u8 {
    let idx = (phys / 4096) as usize;
    unsafe {
        if let Some(ref slice) = FRAME_REFCOUNTS {
            if idx < MAX_FRAMES {
                return slice[idx].load(Ordering::SeqCst);
            }
        }
    }
    0
}

pub fn increment_refcount(phys: u64) {
    let idx = (phys / 4096) as usize;
    unsafe {
        if let Some(ref slice) = FRAME_REFCOUNTS {
            if idx < MAX_FRAMES {
                slice[idx].fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

pub fn decrement_refcount(phys: u64) -> u8 {
    let idx = (phys / 4096) as usize;
    unsafe {
        if let Some(ref slice) = FRAME_REFCOUNTS {
            if idx < MAX_FRAMES {
                return slice[idx].fetch_sub(1, Ordering::SeqCst) - 1;
            }
        }
    }
    0
}

pub fn set_refcount(phys: u64, val: u8) {
    let idx = (phys / 4096) as usize;
    unsafe {
        if let Some(ref slice) = FRAME_REFCOUNTS {
            if idx < MAX_FRAMES {
                slice[idx].store(val, Ordering::SeqCst);
            }
        }
    }
}
