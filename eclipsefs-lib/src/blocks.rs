//! Block allocation and management for EclipseFS
//! Inspired by ext4 block allocator and XFS allocation groups

use crate::error::{EclipseFSError, EclipseFSResult};
use crate::extent::Extent;

#[cfg(feature = "std")]
use std::collections::HashMap;
#[cfg(feature = "std")]
use std::vec::Vec;

#[cfg(not(feature = "std"))]
use heapless::{FnvIndexMap, Vec};

/// Block size in bytes (4KB, standard for modern filesystems)
pub const BLOCK_SIZE: u64 = 4096;

/// Maximum blocks for no_std
#[cfg(not(feature = "std"))]
const MAX_BLOCKS: usize = 4096;

#[cfg(not(feature = "std"))]
const MAX_GROUPS: usize = 16;

/// Maximum delayed allocations for no_std
#[cfg(not(feature = "std"))]
const MAX_DELAYED: usize = 64;

/// Type alias for list of extents, handling differences between std and no_std
#[cfg(feature = "std")]
pub type ExtentList = Vec<Extent>;

#[cfg(not(feature = "std"))]
pub type ExtentList = Vec<Extent, MAX_DELAYED>;

/// Block allocation group (inspired by XFS allocation groups)
/// Divides the filesystem into independent regions for parallel allocation
#[derive(Debug, Clone)]
pub struct AllocationGroup {
    /// Group ID
    pub id: u32,
    /// Starting block number
    pub start_block: u64,
    /// Number of blocks in this group
    pub block_count: u64,
    /// Free blocks in this group
    pub free_blocks: u64,
    /// Bitmap of free blocks (bit = 1 means free)
    #[cfg(feature = "std")]
    bitmap: Vec<u64>,
    #[cfg(not(feature = "std"))]
    bitmap: Vec<u64, 64>, // 64 * 64 bits = 4096 blocks max for no_std
}

impl AllocationGroup {
    /// Create a new allocation group
    pub fn new(id: u32, start_block: u64, block_count: u64) -> Self {
        let bitmap_size = ((block_count + 63) / 64) as usize;
        
        #[cfg(feature = "std")]
        let bitmap = vec![0xFFFFFFFFFFFFFFFF; bitmap_size];
        
        #[cfg(not(feature = "std"))]
        let mut bitmap = Vec::new();
        #[cfg(not(feature = "std"))]
        for _ in 0..bitmap_size.min(64) {
            let _ = bitmap.push(0xFFFFFFFFFFFFFFFF);
        }

        Self {
            id,
            start_block,
            block_count,
            free_blocks: block_count,
            bitmap,
        }
    }

    /// Allocate a single block
    pub fn allocate_block(&mut self) -> Option<u64> {
        if self.free_blocks == 0 {
            return None;
        }

        // Find first free block
        for (i, &word) in self.bitmap.iter().enumerate() {
            if word != 0 {
                // Find first set bit
                let bit = word.trailing_zeros() as usize;
                let block_offset = i * 64 + bit;
                
                if block_offset < self.block_count as usize {
                    // Mark as allocated
                    self.bitmap[i] &= !(1u64 << bit);
                    self.free_blocks -= 1;
                    return Some(self.start_block + block_offset as u64);
                }
            }
        }

        None
    }

    /// Allocate contiguous blocks (for extent allocation)
    pub fn allocate_contiguous(&mut self, count: u32) -> Option<u64> {
        if self.free_blocks < count as u64 {
            return None;
        }

        // Search for contiguous free blocks
        let mut consecutive = 0;
        let mut start_block = 0;

        for (word_idx, &word) in self.bitmap.iter().enumerate() {
            if word == 0 {
                consecutive = 0;
                continue;
            }

            for bit in 0..64 {
                let block_offset = word_idx * 64 + bit;
                if block_offset >= self.block_count as usize {
                    break;
                }

                if (word & (1u64 << bit)) != 0 {
                    if consecutive == 0 {
                        start_block = block_offset;
                    }
                    consecutive += 1;

                    if consecutive == count {
                        // Found enough contiguous blocks, allocate them
                        for i in 0..count {
                            let block_idx = start_block + i as usize;
                            let word_i = block_idx / 64;
                            let bit_i = block_idx % 64;
                            self.bitmap[word_i] &= !(1u64 << bit_i);
                        }
                        self.free_blocks -= count as u64;
                        return Some(self.start_block + start_block as u64);
                    }
                } else {
                    consecutive = 0;
                }
            }
        }

        None
    }

