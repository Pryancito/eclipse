use x86_64::PhysAddr;
use x86_64::structures::paging::PhysFrame;
use crate::boot::{BootInfo, MemoryRegionType};
use crate::serial;
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct BumpFrameAllocator {
    next: usize,
    current_region: usize,
    regions: &'static [crate::boot::MemoryRegion],
}

pub static TOTAL_FRAMES: AtomicUsize = AtomicUsize::new(0);
pub static USED_FRAMES: AtomicUsize = AtomicUsize::new(0);

impl BumpFrameAllocator {
    pub unsafe fn init(boot_info: &'static BootInfo) -> Self {
        serial::serial_print("[FRAME] Initializing with memory map:\n");
        for i in 0..boot_info.memory_map_count {
            let region = &boot_info.memory_map[i];
            serial::serial_print("  Region ");
            serial::serial_print_dec(i as u64);
            serial::serial_print(": ");
            serial::serial_print_hex(region.start);
            serial::serial_print(" - ");
            serial::serial_print_hex(region.start + region.len);
            serial::serial_print(" type=");
            serial::serial_print_dec(region.kind as u64);
            serial::serial_print("\n");
        }
        
        let regions = &boot_info.memory_map[0..boot_info.memory_map_count];
        
        // Count total usable frames
        let mut total_usable: usize = 0;
        for region in regions {
            if region.kind == MemoryRegionType::Usable {
                total_usable += (region.len / 4096) as usize;
            }
        }
        TOTAL_FRAMES.store(total_usable, Ordering::SeqCst);
        serial::serial_print("[FRAME] Total usable frames: ");
        serial::serial_print_dec(total_usable as u64);
        serial::serial_print("\n");
        
        BumpFrameAllocator {
            next: 0,
            current_region: 0,
            regions,
        }
    }

    pub fn allocate_frame(&mut self) -> Option<PhysFrame> {
        loop {
            if self.current_region >= self.regions.len() {
                return None;
            }

            let region = &self.regions[self.current_region];
            
            if region.kind != MemoryRegionType::Usable {
                self.current_region += 1;
                self.next = 0;
                continue;
            }

            let region_start = region.start;
            let region_len = region.len;
            
            // Check if current allocation pointer is within this region
            let alloc_start = region_start + (self.next as u64 * 4096);
            
            if alloc_start >= region_start + region_len {
                // Region exhausted
                self.current_region += 1;
                self.next = 0;
                continue;
            }

            // Return frame
            self.next += 1;
            USED_FRAMES.fetch_add(1, Ordering::SeqCst);
            return Some(PhysFrame::from_start_address(PhysAddr::new(alloc_start)).unwrap());
        }
    }
}

// Global Allocator Instance (initialized later)
static mut FRAME_ALLOCATOR: Option<BumpFrameAllocator> = None;

pub fn init(boot_info: &'static BootInfo) {
    unsafe {
        FRAME_ALLOCATOR = Some(BumpFrameAllocator::init(boot_info));
        serial::serial_print("Frame Allocator Initialized\n");
    }
}

pub fn alloc_frame() -> Option<PhysFrame> {
    unsafe {
        if let Some(ref mut allocator) = FRAME_ALLOCATOR {
            allocator.allocate_frame()
        } else {
            panic!("Frame Allocator not initialized!");
        }
    }
}
/// Get memory usage stats (total_frames, used_frames)
pub fn get_memory_usage_stats() -> (u64, u64) {
    (
        TOTAL_FRAMES.load(Ordering::SeqCst) as u64,
        USED_FRAMES.load(Ordering::SeqCst) as u64,
    )
}
