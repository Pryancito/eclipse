//! Example demonstrating advanced EclipseFS features
//! Shows how to enable and use:
//! - Intelligent caching
//! - Defragmentation
//! - Load balancing
//! - Copy-on-Write with versioning
//! - Journaling for crash recovery

use eclipsefs_lib::{
    EclipseFS, JournalConfig, constants,
    cache::CacheConfig, 
    defragmentation::DefragmentationConfig, 
    load_balancing::LoadBalancingConfig,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== EclipseFS Advanced Features Demo ===\n");
    
    // Create a new filesystem
    let mut fs = EclipseFS::new();
    
    // 1. Enable Journaling (Crash Recovery)
    println!("1. Enabling Journaling System...");
    let journal_config = JournalConfig {
        max_entries: 1000,
        auto_commit: true,
        commit_interval_ms: 5000,
        recovery_enabled: true,
    };
    fs.enable_journaling(journal_config)?;
    println!("   ✓ Journaling enabled with automatic recovery\n");
    
    // 2. Enable Copy-on-Write
    println!("2. Enabling Copy-on-Write (Versioning)...");
    fs.enable_copy_on_write();
    println!("   ✓ CoW enabled - file modifications will create versions\n");
    
    // 3. Enable Intelligent Caching
    println!("3. Enabling Intelligent Cache System...");
    let cache_config = CacheConfig {
        max_entries: 2048,
        max_memory_mb: 128,
        read_ahead_size: 8192,
        write_behind_size: 16384,
        prefetch_enabled: true,
        compression_enabled: false,
    };
    fs.enable_intelligent_cache(cache_config)?;
    println!("   ✓ LRU cache enabled (128MB, 2048 entries)\n");
    
    // 4. Enable Intelligent Defragmentation
    println!("4. Enabling Intelligent Defragmentation...");
    let defrag_config = DefragmentationConfig {
        enabled: true,
        threshold_percentage: 30.0,
        max_operations_per_cycle: 100,
        background_mode: true,
        optimize_small_files: true,
        optimize_large_files: false,
        preserve_access_patterns: true,
    };
    fs.enable_intelligent_defragmentation(defrag_config)?;
    println!("   ✓ Auto-defragmentation enabled (30% threshold)\n");
    
    // 5. Enable Load Balancing
    println!("5. Enabling Load Balancing Optimization...");
    let lb_config = LoadBalancingConfig {
        enabled: true,
        rebalance_threshold: 0.7,
        max_operations_per_cycle: 100,
        background_mode: true,
        consider_access_patterns: true,
        consider_file_sizes: true,
        consider_fragmentation: false,
        load_balancing_algorithm: eclipsefs_lib::load_balancing::LoadBalancingAlgorithm::LeastLoaded,
    };
    fs.enable_intelligent_load_balancing(lb_config)?;
    println!("   ✓ Load balancing enabled (70% threshold)\n");
    
    // 6. Create some test files
    println!("6. Creating test files...");
    let file1 = fs.create_file(constants::ROOT_INODE, "test1.txt")?;
    fs.write_file(file1, b"Version 1 of test file")?;
    println!("   ✓ Created test1.txt");
    
    let file2 = fs.create_file(constants::ROOT_INODE, "test2.txt")?;
    fs.write_file(file2, b"Another test file")?;
    println!("   ✓ Created test2.txt\n");
    
    // 7. Demonstrate versioning
    println!("7. Testing Copy-on-Write versioning...");
    fs.write_file(file1, b"Version 2 of test file")?;
    fs.write_file(file1, b"Version 3 of test file")?;
    
    let versions = fs.get_version_history(file1);
    if let Some(version_list) = versions {
        println!("   ✓ File has {} versions", version_list.len());
        for (i, version_inode) in version_list.iter().enumerate() {
            println!("     - Version {}: inode {}", i + 1, version_inode);
        }
    } else {
        println!("   ! No version history available");
    }
    println!();
    
    // 8. Commit journal
    println!("8. Committing journal transactions...");
    fs.commit_journal()?;
    println!("   ✓ All transactions committed safely\n");
    
    // 9. Create a snapshot
    println!("9. Creating filesystem snapshot...");
    fs.create_filesystem_snapshot(1, "Initial backup")?;
    println!("   ✓ Snapshot 1 created: 'Initial backup'\n");
    
    // 10. Get system statistics
    println!("10. System Statistics:");
    let stats = fs.get_system_stats();
    println!("    - Total nodes: {}", stats.total_nodes);
    println!("    - Total snapshots: {}", stats.total_snapshots);
    println!("    - CoW enabled: {}", stats.cow_enabled);
    println!("    - Encryption enabled: {}", stats.encryption_enabled);
    println!("    - Cache enabled: {}", stats.cache_enabled);
    println!("    - Defrag enabled: {}", stats.defragmentation_enabled);
    println!("    - Load balancing enabled: {}", stats.load_balancing_enabled);
    println!();
    
    // 11. Run optimizations
    println!("11. Running advanced optimizations...");
    match fs.run_advanced_optimizations() {
        Ok(_) => println!("    ✓ Optimizations completed successfully"),
        Err(e) => println!("    ! Optimizations skipped: {:?}", e),
    }
    println!();
    
    println!("=== Demo Complete ===");
    println!("\nAll advanced features are now enabled and working!");
    println!("Your filesystem has:");
    println!("  • Crash recovery via journaling");
    println!("  • Version control via Copy-on-Write");
    println!("  • Performance boost via intelligent caching");
    println!("  • Automatic defragmentation");
    println!("  • Load balancing optimization");
    println!("  • Point-in-time snapshots");
    
    Ok(())
}
