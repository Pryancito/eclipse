use crate::serial;
use core::cmp::min;
use alloc::vec::Vec;
use alloc::vec;
use alloc::boxed::Box;
use alloc::string::String;
use eclipsefs_lib::format::{EclipseFSHeader, InodeTableEntry, tlv_tags, constants};
use eclipsefs_lib::NodeKind;

/// Block size for filesystem operations
pub const BLOCK_SIZE: usize = 4096;

/// Maximum record size to prevent OOM (2 MiB)
pub const MAX_RECORD_SIZE: usize = 2 * 1024 * 1024;

/// Read a block from the underlying block device using the scheme system
fn read_block_from_device(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    unsafe {
        if !FS.mounted && FS.disk_resource_id == 0 && FS.disk_scheme_id == 0 {
            // During mount, we might not have a handle yet.
            // For now, use the old way during early bootstrap or a dummy handle.
            // Actually, we should open the handle IN mount().
        }

        let offset = block_num * 4096;
        let _ = crate::scheme::lseek(FS.disk_scheme_id, FS.disk_resource_id, offset as isize, 0)
            .map_err(|_| "Disk seek error")?;
        
        match crate::scheme::read(FS.disk_scheme_id, FS.disk_resource_id, buffer) {
            Ok(_) => Ok(()),
            Err(_) => Err("Disk read error"),
        }
    }
}

/// Write a block to the underlying block device using the scheme system
fn write_block_to_device(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    unsafe {
        let offset = block_num * 4096;
        let _ = crate::scheme::lseek(FS.disk_scheme_id, FS.disk_resource_id, offset as isize, 0)
            .map_err(|_| "Disk seek error")?;
        
        match crate::scheme::write(FS.disk_scheme_id, FS.disk_resource_id, buffer) {
            Ok(_) => Ok(()),
            Err(_) => Err("Disk write error"),
        }
    }
}

/// Filesystem state
pub struct Filesystem {
    mounted: bool,
    header: Option<EclipseFSHeader>,
    inode_table_offset: u64,
    disk_scheme_id: usize,
    disk_resource_id: usize,
}

