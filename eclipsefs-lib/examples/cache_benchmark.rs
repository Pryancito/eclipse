//! Cache Performance Benchmark for EclipseFS
//! Tests the effectiveness of the node caching system

use eclipsefs_lib::{EclipseFSNode, EclipseFSReader, EclipseFSWriter, NodeKind};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== EclipseFS Cache Performance Benchmark ===\n");

    // Create a test filesystem with many files in a directory
    let test_image = "/tmp/cache_test.eclipsefs";
    create_test_filesystem(test_image, 500)?;

    println!("Test 1: First read of directory (cold cache)...");
    let cold_time = benchmark_directory_read(test_image, false)?;
    println!("✅ Cold read: {:.2}ms ({:.2}µs per file)\n", 
             cold_time * 1000.0, cold_time * 1000000.0 / 500.0);

    println!("Test 2: Second read of directory (warm cache)...");
    let warm_time = benchmark_directory_read(test_image, true)?;
    println!("✅ Warm read: {:.2}ms ({:.2}µs per file)\n", 
             warm_time * 1000.0, warm_time * 1000000.0 / 500.0);

    let speedup = cold_time / warm_time;
    println!("=== Cache Performance ===");
    println!("Cold read: {:.2}ms", cold_time * 1000.0);
    println!("Warm read: {:.2}ms", warm_time * 1000.0);
    println!("Speedup: {:.1}x faster with cache", speedup);

    if speedup > 2.0 {
        println!("\n✅ EXCELLENT: Cache provides {:.1}x speedup!", speedup);
    } else if speedup > 1.5 {
        println!("\n✅ GOOD: Cache provides {:.1}x speedup", speedup);
    } else {
        println!("\n⚠️  WARNING: Cache speedup is only {:.1}x", speedup);
    }

    Ok(())
}

fn create_test_filesystem(path: &str, num_files: usize) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut writer = EclipseFSWriter::new(file);
    
    writer.create_root()?;
    
    // Create a directory with many files
    let mut dir_node = EclipseFSNode::new_dir();
    
    for i in 0..num_files {
        let file_name = format!("file_{:04}.txt", i);
        let mut file_node = EclipseFSNode::new_file();
        let data = format!("Content of file {}", i);
        file_node.data = data.into_bytes();
        file_node.size = file_node.data.len() as u64;
        
        let file_inode = writer.allocate_inode();
        writer.add_node(file_inode, file_node)?;
        dir_node.add_child(&file_name, file_inode)?;
    }
    
    let dir_inode = writer.allocate_inode();
    writer.add_node(dir_inode, dir_node)?;
    
    writer.get_root()?.add_child("testdir", dir_inode)?;
    writer.write_image()?;
    
    Ok(())
}

fn benchmark_directory_read(path: &str, reuse_reader: bool) -> Result<f64, Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new(path)?;
    
    // Navigate to testdir
    let root = reader.get_root()?;
    let dir_inode = root.get_child_inode("testdir")
        .ok_or("testdir not found")?;
    
    // If reuse_reader is true, read once to warm the cache
    if reuse_reader {
        let dir_node = reader.read_node(dir_inode)?;
        for child_inode in dir_node.get_children().values() {
            let _ = reader.read_node(*child_inode);
        }
    }
    
    // Now benchmark the actual read
    let start = Instant::now();
    
    let dir_node = reader.read_node(dir_inode)?;
    
    // Simulate what FUSE readdir does - read metadata of all children
    for child_inode in dir_node.get_children().values() {
        let _child_node = reader.read_node(*child_inode)?;
        // In real FUSE, we'd also get the file type, size, etc.
    }
    
    let elapsed = start.elapsed();
    
    // Print cache stats
    let stats = reader.get_cache_stats();
    println!("   Cache: {} nodes cached (capacity: {})", 
             stats.cached_nodes, stats.cache_capacity);
    
    Ok(elapsed.as_secs_f64())
}
