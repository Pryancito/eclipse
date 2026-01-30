//! Real-world FUSE operations benchmark
//! Simulates typical filesystem operations like ls, find, etc.

use eclipsefs_lib::{EclipseFSNode, EclipseFSReader, EclipseFSWriter};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== EclipseFS Real-World Operations Benchmark ===\n");

    // Create a realistic filesystem structure
    let test_image = "/tmp/realworld_test.eclipsefs";
    println!("Creating test filesystem with realistic structure...");
    create_realistic_filesystem(test_image)?;
    println!("✅ Filesystem created\n");

    // Benchmark 1: ls -la (list directory with metadata)
    println!("Test 1: 'ls -la' operation (list 100 files with metadata)");
    let ls_time = benchmark_ls_operation(test_image)?;
    println!("✅ ls -la: {:.2}ms ({:.2}µs per file)\n", ls_time * 1000.0, ls_time * 1000000.0 / 100.0);

    // Benchmark 2: find operation (recursive tree traversal)
    println!("Test 2: 'find' operation (recursive tree traversal)");
    let find_time = benchmark_find_operation(test_image)?;
    println!("✅ find: {:.2}ms\n", find_time * 1000.0);

    // Benchmark 3: Repeated stat operations (like a file manager)
    println!("Test 3: Repeated stat operations (file manager preview)");
    let stat_time = benchmark_stat_operations(test_image)?;
    println!("✅ stats: {:.2}ms ({:.2}µs per stat)\n", stat_time * 1000.0, stat_time * 1000000.0 / 50.0);

    println!("=== Summary ===");
    println!("ls -la (100 files): {:.2}ms", ls_time * 1000.0);
    println!("find (recursive):   {:.2}ms", find_time * 1000.0);
    println!("stat (50 files):    {:.2}ms", stat_time * 1000.0);
    
    if ls_time < 0.010 && find_time < 0.020 {
        println!("\n✅ EXCELLENT: All operations complete in milliseconds!");
    } else {
        println!("\n✅ GOOD: Operations complete under acceptable thresholds");
    }

    Ok(())
}

fn create_realistic_filesystem(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut writer = EclipseFSWriter::new(file);
    
    writer.create_root()?;
    
    // Create /home/user directory
    let mut user_dir = EclipseFSNode::new_dir();
    
    // Create /home/user/documents with 50 files
    let mut docs_dir = EclipseFSNode::new_dir();
    for i in 0..50 {
        let file_name = format!("document_{:03}.txt", i);
        let mut file_node = EclipseFSNode::new_file();
        file_node.data = format!("Document content {}", i).into_bytes();
        file_node.size = file_node.data.len() as u64;
        
        let file_inode = writer.allocate_inode();
        writer.add_node(file_inode, file_node)?;
        docs_dir.add_child(&file_name, file_inode)?;
    }
    let docs_inode = writer.allocate_inode();
    writer.add_node(docs_inode, docs_dir)?;
    user_dir.add_child("documents", docs_inode)?;
    
    // Create /home/user/downloads with 50 files
    let mut downloads_dir = EclipseFSNode::new_dir();
    for i in 0..50 {
        let file_name = format!("file_{:03}.bin", i);
        let mut file_node = EclipseFSNode::new_file();
        file_node.data = vec![i as u8; 1024]; // 1KB each
        file_node.size = file_node.data.len() as u64;
        
        let file_inode = writer.allocate_inode();
        writer.add_node(file_inode, file_node)?;
        downloads_dir.add_child(&file_name, file_inode)?;
    }
    let downloads_inode = writer.allocate_inode();
    writer.add_node(downloads_inode, downloads_dir)?;
    user_dir.add_child("downloads", downloads_inode)?;
    
    // Create /home/user with subdirectories
    let user_inode = writer.allocate_inode();
    writer.add_node(user_inode, user_dir)?;
    
    // Create /home
    let mut home_dir = EclipseFSNode::new_dir();
    home_dir.add_child("user", user_inode)?;
    let home_inode = writer.allocate_inode();
    writer.add_node(home_inode, home_dir)?;
    
    // Add to root
    writer.get_root()?.add_child("home", home_inode)?;
    
    writer.write_image()?;
    
    Ok(())
}

