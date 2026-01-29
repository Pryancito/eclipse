//! Extent-based allocation system for EclipseFS
//! Inspired by ext4 and XFS extent trees

use crate::error::{EclipseFSError, EclipseFSResult};

#[cfg(feature = "std")]
use std::vec::Vec;

#[cfg(not(feature = "std"))]
use heapless::Vec;

/// Maximum number of extents per inode for no_std
#[cfg(not(feature = "std"))]
const MAX_EXTENTS_PER_INODE: usize = 16;

/// An extent represents a contiguous range of blocks
/// Similar to ext4's ext4_extent structure
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extent {
    /// Logical block number (offset in file)
    pub logical_block: u64,
    /// Physical block number (location on disk)
    pub physical_block: u64,
    /// Number of contiguous blocks
    pub length: u32,
    /// Extent flags (unwritten, etc.)
    pub flags: u16,
}

/// Extent flags
pub const EXTENT_FLAG_UNWRITTEN: u16 = 0x0001;  // Allocated but not written
pub const EXTENT_FLAG_COMPRESSED: u16 = 0x0002; // Data is compressed
pub const EXTENT_FLAG_ENCRYPTED: u16 = 0x0004;  // Data is encrypted

impl Extent {
    /// Create a new extent
    pub fn new(logical_block: u64, physical_block: u64, length: u32) -> Self {
        Self {
            logical_block,
            physical_block,
            length,
            flags: 0,
        }
    }

    /// Check if extent is unwritten (allocated but not yet written)
    pub fn is_unwritten(&self) -> bool {
        (self.flags & EXTENT_FLAG_UNWRITTEN) != 0
    }

    /// Mark extent as unwritten
    pub fn mark_unwritten(&mut self) {
        self.flags |= EXTENT_FLAG_UNWRITTEN;
    }

    /// Mark extent as written
    pub fn mark_written(&mut self) {
        self.flags &= !EXTENT_FLAG_UNWRITTEN;
    }

    /// Get the end logical block (exclusive)
    pub fn logical_end(&self) -> u64 {
        self.logical_block + self.length as u64
    }

    /// Get the end physical block (exclusive)
    pub fn physical_end(&self) -> u64 {
        self.physical_block + self.length as u64
    }

    /// Check if this extent can be merged with another
    pub fn can_merge(&self, other: &Extent) -> bool {
        // Must be contiguous in both logical and physical space
        // and have the same flags
        self.flags == other.flags &&
            ((self.logical_end() == other.logical_block && 
              self.physical_end() == other.physical_block) ||
             (other.logical_end() == self.logical_block && 
              other.physical_end() == self.physical_block))
    }

    /// Try to merge with another extent
    pub fn try_merge(&mut self, other: &Extent) -> bool {
        if !self.can_merge(other) {
            return false;
        }

        if other.logical_block < self.logical_block {
            // Other comes before us
            self.logical_block = other.logical_block;
            self.physical_block = other.physical_block;
        }
        
        self.length += other.length;
        true
    }
}

/// Extent tree for managing file extents
/// Inspired by ext4's extent tree (ext4_extent_header + ext4_extent_idx)
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct ExtentTree {
    /// List of extents for this inode
    extents: Vec<Extent>,
    /// Total number of blocks allocated
    total_blocks: u64,
}

#[cfg(not(feature = "std"))]
#[derive(Debug, Clone)]
pub struct ExtentTree {
    /// List of extents for this inode (limited for no_std)
    extents: Vec<Extent, MAX_EXTENTS_PER_INODE>,
    /// Total number of blocks allocated
    total_blocks: u64,
}

