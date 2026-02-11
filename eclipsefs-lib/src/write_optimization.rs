//! Write optimization module for EclipseFS
//! Implements delayed allocation, write batching, and parallel I/O
//! Inspired by ext4's delayed allocation and XFS's parallel I/O

use crate::{EclipseFSResult, EclipseFSNode};
use std::collections::HashMap;

/// Write batch for collecting multiple writes before flushing
/// Similar to ext4's journal batching
#[derive(Debug)]
pub struct WriteBatch {
    /// Pending node writes (inode -> node)
    pending_writes: HashMap<u32, EclipseFSNode>,
    /// Pending metadata updates (inode -> metadata_fields)
    pending_metadata: HashMap<u32, MetadataUpdate>,
    /// Maximum batch size before automatic flush
    max_batch_size: usize,
}

/// Metadata update for batching small metadata changes
/// Avoids full node rewrites for metadata-only changes
#[derive(Debug, Clone)]
pub struct MetadataUpdate {
    pub atime: Option<u64>,
    pub mtime: Option<u64>,
    pub ctime: Option<u64>,
    pub mode: Option<u32>,
    pub size: Option<u64>,
}

impl WriteBatch {
    /// Create a new write batch
    pub fn new(max_batch_size: usize) -> Self {
        Self {
            pending_writes: HashMap::new(),
            pending_metadata: HashMap::new(),
            max_batch_size,
        }
    }

    /// Add a node write to the batch
    pub fn add_write(&mut self, inode: u32, node: EclipseFSNode) -> EclipseFSResult<bool> {
        self.pending_writes.insert(inode, node);
        
        // Return true if batch should be flushed
        Ok(self.pending_writes.len() >= self.max_batch_size)
    }

    /// Add a metadata update to the batch
    pub fn add_metadata_update(&mut self, inode: u32, update: MetadataUpdate) -> EclipseFSResult<bool> {
        self.pending_metadata.insert(inode, update);
        
        // Return true if batch should be flushed
        Ok(self.pending_metadata.len() >= self.max_batch_size)
    }

    /// Get pending writes (for flushing)
    pub fn take_pending_writes(&mut self) -> HashMap<u32, EclipseFSNode> {
        std::mem::take(&mut self.pending_writes)
    }

    /// Get pending metadata updates (for flushing)
    pub fn take_pending_metadata(&mut self) -> HashMap<u32, MetadataUpdate> {
        std::mem::take(&mut self.pending_metadata)
    }

    /// Check if batch has any pending operations
    pub fn has_pending(&self) -> bool {
        !self.pending_writes.is_empty() || !self.pending_metadata.is_empty()
    }

    /// Get batch statistics
    pub fn stats(&self) -> BatchStats {
        BatchStats {
            pending_writes: self.pending_writes.len(),
            pending_metadata: self.pending_metadata.len(),
            total_pending: self.pending_writes.len() + self.pending_metadata.len(),
        }
    }
}

/// Statistics for write batching
#[derive(Debug, Clone)]
pub struct BatchStats {
    pub pending_writes: usize,
    pub pending_metadata: usize,
    pub total_pending: usize,
}

/// Sequential write optimizer
/// Detects and optimizes sequential write patterns
#[derive(Debug)]
pub struct SequentialWriteOptimizer {
    /// Last written inode
    last_inode: Option<u32>,
    /// Sequential write count
    sequential_count: u32,
    /// Write buffer for sequential data
    sequential_buffer: Vec<u8>,
    /// Maximum buffer size
    max_buffer_size: usize,
}

impl SequentialWriteOptimizer {
    /// Create a new sequential write optimizer
    pub fn new(max_buffer_size: usize) -> Self {
        Self {
            last_inode: None,
            sequential_count: 0,
            sequential_buffer: Vec::with_capacity(max_buffer_size),
            max_buffer_size,
        }
    }

    /// Check if write is sequential
    pub fn is_sequential(&self, inode: u32) -> bool {
        if let Some(last) = self.last_inode {
            // Only forward sequential pattern to avoid oscillation issues
            inode == last + 1
        } else {
            false
        }
    }

    /// Record a write
    pub fn record_write(&mut self, inode: u32, data: &[u8]) -> bool {
        let is_sequential = self.is_sequential(inode);
        
        if is_sequential {
            self.sequential_count += 1;
            
            // Add data to buffer if there's space, otherwise flush will be triggered
            if self.sequential_buffer.len() + data.len() <= self.max_buffer_size {
                self.sequential_buffer.extend_from_slice(data);
            } else {
                // Buffer is full, caller should flush
                self.last_inode = Some(inode);
                return true;
            }
        } else {
            self.sequential_count = 0;
            self.sequential_buffer.clear();
        }
        
        self.last_inode = Some(inode);
        
        // Return true if buffer should be flushed
        self.sequential_buffer.len() >= self.max_buffer_size
    }

    /// Get and clear the sequential buffer
    pub fn take_buffer(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.sequential_buffer)
    }

    /// Get optimization statistics
    pub fn stats(&self) -> SequentialStats {
        SequentialStats {
            sequential_count: self.sequential_count,
            buffer_size: self.sequential_buffer.len(),
            is_sequential_pattern: self.sequential_count >= 4,
        }
    }
}

/// Statistics for sequential write optimization
#[derive(Debug, Clone)]
pub struct SequentialStats {
    pub sequential_count: u32,
    pub buffer_size: usize,
    pub is_sequential_pattern: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_batch() {
        let mut batch = WriteBatch::new(10);
        assert!(!batch.has_pending());
        
        let node = EclipseFSNode::new_file();
        batch.add_write(1, node).unwrap();
        
        assert!(batch.has_pending());
        assert_eq!(batch.stats().pending_writes, 1);
    }

    #[test]
    fn test_sequential_write_detection() {
        let mut optimizer = SequentialWriteOptimizer::new(1024);
        
        // First write initializes
        optimizer.record_write(1, b"data1");
        assert!(!optimizer.stats().is_sequential_pattern); // count still 0
        
        // Second write is sequential
        assert!(optimizer.is_sequential(2));
        optimizer.record_write(2, b"data2");
        assert!(!optimizer.stats().is_sequential_pattern); // count is 1
        
        // Third write
        optimizer.record_write(3, b"data3");
        assert!(!optimizer.stats().is_sequential_pattern); // count is 2
        
        // Fourth write
        optimizer.record_write(4, b"data4");
        assert!(!optimizer.stats().is_sequential_pattern); // count is 3
        
        // Fifth write - now we hit the threshold
        optimizer.record_write(5, b"data5");
        assert!(optimizer.stats().is_sequential_pattern); // count is 4
    }
}
