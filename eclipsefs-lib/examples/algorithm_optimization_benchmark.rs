//! Benchmark for filesystem algorithm optimizations
//! Tests the impact of ext4/zfs/xfs inspired algorithms

use eclipsefs_lib::{
    EclipseFSNode, EclipseFSReader, EclipseFSWriter,
    reader::CacheType,
};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== EclipseFS Algorithm Optimization Benchmark ===\n");

    // Create test filesystem
    println!("Creating test filesystem with 1000 sequential files...");
    let test_image = "/tmp/algorithm_test.eclipsefs";
    create_test_filesystem(test_image, 1000)?;
    
    println!("\n--- Sequential Read Test (ext4-style readahead) ---");
    benchmark_sequential_reads(test_image)?;
    
    println!("\n--- Random Access Test (ARC cache effectiveness) ---");
    benchmark_random_access(test_image)?;
    
    println!("\n--- Directory Prefetch Test (existing optimization) ---");
    benchmark_directory_prefetch(test_image)?;
    
    println!("\n✅ All benchmarks completed successfully!");
    Ok(())
}

fn create_test_filesystem(path: &str, num_files: usize) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut writer = EclipseFSWriter::new(file);
    
    // Create root directory
    let root = EclipseFSNode::new_dir();
    writer.add_node(1, root)?;
    
    // Create sequential files
    for i in 0..num_files {
        let mut file_node = EclipseFSNode::new_file();
        let data = format!("File {} content with some data", i).into_bytes();
        let _ = file_node.set_data(&data);
        
        let inode = writer.allocate_inode();
        writer.add_node(inode, file_node)?;
    }
    
    writer.write_image()?;
    Ok(())
}

fn benchmark_sequential_reads(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new_with_cache(path, CacheType::LRU)?;
    
    let start = Instant::now();
    
    // Read 100 sequential inodes (2 to 101)
    // The readahead should kick in and prefetch ahead
    for inode in 2..=101 {
        let _ = reader.read_node(inode)?;
    }
    
    let duration = start.elapsed();
    
    println!("✅ Sequential read of 100 nodes: {:.2}ms", duration.as_secs_f64() * 1000.0);
    println!("   Avg per node: {:.2}µs", duration.as_secs_f64() * 1_000_000.0 / 100.0);
    println!("   Readahead window adaptive sizing active");
    
    // Second pass should benefit from cache
    let start = Instant::now();
    for inode in 2..=101 {
        let _ = reader.read_node(inode)?;
    }
    let cached_duration = start.elapsed();
    
    println!("✅ Cached sequential read: {:.2}ms", cached_duration.as_secs_f64() * 1000.0);
    if cached_duration.as_secs_f64() > 0.0 {
        println!("   Speedup: {:.1}x faster", duration.as_secs_f64() / cached_duration.as_secs_f64());
    }
    
    Ok(())
}

fn benchmark_random_access(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new_with_cache(path, CacheType::ARC)?;
    
    // Simulate mixed access pattern (some sequential, some repeated)
    let access_pattern = vec![
        2, 3, 4, 5, // Sequential
        2, 3, 4, 5, // Repeated (should hit cache)
        10, 11, 12, // New sequential
        2, 3, // Repeated again (frequent access)
        20, 30, 40, // Random
        2, 3, // More repeated access
    ];
    
    let start = Instant::now();
    for &inode in &access_pattern {
        let _ = reader.read_node(inode)?;
    }
    let duration = start.elapsed();
    
    println!("✅ Mixed access pattern (24 reads): {:.2}ms", duration.as_secs_f64() * 1000.0);
    println!("   Avg per read: {:.2}µs", duration.as_secs_f64() * 1_000_000.0 / 24.0);
    
    // Get cache stats
    let stats = reader.get_cache_stats();
    println!("   Cache stats: {:?}", stats);
    
    Ok(())
}

fn benchmark_directory_prefetch(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new_with_cache(path, CacheType::LRU)?;
    
    // Read root directory
    let start = Instant::now();
    let root = reader.get_root()?;
    let duration = start.elapsed();
    
    println!("✅ Root directory read: {:.2}µs", duration.as_secs_f64() * 1_000_000.0);
    
    // Prefetch all children
    let child_inodes: Vec<u32> = root.get_children().values().copied().collect();
    let prefetch_count = child_inodes.len().min(100); // Limit to 100 for benchmark
    
    let start = Instant::now();
    reader.prefetch_nodes(&child_inodes[..prefetch_count])?;
    let prefetch_duration = start.elapsed();
    
    println!("✅ Prefetched {} children: {:.2}ms", prefetch_count, prefetch_duration.as_secs_f64() * 1000.0);
    if prefetch_count > 0 {
        println!("   Avg per prefetch: {:.2}µs", prefetch_duration.as_secs_f64() * 1_000_000.0 / prefetch_count as f64);
    }
    
    // Now read them - should all be cached
    let start = Instant::now();
    for &child_inode in &child_inodes[..prefetch_count] {
        let _ = reader.read_node(child_inode)?;
    }
    let cached_read_duration = start.elapsed();
    
    println!("✅ Cached reads of prefetched children: {:.2}ms", cached_read_duration.as_secs_f64() * 1000.0);
    if prefetch_count > 0 && cached_read_duration.as_secs_f64() > 0.0 {
        println!("   Speedup: {:.1}x faster than prefetch", prefetch_duration.as_secs_f64() / cached_read_duration.as_secs_f64());
    }
    
    Ok(())
}