static mut FS: Filesystem = Filesystem {
    mounted: false,
    header: None,
    inode_table_offset: 0,
    disk_scheme_id: 0,
    disk_resource_id: 0,
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

        // Open disk:0 (RootFS in our QEMU setup)
        serial::serial_print("[FS] Opening disk:0 via scheme registry...\n");
        match crate::scheme::open("disk:0", 0, 0) {
            Ok((s_id, r_id)) => {
                FS.disk_scheme_id = s_id;
                FS.disk_resource_id = r_id;
                serial::serial_print("[FS] Disk handle opened successfully\n");
            }
            Err(e) => {
                serial::serial_print("[FS] Failed to open disk:1 - error ");
                serial::serial_print_dec(e as u64);
                serial::serial_print("\n");
                return Err("Failed to open storage device");
            }
        }
        
        serial::serial_print("[FS] Allocating superblock buffer...\n");
        // Use heap to avoid stack overflow
        let mut superblock = vec![0u8; 4096];
        
        serial::serial_print("[FS] Reading superblock from block device...\n");
        read_block_from_device(Self::PARTITION_OFFSET_BLOCKS, &mut superblock)?;
        serial::serial_print("[FS] Superblock read successfully\n");
        
        // Parse header using library
        serial::serial_print("[FS] Raw superblock dump (64 bytes):\n");
        for i in (0..64).step_by(8) {
            serial::serial_print_hex(u64::from_le_bytes([
                superblock[i], superblock[i+1], superblock[i+2], superblock[i+3],
                superblock[i+4], superblock[i+5], superblock[i+6], superblock[i+7]
            ]));
            serial::serial_print(" ");
        }
        serial::serial_print("\n");

        match EclipseFSHeader::from_bytes(&superblock) {
            Ok(header) => {
                serial::serial_print("[FS] EclipseFS signature found\n");
                serial::serial_print("[FS] Version: ");
                serial::serial_print_dec((header.version >> 16) as u64);
                serial::serial_print(".");
                serial::serial_print_dec((header.version & 0xFFFF) as u64);
                serial::serial_print("\n");
                
                FS.inode_table_offset = header.inode_table_offset;
                
                // DevFS Safety: Ensure disk inodes don't collide with 0xF0000000+ range
                if header.total_inodes >= 0xF0000000 {
                    serial::serial_print("[FS] WARNING: Total inodes exceeds safe range! DevFS collisions possible.\n");
                    return Err("Filesystem too large (inodes overlap with DevFS)");
                }
                
                FS.header = Some(header);
                FS.mounted = true;
                
                serial::serial_print("[FS] Filesystem mounted successfully. FS.mounted set to true.\n");

                Ok(())
            },
            Err(e) => {
                serial::serial_print("[FS] Invalid EclipseFS header: ");
                serial::serial_print_dec(e as u64);
                serial::serial_print("\n");
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
            
            serial::serial_print("[FS] read_inode(");
            serial::serial_print_dec(inode as u64);
            serial::serial_print(") table_off=0x");
            serial::serial_print_hex(FS.inode_table_offset);
            serial::serial_print(" entry_off=0x");
            serial::serial_print_hex(entry_offset);
            serial::serial_print("\n");
            
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
            
            serial::serial_print("[FS] INODE RAW: num=");
            serial::serial_print_dec(inode_num);
            serial::serial_print(" rel_off=");
            serial::serial_print_dec(rel_offset);
            serial::serial_print("\n");

            serial::serial_print("[FS] Raw buffer dump (32 bytes at offset_in_block): ");
            for i in 0..32 {
                if offset_in_block + i < 4096 {
                    serial::serial_print_hex(buffer[offset_in_block + i] as u64);
                    serial::serial_print(" ");
                }
            }
            serial::serial_print("\n");
             
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

    /// Read file content by inode with offset
    /// Does NOT buffer the entire file. Reads directly for the requested range.
    pub fn read_file_by_inode_at(inode: u32, buffer: &mut [u8], offset: u64) -> Result<usize, &'static str> {
        let entry = Self::read_inode_entry(inode)?;
        
        // Read the first block of the node record to find CONTENT TLV
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

        // Parse TLVs in the first block to find CONTENT
        // We assume headers structure (CONTENT tag) appears in the first block.
        // If not, we'd need to scan more blocks, but population tool puts it early.
        
        let header_end = min(4096, offset_in_block + record_size);
        let valid_data = &block_buffer[offset_in_block+8..header_end]; // Skip 8 byte node header
        
        let mut tlv_cursor = 0;
        let mut content_start_offset_rel = 0; // Relative to node data start (after 8 bytes)
        let mut content_length = 0;
        let mut found = false;
        
        while tlv_cursor + 6 <= valid_data.len() {
            let tag = u16::from_le_bytes([valid_data[tlv_cursor], valid_data[tlv_cursor+1]]);
            let length = u32::from_le_bytes([
                valid_data[tlv_cursor+2], valid_data[tlv_cursor+3], 
                valid_data[tlv_cursor+4], valid_data[tlv_cursor+5]
            ]) as usize;
            
            if tag == tlv_tags::CONTENT {
                content_start_offset_rel = tlv_cursor + 6;
                content_length = length;
                found = true;
                break;
            }
            
            tlv_cursor += 6 + length;
        }
        
        if !found {
            // Could be in next block if first block is filled with metadata?
            // For now, assume it's in first block.
             return Err("CONTENT TLV not found in first block");
        }
        
        if offset >= content_length as u64 {
            return Ok(0); // EOF
        }
        
        let read_len = min(buffer.len(), content_length - offset as usize);
        
        // precise byte offset on disk where requested data starts
        let absolute_data_start = entry.offset + 8 + content_start_offset_rel as u64 + offset;
        
        // Read data from disk block by block
        let mut bytes_read = 0;
        let mut current_abs_pos = absolute_data_start;
        
        while bytes_read < read_len {
            let current_block = (current_abs_pos / BLOCK_SIZE as u64) + Self::PARTITION_OFFSET_BLOCKS;
            let current_off = (current_abs_pos % BLOCK_SIZE as u64) as usize;
            
            read_block_from_device(current_block, &mut block_buffer)?;
            
            let chunk_size = min(read_len - bytes_read, 4096 - current_off);
            buffer[bytes_read..bytes_read+chunk_size].copy_from_slice(&block_buffer[current_off..current_off+chunk_size]);
            
            bytes_read += chunk_size;
            current_abs_pos += chunk_size as u64;
        }
        
        Ok(read_len)
    }

    /// Backwards compatibility wrapper
    pub fn read_file_by_inode(inode: u32, buffer: &mut [u8]) -> Result<usize, &'static str> {
        Self::read_file_by_inode_at(inode, buffer, 0)
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
        
        // OOM Protection
        if record_size > MAX_RECORD_SIZE {
            return Err("File record too large (exceeds MAX_RECORD_SIZE)");
        }
        
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
        
        serial::serial_print("[FS] lookup_path('");
        serial::serial_print(path);
        serial::serial_print("')\n");

        if path == "/" {
            return Ok(1); // Root
        }
        
        // Handle /dev paths
        if is_device_path(path) {
            if path == "/dev" {
                return Ok(2); // Mock directory inode for /dev
            }
            if let Some(dev_name) = parse_device_name(path) {
                if let Some(_node) = lookup_device(dev_name) {
                    // Return a "virtual" inode for the device
                    // We hash the name to get a semi-stable ID or just use a high number
                    // For now, let's use a simple hashing or just return a dummy high ID.
                    // A proper implementation would map name -> stable ID.
                    // hack: simple hash
                    let mut hash: u32 = 0xF0000000;
                    for b in dev_name.bytes() {
                        hash = hash.wrapping_add(b as u32);
                    }
                    return Ok(hash);
                }
            }
        }
        
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current_inode = 1;
        
        for part in parts {
            serial::serial_print("[FS] Looking up '");
            serial::serial_print(part);
            serial::serial_print("' in inode ");
            serial::serial_print_dec(current_inode as u64);
            serial::serial_print("\n");

            // Read directory inode
             // Reuse read logic but we need the raw record to find children
             let entry = Self::read_inode_entry(current_inode)?;
             
             // Load whole record (same logic as read_file_by_inode basically)
             // TODO: Refactor duplication
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
            
            serial::serial_print("[FS] Dir record size: ");
            serial::serial_print_dec(record_size as u64);
            serial::serial_print("\n");

            if record_size < 8 {
                // This might be a padding byte or end of directory marker if 0, 
                // but for now let's treat it as an error/not found to avoid panic
                serial::serial_print("[FS] Error: Record too small (<8)\n");
                return Err("Invalid directory record size (too small)");
            }
            
            // OOM Protection
            if record_size > MAX_RECORD_SIZE {
                 serial::serial_print("[FS] Error: Record too large (>MAX)\n");
                 return Err("Directory record too large (exceeds MAX_RECORD_SIZE)");
            }
            
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
            // Safety: We verified record_size >= 8 above, and record_data.len() == record_size
            if let Some(inode) = Self::find_child_in_dir(&record_data[8..], part) {
                serial::serial_print("[FS] Found '");
                serial::serial_print(part);
                serial::serial_print("' -> inode ");
                serial::serial_print_dec(inode as u64);
                serial::serial_print("\n");
                current_inode = inode;
            } else {
                serial::serial_print("[FS] Child '");
                serial::serial_print(part);
                serial::serial_print("' not found in directory\n");
                return Err("File not found");
            }
        }
        
        Ok(current_inode)
    }
}


