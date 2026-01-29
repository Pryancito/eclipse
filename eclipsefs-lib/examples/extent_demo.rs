//! Example demonstrating extent-based allocation and block management
//!
//! This example shows how the new ext4/XFS-inspired features work in EclipseFS v0.3.0

use eclipsefs_lib::{
    BlockAllocator, Extent, ExtentTree, BLOCK_SIZE
};

fn main() {
    println!("=== EclipseFS v0.3.0 - Extent-Based Allocation Demo ===\n");
    
    // Demo 1: Extent Trees
    demo_extent_trees();
    
    // Demo 2: Block Allocator
    demo_block_allocator();
    
    // Demo 3: Delayed Allocation
    demo_delayed_allocation();
    
    // Demo 4: Fragmentation Analysis
    demo_fragmentation_analysis();
}

fn demo_extent_trees() {
    println!("--- Demo 1: Extent Trees ---");
    
    let mut tree = ExtentTree::new();
    
    // Simulate allocating space for a large file
    println!("Allocating extents for a large file...");
    tree.add_extent(Extent::new(0, 1000, 100)).unwrap();
    tree.add_extent(Extent::new(100, 1100, 50)).unwrap();
    tree.add_extent(Extent::new(150, 1150, 25)).unwrap();
    
    // The extents should automatically merge
    println!("  Extents added: 3");
    println!("  Extents after merge: {}", tree.extent_count());
    println!("  Total blocks: {}", tree.total_blocks());
    println!("  Is contiguous: {}", tree.is_contiguous());
    
    // Lookup logical to physical mapping
    let logical_block = 75;
    if let Some(physical) = tree.logical_to_physical(logical_block) {
        println!("  Logical block {} -> Physical block {}", logical_block, physical);
    }
    
    let stats = tree.get_stats();
    println!("  Fragmentation score: {:.2}%", stats.fragmentation_score);
    println!();
}

fn demo_block_allocator() {
    println!("--- Demo 2: Block Allocator with Allocation Groups ---");
    
    // Create allocator with 10,000 blocks divided into 10 groups
    let mut allocator = BlockAllocator::new(10000, 1000);
    
    println!("Created allocator:");
    let stats = allocator.get_stats();
    println!("  Total blocks: {}", stats.total_blocks);
    println!("  Block size: {} bytes", BLOCK_SIZE);
    println!("  Allocation groups: {}", stats.total_groups);
    println!("  Total capacity: {} MB", (stats.total_blocks * BLOCK_SIZE) / (1024 * 1024));
    
    // Allocate some extents
    println!("\nAllocating extents...");
    let extent1 = allocator.allocate_extent(100).unwrap();
    println!("  Allocated extent 1: {} blocks starting at block {}", 
             extent1.length, extent1.physical_block);
    
    let extent2 = allocator.allocate_extent(250).unwrap();
    println!("  Allocated extent 2: {} blocks starting at block {}", 
             extent2.length, extent2.physical_block);
    
    let extent3 = allocator.allocate_extent(150).unwrap();
    println!("  Allocated extent 3: {} blocks starting at block {}", 
             extent3.length, extent3.physical_block);
    
    let stats = allocator.get_stats();
    println!("\nAllocator state:");
    println!("  Used blocks: {}", stats.used_blocks);
    println!("  Free blocks: {}", stats.free_blocks);
    println!("  Average group free: {:.1}%", stats.average_free_percentage);
    
    // Free an extent
    allocator.free_extent(&extent2).unwrap();
    println!("\nFreed extent 2");
    let stats = allocator.get_stats();
    println!("  Free blocks after free: {}", stats.free_blocks);
    println!();
}

fn demo_delayed_allocation() {
    println!("--- Demo 3: Delayed Allocation (ext4 delalloc) ---");
    
    let mut allocator = BlockAllocator::new(10000, 1000);
    
    println!("Registering delayed allocations (simulating buffered writes)...");
    allocator.delay_allocation(0, 50).unwrap();
    println!("  Delayed: logical block 0, 50 blocks");
    
    allocator.delay_allocation(50, 100).unwrap();
    println!("  Delayed: logical block 50, 100 blocks");
    
    allocator.delay_allocation(150, 75).unwrap();
    println!("  Delayed: logical block 150, 75 blocks");
    
    let stats = allocator.get_stats();
    println!("\nPending allocations: {}", stats.delayed_allocations);
    
    // Flush delayed allocations (happens on fsync or periodic flush)
    println!("\nFlushing delayed allocations...");
    let extents = allocator.flush_delayed_allocations().unwrap();
    
    println!("  Allocated {} extents:", extents.len());
    for (i, extent) in extents.iter().enumerate() {
        println!("    Extent {}: logical={}, physical={}, length={}", 
                 i + 1, extent.logical_block, extent.physical_block, extent.length);
    }
    
    let stats = allocator.get_stats();
    println!("\nAfter flush:");
    println!("  Pending allocations: {}", stats.delayed_allocations);
    println!("  Total allocated: {} blocks", stats.used_blocks);
    println!();
}

fn demo_fragmentation_analysis() {
    println!("--- Demo 4: Fragmentation Analysis ---");
    
    // Create two files with different allocation patterns
    
    // File 1: Well-allocated (contiguous)
    let mut tree1 = ExtentTree::new();
    tree1.add_extent(Extent::new(0, 5000, 1000)).unwrap();
    
    println!("File 1 (well-allocated):");
    let stats1 = tree1.get_stats();
    println!("  Extents: {}", stats1.total_extents);
    println!("  Total blocks: {}", stats1.total_blocks);
    println!("  Average extent size: {:.1} blocks", stats1.average_extent_size);
    println!("  Fragmentation: {:.2}%", stats1.fragmentation_score);
    println!("  Contiguous: {}", stats1.is_contiguous);
    
    // File 2: Fragmented
    let mut tree2 = ExtentTree::new();
    tree2.add_extent(Extent::new(0, 1000, 100)).unwrap();
    tree2.add_extent(Extent::new(100, 3000, 50)).unwrap();
    tree2.add_extent(Extent::new(150, 5000, 75)).unwrap();
    tree2.add_extent(Extent::new(225, 8000, 25)).unwrap();
    tree2.add_extent(Extent::new(250, 10000, 100)).unwrap();
    
    println!("\nFile 2 (fragmented):");
    let stats2 = tree2.get_stats();
    println!("  Extents: {}", stats2.total_extents);
    println!("  Total blocks: {}", stats2.total_blocks);
    println!("  Average extent size: {:.1} blocks", stats2.average_extent_size);
    println!("  Fragmentation: {:.2}%", stats2.fragmentation_score);
    println!("  Contiguous: {}", stats2.is_contiguous);
    
    println!("\n=== End of Demo ===");
}
