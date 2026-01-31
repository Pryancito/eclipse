//! Minimal filesystem support for Eclipse microkernel
//! 
//! This module provides a basic interface for mounting and accessing
//! the eclipsefs filesystem. For full implementation, this should be
//! moved to userspace according to microkernel principles.

use crate::serial;

/// Block size for filesystem operations
pub const BLOCK_SIZE: usize = 4096;

/// Maximum number of open files
const MAX_OPEN_FILES: usize = 16;

/// Maximum number of inodes supported
const MAX_INODES: usize = 1024;

/// Root inode number
const ROOT_INODE: u32 = 1;

/// TLV tags for node records
const TAG_NODE_TYPE: u16 = 0x0001;
const TAG_SIZE: u16 = 0x0005;
const TAG_CONTENT: u16 = 0x000A;
const TAG_DIRECTORY_ENTRIES: u16 = 0x000B;

/// File handle
#[derive(Clone, Copy)]
pub struct FileHandle {
    pub inode: u32,
    pub offset: u64,
    pub flags: u32,
}

/// Inode table entry
#[derive(Clone, Copy)]
struct InodeTableEntry {
    inode: u32,
    offset: u64,  // Absolute offset on disk
}

/// EclipseFS header (from superblock)
struct Header {
    inode_table_offset: u64,
    inode_table_size: u64,
    total_inodes: u32,
}

/// Filesystem state
pub struct Filesystem {
    mounted: bool,
    root_inode: u32,
    header: Header,
    inode_table: [InodeTableEntry; MAX_INODES],
    inode_count: usize,
}

static mut FS: Filesystem = Filesystem {
    mounted: false,
    root_inode: ROOT_INODE,
    header: Header {
        inode_table_offset: 0,
        inode_table_size: 0,
        total_inodes: 0,
    },
    inode_table: [InodeTableEntry { inode: 0, offset: 0 }; MAX_INODES],
    inode_count: 0,
};

