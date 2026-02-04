use crate::serial;
use core::cmp::min;
use alloc::vec::Vec;
use alloc::vec;
use alloc::string::String;
use eclipsefs_lib::format::{EclipseFSHeader, InodeTableEntry, tlv_tags, constants};
use eclipsefs_lib::NodeKind;

/// Block size for filesystem operations
pub const BLOCK_SIZE: usize = 4096;

/// Read a block from the underlying block device
/// Tries VirtIO first, falls back to ATA
fn read_block_from_device(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    // Try VirtIO first (preferred for QEMU)
    match crate::virtio::read_block(block_num, buffer) {
        Ok(_) => {
            return Ok(());
        }
        Err(_) => {
            // Fall back to ATA
            crate::ata::read_block(block_num, buffer)
        }
    }
}

/// Write a block to the underlying block device
/// Tries VirtIO first, falls back to ATA
fn write_block_to_device(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    // Try VirtIO first (preferred for QEMU)
    match crate::virtio::write_block(block_num, buffer) {
        Ok(_) => {
            return Ok(());
        }
        Err(_) => {
            // ATA write not implemented yet
            Err("ATA write not implemented")
        }
    }
}

/// Filesystem state
pub struct Filesystem {
    mounted: bool,
    header: Option<EclipseFSHeader>,
    inode_table_offset: u64,
}

static mut FS: Filesystem = Filesystem {
    mounted: false,
    header: None,
    inode_table_offset: 0,
};

impl Filesystem {
    /// Mount the root filesystem
// Hardcoded partition offset for now (513 MiB / 4096 bytes = 131328 blocks)
const PARTITION_OFFSET_BLOCKS: u64 = 131328;

pub fn mount() -> Result<(), &'static str> {
    // Enforce ATA initialization before mounting
    // This is handled in main.rs but good to be sure
    
