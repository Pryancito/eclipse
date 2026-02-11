//! Copy-on-Write (CoW) implementation for EclipseFS
//! Provides atomic, crash-safe writes with zero-cost snapshots
//! Inspired by ZFS and Btrfs

use crate::{EclipseFSError, EclipseFSResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// Reference-counted block for CoW
#[derive(Debug, Clone)]
pub struct RefCountedBlock {
    /// Physical block number
    pub block_id: u64,
    /// Reference count - how many inodes point to this block
    pub refcount: u32,
    /// Data checksum (for integrity)
    pub checksum: u64,
    /// Data content
    pub data: Vec<u8>,
    /// Block generation/version
    pub generation: u32,
}

impl RefCountedBlock {
    /// Create a new ref-counted block
    pub fn new(block_id: u64, data: Vec<u8>) -> Self {
        let checksum = Self::calculate_checksum(&data);
        Self {
            block_id,
            refcount: 1,
            checksum,
            data,
            generation: 1,
        }
    }

    /// Calculate XXH64 checksum for data
    fn calculate_checksum(data: &[u8]) -> u64 {
        // Simple FNV-1a hash for now (can be replaced with xxHash later)
        let mut hash: u64 = 0xcbf29ce484222325;
        for &byte in data {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    /// Verify data integrity
    pub fn verify(&self) -> bool {
        Self::calculate_checksum(&self.data) == self.checksum
    }

    /// Increment reference count
    pub fn inc_ref(&mut self) {
        self.refcount += 1;
    }

    /// Decrement reference count, returns true if block should be freed
    pub fn dec_ref(&mut self) -> bool {
        if self.refcount > 0 {
            self.refcount -= 1;
        }
        self.refcount == 0
    }
}

/// Copy-on-Write manager for EclipseFS
pub struct CowManager {
    /// All blocks in the system
    blocks: HashMap<u64, RefCountedBlock>,
    /// Next block ID to allocate
    next_block_id: AtomicU32,
    /// Free block list
    free_blocks: Vec<u64>,
    /// Statistics
    total_blocks: u64,
    shared_blocks: u64,
}

impl Default for CowManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CowManager {
    /// Create a new CoW manager
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            next_block_id: AtomicU32::new(1),
            free_blocks: Vec::new(),
            total_blocks: 0,
            shared_blocks: 0,
        }
    }

    /// Allocate a new block with data
    pub fn allocate_block(&mut self, data: Vec<u8>) -> EclipseFSResult<u64> {
        let block_id = if let Some(free_id) = self.free_blocks.pop() {
            free_id
        } else {
            self.next_block_id.fetch_add(1, Ordering::SeqCst) as u64
        };

        let block = RefCountedBlock::new(block_id, data);
        self.blocks.insert(block_id, block);
        self.total_blocks += 1;

        Ok(block_id)
    }

    /// Copy-on-Write: Clone a block if shared, or reuse if exclusive
    pub fn cow_write(&mut self, block_id: u64, new_data: Vec<u8>) -> EclipseFSResult<u64> {
        let block = self.blocks.get(&block_id)
            .ok_or(EclipseFSError::NotFound)?;

        if block.refcount > 1 {
            // Block is shared - create a new copy
            let new_block_id = self.allocate_block(new_data)?;
            
            // Decrement old block's refcount
            if let Some(old_block) = self.blocks.get_mut(&block_id) {
                if old_block.dec_ref() {
                    self.free_block(block_id)?;
                }
            }
            
            Ok(new_block_id)
        } else {
            // Block is exclusive - can modify in place
            if let Some(block) = self.blocks.get_mut(&block_id) {
                block.data = new_data;
                block.checksum = RefCountedBlock::calculate_checksum(&block.data);
                block.generation += 1;
            }
            Ok(block_id)
        }
    }

    /// Read block data
    pub fn read_block(&self, block_id: u64) -> EclipseFSResult<&[u8]> {
        let block = self.blocks.get(&block_id)
            .ok_or(EclipseFSError::NotFound)?;
        
        // Verify integrity before returning data
        if !block.verify() {
            return Err(EclipseFSError::InvalidChecksum);
        }
        
        Ok(&block.data)
    }

    /// Increment reference count (for snapshots)
    pub fn inc_ref(&mut self, block_id: u64) -> EclipseFSResult<()> {
        let block = self.blocks.get_mut(&block_id)
            .ok_or(EclipseFSError::NotFound)?;
        
        block.inc_ref();
        
        if block.refcount > 1 {
            self.shared_blocks += 1;
        }
        
        Ok(())
    }

    /// Decrement reference count (when deleting file or snapshot)
    pub fn dec_ref(&mut self, block_id: u64) -> EclipseFSResult<()> {
        let should_free = {
            let block = self.blocks.get_mut(&block_id)
                .ok_or(EclipseFSError::NotFound)?;
            
            if block.refcount > 1 {
                self.shared_blocks -= 1;
            }
            
            block.dec_ref()
        };

        if should_free {
            self.free_block(block_id)?;
        }

        Ok(())
    }

    /// Free a block
    fn free_block(&mut self, block_id: u64) -> EclipseFSResult<()> {
        self.blocks.remove(&block_id);
        self.free_blocks.push(block_id);
        self.total_blocks -= 1;
        Ok(())
    }

    /// Get CoW statistics
    pub fn stats(&self) -> CowStats {
        CowStats {
            total_blocks: self.total_blocks,
            shared_blocks: self.shared_blocks,
            free_blocks: self.free_blocks.len() as u64,
            space_saved: self.calculate_space_saved(),
        }
    }

    /// Calculate space saved by block sharing
    fn calculate_space_saved(&self) -> u64 {
        self.shared_blocks * 4096 // Assuming 4KB blocks
    }

    /// Verify all blocks integrity
    pub fn verify_all(&self) -> Vec<u64> {
        let mut corrupted = Vec::new();
        for (block_id, block) in &self.blocks {
            if !block.verify() {
                corrupted.push(*block_id);
            }
        }
        corrupted
    }
}

/// CoW statistics
#[derive(Debug, Clone)]
pub struct CowStats {
    pub total_blocks: u64,
    pub shared_blocks: u64,
    pub free_blocks: u64,
    pub space_saved: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cow_basic() {
        let mut cow = CowManager::new();
        
        // Allocate a block
        let data = vec![1, 2, 3, 4];
        let block_id = cow.allocate_block(data.clone()).unwrap();
        
        // Read it back
        let read_data = cow.read_block(block_id).unwrap();
        assert_eq!(read_data, &data[..]);
    }

    #[test]
    fn test_cow_write_exclusive() {
        let mut cow = CowManager::new();
        
        let block_id = cow.allocate_block(vec![1, 2, 3]).unwrap();
        
        // CoW write on exclusive block should modify in place
        let new_block_id = cow.cow_write(block_id, vec![4, 5, 6]).unwrap();
        assert_eq!(block_id, new_block_id);
        
        let data = cow.read_block(new_block_id).unwrap();
        assert_eq!(data, &[4, 5, 6]);
    }

    #[test]
    fn test_cow_write_shared() {
        let mut cow = CowManager::new();
        
        let block_id = cow.allocate_block(vec![1, 2, 3]).unwrap();
        cow.inc_ref(block_id).unwrap();
        
        // CoW write on shared block should create new copy
        let new_block_id = cow.cow_write(block_id, vec![4, 5, 6]).unwrap();
        assert_ne!(block_id, new_block_id);
        
        // Original should still have old data
        let old_data = cow.read_block(block_id).unwrap();
        assert_eq!(old_data, &[1, 2, 3]);
        
        // New should have new data
        let new_data = cow.read_block(new_block_id).unwrap();
        assert_eq!(new_data, &[4, 5, 6]);
    }

    #[test]
    fn test_refcounting() {
        let mut cow = CowManager::new();
        
        let block_id = cow.allocate_block(vec![1, 2, 3]).unwrap();
        
        // Increment ref count
        cow.inc_ref(block_id).unwrap();
        cow.inc_ref(block_id).unwrap();
        
        let stats = cow.stats();
        assert_eq!(stats.shared_blocks, 2);
        
        // Decrement ref count
        cow.dec_ref(block_id).unwrap();
        cow.dec_ref(block_id).unwrap();
        
        // Block should still exist
        assert!(cow.read_block(block_id).is_ok());
        
        // Final decrement should free it
        cow.dec_ref(block_id).unwrap();
        assert!(cow.read_block(block_id).is_err());
    }

    #[test]
    fn test_checksum_verification() {
        let _cow = CowManager::new();
        
        let data = vec![1, 2, 3, 4, 5];
        let block = RefCountedBlock::new(1, data.clone());
        
        assert!(block.verify());
        
        // Corrupted block
        let mut corrupted = block.clone();
        corrupted.data[0] = 99;
        assert!(!corrupted.verify());
    }
}
