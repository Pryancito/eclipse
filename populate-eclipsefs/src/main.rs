//! populate-eclipsefs - Tool to populate an EclipseFS filesystem with files from a directory

use clap::Parser;
use eclipsefs_lib::{EclipseFSNode, EclipseFSWriter};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(name = "populate-eclipsefs")]
#[command(about = "Populate an EclipseFS filesystem with files from a directory")]
struct Args {
    /// Device or image file
    #[arg(value_name = "DEVICE")]
    device: PathBuf,

    /// Source directory to copy from
    #[arg(value_name = "SOURCE")]
    source: PathBuf,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║           populate-eclipsefs - EclipseFS Population Tool            ║");
    println!("╚══════════════════════════════════════════════════════════════════════╝");
    println!();

    // Verify source directory exists
    if !args.source.exists() {
        eprintln!("Error: Source directory {:?} does not exist", args.source);
        std::process::exit(1);
    }

    if !args.source.is_dir() {
        eprintln!("Error: Source {:?} is not a directory", args.source);
        std::process::exit(1);
    }

    println!("📂 Source directory: {:?}", args.source);
    println!("💾 Target device: {:?}", args.device);
    println!();

    // Open the device for writing
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&args.device)?;

    let mut writer = EclipseFSWriter::new(file);

    // Create root directory
    println!("📁 Creating root directory...");
    writer.create_root()?;

    // Map to track directory inodes
    let mut dir_inodes: HashMap<PathBuf, u32> = HashMap::new();
    dir_inodes.insert(PathBuf::from("/"), eclipsefs_lib::constants::ROOT_INODE);

    // First pass: Create all directories
    println!("🗂️  Creating directory structure...");
    let mut dirs: Vec<PathBuf> = Vec::new();
    
    for entry in WalkDir::new(&args.source)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_dir() {
            let relative = path.strip_prefix(&args.source)?;
            dirs.push(relative.to_path_buf());
        }
    }

    // Sort directories by depth to create parents before children
    dirs.sort_by_key(|p| p.components().count());

    for dir_path in &dirs {
        let dir_node = EclipseFSNode::new_dir();
        let dir_inode = writer.create_node(dir_node)?;
        
        let fs_path = PathBuf::from("/").join(dir_path);
        dir_inodes.insert(fs_path.clone(), dir_inode);

        if args.verbose {
            println!("  Created directory: {:?} (inode {})", fs_path, dir_inode);
        }

        // Add to parent directory
        if let Some(parent_path) = fs_path.parent() {
            let parent_path_buf = parent_path.to_path_buf();
            if let Some(&parent_inode) = dir_inodes.get(&parent_path_buf) {
                let parent = writer.get_node(parent_inode)?;
                let name = fs_path.file_name().unwrap().to_str().unwrap();
                parent.add_child(name, dir_inode)?;
                
                if args.verbose {
                    println!("    Added to parent {:?}", parent_path_buf);
                }
            }
        }
    }

    // Second pass: Create all files
    println!("📄 Copying files...");
    let mut file_count = 0;
    let mut total_bytes = 0u64;

    for entry in WalkDir::new(&args.source)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() {
            let relative = path.strip_prefix(&args.source)?;
            let fs_path = PathBuf::from("/").join(relative);

            // Read file content
            let content = fs::read(path)?;
            total_bytes += content.len() as u64;

            // Create file node
            let mut file_node = EclipseFSNode::new_file();
            file_node.set_data(&content)?;
            let file_inode = writer.create_node(file_node)?;

            file_count += 1;

            if args.verbose || file_count % 10 == 0 {
                println!("  Copied: {:?} ({} bytes, inode {})",
                    fs_path, content.len(), file_inode);
            }

            // Add to parent directory
            if let Some(parent_path) = fs_path.parent() {
                let parent_path_buf = parent_path.to_path_buf();
                if let Some(&parent_inode) = dir_inodes.get(&parent_path_buf) {
                    let parent = writer.get_node(parent_inode)?;
                    let name = fs_path.file_name().unwrap().to_str().unwrap();
                    parent.add_child(name, file_inode)?;
                }
            }
        }
    }

    // Third pass: Handle symlinks
    println!("🔗 Creating symlinks...");
    for entry in WalkDir::new(&args.source)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_symlink() {
            let relative = path.strip_prefix(&args.source)?;
            let fs_path = PathBuf::from("/").join(relative);

            let target = fs::read_link(path)?;
            let target_str = target.to_str().unwrap_or("");

            let link_node = EclipseFSNode::new_symlink(target_str);
            let link_inode = writer.create_node(link_node)?;

            if args.verbose {
                println!("  Created symlink: {:?} -> {:?} (inode {})",
                    fs_path, target_str, link_inode);
            }

            // Add to parent directory
            if let Some(parent_path) = fs_path.parent() {
                let parent_path_buf = parent_path.to_path_buf();
                if let Some(&parent_inode) = dir_inodes.get(&parent_path_buf) {
                    let parent = writer.get_node(parent_inode)?;
                    let name = fs_path.file_name().unwrap().to_str().unwrap();
                    parent.add_child(name, link_inode)?;
                }
            }
        }
    }

    // Write the filesystem image
    println!();
    println!("💾 Writing filesystem image...");
    writer.write_image()?;

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════╗");
    println!("║                  ✅ POPULATION COMPLETED SUCCESSFULLY                 ║");
    println!("╚══════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("Filesystem populated:");
    println!("  Directories: {}", dirs.len());
    println!("  Files:       {}", file_count);
    println!("  Total data:  {} KB ({} MB)",
        total_bytes / 1024,
        total_bytes / 1024 / 1024);
    println!();

    Ok(())
}
