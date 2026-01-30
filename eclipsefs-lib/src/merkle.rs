//! Merkle Tree implementation for EclipseFS
//! Provides hierarchical data verification and efficient integrity checking
//! Inspired by ZFS's Merkle tree and Btrfs's checksumming

use crate::{EclipseFSError, EclipseFSResult};
use std::collections::HashMap;

/// Hash type for Merkle tree
pub type Hash = [u8; 32]; // 256-bit hash

/// Merkle tree node
#[derive(Debug, Clone)]
pub struct MerkleNode {
    /// Hash of this node (either data hash or combined child hashes)
    pub hash: Hash,
    /// Is this a leaf node?
    pub is_leaf: bool,
    /// Children hashes (for internal nodes)
    pub children: Vec<Hash>,
    /// Block ID (for leaf nodes)
    pub block_id: Option<u64>,
}

impl MerkleNode {
    /// Create a leaf node from data
    pub fn new_leaf(data: &[u8], block_id: u64) -> Self {
        Self {
            hash: Self::hash_data(data),
            is_leaf: true,
            children: Vec::new(),
            block_id: Some(block_id),
        }
    }

    /// Create an internal node from child hashes
    pub fn new_internal(children: Vec<Hash>) -> Self {
        let hash = Self::hash_children(&children);
        Self {
            hash,
            is_leaf: false,
            children,
            block_id: None,
        }
    }

    /// Calculate hash of data (SHA-256-like, simplified)
    fn hash_data(data: &[u8]) -> Hash {
        let mut hash = [0u8; 32];
        
        // Simple hash function (in production, use SHA-256 or BLAKE3)
        // Using FNV-1a extended to 256 bits
        let mut h: u64 = 0xcbf29ce484222325;
        for &byte in data {
            h ^= byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        
        // Spread the 64-bit hash across 256 bits
        for i in 0..4 {
            let offset = i * 8;
            hash[offset..offset+8].copy_from_slice(&h.to_le_bytes());
            h = h.wrapping_mul(0x100000001b3);
        }
        
        hash
    }

    /// Calculate hash of children
    fn hash_children(children: &[Hash]) -> Hash {
        let mut combined = Vec::new();
        for child in children {
            combined.extend_from_slice(child);
        }
        Self::hash_data(&combined)
    }
}

/// Merkle tree for file data verification
pub struct MerkleTree {
    /// Root hash
    pub root_hash: Hash,
    /// All nodes indexed by hash
    nodes: HashMap<Hash, MerkleNode>,
    /// Height of tree
    height: usize,
    /// Fanout (children per internal node)
    fanout: usize,
}

impl MerkleTree {
    /// Create a new Merkle tree
    pub fn new(fanout: usize) -> Self {
        Self {
            root_hash: [0u8; 32],
            nodes: HashMap::new(),
            height: 0,
            fanout,
        }
    }

    /// Build tree from data blocks
    pub fn build_from_blocks(&mut self, blocks: &[(u64, Vec<u8>)]) -> EclipseFSResult<()> {
        if blocks.is_empty() {
            return Ok(());
        }

        // Create leaf nodes
        let mut current_level: Vec<MerkleNode> = blocks.iter()
            .map(|(block_id, data)| MerkleNode::new_leaf(data, *block_id))
            .collect();

        // Store leaf nodes
        for node in &current_level {
            self.nodes.insert(node.hash, node.clone());
        }

        self.height = 1;

        // Build tree bottom-up
        while current_level.len() > 1 {
            let mut next_level = Vec::new();
            
            for chunk in current_level.chunks(self.fanout) {
                let child_hashes: Vec<Hash> = chunk.iter()
                    .map(|n| n.hash)
                    .collect();
                
                let internal_node = MerkleNode::new_internal(child_hashes);
                self.nodes.insert(internal_node.hash, internal_node.clone());
                next_level.push(internal_node);
            }
            
            current_level = next_level;
            self.height += 1;
        }

        if let Some(root) = current_level.first() {
            self.root_hash = root.hash;
        }

        Ok(())
    }

    /// Verify a block's integrity
    pub fn verify_block(&self, block_id: u64, data: &[u8]) -> bool {
        let leaf = MerkleNode::new_leaf(data, block_id);
        self.nodes.contains_key(&leaf.hash)
    }

    /// Get proof of inclusion for a block
    pub fn get_proof(&self, block_id: u64) -> Option<MerkleProof> {
        // Find the leaf node
        let leaf = self.nodes.values()
            .find(|n| n.is_leaf && n.block_id == Some(block_id))?;

        let mut proof_hashes = Vec::new();
        let mut current_hash = leaf.hash;

        // Traverse up the tree collecting sibling hashes
        for _ in 0..self.height {
            // Find parent that contains current_hash
            if let Some(parent) = self.find_parent(&current_hash) {
                // Add sibling hashes to proof
                for child_hash in &parent.children {
                    if child_hash != &current_hash {
                        proof_hashes.push(*child_hash);
                    }
                }
                current_hash = parent.hash;
            } else {
                break;
            }
        }

        Some(MerkleProof {
            block_id,
            leaf_hash: leaf.hash,
            proof_hashes,
            root_hash: self.root_hash,
        })
    }