impl ExtentTree {
    /// Create a new empty extent tree
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "std")]
            extents: Vec::new(),
            #[cfg(not(feature = "std"))]
            extents: Vec::new(),
            total_blocks: 0,
        }
    }

    /// Add an extent to the tree
    pub fn add_extent(&mut self, extent: Extent) -> EclipseFSResult<()> {
        // Try to merge with existing extents first
        for existing in &mut self.extents {
            if existing.try_merge(&extent) {
                self.total_blocks += extent.length as u64;
                return Ok(());
            }
        }

        // Insert new extent in sorted order by logical block
        let insert_pos = self.extents
            .iter()
            .position(|e| e.logical_block > extent.logical_block)
            .unwrap_or(self.extents.len());

        #[cfg(feature = "std")]
        {
            self.extents.insert(insert_pos, extent);
        }

        #[cfg(not(feature = "std"))]
        {
            if self.extents.len() >= MAX_EXTENTS_PER_INODE {
                return Err(EclipseFSError::DeviceFull);
            }
            self.extents.insert(insert_pos, extent)
                .map_err(|_| EclipseFSError::DeviceFull)?;
        }

        self.total_blocks += extent.length as u64;
        Ok(())
    }

    /// Find the extent containing a logical block
    pub fn find_extent(&self, logical_block: u64) -> Option<&Extent> {
        self.extents.iter().find(|e| {
            logical_block >= e.logical_block && logical_block < e.logical_end()
        })
    }

    /// Get physical block for a logical block
    pub fn logical_to_physical(&self, logical_block: u64) -> Option<u64> {
        self.find_extent(logical_block).map(|extent| {
            let offset = logical_block - extent.logical_block;
            extent.physical_block + offset
        })
    }

    /// Remove an extent
    pub fn remove_extent(&mut self, logical_block: u64) -> EclipseFSResult<()> {
        if let Some(pos) = self.extents.iter().position(|e| e.logical_block == logical_block) {
            let extent = self.extents.remove(pos);
            self.total_blocks -= extent.length as u64;
            Ok(())
        } else {
            Err(EclipseFSError::NotFound)
        }
    }

    /// Get total allocated blocks
    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// Get number of extents
    pub fn extent_count(&self) -> usize {
        self.extents.len()
    }

    /// Get all extents (for serialization)
    pub fn extents(&self) -> &[Extent] {
        &self.extents
    }

    /// Calculate fragmentation score (lower is better)
    /// Returns percentage: 0 = perfect (1 extent), 100 = worst case
    pub fn fragmentation_score(&self) -> f32 {
        if self.extents.is_empty() {
            return 0.0;
        }
        
        // Ideal: 1 extent per file
        // Worst: many small extents
        let ideal_extents = 1.0;
        let actual_extents = self.extents.len() as f32;
        
        ((actual_extents - ideal_extents) / actual_extents) * 100.0
    }

    /// Check if extents are contiguous
    pub fn is_contiguous(&self) -> bool {
        if self.extents.len() <= 1 {
            return true;
        }

        for i in 1..self.extents.len() {
            let prev = &self.extents[i - 1];
            let curr = &self.extents[i];
            
            if prev.logical_end() != curr.logical_block ||
               prev.physical_end() != curr.physical_block {
                return false;
            }
        }
        
        true
    }
}

/// Extent statistics
#[derive(Debug, Clone)]
pub struct ExtentStats {
    pub total_extents: usize,
    pub total_blocks: u64,
    pub average_extent_size: f32,
    pub fragmentation_score: f32,
    pub is_contiguous: bool,
}

impl ExtentTree {
    /// Get statistics about this extent tree
    pub fn get_stats(&self) -> ExtentStats {
        let total_extents = self.extent_count();
        let total_blocks = self.total_blocks();
        let average_extent_size = if total_extents > 0 {
            total_blocks as f32 / total_extents as f32
        } else {
            0.0
        };

        ExtentStats {
            total_extents,
            total_blocks,
            average_extent_size,
            fragmentation_score: self.fragmentation_score(),
            is_contiguous: self.is_contiguous(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extent_creation() {
        let extent = Extent::new(0, 1000, 10);
        assert_eq!(extent.logical_block, 0);
        assert_eq!(extent.physical_block, 1000);
        assert_eq!(extent.length, 10);
        assert!(!extent.is_unwritten());
    }

    #[test]
    fn test_extent_flags() {
        let mut extent = Extent::new(0, 1000, 10);
        extent.mark_unwritten();
        assert!(extent.is_unwritten());
        extent.mark_written();
        assert!(!extent.is_unwritten());
    }

    #[test]
    fn test_extent_merge() {
        let mut extent1 = Extent::new(0, 1000, 10);
        let extent2 = Extent::new(10, 1010, 5);
        
        assert!(extent1.can_merge(&extent2));
        assert!(extent1.try_merge(&extent2));
        assert_eq!(extent1.length, 15);
    }

    #[test]
    fn test_extent_tree_add() {
        let mut tree = ExtentTree::new();
        let extent = Extent::new(0, 1000, 10);
        
        tree.add_extent(extent).unwrap();
        assert_eq!(tree.extent_count(), 1);
        assert_eq!(tree.total_blocks(), 10);
    }

    #[test]
    fn test_extent_tree_lookup() {
        let mut tree = ExtentTree::new();
        tree.add_extent(Extent::new(0, 1000, 10)).unwrap();
        tree.add_extent(Extent::new(10, 2000, 5)).unwrap();
        
        assert_eq!(tree.logical_to_physical(0), Some(1000));
        assert_eq!(tree.logical_to_physical(5), Some(1005));
        assert_eq!(tree.logical_to_physical(10), Some(2000));
        assert_eq!(tree.logical_to_physical(15), None);
    }

    #[test]
    fn test_extent_tree_merge() {
        let mut tree = ExtentTree::new();
        tree.add_extent(Extent::new(0, 1000, 10)).unwrap();
        tree.add_extent(Extent::new(10, 1010, 5)).unwrap();
        
        // Should merge into single extent
        assert_eq!(tree.extent_count(), 1);
        assert!(tree.is_contiguous());
    }

    #[test]
    fn test_fragmentation_score() {
        let mut tree = ExtentTree::new();
        tree.add_extent(Extent::new(0, 1000, 10)).unwrap();
        
        // Single extent = low fragmentation
        assert!(tree.fragmentation_score() < 10.0);
        
        // Add non-contiguous extent
        tree.add_extent(Extent::new(20, 3000, 10)).unwrap();
        
        // Multiple extents = higher fragmentation
        assert!(tree.fragmentation_score() > 0.0);
    }
}
