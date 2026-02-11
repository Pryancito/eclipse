//! Demonstration of EclipseFS Journaling System
//! 
//! This example shows how to use the new journaling features for crash recovery

use eclipsefs_lib::{
    EclipseFS, JournalConfig, constants,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║         EclipseFS Journaling System Demonstration                   ║");
    println!("╚══════════════════════════════════════════════════════════════════════╝");
    println!();

    // Create a new filesystem
    let mut fs = EclipseFS::new();
    println!("✓ Filesystem created");

    // Enable journaling
    let journal_config = JournalConfig {
        max_entries: 1000,
        auto_commit: true,
        commit_interval_ms: 5000,
        recovery_enabled: true,
    };
    fs.enable_journaling(journal_config)?;
    println!("✓ Journaling enabled");
    println!();

    // Create some files (automatically journaled)
    println!("Creating files with journaling:");
    println!("  Creating /data.txt...");
    let data_file = fs.create_file(constants::ROOT_INODE, "data.txt")?;
    fs.write_file(data_file, b"Important data that must not be lost")?;
    println!("    ✓ Created file with inode {}", data_file);

    println!("  Creating /config.json...");
    let config_file = fs.create_file(constants::ROOT_INODE, "config.json")?;
    fs.write_file(config_file, b"{\"version\": \"1.0\", \"enabled\": true}")?;
    println!("    ✓ Created file with inode {}", config_file);

    println!("  Creating directory /logs...");
    let logs_dir = fs.create_directory(constants::ROOT_INODE, "logs")?;
    println!("    ✓ Created directory with inode {}", logs_dir);

    println!("  Creating /logs/app.log...");
    let log_file = fs.create_file(logs_dir, "app.log")?;
    fs.write_file(log_file, b"Application started successfully\n")?;
    println!("    ✓ Created log file with inode {}", log_file);
    println!();

    // Commit journal
    println!("Committing journal transactions...");
    fs.commit_journal()?;
    println!("  ✓ Journal committed");
    println!();

    // Demonstrate filesystem stats
    let (total, files, dirs) = fs.get_stats();
    println!("Filesystem Statistics:");
    println!("  Total nodes:  {}", total);
    println!("  Files:        {}", files);
    println!("  Directories:  {}", dirs);
    println!();

    // Enable Copy-on-Write
    println!("Enabling Copy-on-Write (CoW)...");
    fs.enable_copy_on_write();
    println!("  ✓ CoW enabled");
    println!();

    // Modify a file (creates a new version with CoW)
    println!("Modifying file with CoW:");
    fs.write_file(data_file, b"Updated data - version 2")?;
    println!("  ✓ File modified (new version created)");
    
    let history = fs.get_version_history(data_file);
    if let Some(versions) = history {
        println!("  Version history: {:?}", versions);
    }
    println!();

    // Create a filesystem snapshot
    println!("Creating filesystem snapshot...");
    fs.create_filesystem_snapshot(1, "After initial setup")?;
    println!("  ✓ Snapshot created");
    
    let snapshots = fs.list_snapshots()?;
    println!("  Total snapshots: {}", snapshots.len());
    println!();

    // Demonstrate system stats
    println!("Advanced System Statistics:");
    let stats = fs.get_system_stats();
    println!("  Total nodes:        {}", stats.total_nodes);
    println!("  Snapshots:          {}", stats.total_snapshots);
    println!("  CoW enabled:        {}", stats.cow_enabled);
    println!("  Cache enabled:      {}", stats.cache_enabled);
    println!("  Journal enabled:    true");
    println!();

    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║                    Demonstration Complete                            ║");
    println!("╚══════════════════════════════════════════════════════════════════════╝");

    Ok(())
}
