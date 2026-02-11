//! Block-level deduplication for EclipseFS
//! Eliminates duplicate data blocks to save space
//! Inspired by ZFS dedup and Btrfs deduplication

use crate::{EclipseFSError, EclipseFSResult};
use std::collections::HashMap;

/// Hash for content-based addressing
pub type ContentHash = [u8; 32];

/// Deduplicated block
#[derive(Debug, Clone)]
pub struct DedupBlock {
    /// Content hash (SHA-256 or similar)
    pub hash: ContentHash,
    /// Physical block ID
    pub block_id: u64,
    /// Reference count
    pub refcount: u32,
    /// Data size
    pub size: usize,
    /// Compression ratio if compressed
    pub compression_ratio: f32,
}

impl DedupBlock {
    /// Create a new dedup block
    pub fn new(hash: ContentHash, block_id: u64, size: usize) -> Self {
        Self {
            hash,
            block_id,
            refcount: 1,
            size,
            compression_ratio: 1.0,
        }
    }

    /// Increment reference count
    pub fn inc_ref(&mut self) {
        self.refcount += 1;
    }

    /// Decrement reference count, returns true if should be freed
    pub fn dec_ref(&mut self) -> bool {
        if self.refcount > 0 {
            self.refcount -= 1;
        }
        self.refcount == 0
    }
}

/// Deduplication manager
pub struct DedupManager {
    /// Hash to block mapping
    hash_table: HashMap<ContentHash, DedupBlock>,
    /// Block ID to hash mapping (for reverse lookup)
    block_to_hash: HashMap<u64, ContentHash>,
    /// Statistics
    total_blocks: u64,
    dedup_blocks: u64,
    bytes_saved: u64,
}

impl Default for DedupManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DedupManager {
    /// Create a new dedup manager
    pub fn new() -> Self {
        Self {
            hash_table: HashMap::new(),
            block_to_hash: HashMap::new(),
            total_blocks: 0,
            dedup_blocks: 0,
            bytes_saved: 0,
        }
    }

    /// Calculate content hash (SHA-256-like)
    pub fn hash_content(data: &[u8]) -> ContentHash {
        let mut hash = [0u8; 32];
        
        // Simple hash (in production, use SHA-256 or BLAKE3)
        let mut h: u64 = 0xcbf29ce484222325;
        for &byte in data {
            h ^= byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        
        // Spread across 256 bits
        for i in 0..4 {
            let offset = i * 8;
            hash[offset..offset+8].copy_from_slice(&h.to_le_bytes());
            h = h.wrapping_mul(0x100000001b3);
        }
        
        hash
    }

    /// Add or deduplicate a block
    pub fn add_block(&mut self, data: &[u8], block_id: u64) -> EclipseFSResult<DedupResult> {
        let hash = Self::hash_content(data);
        
        // Always track block_id to hash mapping
        self.block_to_hash.insert(block_id, hash);
        
        if let Some(existing) = self.hash_table.get_mut(&hash) {
            // Duplicate found!
            existing.inc_ref();
            self.dedup_blocks += 1;
            self.bytes_saved += data.len() as u64;
            self.total_blocks += 1;  // Still count total blocks added
            
            Ok(DedupResult::Duplicate {
                existing_block_id: existing.block_id,
                hash,
                bytes_saved: data.len(),
            })
        } else {
            // New unique block
            let block = DedupBlock::new(hash, block_id, data.len());
            self.hash_table.insert(hash, block);
            self.total_blocks += 1;
            
            Ok(DedupResult::Unique {
                block_id,
                hash,
            })
        }
    }

    /// Remove a block reference
    pub fn remove_block(&mut self, block_id: u64) -> EclipseFSResult<()> {
        let hash = *self.block_to_hash.get(&block_id)
            .ok_or(EclipseFSError::NotFound)?;
        
        let should_free = {
            let block = self.hash_table.get_mut(&hash)
                .ok_or(EclipseFSError::NotFound)?;
            
            if block.refcount > 1 {
                self.dedup_blocks -= 1;
                self.bytes_saved -= block.size as u64;
            }
            
            block.dec_ref()
        };
        
        if should_free {
            self.hash_table.remove(&hash);
            self.block_to_hash.remove(&block_id);
            self.total_blocks -= 1;
        }
        
        Ok(())
    }

    /// Check if data is a duplicate
    pub fn is_duplicate(&self, data: &[u8]) -> Option<u64> {
        let hash = Self::hash_content(data);
        self.hash_table.get(&hash).map(|b| b.block_id)
    }

    /// Get deduplication statistics
    pub fn stats(&self) -> DedupStats {
        DedupStats {
            total_blocks: self.total_blocks,
            unique_blocks: self.hash_table.len() as u64,
            duplicate_refs: self.dedup_blocks,
            bytes_saved: self.bytes_saved,
            dedup_ratio: if self.total_blocks > 0 {
                self.dedup_blocks as f32 / self.total_blocks as f32
            } else {
                0.0
            },
        }
    }

    /// Get all duplicates (for analysis)
    pub fn find_duplicates(&self) -> Vec<(ContentHash, Vec<u64>)> {
        let mut duplicates = Vec::new();
        
        for (hash, block) in &self.hash_table {
            if block.refcount > 1 {
                // Find all block IDs with this hash
                let block_ids: Vec<u64> = self.block_to_hash.iter()
                    .filter(|(_, h)| *h == hash)
                    .map(|(id, _)| *id)
                    .collect();
                
                if !block_ids.is_empty() {
                    duplicates.push((*hash, block_ids));
                }
            }
        }
        
        duplicates
    }

    /// Compact hash table (remove freed entries)
    pub fn compact(&mut self) {
        self.hash_table.retain(|_, block| block.refcount > 0);
    }
}

/// Result of dedup operation
#[derive(Debug, Clone)]
pub enum DedupResult {
    Unique {
        block_id: u64,
        hash: ContentHash,
    },
    Duplicate {
        existing_block_id: u64,
        hash: ContentHash,
        bytes_saved: usize,
    },
}

/// Deduplication statistics
#[derive(Debug, Clone)]
pub struct DedupStats {
    pub total_blocks: u64,
    pub unique_blocks: u64,
    pub duplicate_refs: u64,
    pub bytes_saved: u64,
    pub dedup_ratio: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_unique() {
        let mut dedup = DedupManager::new();
        
        let data1 = vec![1, 2, 3, 4];
        let result = dedup.add_block(&data1, 1).unwrap();
        
        match result {
            DedupResult::Unique { block_id, .. } => {
                assert_eq!(block_id, 1);
            }
            _ => panic!("Expected Unique"),
        }
    }

