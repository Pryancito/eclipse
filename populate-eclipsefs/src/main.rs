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
        if entry.file_type().is_dir() {
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
        // Use fs::metadata (follows symlinks) so that symlinks to regular files
        // are included with their actual binary content, not as symlink nodes.
        let is_regular_file = if entry.file_type().is_file() {
            true
        } else if entry.file_type().is_symlink() {
            path.metadata().map(|m| m.is_file()).unwrap_or(false)
        } else {
            false
        };
        if is_regular_file {
            let relative = path.strip_prefix(&args.source)?;
            let fs_path = PathBuf::from("/").join(relative);

            // Read file content; fs::read follows symlinks automatically.
            let content = fs::read(path)?;

            // Executables in bin directories must never be 0 bytes — a 0-byte
            // binary means the cross-compilation failed or was not run before
            // populate.  Fail loudly here rather than silently building a broken
            // filesystem image that only manifests at runtime.
            if content.is_empty() {
                let components: Vec<_> = fs_path.components().collect();
                let in_bin_dir = components.windows(2).any(|w| {
                    let parent_name = w[0].as_os_str().to_str().unwrap_or("");
                    let file_name   = w[1].as_os_str().to_str().unwrap_or("");
                    (parent_name == "bin" || parent_name == "sbin") && !file_name.is_empty()
                }) || components.windows(3).any(|w| {
                    let grandparent = w[0].as_os_str().to_str().unwrap_or("");
                    let parent      = w[1].as_os_str().to_str().unwrap_or("");
                    let file_name   = w[2].as_os_str().to_str().unwrap_or("");
                    grandparent == "usr" && (parent == "bin" || parent == "sbin") && !file_name.is_empty()
                });
                if in_bin_dir {
                    eprintln!(
                        "ERROR: {:?} is 0 bytes — binary not built or build failed. \
                         Aborting populate to prevent a broken filesystem image.",
                        fs_path
                    );
                    std::process::exit(1);
                }
            }

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
        println!("  Processing path: {:?}", path);
        if entry.file_type().is_symlink() {
            // Skip symlinks that resolve to regular files — they were already
            // stored with their actual binary content in the file pass above.
            if path.metadata().map(|m| m.is_file()).unwrap_or(false) {
                continue;
            }

            let relative = path.strip_prefix(&args.source)?;
            let fs_path = PathBuf::from("/").join(relative);

            let target = fs::read_link(path)?;
            let target_str = target.to_str().unwrap_or("");

            println!("  Processing symlink: {:?}", fs_path);
            
            let link_node = EclipseFSNode::new_symlink(target_str);
            let link_inode = match writer.create_node(link_node) {
                Ok(inode) => inode,
                Err(e) => {
                    eprintln!("ERROR: create_node failed for {:?} with {:?}", fs_path, e);
                    return Err(Box::new(e));
                }
            };

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
                    if let Err(e) = parent.add_child(name, link_inode) {
                        eprintln!("ERROR: Failed to add symlink {:?} to parent {:?}: {:?}", name, parent_path_buf, e);
                        // Print children of parent to see what exists
                        for (existing_name, _) in parent.children.iter() {
                            eprintln!("  Existing: {}", existing_name);
                        }
                        return Err(Box::new(e));
                    }
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
