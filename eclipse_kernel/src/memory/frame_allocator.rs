use x86_64::PhysAddr;
use x86_64::structures::paging::PhysFrame;
use crate::boot::BootInfo;
use crate::serial;
use core::sync::atomic::{AtomicUsize, Ordering};

pub static TOTAL_FRAMES: AtomicUsize = AtomicUsize::new(0);
pub static USED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// Physical frame allocator using a bitmap for tracking availability.
/// Supports allocation and deallocation (reclamation).
pub struct BitMapFrameAllocator {
    bitmap: &'static mut [u8],
    total_frames: usize,
    last_allocated_index: usize,
}

impl BitMapFrameAllocator {
    /// Initialize the bitmap allocator.
    pub unsafe fn init(boot_info: &'static BootInfo) -> Self {
        serial::serial_print("[FRAME] Initializing BitMap allocator...\n");
        
        let max_phys = boot_info.conventional_mem_total_bytes;
        let total_frames = (max_phys / 4096) as usize;
        let bitmap_size_bytes = (total_frames + 7) / 8;
        
        serial::serial_print("[FRAME] Conventional RAM: ");
        serial::serial_print_dec(max_phys / 1024 / 1024);
        serial::serial_print(" MiB, Total frames: ");
        serial::serial_print_dec(total_frames as u64);
        serial::serial_print("\n");
        serial::serial_print("[FRAME] Bitmap size: ");
        serial::serial_print_dec(bitmap_size_bytes as u64);
        serial::serial_print(" bytes\n");

        // Put the bitmap just after the kernel heap region provided by the bootloader.
        let bitmap_phys_addr = boot_info.heap_phys_base + boot_info.heap_phys_size;
        
        if bitmap_phys_addr + bitmap_size_bytes as u64 > max_phys {
            panic!("Not enough conventional memory for the frame bitmap!");
        }

        serial::serial_print("[FRAME] Bitmap allocated at phys: ");
        serial::serial_print_hex(bitmap_phys_addr);
        serial::serial_print("\n");

        // Convert phys to virt using HHDM offset
        let bitmap_virt_addr = crate::memory::phys_to_virt(bitmap_phys_addr) as *mut u8;
        let bitmap = core::slice::from_raw_parts_mut(bitmap_virt_addr, bitmap_size_bytes);
        
        // 1. Initialize bitmap: mark EVERYTHING as free (0) initially
        bitmap.fill(0x00);
        
        let mut allocator = BitMapFrameAllocator {
            bitmap,
            total_frames,
            last_allocated_index: 0,
        };
        
        // 2. Reserve critical regions
        
        // Reserve the first 16MB (includes BIOS, bootloader, kernel binary, and stacks)
        allocator.reserve_region(0, 16 * 1024 * 1024);
        
        // Reserve the kernel heap
        allocator.reserve_region(boot_info.heap_phys_base, boot_info.heap_phys_size);
        
        // Reserve the bitmap itself
        allocator.reserve_region(bitmap_phys_addr, bitmap_size_bytes as u64);

        // Calculate actually free frames
        let mut free_count = 0;
        for i in 0..total_frames {
            if allocator.is_free(i) {
                free_count += 1;
            }
        }

        TOTAL_FRAMES.store(free_count, Ordering::SeqCst);
        USED_FRAMES.store(0, Ordering::SeqCst);
        
        allocator
    }

    #[inline]
    fn is_free(&self, index: usize) -> bool {
        let byte = index / 8;
        let bit = index % 8;
        (self.bitmap[byte] & (1 << bit)) == 0
    }

    #[inline]
    fn mark_used(&mut self, index: usize) {
        let byte = index / 8;
        let bit = index % 8;
        self.bitmap[byte] |= 1 << bit;
    }

    #[inline]
    fn mark_free(&mut self, index: usize) {
        let byte = index / 8;
        let bit = index % 8;
        self.bitmap[byte] &= !(1 << bit);
    }

    pub fn allocate_frame(&mut self) -> Option<PhysFrame> {
        // Start searching from last_allocated_index for speed
        let start = self.last_allocated_index;
        for i in 0..self.total_frames {
            let idx = (start + i) % self.total_frames;
            if self.is_free(idx) {
                self.mark_used(idx);
                self.last_allocated_index = idx;
                USED_FRAMES.fetch_add(1, Ordering::SeqCst);
                let phys = idx as u64 * 4096;
                return Some(PhysFrame::from_start_address(PhysAddr::new(phys)).unwrap());
            }
        }
        None
    }

    pub fn deallocate_frame(&mut self, frame: PhysFrame) {
        let phys = frame.start_address().as_u64();
        let idx = (phys / 4096) as usize;
        if idx < self.total_frames {
            if !self.is_free(idx) {
                self.mark_free(idx);
                USED_FRAMES.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
    pub fn reserve_region(&mut self, start_phys: u64, len: u64) {
        let start_idx = (start_phys / 4096) as usize;
        let end_idx = ((start_phys + len + 4095) / 4096) as usize;
        for i in start_idx..end_idx {
            if i < self.total_frames && self.is_free(i) {
                self.mark_used(i);
                TOTAL_FRAMES.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

// Global Allocator Instance
static mut FRAME_ALLOCATOR: Option<BitMapFrameAllocator> = None;
static FRAME_ALLOC_LOCK: spin::Mutex<()> = spin::Mutex::new(());

pub fn init(boot_info: &'static BootInfo) {
    unsafe {
        FRAME_ALLOCATOR = Some(BitMapFrameAllocator::init(boot_info));
        serial::serial_print("BitMap Frame Allocator Initialized\n");
    }
}

pub fn reserve_region(start_phys: u64, len: u64) {
    let _lock = FRAME_ALLOC_LOCK.lock();
    unsafe {
        if let Some(ref mut allocator) = FRAME_ALLOCATOR {
            allocator.reserve_region(start_phys, len);
        }
    }
}

pub fn alloc_frame() -> Option<PhysFrame> {
    let _lock = FRAME_ALLOC_LOCK.lock();
    unsafe {
        if let Some(ref mut allocator) = FRAME_ALLOCATOR {
            allocator.allocate_frame()
        } else {
            panic!("Frame Allocator not initialized!");
        }
    }
}

pub fn dealloc_frame(frame: PhysFrame) {
    let _lock = FRAME_ALLOC_LOCK.lock();
    unsafe {
        if let Some(ref mut allocator) = FRAME_ALLOCATOR {
            allocator.deallocate_frame(frame);
        }
    }
}

pub fn get_memory_usage_stats() -> (u64, u64) {
    (
        TOTAL_FRAMES.load(Ordering::SeqCst) as u64,
        USED_FRAMES.load(Ordering::SeqCst) as u64,
    )
}
