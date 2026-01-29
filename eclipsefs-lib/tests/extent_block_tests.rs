//! Tests for extent-based allocation and block management

use eclipsefs_lib::{
    Extent, ExtentTree, BlockAllocator, BLOCK_SIZE,
};

#[test]
fn test_extent_based_allocation() {
    // Test basic extent functionality
    let mut tree = ExtentTree::new();
    
    // Add an extent
    let extent = Extent::new(0, 1000, 100);
    tree.add_extent(extent).unwrap();
    
    assert_eq!(tree.extent_count(), 1);
    assert_eq!(tree.total_blocks(), 100);
    
    // Lookup logical to physical mapping
    assert_eq!(tree.logical_to_physical(0), Some(1000));
    assert_eq!(tree.logical_to_physical(50), Some(1050));
    assert_eq!(tree.logical_to_physical(99), Some(1099));
    assert_eq!(tree.logical_to_physical(100), None);
}

#[test]
fn test_extent_merging() {
    // Test that contiguous extents are merged
    let mut tree = ExtentTree::new();
    
    tree.add_extent(Extent::new(0, 1000, 10)).unwrap();
    tree.add_extent(Extent::new(10, 1010, 10)).unwrap();
    
    // Should merge into single extent
    assert_eq!(tree.extent_count(), 1);
    assert!(tree.is_contiguous());
    
    // Add non-contiguous extent
    tree.add_extent(Extent::new(30, 2000, 10)).unwrap();
    
    // Now should have 2 extents
    assert_eq!(tree.extent_count(), 2);
    assert!(!tree.is_contiguous());
}

#[test]
fn test_extent_fragmentation() {
    let mut tree = ExtentTree::new();
    
    // Single extent = low fragmentation
    tree.add_extent(Extent::new(0, 1000, 100)).unwrap();
    let stats1 = tree.get_stats();
    assert!(stats1.fragmentation_score < 10.0);
    assert!(stats1.is_contiguous);
    
    // Multiple non-contiguous extents = higher fragmentation
    tree.add_extent(Extent::new(200, 3000, 50)).unwrap();
    tree.add_extent(Extent::new(300, 5000, 30)).unwrap();
    
    let stats2 = tree.get_stats();
    assert!(stats2.fragmentation_score > 0.0);
    assert!(!stats2.is_contiguous);
    assert_eq!(stats2.total_extents, 3);
}

#[test]
fn test_block_allocator_basic() {
    // Create allocator with 1000 blocks, 100 blocks per group
    let mut allocator = BlockAllocator::new(1000, 100);
    
    let stats = allocator.get_stats();
    assert_eq!(stats.total_blocks, 1000);
    assert_eq!(stats.free_blocks, 1000);
    assert_eq!(stats.total_groups, 10);
}

#[test]
fn test_block_allocator_extent_allocation() {
    let mut allocator = BlockAllocator::new(1000, 100);
    
    // Allocate an extent
    let extent = allocator.allocate_extent(50).unwrap();
    assert_eq!(extent.length, 50);
    
    let stats = allocator.get_stats();
    assert_eq!(stats.free_blocks, 950);
    assert_eq!(stats.used_blocks, 50);
}

#[test]
fn test_block_allocator_free_extent() {
    let mut allocator = BlockAllocator::new(1000, 100);
    
    // Allocate and free an extent
    let extent = allocator.allocate_extent(50).unwrap();
    allocator.free_extent(&extent).unwrap();
    
    let stats = allocator.get_stats();
    assert_eq!(stats.free_blocks, 1000);
    assert_eq!(stats.used_blocks, 0);
}

#[test]
fn test_delayed_allocation() {
    let mut allocator = BlockAllocator::new(1000, 100);
    
    // Register delayed allocations
    allocator.delay_allocation(0, 10).unwrap();
    allocator.delay_allocation(10, 20).unwrap();
    allocator.delay_allocation(30, 15).unwrap();
    
    let stats = allocator.get_stats();
    assert_eq!(stats.delayed_allocations, 3);
    
    // Flush delayed allocations
    let extents = allocator.flush_delayed_allocations().unwrap();
    assert_eq!(extents.len(), 3);
    
    // Verify extents have correct logical blocks and lengths
    // Note: order may vary
    let mut found_10 = false;
    let mut found_20 = false;
    let mut found_15 = false;
    
    for extent in &extents {
        if extent.length == 10 && extent.logical_block == 0 {
            found_10 = true;
        } else if extent.length == 20 && extent.logical_block == 10 {
            found_20 = true;
        } else if extent.length == 15 && extent.logical_block == 30 {
            found_15 = true;
        }
    }
    
    assert!(found_10, "Should have extent with length 10");
    assert!(found_20, "Should have extent with length 20");
    assert!(found_15, "Should have extent with length 15");
    
    let stats2 = allocator.get_stats();
    assert_eq!(stats2.delayed_allocations, 0);
    assert_eq!(stats2.free_blocks, 1000 - 10 - 20 - 15);
}

#[test]
fn test_extent_flags() {
    let mut extent = Extent::new(0, 1000, 10);
    
    // Test unwritten flag
    assert!(!extent.is_unwritten());
    extent.mark_unwritten();
    assert!(extent.is_unwritten());
    extent.mark_written();
    assert!(!extent.is_unwritten());
}

#[test]
fn test_block_size_constant() {
    // Verify block size is standard 4KB
    assert_eq!(BLOCK_SIZE, 4096);
}

#[test]
fn test_extent_end_calculations() {
    let extent = Extent::new(100, 1000, 50);
    
    assert_eq!(extent.logical_end(), 150);
    assert_eq!(extent.physical_end(), 1050);
}

#[test]
fn test_multiple_extent_allocation() {
    let mut allocator = BlockAllocator::new(10000, 1000);
    
    // Allocate multiple extents
    let extent1 = allocator.allocate_extent(100).unwrap();
    let extent2 = allocator.allocate_extent(200).unwrap();
    let extent3 = allocator.allocate_extent(150).unwrap();
    
    assert_eq!(extent1.length, 100);
    assert_eq!(extent2.length, 200);
    assert_eq!(extent3.length, 150);
    
    let stats = allocator.get_stats();
    assert_eq!(stats.used_blocks, 450);
    assert_eq!(stats.free_blocks, 9550);
}

#[test]
fn test_allocation_group_free_percentage() {
    use eclipsefs_lib::AllocationGroup;
    
    let mut group = AllocationGroup::new(0, 0, 1000);
    assert_eq!(group.free_percentage(), 100.0);
    
    // Allocate some blocks
    group.allocate_contiguous(500).unwrap();
    assert_eq!(group.free_percentage(), 50.0);
    
    group.allocate_contiguous(250).unwrap();
    assert_eq!(group.free_percentage(), 25.0);
}
