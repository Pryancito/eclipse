//! Performance benchmark for EclipseFS
//! Tests reading and writing large files to measure improvements

use eclipsefs_lib::{constants, EclipseFSNode, EclipseFSReader, EclipseFSResult, EclipseFSWriter};
use std::fs::File;
use std::time::Instant;

fn main() -> EclipseFSResult<()> {
    println!("=== EclipseFS Performance Benchmark ===\n");

    let test_file = "benchmark_eclipsefs.img";

    // Test 1: Write a 10MB file
    println!("Test 1: Creating filesystem with 10MB file...");
    let write_start = Instant::now();
    {
        let file = File::create(test_file)?;
        let mut writer = EclipseFSWriter::new(file);

        // Create root
        writer.create_root()?;

        // Create a 10MB file (10 * 1024 * 1024 bytes)
        let file_size = 10 * 1024 * 1024;
        let mut large_file = EclipseFSNode::new_file();
        
        // Create realistic data pattern (not all zeros)
        let mut data = Vec::with_capacity(file_size);
        for i in 0..file_size {
            data.push((i % 256) as u8);
        }
        
        large_file.set_data(&data)?;
        let large_inode = writer.create_node(large_file)?;

        // Add to root
        let root = writer.get_root()?;
        root.add_child("large_file.bin", large_inode)?;

        // Write the image
        writer.write_image()?;
    }
    let write_duration = write_start.elapsed();
    println!("✅ Write completed in {:.2?}", write_duration);
    let write_speed = 10.0 / write_duration.as_secs_f64().max(0.000001);
    println!("   Speed: {:.2} MB/s\n", write_speed);

    // Test 2: Read the 10MB file
    println!("Test 2: Reading 10MB file from filesystem...");
    let read_start = Instant::now();
    {
        let mut reader = EclipseFSReader::from_file(File::open(test_file)?)?;

        // Verify header
        let header = reader.get_header();
        println!("   Header version: 0x{:08X}", header.version);
        println!("   Total inodes: {}", header.total_inodes);

        // Read the large file
        let large_inode = reader.lookup(constants::ROOT_INODE as u64, "large_file.bin")?;
        let large_node = reader.get_node(large_inode)?;
        
        println!("   File size: {} bytes ({:.2} MB)", large_node.data.len(), large_node.data.len() as f64 / 1024.0 / 1024.0);
        
        // Verify data integrity (sample check)
        let sample_size = 1000.min(large_node.data.len());
        let mut errors = 0;
        for i in 0..sample_size {
            if large_node.data[i] != (i % 256) as u8 {
                errors += 1;
            }
        }
        println!("   Data integrity: {} errors in {} sampled bytes", errors, sample_size);
    }
    let read_duration = read_start.elapsed();
    println!("✅ Read completed in {:.2?}", read_duration);
    let read_speed = 10.0 / read_duration.as_secs_f64().max(0.000001);
    println!("   Speed: {:.2} MB/s\n", read_speed);

    // Test 3: Multiple small reads (simulating kernel startup)
    println!("Test 3: Multiple small file reads (100 files)...");
    let multi_start = Instant::now();
    {
        let file = File::create("benchmark_multi.img")?;
        let mut writer = EclipseFSWriter::new(file);

        writer.create_root()?;

        // Create 100 small files (10KB each)
        let small_file_size = 10 * 1024;
        let mut small_data = Vec::with_capacity(small_file_size);
        for i in 0..small_file_size {
            small_data.push((i % 256) as u8);
        }

        // Create all nodes first
        let mut inodes = Vec::new();
        for _i in 0..100 {
            let mut small_file = EclipseFSNode::new_file();
            small_file.set_data(&small_data)?;
            let inode = writer.create_node(small_file)?;
            inodes.push(inode);
        }

        // Then add to root
        let root = writer.get_root()?;
        for (i, inode) in inodes.into_iter().enumerate() {
            root.add_child(&format!("file_{:03}.dat", i), inode)?;
        }

        writer.write_image()?;
    }
    
    // Now read all 100 files
    {
        let mut reader = EclipseFSReader::from_file(File::open("benchmark_multi.img")?)?;
        
        for i in 0..100 {
            let file_inode = reader.lookup(constants::ROOT_INODE as u64, &format!("file_{:03}.dat", i))?;
            let _file_node = reader.get_node(file_inode)?;
        }
    }
    let multi_duration = multi_start.elapsed();
    println!("✅ Multiple reads completed in {:.2?}", multi_duration);
    println!("   Average per file: {:.2?}\n", multi_duration / 100);

    // Summary
    println!("=== Performance Summary ===");
    let write_speed = 10.0 / write_duration.as_secs_f64().max(0.000001);
    let read_speed = 10.0 / read_duration.as_secs_f64().max(0.000001);
    println!("10MB write: {:.2?} ({:.2} MB/s)", write_duration, write_speed);
    println!("10MB read:  {:.2?} ({:.2} MB/s)", read_duration, read_speed);
    println!("100 x 10KB reads: {:.2?} ({:.2?} per file)", multi_duration, multi_duration / 100);
    
    if read_duration.as_secs_f64() < 5.0 {
        println!("\n✅ PASS: Read time under 5 seconds target!");
    } else {
        println!("\n⚠️  WARNING: Read time exceeds 5 second target");
    }

    // Cleanup (use RAII pattern for better cleanup on error)
    let _ = std::fs::remove_file(test_file);
    let _ = std::fs::remove_file("benchmark_multi.img");

    Ok(())
}