    /// Free a block
    pub fn free_block(&mut self, block: u64) -> EclipseFSResult<()> {
        if block < self.start_block || block >= self.start_block + self.block_count {
            return Err(EclipseFSError::InvalidOperation);
        }

        let offset = (block - self.start_block) as usize;
        let word_idx = offset / 64;
        let bit_idx = offset % 64;

        if word_idx >= self.bitmap.len() {
            return Err(EclipseFSError::InvalidOperation);
        }

        // Check if block is already free (prevent double-free)
        if (self.bitmap[word_idx] & (1u64 << bit_idx)) != 0 {
            // Block is already free
            return Err(EclipseFSError::InvalidOperation);
        }

        // Mark as free
        self.bitmap[word_idx] |= 1u64 << bit_idx;
        self.free_blocks += 1;

        Ok(())
    }

    /// Free a range of blocks
    pub fn free_blocks(&mut self, start: u64, count: u32) -> EclipseFSResult<()> {
        for i in 0..count {
            self.free_block(start + i as u64)?;
        }
        Ok(())
    }

    /// Get free space percentage
    pub fn free_percentage(&self) -> f32 {
        if self.block_count == 0 {
            return 0.0;
        }
        (self.free_blocks as f32 / self.block_count as f32) * 100.0
    }
}

/// Block allocator for the entire filesystem
/// Implements delayed allocation and extent-based allocation
#[cfg(feature = "std")]
#[derive(Debug)]
pub struct BlockAllocator {
    /// Allocation groups
    groups: Vec<AllocationGroup>,
    /// Total blocks in filesystem
    total_blocks: u64,
    /// Total free blocks
    free_blocks: u64,
    /// Next group to try for allocation (round-robin)
    next_group: usize,
    /// Delayed allocation buffer (logical_block -> pending_size)
    delayed_allocs: HashMap<u64, u32>,
}

#[cfg(not(feature = "std"))]
#[derive(Debug)]
pub struct BlockAllocator {
    /// Allocation groups
    groups: Vec<AllocationGroup, MAX_GROUPS>,
    /// Total blocks in filesystem
    total_blocks: u64,
    /// Total free blocks
    free_blocks: u64,
    /// Next group to try for allocation (round-robin)
    next_group: usize,
    /// Delayed allocation buffer (limited for no_std)
    delayed_allocs: FnvIndexMap<u64, u32, 64>,
}

impl BlockAllocator {
    /// Create a new block allocator
    pub fn new(total_blocks: u64, blocks_per_group: u64) -> Self {
        let group_count = ((total_blocks + blocks_per_group - 1) / blocks_per_group) as usize;
        
        #[cfg(feature = "std")]
        let mut groups = Vec::new();
        #[cfg(not(feature = "std"))]
        let mut groups = Vec::new();

        for i in 0..group_count {
            let start = i as u64 * blocks_per_group;
            let count = (blocks_per_group).min(total_blocks - start);
            let group = AllocationGroup::new(i as u32, start, count);
            
            #[cfg(feature = "std")]
            groups.push(group);
            
            #[cfg(not(feature = "std"))]
            {
                if groups.len() < MAX_GROUPS {
                    let _ = groups.push(group);
                }
            }
        }

        Self {
            groups,
            total_blocks,
            free_blocks: total_blocks,
            next_group: 0,
            #[cfg(feature = "std")]
            delayed_allocs: HashMap::new(),
            #[cfg(not(feature = "std"))]
            delayed_allocs: FnvIndexMap::new(),
        }
    }

    /// Allocate an extent with the specified number of blocks
    /// Uses best-fit strategy to minimize fragmentation
    pub fn allocate_extent(&mut self, block_count: u32) -> EclipseFSResult<Extent> {
        if self.free_blocks < block_count as u64 {
            return Err(EclipseFSError::DeviceFull);
        }

        // Try to allocate contiguous blocks starting from next_group
        for i in 0..self.groups.len() {
            let group_idx = (self.next_group + i) % self.groups.len();
            
            if let Some(start_block) = self.groups[group_idx].allocate_contiguous(block_count) {
                self.free_blocks -= block_count as u64;
                self.next_group = (group_idx + 1) % self.groups.len();
                
                return Ok(Extent::new(0, start_block, block_count));
            }
        }

        // If no contiguous space, try to allocate smaller extents
        // This is a fallback for fragmented filesystems
        Err(EclipseFSError::DeviceFull)
    }

    /// Free an extent
    pub fn free_extent(&mut self, extent: &Extent) -> EclipseFSResult<()> {
        // Validate extent is fully contained within a single allocation group
        let extent_end = extent.physical_block + extent.length as u64;
        
        // Find the group containing this extent
        for group in &mut self.groups {
            if extent.physical_block >= group.start_block &&
               extent.physical_block < group.start_block + group.block_count {
                // Validate extent doesn't span multiple groups
                if extent_end > group.start_block + group.block_count {
                    return Err(EclipseFSError::InvalidOperation);
                }
                
                group.free_blocks(extent.physical_block, extent.length)?;
                self.free_blocks += extent.length as u64;
                return Ok(());
            }
        }

        Err(EclipseFSError::InvalidOperation)
    }