    #[test]
    fn test_dedup_duplicate() {
        let mut dedup = DedupManager::new();
        
        let data = vec![1, 2, 3, 4];
        
        // First block
        dedup.add_block(&data, 1).unwrap();
        
        // Second identical block
        let result = dedup.add_block(&data, 2).unwrap();
        
        match result {
            DedupResult::Duplicate { existing_block_id, bytes_saved, .. } => {
                assert_eq!(existing_block_id, 1);
                assert_eq!(bytes_saved, 4);
            }
            _ => panic!("Expected Duplicate"),
        }
    }

    #[test]
    fn test_dedup_stats() {
        let mut dedup = DedupManager::new();
        
        let data1 = vec![1, 2, 3];
        let data2 = vec![4, 5, 6];
        
        // Add unique blocks
        dedup.add_block(&data1, 1).unwrap();
        dedup.add_block(&data2, 2).unwrap();
        
        // Add duplicate
        dedup.add_block(&data1, 3).unwrap();
        
        let stats = dedup.stats();
        assert_eq!(stats.total_blocks, 3);
        assert_eq!(stats.unique_blocks, 2);
        assert_eq!(stats.duplicate_refs, 1);
        assert_eq!(stats.bytes_saved, 3);
    }

    #[test]
    fn test_dedup_remove() {
        let mut dedup = DedupManager::new();
        
        let data = vec![1, 2, 3];
        
        dedup.add_block(&data, 1).unwrap();
        dedup.add_block(&data, 2).unwrap();
        
        // Remove one reference
        dedup.remove_block(2).unwrap();
        
        let stats = dedup.stats();
        // total_blocks is 2 because we added 2 blocks
        // After removing one, we still have 2 total added, but now 1 unique with refcount 1
        assert_eq!(stats.total_blocks, 2);
        // Block 1 still exists with refcount 1 after decrementing from 2
        assert_eq!(stats.duplicate_refs, 0);
    }

    #[test]
    fn test_is_duplicate() {
        let mut dedup = DedupManager::new();
        
        let data = vec![1, 2, 3];
        dedup.add_block(&data, 1).unwrap();
        
        assert_eq!(dedup.is_duplicate(&data), Some(1));
        assert_eq!(dedup.is_duplicate(&[9, 9, 9]), None);
    }

    #[test]
    fn test_find_duplicates() {
        let mut dedup = DedupManager::new();
        
        let data = vec![1, 2, 3];
        
        dedup.add_block(&data, 1).unwrap();
        dedup.add_block(&data, 2).unwrap();
        dedup.add_block(&data, 3).unwrap();
        
        let duplicates = dedup.find_duplicates();
        assert_eq!(duplicates.len(), 1);
    }
}