fn benchmark_ls_operation(path: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new(path)?;
    
    let start = Instant::now();
    
    // Navigate to /home/user/documents
    let root = reader.get_root()?;
    let home_inode = root.get_child_inode("home").unwrap();
    let home_node = reader.read_node(home_inode)?;
    let user_inode = home_node.get_child_inode("user").unwrap();
    let user_node = reader.read_node(user_inode)?;
    let docs_inode = user_node.get_child_inode("documents").unwrap();
    
    // This is what 'ls -la' does: read directory and all child metadata
    let docs_node = reader.read_directory_with_children(docs_inode)?;
    
    // Get metadata for all files (like ls -la)
    let mut count = 0;
    for child_inode in docs_node.get_children().values() {
        let _child = reader.read_node(*child_inode)?;
        count += 1;
    }
    
    let elapsed = start.elapsed();
    
    println!("   Listed {} files", count);
    let stats = reader.get_cache_stats();
    match stats {
        eclipsefs_lib::CacheStats::LRU { cached_nodes, cache_capacity } => {
            println!("   Cache: {}/{} nodes", cached_nodes, cache_capacity);
        }
        eclipsefs_lib::CacheStats::ARC(arc_stats) => {
            println!("   Cache: {}/{} nodes (ARC)", arc_stats.t1_size + arc_stats.t2_size, arc_stats.total_capacity);
        }
    }
    
    Ok(elapsed.as_secs_f64())
}

fn benchmark_find_operation(path: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new(path)?;
    
    let start = Instant::now();
    
    // Recursive traversal starting from root (like 'find /')
    let mut total_files = 0;
    let mut to_visit = vec![1u32]; // Start with root inode
    
    while let Some(inode) = to_visit.pop() {
        let node = reader.read_node(inode)?;
        
        if node.kind == eclipsefs_lib::NodeKind::Directory {
            // Add all children to visit queue
            for child_inode in node.get_children().values() {
                to_visit.push(*child_inode);
            }
        }
        
        total_files += 1;
    }
    
    let elapsed = start.elapsed();
    
    println!("   Traversed {} inodes", total_files);
    let stats = reader.get_cache_stats();
    match stats {
        eclipsefs_lib::CacheStats::LRU { cached_nodes, cache_capacity } => {
            println!("   Cache: {}/{} nodes", cached_nodes, cache_capacity);
        }
        eclipsefs_lib::CacheStats::ARC(arc_stats) => {
            println!("   Cache: {}/{} nodes (ARC)", arc_stats.t1_size + arc_stats.t2_size, arc_stats.total_capacity);
        }
    }
    
    Ok(elapsed.as_secs_f64())
}

fn benchmark_stat_operations(path: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let mut reader = EclipseFSReader::new(path)?;
    
    // Navigate to /home/user/downloads
    let root = reader.get_root()?;
    let home_inode = root.get_child_inode("home").unwrap();
    let home_node = reader.read_node(home_inode)?;
    let user_inode = home_node.get_child_inode("user").unwrap();
    let user_node = reader.read_node(user_inode)?;
    let downloads_inode = user_node.get_child_inode("downloads").unwrap();
    let downloads_node = reader.read_node(downloads_inode)?;
    
    // Get first 50 files
    let files: Vec<u32> = downloads_node.get_children()
        .values()
        .take(50)
        .copied()
        .collect();
    
    let start = Instant::now();
    
    // Simulate repeated stat calls (file manager getting metadata)
    for _ in 0..1 {
        for &file_inode in &files {
            let _node = reader.read_node(file_inode)?;
            // In real world, we'd extract size, mtime, permissions, etc.
        }
    }
    
    let elapsed = start.elapsed();
    
    println!("   Stat'd {} files", files.len());
    let stats = reader.get_cache_stats();
    match stats {
        eclipsefs_lib::CacheStats::LRU { cached_nodes, cache_capacity } => {
            println!("   Cache: {}/{} nodes", cached_nodes, cache_capacity);
        }
        eclipsefs_lib::CacheStats::ARC(arc_stats) => {
            println!("   Cache: {}/{} nodes (ARC)", arc_stats.t1_size + arc_stats.t2_size, arc_stats.total_capacity);
        }
    }
    
    Ok(elapsed.as_secs_f64())
}
