//! EclipseFS CLI - Command-line tool for managing EclipseFS filesystems

use clap::{Parser, Subcommand};
use colored::*;
use eclipsefs_lib::{EclipseFSReader, NodeKind};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "eclipsefs")]
#[command(about = "EclipseFS filesystem management tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show filesystem information
    Info {
        /// Device or image file
        device: PathBuf,
    },
    /// List directory contents
    Ls {
        /// Device or image file
        device: PathBuf,
        /// Path to list
        #[arg(default_value = "/")]
        path: String,
    },
    /// Display file contents
    Cat {
        /// Device or image file
        device: PathBuf,
        /// File path
        path: String,
    },
    /// Show filesystem tree
    Tree {
        /// Device or image file
        device: PathBuf,
        /// Maximum depth
        #[arg(short, long, default_value = "10")]
        depth: usize,
    },
    /// Check filesystem integrity
    Check {
        /// Device or image file
        device: PathBuf,
    },
    /// Show detailed statistics
    Stats {
        /// Device or image file
        device: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Info { device } => cmd_info(&device),
        Commands::Ls { device, path } => cmd_ls(&device, &path),
        Commands::Cat { device, path } => cmd_cat(&device, &path),
        Commands::Tree { device, depth } => cmd_tree(&device, depth),
        Commands::Check { device } => cmd_check(&device),
        Commands::Stats { device } => cmd_stats(&device),
    };

    if let Err(e) = result {
        eprintln!("{} {}", "Error:".red().bold(), e);
        std::process::exit(1);
    }
}

fn cmd_info(device: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let device_str = device.to_str().ok_or("Invalid device path")?;
    let mut reader = EclipseFSReader::new(device_str)?;

    println!("{}", "=== EclipseFS Information ===".green().bold());
    println!();

    let header = reader.get_header();
    let magic = String::from_utf8_lossy(&header.magic);
    
    println!("{}     {}", "Magic:".cyan(), magic);
    println!("{}   0x{:08X}", "Version:".cyan(), header.version);
    println!("{} {}", "Total Inodes:".cyan(), header.total_inodes);
    println!("{} 0x{:016X}", "Inode Table Offset:".cyan(), header.inode_table_offset);
    println!("{} {} bytes", "Inode Table Size:".cyan(), header.inode_table_size);
    
    // Get root node
    let root = reader.get_root()?;
    let child_count = root.get_children().len();
    
    println!();
    println!("{}", "=== Root Directory ===".green().bold());
    println!("{} {}", "Children:".cyan(), child_count);
    println!("{}      {}", "Mode:".cyan(), format!("{:o}", root.mode));
    
    Ok(())
}

fn cmd_ls(device: &PathBuf, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let device_str = device.to_str().ok_or("Invalid device path")?;
    let mut reader = EclipseFSReader::new(device_str)?;

    // Lookup path
    let inode = if path == "/" {
        eclipsefs_lib::constants::ROOT_INODE
    } else {
        reader.lookup_path(path)?
    };

    let node = reader.read_node(inode)?;

    if node.kind != NodeKind::Directory {
        return Err("Not a directory".into());
    }

    println!("{} {}", "Contents of".green().bold(), path.cyan());
    println!();

    for (name, child_inode) in node.get_children().iter() {
        let child = reader.read_node(*child_inode)?;
        
        let type_str = match child.kind {
            NodeKind::File => "FILE".blue(),
            NodeKind::Directory => "DIR ".yellow(),
            NodeKind::Symlink => "LINK".magenta(),
        };
        
        let size_str = format!("{:>10}", child.size);
        let mode_str = format!("{:o}", child.mode);
        
        println!("{} {} {:>6} {}", type_str, size_str, mode_str, name);
    }

    Ok(())
}

fn cmd_cat(device: &PathBuf, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let device_str = device.to_str().ok_or("Invalid device path")?;
    let mut reader = EclipseFSReader::new(device_str)?;

    let inode = reader.lookup_path(path)?;
    let node = reader.read_node(inode)?;

    if node.kind != NodeKind::File && node.kind != NodeKind::Symlink {
        return Err("Not a file".into());
    }

    let data = node.get_data();
    let content = String::from_utf8_lossy(data);
    print!("{}", content);

    Ok(())
}

