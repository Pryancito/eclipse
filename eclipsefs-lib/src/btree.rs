//! B-Tree implementation for EclipseFS directory indexing
//! Provides O(log n) lookups for directories with millions of entries
//! Inspired by Btrfs and XFS directory structures

use crate::{EclipseFSError, EclipseFSResult};
use std::cmp::Ordering;

/// B-Tree node order (max children = 2 * ORDER)
const ORDER: usize = 128;

/// Entry in a B-Tree node (filename -> inode)
#[derive(Debug, Clone)]
pub struct BTreeEntry {
    pub name: String,
    pub inode: u32,
}

impl PartialEq for BTreeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for BTreeEntry {}

impl PartialOrd for BTreeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BTreeEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

/// B-Tree node
#[derive(Debug, Clone)]
pub struct BTreeNode {
    /// Entries in this node (sorted by name)
    entries: Vec<BTreeEntry>,
    /// Child node IDs (for internal nodes)
    children: Vec<u32>,
    /// Is this a leaf node?
    is_leaf: bool,
    /// Node ID
    node_id: u32,
}

impl BTreeNode {
    /// Create a new leaf node
    pub fn new_leaf(node_id: u32) -> Self {
        Self {
            entries: Vec::new(),
            children: Vec::new(),
            is_leaf: true,
            node_id,
        }
    }

    /// Create a new internal node
    pub fn new_internal(node_id: u32) -> Self {
        Self {
            entries: Vec::new(),
            children: Vec::new(),
            is_leaf: false,
            node_id,
        }
    }

    /// Check if node is full
    pub fn is_full(&self) -> bool {
        self.entries.len() >= 2 * ORDER - 1
    }

    /// Check if node is minimal (for deletion)
    pub fn is_minimal(&self) -> bool {
        self.entries.len() <= ORDER - 1
    }

    /// Insert entry into node (assumes not full)
    pub fn insert_non_full(&mut self, entry: BTreeEntry) {
        let pos = self.entries.binary_search(&entry)
            .unwrap_or_else(|e| e);
        self.entries.insert(pos, entry);
    }

    /// Search for entry in this node
    pub fn search_local(&self, name: &str) -> Option<u32> {
        self.entries.iter()
            .find(|e| e.name == name)
            .map(|e| e.inode)
    }

    /// Find child index for given key
    pub fn find_child_index(&self, name: &str) -> usize {
        self.entries.iter()
            .position(|e| name < &e.name)
            .unwrap_or(self.entries.len())
    }
}

/// B-Tree for directory indexing
pub struct BTree {
    /// All nodes in the tree
    nodes: Vec<BTreeNode>,
    /// Root node ID
    root_id: u32,
    /// Next node ID to allocate
    next_node_id: u32,
    /// Total entries in tree
    entry_count: usize,
}

impl BTree {
    /// Create a new empty B-Tree
    pub fn new() -> Self {
        let root = BTreeNode::new_leaf(0);
        Self {
            nodes: vec![root],
            root_id: 0,
            next_node_id: 1,
            entry_count: 0,
        }
    }

    /// Insert an entry
    pub fn insert(&mut self, name: String, inode: u32) -> EclipseFSResult<()> {
        let entry = BTreeEntry { name, inode };
        
        // Check if root is full
        if self.get_node(self.root_id).is_full() {
            // Split root
            let new_root_id = self.next_node_id;
            self.next_node_id += 1;
            
            let mut new_root = BTreeNode::new_internal(new_root_id);
            new_root.children.push(self.root_id);
            
            self.nodes.push(new_root);
            self.split_child(new_root_id, 0)?;
            self.root_id = new_root_id;
        }
        
        self.insert_non_full(self.root_id, entry)?;
        self.entry_count += 1;
        Ok(())
    }

    /// Insert into a non-full node
    fn insert_non_full(&mut self, node_id: u32, entry: BTreeEntry) -> EclipseFSResult<()> {
        let is_leaf = self.get_node(node_id).is_leaf;
        
        if is_leaf {
            // Insert directly into leaf
            self.get_node_mut(node_id).insert_non_full(entry);
        } else {
            // Find child to descend to
            let child_idx = self.get_node(node_id).find_child_index(&entry.name);
            let child_id = self.get_node(node_id).children[child_idx];
            
            // Check if child is full
            if self.get_node(child_id).is_full() {
                self.split_child(node_id, child_idx)?;
                
                // After split, entry might go to the new child
                let entry_clone = entry.clone();
                let median = &self.get_node(node_id).entries[child_idx];
                let child_id = if entry_clone.name > median.name {
                    self.get_node(node_id).children[child_idx + 1]
                } else {
                    self.get_node(node_id).children[child_idx]
                };
                
                self.insert_non_full(child_id, entry)?;
            } else {
                self.insert_non_full(child_id, entry)?;
            }
        }
        
        Ok(())
    }