impl Filesystem {
    /// Read u32 from buffer at offset (little-endian)
    fn read_u32(buffer: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ])
    }
    
    /// Read u64 from buffer at offset (little-endian)
    fn read_u64(buffer: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
            buffer[offset + 4],
            buffer[offset + 5],
            buffer[offset + 6],
            buffer[offset + 7],
        ])
    }
    
    /// Read u16 from buffer at offset (little-endian)
    fn read_u16(buffer: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([buffer[offset], buffer[offset + 1]])
    }
    
    /// Mount the root filesystem
    pub fn mount() -> Result<(), &'static str> {
        unsafe {
            if FS.mounted {
                return Err("Filesystem already mounted");
            }
            
            serial::serial_print("[FS] Attempting to mount eclipsefs...\n");
            
            // Read superblock from block 0
            let mut superblock = [0u8; 4096];
            if let Err(e) = crate::virtio::read_block(0, &mut superblock) {
                serial::serial_print("[FS] Failed to read superblock: ");
                serial::serial_print(e);
                serial::serial_print("\n");
                return Err("Failed to read superblock");
            }
            
            // Check magic number - "ECLIPSEFS" (9 bytes)
            if &superblock[0..9] == b"ECLIPSEFS" {
                serial::serial_print("[FS] EclipseFS signature found\n");
            } else {
                serial::serial_print("[FS] Warning: Invalid EclipseFS signature\n");
                // Still try to continue
            }
            
            // Parse header
            FS.header.inode_table_offset = Self::read_u64(&superblock, 13);
            FS.header.inode_table_size = Self::read_u64(&superblock, 21);
            FS.header.total_inodes = Self::read_u32(&superblock, 29);
            
            serial::serial_print("[FS] Inode table offset: ");
            serial::serial_print_dec(FS.header.inode_table_offset);
            serial::serial_print("\n[FS] Inode table size: ");
            serial::serial_print_dec(FS.header.inode_table_size);
            serial::serial_print("\n[FS] Total inodes: ");
            serial::serial_print_dec(FS.header.total_inodes as u64);
            serial::serial_print("\n");
            
            // Read inode table
            let inode_table_blocks = ((FS.header.inode_table_size as usize) + BLOCK_SIZE - 1) / BLOCK_SIZE;
            let start_block = (FS.header.inode_table_offset as usize) / BLOCK_SIZE;
            
            let mut inode_idx = 0;
            let max_inodes = FS.header.total_inodes.min(MAX_INODES as u32) as usize;
            
            for block_idx in 0..inode_table_blocks {
                let mut block_buffer = [0u8; 4096];
                if let Err(e) = crate::virtio::read_block((start_block + block_idx) as u64, &mut block_buffer) {
                    serial::serial_print("[FS] Failed to read inode table block: ");
                    serial::serial_print(e);
                    serial::serial_print("\n");
                    return Err("Failed to read inode table");
                }
                
                // Each inode table entry is 8 bytes: u32 inode + u32 relative_offset
                let entries_per_block = BLOCK_SIZE / 8;
                for entry_idx in 0..entries_per_block {
                    if inode_idx >= max_inodes {
                        break;
                    }
                    
                    let offset = entry_idx * 8;
                    let inode = Self::read_u32(&block_buffer, offset);
                    let relative_offset = Self::read_u32(&block_buffer, offset + 4) as u64;
                    let absolute_offset = FS.header.inode_table_offset + FS.header.inode_table_size + relative_offset;
                    
                    FS.inode_table[inode_idx] = InodeTableEntry {
                        inode,
                        offset: absolute_offset,
                    };
                    inode_idx += 1;
                }
            }
            
            FS.inode_count = inode_idx;
            FS.mounted = true;
            FS.root_inode = ROOT_INODE;
            
            serial::serial_print("[FS] Filesystem mounted successfully (");
            serial::serial_print_dec(FS.inode_count as u64);
            serial::serial_print(" inodes loaded)\n");
            Ok(())
        }
    }
    
    /// Check if filesystem is mounted
    pub fn is_mounted() -> bool {
        unsafe { FS.mounted }
    }
    
    /// Read a node from disk by inode number
    /// Returns (node_type, size, data_offset, data_size)
    fn read_node_metadata(inode: u32) -> Result<(u8, u64, u64, usize), &'static str> {
        unsafe {
            // Find inode in table
            let mut entry_offset = 0u64;
            let mut found = false;
            
            for i in 0..FS.inode_count {
                if FS.inode_table[i].inode == inode {
                    entry_offset = FS.inode_table[i].offset;
                    found = true;
                    break;
                }
            }
            
            if !found {
                serial::serial_print("[FS] Inode ");
                serial::serial_print_dec(inode as u64);
                serial::serial_print(" not found\n");
                return Err("Inode not found");
            }
            
            // Read node record header (8 bytes: inode + record_size)
            let block_num = (entry_offset / BLOCK_SIZE as u64) as usize;
            let offset_in_block = (entry_offset % BLOCK_SIZE as u64) as usize;
            
            let mut block_buffer = [0u8; 4096];
            crate::virtio::read_block(block_num as u64, &mut block_buffer)?;
            
            let recorded_inode = Self::read_u32(&block_buffer, offset_in_block);
            let record_size = Self::read_u32(&block_buffer, offset_in_block + 4) as usize;
            
            if recorded_inode != inode {
                serial::serial_print("[FS] Inode mismatch: expected ");
                serial::serial_print_dec(inode as u64);
                serial::serial_print(", got ");
                serial::serial_print_dec(recorded_inode as u64);
                serial::serial_print("\n");
                return Err("Inode mismatch");
            }
            
            // Parse TLV records to find node type, size, and content
            let mut pos = offset_in_block + 8;  // Skip header
            let mut end_pos = offset_in_block + 8 + record_size;
            let mut node_type = 0u8;
            let mut file_size = 0u64;
            let mut content_offset = 0u64;
            let mut content_size = 0usize;
            
            while pos < end_pos && pos + 3 <= BLOCK_SIZE {
                let tag = Self::read_u16(&block_buffer, pos);
                let length = block_buffer[pos + 2] as usize;
                pos += 3;
                
                if pos + length > BLOCK_SIZE {
                    // TLV entry spans block boundary - need to read next block
                    // For simplicity, we'll handle this case by reading continuous data
                    let next_block_num = block_num + 1;
                    let mut next_block = [0u8; 4096];
                    crate::virtio::read_block(next_block_num as u64, &mut next_block)?;
                    
                    // Copy remaining data from current block and continuation from next
                    let remaining_in_current = BLOCK_SIZE - pos;
                    let from_next = length - remaining_in_current;
                    
                    if tag == TAG_NODE_TYPE && length >= 1 {
                        if remaining_in_current > 0 {
                            node_type = block_buffer[pos];
                        } else {
                            node_type = next_block[0];
                        }
                    } else if tag == TAG_SIZE && length >= 8 {
                        let mut size_bytes = [0u8; 8];
                        for i in 0..8 {
                            if i < remaining_in_current {
                                size_bytes[i] = block_buffer[pos + i];
                            } else {
                                size_bytes[i] = next_block[i - remaining_in_current];
                            }
                        }
                        file_size = u64::from_le_bytes(size_bytes);
                    } else if tag == TAG_CONTENT {
                        content_offset = entry_offset + (pos as u64);
                        content_size = length;
                    }
                    
                    pos += length;
                    if pos >= BLOCK_SIZE {
                        // Continue in next block
                        block_buffer.copy_from_slice(&next_block);
                        pos -= BLOCK_SIZE;
                        end_pos = end_pos.saturating_sub(BLOCK_SIZE);
                    }
                } else {
                    // TLV entry is within current block
                    if tag == TAG_NODE_TYPE && length >= 1 {
                        node_type = block_buffer[pos];
                    } else if tag == TAG_SIZE && length >= 8 {
                        file_size = Self::read_u64(&block_buffer, pos);
                    } else if tag == TAG_CONTENT {
                        // Content starts at current position
                        content_offset = entry_offset + (pos as u64);
                        content_size = length;
                    }
                    
                    pos += length;
                }
            }
            
            Ok((node_type, file_size, content_offset, content_size))
        }
    }
    
    /// Look up a child in a directory node
    /// Returns the inode number of the child, or None if not found
    fn lookup_child(dir_inode: u32, name: &str) -> Result<Option<u32>, &'static str> {
        unsafe {
            // Find directory inode in table
            let mut entry_offset = 0u64;
            let mut found = false;
            
            for i in 0..FS.inode_count {
                if FS.inode_table[i].inode == dir_inode {
                    entry_offset = FS.inode_table[i].offset;
                    found = true;
                    break;
                }
            }
            
            if !found {
                return Err("Directory inode not found");
            }
            
            // Read directory node to find DIRECTORY_ENTRIES tag
            let block_num = (entry_offset / BLOCK_SIZE as u64) as usize;
            let offset_in_block = (entry_offset % BLOCK_SIZE as u64) as usize;
            
            let mut block_buffer = [0u8; 4096];
            crate::virtio::read_block(block_num as u64, &mut block_buffer)?;
            
            let record_size = Self::read_u32(&block_buffer, offset_in_block + 4) as usize;
            
            // Parse TLV to find DIRECTORY_ENTRIES
            let mut pos = offset_in_block + 8;
            let end_pos = offset_in_block + 8 + record_size;
            
            while pos < end_pos && pos + 3 <= BLOCK_SIZE {
                let tag = Self::read_u16(&block_buffer, pos);
                let length = block_buffer[pos + 2] as usize;
                pos += 3;
                
                if tag == TAG_DIRECTORY_ENTRIES {
                    // Parse directory entries: sequence of (name_len, name, inode)
                    let mut entries_pos = pos;
                    let entries_end = pos + length;
                    
                    while entries_pos < entries_end && entries_pos < BLOCK_SIZE {
                        let name_len = block_buffer[entries_pos] as usize;
                        entries_pos += 1;
                        
                        if entries_pos + name_len + 4 > BLOCK_SIZE {
                            // Entry spans boundary - this is a limitation of the current simple implementation
                            serial::serial_print("[FS] Warning: directory entry spans block boundary, skipping\n");
                            break;
                        }
                        
                        // Extract entry name
                        let entry_name = &block_buffer[entries_pos..entries_pos + name_len];
                        entries_pos += name_len;
                        
                        let child_inode = Self::read_u32(&block_buffer, entries_pos);
                        entries_pos += 4;
                        
                        // Compare names
                        if name.as_bytes() == entry_name {
                            return Ok(Some(child_inode));
                        }
                    }
                    
                    return Ok(None);  // Name not found
                }
                
                pos += length;
            }
            
            Ok(None)  // No directory entries found
        }
    }
    
    /// Resolve a path to an inode number
    fn resolve_path(path: &str) -> Result<u32, &'static str> {
        unsafe {
            if !FS.mounted {
                return Err("Filesystem not mounted");
            }
            
            // Handle root path
            if path == "/" {
                return Ok(ROOT_INODE);
            }
            
            // Split path into components
            let mut current_inode = ROOT_INODE;
            let mut start = 0;
            let path_bytes = path.as_bytes();
            
            // Skip leading slash
            if !path_bytes.is_empty() && path_bytes[0] == b'/' {
                start = 1;
            }
            
            let mut component_start = start;
            for i in start..path_bytes.len() + 1 {
                if i == path_bytes.len() || path_bytes[i] == b'/' {
                    if i > component_start {
                        // Extract component
                        let component = &path[component_start..i];
                        
                        // Look up component in current directory
                        match Self::lookup_child(current_inode, component)? {
                            Some(child_inode) => {
                                current_inode = child_inode;
                            }
                            None => {
                                serial::serial_print("[FS] Component '");
                                serial::serial_print(component);
                                serial::serial_print("' not found\n");
                                return Err("Path component not found");
                            }
                        }
                    }
                    component_start = i + 1;
                }
            }
            
            Ok(current_inode)
        }
    }
    
    /// Open a file
    pub fn open(path: &str) -> Result<FileHandle, &'static str> {
        unsafe {
            if !FS.mounted {
                return Err("Filesystem not mounted");
            }
            
            let inode = Self::resolve_path(path)?;
            
            Ok(FileHandle {
                inode,
                offset: 0,
                flags: 0,
            })
        }
    }
    
    /// Read from a file
    pub fn read(handle: FileHandle, buffer: &mut [u8]) -> Result<usize, &'static str> {
        unsafe {
            if !FS.mounted {
                return Err("Filesystem not mounted");
            }
            
            // Get node metadata
            let (node_type, file_size, content_offset, content_size) = Self::read_node_metadata(handle.inode)?;
            
            // Verify it's a file (type 0)
            if node_type != 0 {
                serial::serial_print("[FS] Not a file (type ");
                serial::serial_print_dec(node_type as u64);
                serial::serial_print(")\n");
                return Err("Not a file");
            }
            
            if content_size == 0 {
                return Ok(0);  // Empty file
            }
            
            // Read file content
            let bytes_to_read = content_size.min(buffer.len());
            let start_block = (content_offset / BLOCK_SIZE as u64) as usize;
            let offset_in_block = (content_offset % BLOCK_SIZE as u64) as usize;
            
            let mut bytes_read = 0;
            let mut current_block = start_block;
            let mut current_offset = offset_in_block;
            
            while bytes_read < bytes_to_read {
                let mut block_buffer = [0u8; 4096];
                crate::virtio::read_block(current_block as u64, &mut block_buffer)?;
                
                let available_in_block = BLOCK_SIZE - current_offset;
                let to_copy = (bytes_to_read - bytes_read).min(available_in_block);
                
                buffer[bytes_read..bytes_read + to_copy]
                    .copy_from_slice(&block_buffer[current_offset..current_offset + to_copy]);
                
                bytes_read += to_copy;
                current_block += 1;
                current_offset = 0;  // Subsequent blocks start at offset 0
            }
            
            Ok(bytes_read)
        }
    }
    
    /// Close a file
    pub fn close(_handle: FileHandle) -> Result<(), &'static str> {
        unsafe {
            if !FS.mounted {
                return Err("Filesystem not mounted");
            }
            
            // Nothing to do for now
            Ok(())
        }
    }
    
    /// Read entire file into buffer (helper function)
    pub fn read_file(path: &str, buffer: &mut [u8]) -> Result<usize, &'static str> {
        // Open, read, close pattern
        let handle = Self::open(path)?;
        let bytes_read = Self::read(handle, buffer)?;
        Self::close(handle)?;
        Ok(bytes_read)
    }
}


/// Initialize the filesystem subsystem
pub fn init() {
    serial::serial_print("Initializing filesystem subsystem...\n");
    
    // The actual mounting will happen after VirtIO is initialized
    // and we have a working block device
}

/// Mount the root filesystem
pub fn mount_root() -> Result<(), &'static str> {
    Filesystem::mount()
}

/// Read a file from the filesystem
pub fn read_file(path: &str, buffer: &mut [u8]) -> Result<usize, &'static str> {
    Filesystem::read_file(path, buffer)
}

/// Check if filesystem is mounted
pub fn is_mounted() -> bool {
    Filesystem::is_mounted()
}
