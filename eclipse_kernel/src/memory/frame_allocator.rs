use x86_64::PhysAddr;
use x86_64::structures::paging::PhysFrame;
use crate::boot::{BootInfo, MemoryRegionType};
use crate::serial;

pub struct BumpFrameAllocator {
    next: usize,
    current_region: usize,
    regions: &'static [crate::boot::MemoryRegion],
}

impl BumpFrameAllocator {
    pub unsafe fn init(boot_info: &'static BootInfo) -> Self {
        BumpFrameAllocator {
            next: 0,
            current_region: 0,
            regions: &boot_info.memory_map[0..boot_info.memory_map_count],
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