    /// Find parent node containing the given child hash
    fn find_parent(&self, child_hash: &Hash) -> Option<&MerkleNode> {
        self.nodes.values()
            .find(|n| !n.is_leaf && n.children.contains(child_hash))
    }

    /// Verify entire tree integrity
    pub fn verify_all(&self) -> bool {
        // Verify all internal nodes
        for node in self.nodes.values() {
            if !node.is_leaf {
                let expected_hash = MerkleNode::hash_children(&node.children);
                if expected_hash != node.hash {
                    return false;
                }
            }
        }
        true
    }

    /// Get statistics
    pub fn stats(&self) -> MerkleStats {
        let leaf_count = self.nodes.values().filter(|n| n.is_leaf).count();
        let internal_count = self.nodes.values().filter(|n| !n.is_leaf).count();
        
        MerkleStats {
            total_nodes: self.nodes.len(),
            leaf_nodes: leaf_count,
            internal_nodes: internal_count,
            height: self.height,
            fanout: self.fanout,
        }
    }
}

/// Proof of inclusion in Merkle tree
#[derive(Debug, Clone)]
pub struct MerkleProof {
    pub block_id: u64,
    pub leaf_hash: Hash,
    pub proof_hashes: Vec<Hash>,
    pub root_hash: Hash,
}

impl MerkleProof {
    /// Verify this proof
    pub fn verify(&self, data: &[u8]) -> bool {
        let computed_leaf = MerkleNode::hash_data(data);
        if computed_leaf != self.leaf_hash {
            return false;
        }

        let mut current_hash = self.leaf_hash;
        
        // Reconstruct path to root
        for proof_hash in &self.proof_hashes {
            let combined = [current_hash, *proof_hash].concat();
            current_hash = MerkleNode::hash_data(&combined);
        }

        current_hash == self.root_hash
    }
}

/// Merkle tree statistics
#[derive(Debug, Clone)]
pub struct MerkleStats {
    pub total_nodes: usize,
    pub leaf_nodes: usize,
    pub internal_nodes: usize,
    pub height: usize,
    pub fanout: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merkle_tree_build() {
        let mut tree = MerkleTree::new(4);
        
        let blocks = vec![
            (1, vec![1, 2, 3]),
            (2, vec![4, 5, 6]),
            (3, vec![7, 8, 9]),
            (4, vec![10, 11, 12]),
        ];
        
        tree.build_from_blocks(&blocks).unwrap();
        
        assert_ne!(tree.root_hash, [0u8; 32]);
        assert!(tree.height > 0);
    }

    #[test]
    fn test_merkle_verify_block() {
        let mut tree = MerkleTree::new(4);
        
        let data1 = vec![1, 2, 3];
        let data2 = vec![4, 5, 6];
        
        let blocks = vec![
            (1, data1.clone()),
            (2, data2.clone()),
        ];
        
        tree.build_from_blocks(&blocks).unwrap();
        
        // Verify correct data
        assert!(tree.verify_block(1, &data1));
        assert!(tree.verify_block(2, &data2));
        
        // Verify wrong data
        assert!(!tree.verify_block(1, &[99, 99, 99]));
    }

    #[test]
    fn test_merkle_proof() {
        let mut tree = MerkleTree::new(2);
        
        let blocks = vec![
            (1, vec![1, 2, 3]),
            (2, vec![4, 5, 6]),
            (3, vec![7, 8, 9]),
            (4, vec![10, 11, 12]),
        ];
        
        tree.build_from_blocks(&blocks).unwrap();
        
        // Get and verify proof
        let proof = tree.get_proof(1).unwrap();
        assert!(proof.verify(&vec![1, 2, 3]));
        assert!(!proof.verify(&vec![99, 99, 99]));
    }

    #[test]
    fn test_merkle_stats() {
        let mut tree = MerkleTree::new(4);
        
        let blocks: Vec<(u64, Vec<u8>)> = (0..16)
            .map(|i| (i, vec![i as u8; 10]))
            .collect();
        
        tree.build_from_blocks(&blocks).unwrap();
        
        let stats = tree.stats();
        assert_eq!(stats.leaf_nodes, 16);
        assert!(stats.internal_nodes > 0);
        assert_eq!(stats.total_nodes, stats.leaf_nodes + stats.internal_nodes);
    }

    #[test]
    fn test_verify_all() {
        let mut tree = MerkleTree::new(4);
        
        let blocks = vec![
            (1, vec![1, 2, 3]),
            (2, vec![4, 5, 6]),
        ];
        
        tree.build_from_blocks(&blocks).unwrap();
        
        assert!(tree.verify_all());
    }
}