    /// Split a full child
    fn split_child(&mut self, parent_id: u32, child_idx: usize) -> EclipseFSResult<()> {
        let child_id = self.get_node(parent_id).children[child_idx];
        let new_child_id = self.next_node_id;
        self.next_node_id += 1;
        
        let is_leaf = self.get_node(child_id).is_leaf;
        let mut new_child = if is_leaf {
            BTreeNode::new_leaf(new_child_id)
        } else {
            BTreeNode::new_internal(new_child_id)
        };
        
        // Split entries
        let child_node = self.get_node_mut(child_id);
        let mid = ORDER - 1;
        
        new_child.entries = child_node.entries.split_off(mid + 1);
        let median = child_node.entries.pop().unwrap();
        
        // Split children if internal node
        if !is_leaf {
            let child_node = self.get_node_mut(child_id);
            new_child.children = child_node.children.split_off(mid + 1);
        }
        
        // Add new child to nodes
        self.nodes.push(new_child);
        
        // Update parent
        let parent = self.get_node_mut(parent_id);
        parent.entries.insert(child_idx, median);
        parent.children.insert(child_idx + 1, new_child_id);
        
        Ok(())
    }

    /// Search for an entry
    pub fn search(&self, name: &str) -> Option<u32> {
        self.search_recursive(self.root_id, name)
    }

    /// Recursive search
    fn search_recursive(&self, node_id: u32, name: &str) -> Option<u32> {
        let node = self.get_node(node_id);
        
        // Check if entry is in this node
        if let Some(inode) = node.search_local(name) {
            return Some(inode);
        }
        
        // If leaf, not found
        if node.is_leaf {
            return None;
        }
        
        // Descend to child
        let child_idx = node.find_child_index(name);
        let child_id = node.children[child_idx];
        self.search_recursive(child_id, name)
    }

    /// List all entries (in-order traversal)
    pub fn list_all(&self) -> Vec<BTreeEntry> {
        let mut result = Vec::new();
        self.traverse_inorder(self.root_id, &mut result);
        result
    }

    /// In-order traversal
    fn traverse_inorder(&self, node_id: u32, result: &mut Vec<BTreeEntry>) {
        let node = self.get_node(node_id);
        
        if node.is_leaf {
            result.extend(node.entries.clone());
        } else {
            for i in 0..node.entries.len() {
                // Visit left child
                self.traverse_inorder(node.children[i], result);
                // Visit entry
                result.push(node.entries[i].clone());
            }
            // Visit rightmost child
            if let Some(&last_child) = node.children.last() {
                self.traverse_inorder(last_child, result);
            }
        }
    }

    /// Get node by ID
    fn get_node(&self, node_id: u32) -> &BTreeNode {
        &self.nodes[node_id as usize]
    }

    /// Get mutable node by ID
    fn get_node_mut(&mut self, node_id: u32) -> &mut BTreeNode {
        &mut self.nodes[node_id as usize]
    }

    /// Get statistics
    pub fn stats(&self) -> BTreeStats {
        BTreeStats {
            entry_count: self.entry_count,
            node_count: self.nodes.len(),
            height: self.calculate_height(self.root_id),
            order: ORDER,
        }
    }

    /// Calculate tree height
    fn calculate_height(&self, node_id: u32) -> usize {
        let node = self.get_node(node_id);
        if node.is_leaf {
            1
        } else {
            1 + self.calculate_height(node.children[0])
        }
    }
}

/// B-Tree statistics
#[derive(Debug, Clone)]
pub struct BTreeStats {
    pub entry_count: usize,
    pub node_count: usize,
    pub height: usize,
    pub order: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_btree_insert_search() {
        let mut tree = BTree::new();
        
        tree.insert("file1.txt".to_string(), 1).unwrap();
        tree.insert("file2.txt".to_string(), 2).unwrap();
        tree.insert("file3.txt".to_string(), 3).unwrap();
        
        assert_eq!(tree.search("file1.txt"), Some(1));
        assert_eq!(tree.search("file2.txt"), Some(2));
        assert_eq!(tree.search("file3.txt"), Some(3));
        assert_eq!(tree.search("notfound.txt"), None);
    }

    #[test]
    fn test_btree_large() {
        let mut tree = BTree::new();
        
        // Insert many entries
        for i in 0..1000 {
            let name = format!("file{:04}.txt", i);
            tree.insert(name, i).unwrap();
        }
        
        // Verify all can be found
        for i in 0..1000 {
            let name = format!("file{:04}.txt", i);
            assert_eq!(tree.search(&name), Some(i));
        }
        
        let stats = tree.stats();
        assert_eq!(stats.entry_count, 1000);
        assert!(stats.height < 10); // Should be shallow
    }

    #[test]
    fn test_btree_list_all() {
        let mut tree = BTree::new();
        
        tree.insert("charlie".to_string(), 3).unwrap();
        tree.insert("alice".to_string(), 1).unwrap();
        tree.insert("bob".to_string(), 2).unwrap();
        
        let entries = tree.list_all();
        assert_eq!(entries.len(), 3);
        
        // Should be sorted
        assert_eq!(entries[0].name, "alice");
        assert_eq!(entries[1].name, "bob");
        assert_eq!(entries[2].name, "charlie");
    }

    #[test]
    fn test_btree_stats() {
        let mut tree = BTree::new();
        
        for i in 0..100 {
            tree.insert(format!("file{}", i), i).unwrap();
        }
        
        let stats = tree.stats();
        assert_eq!(stats.entry_count, 100);
        assert!(stats.node_count > 0);
        assert!(stats.height > 0);
        assert_eq!(stats.order, ORDER);
    }
}