/// Initialize the filesystem subsystem
pub fn init() {
    serial::serial_print("Initializing filesystem subsystem...\n");
    crate::bcache::init();
    crate::scheme::register_scheme("file", alloc::sync::Arc::new(FileSystemScheme));
    crate::scheme::register_scheme("dev", alloc::sync::Arc::new(DevScheme));
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

// ============================================================================
// DEVICE FILESYSTEM (DevFS) SUPPORT
// ============================================================================

/// Device Type definition
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DeviceType {
    Block,      // Block devices (disks)
    Char,       // Character devices (console, tty)
    Network,    // Network interfaces
    Input,      // Input devices
    Audio,      // Audio devices
    Display,    // Display/framebuffer
    USB,        // USB controllers
    Unknown,
}

/// Device Node entry in registry
#[derive(Clone)]
pub struct DeviceNode {
    pub name: String,
    pub device_type: DeviceType,
    pub driver_pid: u64,
}

use spin::Mutex;
use alloc::collections::BTreeMap;

/// Global Device Registry
/// Maps path ("null", "sda", etc.) to DeviceNode
static DEVICE_REGISTRY: Mutex<Option<BTreeMap<String, DeviceNode>>> = Mutex::new(None);

/// Initialize the device registry
pub fn init_devfs() {
    let mut registry = DEVICE_REGISTRY.lock();
    *registry = Some(BTreeMap::new());
    serial::serial_print("[FS] Device registry initialized\n");
}

/// Register a new device
pub fn register_device(name: &str, device_type: DeviceType, driver_pid: u64) -> bool {
    // Ensure registry is initialized
    let mut registry_lock = DEVICE_REGISTRY.lock();
    if registry_lock.is_none() {
        *registry_lock = Some(BTreeMap::new());
    }
    
    if let Some(registry) = registry_lock.as_mut() {
        registry.insert(String::from(name), DeviceNode {
            name: String::from(name),
            device_type,
            driver_pid,
        });
        
        serial::serial_print("[FS] Registered device: /dev/");
        serial::serial_print(name);
        serial::serial_print("\n");
        return true;
    }
    false
}

/// Lookup a device by name (e.g., "null")
pub fn lookup_device(name: &str) -> Option<DeviceNode> {
    let registry_lock = DEVICE_REGISTRY.lock();
    if let Some(registry) = registry_lock.as_ref() {
        registry.get(name).cloned()
    } else {
        None
    }
}

// --- Redox-style Scheme Implementation ---

use crate::scheme::{Scheme, Stat, error as scheme_error};

struct OpenFile {
    inode: u32,
    offset: u64,
}

static OPEN_FILES_SCHEME: Mutex<alloc::vec::Vec<Option<OpenFile>>> = Mutex::new(alloc::vec::Vec::new());

pub struct FileSystemScheme;

impl Scheme for FileSystemScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let mounted = is_mounted();
        if !mounted {
            serial::serial_print("[FS-SCHEME] open() failed: filesystem NOT mounted\n");
            return Err(scheme_error::EIO);
        }
        
        serial::serial_print("[FS-SCHEME] open(");
        serial::serial_print(path);
        serial::serial_print(")\n");

        // Clean path to remove leading slash if present
        let clean_path = if path.starts_with('/') { &path[1..] } else { path };

        match Filesystem::lookup_path(clean_path) {
            Ok(inode) => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                for (i, slot) in open_files.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(OpenFile { inode, offset: 0 });
                        return Ok(i);
                    }
                }
                
                let id = open_files.len();
                open_files.push(Some(OpenFile { inode, offset: 0 }));
                Ok(id)
            }
            Err(_) => Err(scheme_error::ENOENT),
        }
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        match Filesystem::read_file_by_inode_at(open_file.inode, buffer, open_file.offset) {
            Ok(bytes_read) => {
                open_file.offset += bytes_read as u64;
                Ok(bytes_read)
            }
            Err(_) => Err(scheme_error::EIO),
        }
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        match Filesystem::write_file_by_inode(open_file.inode, buffer, open_file.offset) {
            Ok(bytes_written) => {
                open_file.offset += bytes_written as u64;
                Ok(bytes_written)
            }
            Err(_) => Err(scheme_error::EIO),
        }
    }

    fn lseek(&self, id: usize, offset: isize, whence: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        let new_offset = match whence {
            0 => offset as u64, // SEEK_SET
            1 => (open_file.offset as isize + offset) as u64, // SEEK_CUR
            _ => return Err(scheme_error::EINVAL),
        };
        
        open_file.offset = new_offset;
        Ok(new_offset as usize)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        if let Some(slot) = open_files.get_mut(id) {
            *slot = None;
            Ok(0)
        } else {
            Err(scheme_error::EBADF)
        }
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        
        match Filesystem::get_file_size(open_file.inode) {
            Ok(size) => {
                stat.size = size;
                Ok(0)
            }
            Err(_) => Err(scheme_error::EIO),
        }
    }
}

// --- Dev Scheme ---

pub struct DevScheme;

impl Scheme for DevScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Remove leading slash if present
        let clean_path = if path.starts_with('/') { &path[1..] } else { path };
        
        if lookup_device(clean_path).is_some() {
            // For now, we return a virtual ID based on the order in the registry
            // This is a placeholder since devfs is currently mostly for registration info
            Ok(0) 
        } else {
            Err(scheme_error::ENOENT)
        }
    }

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> {
        Ok(0) // Placeholder
    }

    fn write(&self, _id: usize, _buffer: &[u8]) -> Result<usize, usize> {
        Ok(_buffer.len()) // Placeholder
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Ok(0)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }
}


/// Check if a path is a device path
pub fn is_device_path(path: &str) -> bool {
    path.starts_with("/dev/") || path == "/dev" || path.starts_with("dev/") || path == "dev"
}

/// Parse device name from path
pub fn parse_device_name(path: &str) -> Option<&str> {
    if path.starts_with("/dev/") {
        Some(&path[5..])
    } else if path.starts_with("dev/") {
        Some(&path[4..])
    } else {
        None
    }
}