    unsafe {
        if FS.mounted {
            return Err("Filesystem already mounted");
        }
        
        serial::serial_print("[FS] Attempting to mount eclipsefs...\n");
        
        serial::serial_print("[FS] Allocating superblock buffer...\n");
        // Use heap to avoid stack overflow
        let mut superblock = vec![0u8; 4096];
        serial::serial_print("[FS] Buffer allocated at: ");
        serial::serial_print_hex(superblock.as_ptr() as u64);
        serial::serial_print("\n");
        
        serial::serial_print("[FS] Reading superblock from block device...\n");
        read_block_from_device(Self::PARTITION_OFFSET_BLOCKS, &mut superblock)?;
        serial::serial_print("[FS] Superblock read successfully\n");
        
        // Parse header using library
        match EclipseFSHeader::from_bytes(&superblock) {
            Ok(header) => {
                serial::serial_print("[FS] EclipseFS signature found\n");
                serial::serial_print("[FS] Version: ");
                serial::serial_print_dec((header.version >> 16) as u64);
                serial::serial_print(".");
                serial::serial_print_dec((header.version & 0xFFFF) as u64);
                serial::serial_print("\n");
                
                FS.inode_table_offset = header.inode_table_offset;
                FS.header = Some(header);
                FS.mounted = true;
                
                serial::serial_print("[FS] Filesystem mounted successfully\n");
                Ok(())
            },
            Err(_) => {
                serial::serial_print("[FS] Invalid EclipseFS header\n");
                Err("Invalid EclipseFS header")
            }
        }
    }
}
    
    /// Read an inode entry
    fn read_inode_entry(inode: u32) -> Result<InodeTableEntry, &'static str> {
        unsafe {
            let header = FS.header.as_ref().ok_or("FS not mounted")?;
            
            if inode < 1 || inode > header.total_inodes {
                return Err("Inode out of range");
            }
            
            // Calculate sector for inode entry
            // Inode table starts at inode_table_offset
            // Each entry is 8 bytes.
            // inode indices are 1-based, table is 0-indexed (inode 1 is at index 0)
            let index = (inode - 1) as u64;
            let entry_offset = FS.inode_table_offset + (index * 8);
            
            let block_num = (entry_offset / BLOCK_SIZE as u64) + Self::PARTITION_OFFSET_BLOCKS;
            let offset_in_block = (entry_offset % BLOCK_SIZE as u64) as usize;
            
            let mut buffer = vec![0u8; 4096];
            read_block_from_device(block_num, &mut buffer)?;
            
            let inode_num = u32::from_le_bytes([
                buffer[offset_in_block], buffer[offset_in_block+1], 
                buffer[offset_in_block+2], buffer[offset_in_block+3]
            ]) as u64;
            
            let rel_offset = u32::from_le_bytes([
                buffer[offset_in_block+4], buffer[offset_in_block+5], 
                buffer[offset_in_block+6], buffer[offset_in_block+7]
            ]) as u64;
             
            let absolute_offset = header.inode_table_offset + header.inode_table_size + rel_offset;
            
            Ok(InodeTableEntry::new(inode_num, absolute_offset))
        }
    }
    
    /// Parse TLV data from a node record
    fn parse_tlv_content(data: &[u8]) -> Result<Vec<u8>, &'static str> {
        let mut offset = 0;
        while offset + 6 <= data.len() {
            let tag = u16::from_le_bytes([data[offset], data[offset+1]]);
            let length = u32::from_le_bytes([
                data[offset+2], data[offset+3], data[offset+4], data[offset+5]
            ]) as usize;
            
            offset += 6;
            
            if offset + length > data.len() {
                break;
            }
            
            if tag == tlv_tags::CONTENT {
                let mut content = vec![0u8; length];
                content.copy_from_slice(&data[offset..offset+length]);
                return Ok(content);
            }
            
            // Directories logic handled separately in lookup
            
            offset += length;
        }
        
        Ok(Vec::new()) // Empty content if not found
    }

    /// Read file content by inode
    pub fn read_file_by_inode(inode: u32, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let entry = Self::read_inode_entry(inode)?;
        
        // Read node record
        let block_num = (entry.offset / BLOCK_SIZE as u64) + Self::PARTITION_OFFSET_BLOCKS;
        let offset_in_block = (entry.offset % BLOCK_SIZE as u64) as usize;
        
        // Node Header: Inode (4) + Size (4) + Header Size (4) ? NO.
        // Node format: 
        // 4 bytes: inode
        // 4 bytes: record size
        // TLV data...
        
        // If record spans blocks, we need logic for that. 
        // For simplicity, assumed nodes fit in one block for initialization phase.
        // But `read_block` reads 4096 bytes.
        // If the record crosses boundary, we failed. Optimistic assumption for bootloader.
        
        let mut block_buffer = vec![0u8; 4096];
        read_block_from_device(block_num, &mut block_buffer)?;
        
        if offset_in_block + 8 > 4096 {
             // Basic boundary cross handling would load next block
             return Err("Node header crosses block boundary (unsupported in simple driver)");
        }

        let _read_inode = u32::from_le_bytes([
            block_buffer[offset_in_block], block_buffer[offset_in_block+1],
            block_buffer[offset_in_block+2], block_buffer[offset_in_block+3]
        ]);
        
        let record_size = u32::from_le_bytes([
            block_buffer[offset_in_block+4], block_buffer[offset_in_block+5],
            block_buffer[offset_in_block+6], block_buffer[offset_in_block+7]
        ]) as usize;
        
        if offset_in_block + record_size > 4096 {
             // For now, assume small nodes. /sbin/init is small but binary might be ~100KB+ stored in content?
             // Actually, file content is inside CONTENT TLV.
             // If content is large, it WILL cross block boundaries.
             // We need a proper loop reading blocks.
             // OR, more likely for an OS, the CONTENT TLV points to extents?
             // EclipseFS simple format puts data INLINE in CONTENT tag.
             // This means /sbin/init (1.2MB) is huge record!
             // So `block_buffer` is not enough.
             
             // WE MUST IMPLEMENT MULTI-BLOCK READ.
             
             // Let's implement reading the full record into a temporary buffer?
             // No, kernel heap is smallish? 
             // We can read directly to destination buffer if we parse TLV correctly.
        }

        // Simpler approach:
        // Read the full record size.
        // If it spans blocks, read multiple blocks.
        
        let mut record_data = vec![0u8; record_size];
        
        // Read first chunk
        let first_chunk_size = min(record_size, 4096 - offset_in_block);
        record_data[0..first_chunk_size].copy_from_slice(&block_buffer[offset_in_block..offset_in_block+first_chunk_size]);
        
        let mut bytes_read = first_chunk_size;
        let mut current_block = block_num + 1;
        
        while bytes_read < record_size {
             let chunk_size = min(4096, record_size - bytes_read);
             read_block_from_device(current_block, &mut block_buffer)?;
             record_data[bytes_read..bytes_read+chunk_size].copy_from_slice(&block_buffer[0..chunk_size]);
             bytes_read += chunk_size;
             current_block += 1;
        }
        
        // Now parse TLV from record_data (skipping 8 byte header)
        let content = Self::parse_tlv_content(&record_data[8..])?;
        
        let copy_len = min(buffer.len(), content.len());
        buffer[..copy_len].copy_from_slice(&content[..copy_len]);
        
        Ok(copy_len)
    }

    /// Write data to file by inode
    /// 
    /// This is a simplified implementation that writes data to an existing file
    /// without extending it. It modifies the file content in-place.
    /// 
    /// Limitations:
    /// - Cannot extend file beyond current size
    /// - Cannot allocate new blocks
    /// - Writes are limited to existing file content
    /// 
    /// Parameters:
    /// - inode: The inode number of the file
    /// - data: The data to write
    /// - offset: Offset within the file content (not block offset)
    /// 
    /// Returns: Number of bytes written
    pub fn write_file_by_inode(inode: u32, data: &[u8], offset: u64) -> Result<usize, &'static str> {
        let entry = Self::read_inode_entry(inode)?;
        
        // Read the full node record first
        let block_num = (entry.offset / BLOCK_SIZE as u64) + Self::PARTITION_OFFSET_BLOCKS;
        let offset_in_block = (entry.offset % BLOCK_SIZE as u64) as usize;
        
        let mut block_buffer = vec![0u8; 4096];
        read_block_from_device(block_num, &mut block_buffer)?;
        
        if offset_in_block + 8 > 4096 {
            return Err("Node header crosses block boundary");
        }
        
        let record_size = u32::from_le_bytes([
            block_buffer[offset_in_block+4], block_buffer[offset_in_block+5],
            block_buffer[offset_in_block+6], block_buffer[offset_in_block+7]
        ]) as usize;
        
        // Read full record
        let mut record_data = vec![0u8; record_size];
        let first_chunk_size = min(record_size, 4096 - offset_in_block);
        record_data[0..first_chunk_size].copy_from_slice(
            &block_buffer[offset_in_block..offset_in_block+first_chunk_size]
        );
        
        let mut bytes_read = first_chunk_size;
        let mut current_block = block_num + 1;
        
        while bytes_read < record_size {
            let chunk_size = min(4096, record_size - bytes_read);
            read_block_from_device(current_block, &mut block_buffer)?;
            record_data[bytes_read..bytes_read+chunk_size].copy_from_slice(&block_buffer[0..chunk_size]);
            bytes_read += chunk_size;
            current_block += 1;
        }
        
        // Find CONTENT TLV and modify it
        let mut tlv_offset = 8; // Skip 8-byte header
        let mut content_found = false;
        let mut content_tlv_offset = 0;
        let mut content_length = 0;
        
        while tlv_offset + 6 <= record_data.len() {
            let tag = u16::from_le_bytes([record_data[tlv_offset], record_data[tlv_offset+1]]);
            let length = u32::from_le_bytes([
                record_data[tlv_offset+2], record_data[tlv_offset+3], 
                record_data[tlv_offset+4], record_data[tlv_offset+5]
            ]) as usize;
            
            if tag == tlv_tags::CONTENT {
                content_found = true;
                content_tlv_offset = tlv_offset + 6; // Offset to actual content data
                content_length = length;
                break;
            }
            
            tlv_offset += 6 + length;
        }
        
        if !content_found {
            return Err("No CONTENT TLV found in file");
        }
        
        // Check bounds
        if offset as usize >= content_length {
            return Err("Write offset beyond file content");
        }
        
        let write_start = content_tlv_offset + offset as usize;
        let max_write = min(data.len(), content_length - offset as usize);
        
        if max_write == 0 {
            return Ok(0);
        }
        
        // Modify the record data
        record_data[write_start..write_start+max_write].copy_from_slice(&data[..max_write]);
        
        // Write the modified record back to disk
        // We need to write all blocks that the record spans
        let start_block = block_num;
        let start_offset = offset_in_block;
        
        // Prepare first block with modified data
        let mut write_buffer = vec![0u8; 4096];
        read_block_from_device(start_block, &mut write_buffer)?;
        
        let first_write_size = min(record_size, 4096 - start_offset);
        write_buffer[start_offset..start_offset+first_write_size].copy_from_slice(&record_data[0..first_write_size]);
        
        write_block_to_device(start_block, &write_buffer)?;
        
        // Write remaining blocks if record spans multiple blocks
        let mut bytes_written = first_write_size;
        let mut write_block_num = start_block + 1;
        
        while bytes_written < record_size {
            let chunk_size = min(4096, record_size - bytes_written);
            
            // Read existing block (to preserve data we're not modifying)
            read_block_from_device(write_block_num, &mut write_buffer)?;
            
            // Overwrite with our new data
            write_buffer[0..chunk_size].copy_from_slice(&record_data[bytes_written..bytes_written+chunk_size]);
            
            // Write back to disk
            write_block_to_device(write_block_num, &write_buffer)?;
            
            bytes_written += chunk_size;
            write_block_num += 1;
        }
        
        Ok(max_write)
    }

    /// Helper to find child inode in directory data
    fn find_child_in_dir(data: &[u8], target_name: &str) -> Option<u32> {
        let mut offset = 0;
        // Skip header 8 bytes if passing raw record, but here we pass TLV value
        while offset + 6 <= data.len() {
             let tag = u16::from_le_bytes([data[offset], data[offset+1]]);
             let length = u32::from_le_bytes([
                data[offset+2], data[offset+3], data[offset+4], data[offset+5]
            ]) as usize;
            offset += 6;
            
            if tag == tlv_tags::DIRECTORY_ENTRIES {
                // Parse dir entries
                // Format: NameLen(4) + Inode(4) + Name(Len)
                let dir_data = &data[offset..offset+length];
                let mut dir_offset = 0;
                while dir_offset + 8 <= dir_data.len() {
                    let name_len = u32::from_le_bytes([
                        dir_data[dir_offset], dir_data[dir_offset+1],
                        dir_data[dir_offset+2], dir_data[dir_offset+3]
                    ]) as usize;
                    
                    let child_inode = u32::from_le_bytes([
                        dir_data[dir_offset+4], dir_data[dir_offset+5],
                        dir_data[dir_offset+6], dir_data[dir_offset+7]
                    ]);
                    
                    if dir_offset + 8 + name_len > dir_data.len() { break; }
                    
                    let name_bytes = &dir_data[dir_offset+8..dir_offset+8+name_len];
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        if name == target_name {
                            return Some(child_inode);
                        }
                    }
                    
                    dir_offset += 8 + name_len;
                }
            }
            offset += length;
        }
        None
    }

    /// Get the size of a file by inode number
    /// Returns the content length from the CONTENT TLV
    pub fn get_file_size(inode: u32) -> Result<u64, &'static str> {
        unsafe {
            let _ = FS.header.as_ref().ok_or("FS not mounted")?;
        }
        
        // Read the inode entry from the inode table
        let entry = Self::read_inode_entry(inode)?;
        
        // Calculate which block the node record starts at
        let record_block = unsafe {
            Self::PARTITION_OFFSET_BLOCKS + (entry.offset / BLOCK_SIZE as u64)
        };
        
        // Read the first block of the node record
        let mut block_buffer = vec![0u8; BLOCK_SIZE];
        read_block_from_device(record_block, &mut block_buffer)?;
        
        // Parse TLV structure to find CONTENT tag
        let mut offset = 0;
        while offset + 8 <= block_buffer.len() {
            // Read tag (4 bytes) and length (4 bytes)
            let tag = u32::from_le_bytes([
                block_buffer[offset], block_buffer[offset+1],
                block_buffer[offset+2], block_buffer[offset+3]
            ]);
            
            let length = u32::from_le_bytes([
                block_buffer[offset+4], block_buffer[offset+5],
                block_buffer[offset+6], block_buffer[offset+7]
            ]) as usize;
            
            if tag == tlv_tags::CONTENT as u32 {
                // Found CONTENT tag, return its length as file size
                return Ok(length as u64);
            }
            
            // Move to next TLV entry
            offset += 8 + length;
            
            // If we've gone past the first block, we need to handle multi-block records
            // For simplicity, assume CONTENT is in first block (reasonable for most files)
            if offset >= BLOCK_SIZE {
                break;
            }
        }
        
        // If we didn't find CONTENT tag, the file is empty or this is a directory
        Ok(0)
    }

    /// Lookup functionality
    pub fn lookup_path(path: &str) -> Result<u32, &'static str> {
        unsafe {
            let _ = FS.header.as_ref().ok_or("FS not mounted")?;
        }
        
        if path == "/" {
            return Ok(1); // Root
        }
        
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current_inode = 1;
        
        for part in parts {
            // Read directory inode
             // Reuse read logic but we need the raw record to find children
             let entry = Self::read_inode_entry(current_inode)?;
             
             // Load whole record (same logic as read_file_by_inode basically)
             // TODO: Refactor duplication
             let block_num = (entry.offset / BLOCK_SIZE as u64) + Self::PARTITION_OFFSET_BLOCKS;
             let offset_in_block = (entry.offset % BLOCK_SIZE as u64) as usize;
             
             let mut block_buffer = vec![0u8; 4096];
             read_block_from_device(block_num, &mut block_buffer)?;
             
             let record_size = u32::from_le_bytes([
                block_buffer[offset_in_block+4], block_buffer[offset_in_block+5],
                block_buffer[offset_in_block+6], block_buffer[offset_in_block+7]
            ]) as usize;
            
            let mut record_data = vec![0u8; record_size];
            // Read first chunk
            let first_chunk_size = min(record_size, 4096 - offset_in_block);
            record_data[0..first_chunk_size].copy_from_slice(&block_buffer[offset_in_block..offset_in_block+first_chunk_size]);
            
            let mut bytes_read = first_chunk_size;
            let mut current_block = block_num + 1;
            while bytes_read < record_size {
                 let chunk_size = min(4096, record_size - bytes_read);
                 read_block_from_device(current_block, &mut block_buffer)?;
                 record_data[bytes_read..bytes_read+chunk_size].copy_from_slice(&block_buffer[0..chunk_size]);
                 bytes_read += chunk_size;
                 current_block += 1;
            }
            
            // Search in record
            if let Some(inode) = Self::find_child_in_dir(&record_data[8..], part) {
                current_inode = inode;
            } else {
                return Err("File not found");
            }
        }
        
        Ok(current_inode)
    }
}


/// Initialize the filesystem subsystem
pub fn init() {
    serial::serial_print("Initializing filesystem subsystem...\n");
}

/// Mount the root filesystem
pub fn mount_root() -> Result<(), &'static str> {
    Filesystem::mount()
}

/// Read a file from the filesystem
pub fn read_file(path: &str, buffer: &mut [u8]) -> Result<usize, &'static str> {
    // 1. Lookup inode
    let inode = Filesystem::lookup_path(path)?;
    // 2. Read file
    Filesystem::read_file_by_inode(inode, buffer)
}

/// Check if filesystem is mounted
pub fn is_mounted() -> bool {
    // Safe because we just read bool
    unsafe { FS.mounted }
}
