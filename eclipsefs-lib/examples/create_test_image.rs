//! Create a test EclipseFS image for testing CLI tools

use eclipsefs_lib::{EclipseFSNode, EclipseFSWriter};
use std::fs::File;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let test_file = "test_eclipsefs.img";
    
    let file = File::create(test_file)?;
    let mut writer = EclipseFSWriter::new(file);

    // Create the root node
    writer.create_root()?;

    // Create directory /bin
    let bin_node = EclipseFSNode::new_dir();
    let bin_inode = writer.create_node(bin_node)?;

    // Create file /bin/hello
    let mut hello_node = EclipseFSNode::new_file();
    hello_node.set_data(b"Hello, EclipseFS!")?;
    let hello_inode = writer.create_node(hello_node)?;

    // Create file /readme.txt
    let mut readme_node = EclipseFSNode::new_file();
    readme_node.set_data(b"Welcome to EclipseFS!\n\nThis is a modern filesystem with:\n- Journaling\n- Copy-on-Write\n- Snapshots\n- And more!\n")?;
    let readme_inode = writer.create_node(readme_node)?;

    // Create symlink /bin/sh -> hello
    let sh_link = EclipseFSNode::new_symlink("hello");
    let sh_inode = writer.create_node(sh_link)?;

    // Add children to root directory
    let root = writer.get_root()?;
    root.add_child("bin", bin_inode)?;
    root.add_child("readme.txt", readme_inode)?;

    // Add children to /bin directory
    let bin_dir = writer.get_node(bin_inode)?;
    bin_dir.add_child("hello", hello_inode)?;
    bin_dir.add_child("sh", sh_inode)?;

    // Write the image
    writer.write_image()?;
    println!("Created test_eclipsefs.img successfully!");
    
    Ok(())
}