    /// Register a delayed allocation
    /// Delays actual block allocation until flush/commit
    /// Note: Registering the same logical_block twice will overwrite the previous count
    pub fn delay_allocation(&mut self, logical_block: u64, count: u32) -> EclipseFSResult<()> {
        #[cfg(feature = "std")]
        {
            if self.delayed_allocs.contains_key(&logical_block) {
                // Warn about overwrite - could indicate a bug
                return Err(EclipseFSError::InvalidOperation);
            }
            self.delayed_allocs.insert(logical_block, count);
            Ok(())
        }

        #[cfg(not(feature = "std"))]
        {
            if self.delayed_allocs.contains_key(&logical_block) {
                return Err(EclipseFSError::InvalidOperation);
            }
            self.delayed_allocs.insert(logical_block, count)
                .map_err(|_| EclipseFSError::DeviceFull)?;
            Ok(())
        }
    }

    /// Flush delayed allocations and return allocated extents
    pub fn flush_delayed_allocations(&mut self) -> EclipseFSResult<ExtentList> {
        #[cfg(feature = "std")]
        let mut extents = Vec::new();
        #[cfg(not(feature = "std"))]
        let mut extents = Vec::new();

        #[cfg(feature = "std")]
        let delayed = std::mem::take(&mut self.delayed_allocs);
        
        #[cfg(not(feature = "std"))]
        let delayed = {
            let mut temp = FnvIndexMap::new();
            core::mem::swap(&mut temp, &mut self.delayed_allocs);
            temp
        };

        for (&logical_block, &count) in delayed.iter() {
            let mut extent = self.allocate_extent(count)?;
            extent.logical_block = logical_block;
            
            #[cfg(feature = "std")]
            extents.push(extent);
            
            #[cfg(not(feature = "std"))]
            {
                extents.push(extent).map_err(|_| EclipseFSError::DeviceFull)?;
            }
        }

        Ok(extents)
    }

    /// Get allocation statistics
    pub fn get_stats(&self) -> AllocatorStats {
        let total_groups = self.groups.len();
        let mut average_free_percentage = 0.0;
        
        for group in &self.groups {
            average_free_percentage += group.free_percentage();
        }
        
        if total_groups > 0 {
            average_free_percentage /= total_groups as f32;
        }

        AllocatorStats {
            total_blocks: self.total_blocks,
            free_blocks: self.free_blocks,
            used_blocks: self.total_blocks - self.free_blocks,
            total_groups,
            average_free_percentage,
            delayed_allocations: self.delayed_allocs.len(),
        }
    }
}

/// Block allocator statistics
#[derive(Debug, Clone)]
pub struct AllocatorStats {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub used_blocks: u64,
    pub total_groups: usize,
    pub average_free_percentage: f32,
    pub delayed_allocations: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocation_group_single_block() {
        let mut group = AllocationGroup::new(0, 0, 100);
        
        let block = group.allocate_block();
        assert_eq!(block, Some(0));
        assert_eq!(group.free_blocks, 99);
    }

    #[test]
    fn test_allocation_group_contiguous() {
        let mut group = AllocationGroup::new(0, 1000, 100);
        
        let start = group.allocate_contiguous(10);
        assert_eq!(start, Some(1000));
        assert_eq!(group.free_blocks, 90);
    }

    #[test]
    fn test_allocation_group_free() {
        let mut group = AllocationGroup::new(0, 0, 100);
        
        let block = group.allocate_block().unwrap();
        group.free_block(block).unwrap();
        assert_eq!(group.free_blocks, 100);
    }

    #[test]
    fn test_block_allocator_extent() {
        let mut allocator = BlockAllocator::new(1000, 100);
        
        let extent = allocator.allocate_extent(10).unwrap();
        assert_eq!(extent.length, 10);
        assert_eq!(allocator.free_blocks, 990);
    }

    #[test]
    fn test_block_allocator_free_extent() {
        let mut allocator = BlockAllocator::new(1000, 100);
        
        let extent = allocator.allocate_extent(10).unwrap();
        allocator.free_extent(&extent).unwrap();
        assert_eq!(allocator.free_blocks, 1000);
    }

    #[test]
    fn test_delayed_allocation() {
        let mut allocator = BlockAllocator::new(1000, 100);
        
        allocator.delay_allocation(0, 10).unwrap();
        allocator.delay_allocation(10, 20).unwrap();
        
        let stats = allocator.get_stats();
        assert_eq!(stats.delayed_allocations, 2);
        
        let extents = allocator.flush_delayed_allocations().unwrap();
        assert_eq!(extents.len(), 2);
    }
}