fn cmd_tree(device: &PathBuf, max_depth: usize) -> Result<(), Box<dyn std::error::Error>> {
    let device_str = device.to_str().ok_or("Invalid device path")?;
    let mut reader = EclipseFSReader::new(device_str)?;

    println!("{}", device_str.cyan().bold());
    print_tree(&mut reader, eclipsefs_lib::constants::ROOT_INODE, "", max_depth, 0)?;

    Ok(())
}

fn print_tree(
    reader: &mut EclipseFSReader,
    inode: u32,
    prefix: &str,
    max_depth: usize,
    current_depth: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if current_depth >= max_depth {
        return Ok(());
    }

    let node = reader.read_node(inode)?;

    if node.kind != NodeKind::Directory {
        return Ok(());
    }

    let children: Vec<_> = node.get_children().iter().collect();
    let child_count = children.len();

    for (i, (name, child_inode)) in children.iter().enumerate() {
        let is_last = i == child_count - 1;
        let connector = if is_last { "└── " } else { "├── " };
        
        let child = reader.read_node(**child_inode)?;
        let name_colored = match child.kind {
            NodeKind::Directory => name.yellow(),
            NodeKind::Symlink => name.magenta(),
            NodeKind::File => name.normal(),
        };
        
        println!("{}{}{}", prefix, connector, name_colored);

        if child.kind == NodeKind::Directory {
            let new_prefix = if is_last {
                format!("{}    ", prefix)
            } else {
                format!("{}│   ", prefix)
            };
            
            print_tree(reader, **child_inode, &new_prefix, max_depth, current_depth + 1)?;
        }
    }

    Ok(())
}

fn cmd_check(device: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let device_str = device.to_str().ok_or("Invalid device path")?;
    let mut reader = EclipseFSReader::new(device_str)?;

    println!("{}", "=== Filesystem Check ===".green().bold());
    println!();

    // Check header
    print!("Checking header... ");
    let header = reader.get_header();
    let magic = String::from_utf8_lossy(&header.magic);
    if magic != "ECLIPSEFS" {
        println!("{}", "FAILED".red());
        return Err("Invalid magic number".into());
    }
    println!("{}", "OK".green());

    // Check root node
    print!("Checking root node... ");
    let root = reader.get_root()?;
    if root.kind != NodeKind::Directory {
        println!("{}", "FAILED".red());
        return Err("Root is not a directory".into());
    }
    println!("{}", "OK".green());

    // Count nodes
    print!("Counting nodes... ");
    let node_count = reader.get_inode_table().len();
    println!("{} {}", "OK".green(), format!("({} nodes)", node_count).cyan());

    // Verify all nodes can be read
    print!("Verifying all nodes... ");
    for i in 0..node_count {
        let inode = (i + 1) as u32;
        if let Err(e) = reader.read_node(inode) {
            println!("{}", "FAILED".red());
            return Err(format!("Error reading node {}: {:?}", i + 1, e).into());
        }
    }
    println!("{}", "OK".green());

    println!();
    println!("{}", "Filesystem check completed successfully!".green().bold());

    Ok(())
}

fn cmd_stats(device: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let device_str = device.to_str().ok_or("Invalid device path")?;
    let mut reader = EclipseFSReader::new(device_str)?;

    println!("{}", "=== Filesystem Statistics ===".green().bold());
    println!();

    let total_nodes = reader.get_inode_table().len();
    
    let mut file_count = 0;
    let mut dir_count = 0;
    let mut symlink_count = 0;
    let mut total_data_size: u64 = 0;

    for i in 0..total_nodes {
        let inode = (i + 1) as u32;
        if let Ok(node) = reader.read_node(inode) {
            match node.kind {
                NodeKind::File => {
                    file_count += 1;
                    total_data_size += node.size;
                }
                NodeKind::Directory => dir_count += 1,
                NodeKind::Symlink => symlink_count += 1,
            }
        }
    }

    println!("{}    {}", "Total Nodes:".cyan(), total_nodes);
    println!("{}         {}", "Files:".cyan(), file_count);
    println!("{}   {}", "Directories:".cyan(), dir_count);
    println!("{}      {}", "Symlinks:".cyan(), symlink_count);
    println!();
    println!("{} {} bytes", "Total Data Size:".cyan(), total_data_size);
    
    if file_count > 0 {
        let avg_size = total_data_size as f64 / file_count as f64;
        println!("{} {:.2} bytes", "Average File Size:".cyan(), avg_size);
    }

    Ok(())
}
