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

/// Maximum record size to prevent OOM (16 MiB)
pub const MAX_RECORD_SIZE: usize = 16 * 1024 * 1024;

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
    /// Hardcoded partition offset for now (513 MiB / 4096 bytes = 131328 blocks)
    pub const PARTITION_OFFSET_BLOCKS: u64 = 131328;

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
    
    /// Read an inode entry from the table
    pub fn read_inode_entry(inode: u32) -> Result<InodeTableEntry, &'static str> {
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
            
            if offset + length > data.len() {
                serial::serial_print("[FS] find_child_in_dir: TLV length exceeds data size\n");
                break; 
            }

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
                    
                    if dir_offset + 8 + name_len > dir_data.len() {
                        serial::serial_print("[FS] find_child_in_dir: Entry name exceeds dir data size\n");
                        break; 
                    }
                    
                    let name_bytes = &dir_data[dir_offset+8..dir_offset+8+name_len];
                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                        /* 
                        serial::serial_print("  [DIR] Entry: '");
                        serial::serial_print(name);
                        serial::serial_print("' -> Inode ");
                        serial::serial_print_dec(child_inode as u64);
                        serial::serial_print("\n");
                        */

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

    /// Returns the content length from the CONTENT TLV
    pub fn get_file_size(inode: u32) -> Result<u64, &'static str> {
        unsafe {
            let _ = FS.header.as_ref().ok_or("FS not mounted")?;
        }
        
        // Read the inode entry from the inode table
        let entry = Self::read_inode_entry(inode)?;
        
        // Calculate record location
        let record_block_start = unsafe {
            Self::PARTITION_OFFSET_BLOCKS + (entry.offset / BLOCK_SIZE as u64)
        };
        let offset_in_first_block = (entry.offset % BLOCK_SIZE as u64) as usize;
        
        // Read first block
        let mut block_buffer = vec![0u8; BLOCK_SIZE];
        read_block_from_device(record_block_start, &mut block_buffer)?;
        
        // Parse record size from header (first 4 bytes after offset)
        // Note: The inode points to the start of the record. The first 4 bytes are undefined/padding?
        // Actually, based on lookup_path:
        // offset_in_block+4..8 is the record size.
        // Let's verify this structure.
        
        if offset_in_first_block + 8 > BLOCK_SIZE {
             return Err("Node header crosses block boundary");
        }
        
        // Get record total size
        let mut record_size = u32::from_le_bytes([
            block_buffer[offset_in_first_block+4], block_buffer[offset_in_first_block+5],
            block_buffer[offset_in_first_block+6], block_buffer[offset_in_first_block+7]
        ]) as usize;
        
        let mut explicit_record_scan = false;

        // Fallback: If record_size is 0 (seen in some file inodes that seem to start with [ID][Offset]),
        // or very small, Use manual scan mode on the first block.
        // Also if record_size is suspiciously large (> MAX), it's likely garbage/header mismatch.
        // Dump showed header matches logical 16-byte [ID][Offset] pattern.
        if record_size < 8 || record_size > MAX_RECORD_SIZE {
             crate::serial::serial_print("[FS] get_file_size: Suspicious record size (");
             crate::serial::serial_print_dec(record_size as u64);
             crate::serial::serial_print("), enabling fallback scan\n");
             // Force a scan of the first block size, minus header
             record_size = BLOCK_SIZE; 
             explicit_record_scan = true;
        }
        
        // Removed the error check here since we handle > MAX by falling back to block scan
        
        // Read the record data
        let mut record_data = if explicit_record_scan {
            // Just use the block buffer we already have
            vec![] 
        } else {
             vec![0u8; record_size]
        };
        
        if !explicit_record_scan {
            let first_chunk_size = min(record_size, BLOCK_SIZE - offset_in_first_block);
            record_data[0..first_chunk_size].copy_from_slice(&block_buffer[offset_in_first_block..offset_in_first_block+first_chunk_size]);
            
            let mut bytes_read = first_chunk_size;
            let mut current_block = record_block_start + 1;
            
            while bytes_read < record_size {
                let chunk_size = min(BLOCK_SIZE, record_size - bytes_read);
                read_block_from_device(current_block, &mut block_buffer)?;
                record_data[bytes_read..bytes_read+chunk_size].copy_from_slice(&block_buffer[0..chunk_size]);
                bytes_read += chunk_size;
                current_block += 1;
            }
        }
        
        // Parse TLVs
        // If explicit scan, use block_buffer directly, starting after 16 bytes (suspected header)
        // If normal, use record_data, starting after 8 bytes (standard header)
        
        let (buffer_to_scan, buffer_offset) = if explicit_record_scan {
            (&block_buffer, offset_in_first_block)
        } else {
            (&record_data, 0)
        };
        
        let mut scan_offset = if explicit_record_scan {
             16 // Skip 16 bytes (ID + Offset?)
        } else {
             8 // Skip 8 bytes (Standard Header)
        };
        
        while scan_offset + 6 <= buffer_to_scan.len() - buffer_offset {
             let idx = buffer_offset + scan_offset;
             if idx + 6 > buffer_to_scan.len() { break; }

             let tag = u16::from_le_bytes([
                 buffer_to_scan[idx], buffer_to_scan[idx+1]
             ]);
             
             let length = u32::from_le_bytes([
                 buffer_to_scan[idx+2], buffer_to_scan[idx+3],
                 buffer_to_scan[idx+4], buffer_to_scan[idx+5]
             ]) as usize;
             
             if tag == tlv_tags::CONTENT {
                 return Ok(length as u64);
             }
             
             scan_offset += 6 + length;
        }
        
        // Not found / Empty file
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

    // Framebuffer will be registered by display_service in userspace
}

/// Register a new device
pub fn register_device(name: &str, device_type: DeviceType, driver_pid: u64) -> bool {
    // Disable interrupts to prevent context switch or stack corruption during lock
    x86_64::instructions::interrupts::without_interrupts(|| {
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
    })
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

/// Resource id for "list directory" when opening dev: or dev:/
const DEVDIR_LIST_ID: usize = 0xFFFF;

/// List registered device names (for dev: directory read)
pub fn list_device_names() -> alloc::vec::Vec<alloc::string::String> {
    let registry_lock = DEVICE_REGISTRY.lock();
    if let Some(registry) = registry_lock.as_ref() {
        registry.keys().cloned().collect()
    } else {
        alloc::vec::Vec::new()
    }
}

// --- Framebuffer Structures ---

#[repr(C)]
struct fb_bitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

#[repr(C)]
struct fb_var_screeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: fb_bitfield,
    pub green: fb_bitfield,
    pub blue: fb_bitfield,
    pub transp: fb_bitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    // ... other fields truncated for simplicity
    pub reserved: [u32; 100], 
}

#[repr(C)]
struct fb_fix_screeninfo {
    pub id: [u8; 16],
    pub smem_start: u64,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: u64,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

// --- Redox-style Scheme Implementation ---

use crate::scheme::{Scheme, Stat, error as scheme_error};

/// Open flags (match POSIX / eclipse-syscall)
const O_CREAT: usize = 0x0040;
const O_EXCL: usize = 0x0080;

/// Virtual files under /tmp (in-memory overlay for O_CREAT)
static VIRTUAL_TMP: Mutex<BTreeMap<String, alloc::vec::Vec<u8>>> = Mutex::new(BTreeMap::new());

enum OpenFile {
    Real { inode: u32, offset: u64 },
    Virtual { path: String, offset: u64 },
    Framebuffer,
}

static OPEN_FILES_SCHEME: Mutex<alloc::vec::Vec<Option<OpenFile>>> = Mutex::new(alloc::vec::Vec::new());

pub struct FileSystemScheme;

impl Scheme for FileSystemScheme {
    fn open(&self, path: &str, flags: usize, _mode: u32) -> Result<usize, usize> {
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

        if clean_path == "dev/fb0" {
             let mut open_files = OPEN_FILES_SCHEME.lock();
             for (i, slot) in open_files.iter_mut().enumerate() {
                 if slot.is_none() {
                     *slot = Some(OpenFile::Framebuffer);
                     return Ok(i);
                 }
             }
             let id = open_files.len();
             open_files.push(Some(OpenFile::Framebuffer));
             return Ok(id);
        }

        match Filesystem::lookup_path(clean_path) {
            Ok(inode) => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                for (i, slot) in open_files.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(OpenFile::Real { inode, offset: 0 });
                        return Ok(i);
                    }
                }
                let id = open_files.len();
                open_files.push(Some(OpenFile::Real { inode, offset: 0 }));
                Ok(id)
            }
            Err(_) => {
                let key = String::from(clean_path);
                let is_tmp = clean_path.starts_with("tmp/") || clean_path == "tmp";
                
                if is_tmp {
                    let mut vtmp = VIRTUAL_TMP.lock();
                    // O_CREAT: create file if it doesn't exist
                    if (flags & O_CREAT) != 0 {
                        if (flags & O_EXCL) != 0 && vtmp.contains_key(&key) {
                            return Err(scheme_error::EEXIST);
                        }
                        vtmp.entry(key.clone()).or_insert_with(alloc::vec::Vec::new);
                    }

                    if vtmp.contains_key(&key) {
                        drop(vtmp);
                        let mut open_files = OPEN_FILES_SCHEME.lock();
                        for (i, slot) in open_files.iter_mut().enumerate() {
                            if slot.is_none() {
                                *slot = Some(OpenFile::Virtual { path: key, offset: 0 });
                                return Ok(i);
                            }
                        }
                        let id = open_files.len();
                        open_files.push(Some(OpenFile::Virtual { path: key, offset: 0 }));
                        return Ok(id);
                    }
                }
                
                Err(scheme_error::ENOENT)
            }
        }
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        match open_file {
            OpenFile::Real { inode, offset } => {
                match Filesystem::read_file_by_inode_at(*inode, buffer, *offset) {
                    Ok(bytes_read) => {
                        *offset += bytes_read as u64;
                        Ok(bytes_read)
                    }
                    Err(_) => Err(scheme_error::EIO),
                }
            }
            OpenFile::Virtual { path, offset } => {
                let path_clone = path.clone();
                let off = *offset;
                drop(open_files);
                let vtmp = VIRTUAL_TMP.lock();
                let content = vtmp.get(&path_clone).ok_or(scheme_error::EIO)?;
                let start = off as usize;
                if start >= content.len() {
                    return Ok(0);
                }
                let len = core::cmp::min(buffer.len(), content.len() - start);
                buffer[..len].copy_from_slice(&content[start..start + len]);
                
                let mut open_files = OPEN_FILES_SCHEME.lock();
                if let Some(Some(OpenFile::Virtual { offset: o, .. })) = open_files.get_mut(id) {
                    *o += len as u64;
                }
                Ok(len)
            }
            OpenFile::Framebuffer => Ok(0),
        }
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        match open_file {
            OpenFile::Real { inode, offset } => {
                match Filesystem::write_file_by_inode(*inode, buffer, *offset) {
                    Ok(bytes_written) => {
                        *offset += bytes_written as u64;
                        Ok(bytes_written)
                    }
                    Err(_) => Err(scheme_error::EIO),
                }
            }
            OpenFile::Virtual { path, offset } => {
                let path_clone = path.clone();
                let off = *offset as usize;
                let n = buffer.len();
                drop(open_files);
                let mut vtmp = VIRTUAL_TMP.lock();
                let content = vtmp.get_mut(&path_clone).ok_or(scheme_error::EIO)?;
                let need_len = off + n;
                if content.len() < need_len {
                    content.resize(need_len, 0);
                }
                content[off..off + n].copy_from_slice(buffer);
                drop(vtmp);
                let mut open_files = OPEN_FILES_SCHEME.lock();
                if let Some(Some(OpenFile::Virtual { offset: o, .. })) = open_files.get_mut(id) {
                    *o += n as u64;
                }
                Ok(n)
            }
            OpenFile::Framebuffer => Ok(buffer.len()),
        }
    }

    fn lseek(&self, id: usize, seek_offset: isize, whence: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        let new_offset = match open_file {
            OpenFile::Real { inode, offset } => {
                let size = Filesystem::get_file_size(*inode).map_err(|_| scheme_error::EIO)? as u64;
                let no = match whence {
                    0 => seek_offset as u64,
                    1 => (*offset as isize + seek_offset) as u64,
                    2 => (size as isize + seek_offset) as u64,
                    _ => return Err(scheme_error::EINVAL),
                };
                *offset = no;
                drop(open_files);
                no
            }
            OpenFile::Virtual { path, offset } => {
                let path_clone = path.clone();
                let off = *offset;
                drop(open_files);
                let vtmp = VIRTUAL_TMP.lock();
                let len = vtmp.get(&path_clone).map(|v| v.len() as u64).unwrap_or(0);
                drop(vtmp);
                let no = match whence {
                    0 => seek_offset as u64,
                    1 => (off as isize + seek_offset) as u64,
                    2 => (len as isize + seek_offset) as u64,
                    _ => return Err(scheme_error::EINVAL),
                };
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
                if let OpenFile::Virtual { offset: o, .. } = open_file {
                    *o = no;
                }
                no
            }
            OpenFile::Framebuffer => 0,
        };
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
        
        match open_file {
            OpenFile::Real { inode, .. } => {
                // Initial default stat
                stat.ino = *inode as u64;
                stat.mode = 0o100644; // Default regular file
                stat.blksize = BLOCK_SIZE as u32;
                stat.blocks = 0; // Will be updated if size is known

                // Read the inode record to get full metadata
                match Filesystem::read_inode_entry(*inode) {
                    Ok(entry) => {
                        let record_block_start = unsafe {
                            Filesystem::PARTITION_OFFSET_BLOCKS + (entry.offset / BLOCK_SIZE as u64)
                        };
                        let offset_in_first_block = (entry.offset % BLOCK_SIZE as u64) as usize;
                        let mut block_buffer = vec![0u8; BLOCK_SIZE];
                        
                        if let Ok(_) = read_block_from_device(record_block_start, &mut block_buffer) {
                             // Simple scan for Metadata TLVs in the first block
                             let mut scan_offset = 8; // Skip record header
                             while scan_offset + 6 <= BLOCK_SIZE - offset_in_first_block {
                                 let idx = offset_in_first_block + scan_offset;
                                 let tag = u16::from_le_bytes([block_buffer[idx], block_buffer[idx+1]]);
                                 let length = u32::from_le_bytes([
                                     block_buffer[idx+2], block_buffer[idx+3],
                                     block_buffer[idx+4], block_buffer[idx+5]
                                 ]) as usize;
                                 
                                 match tag {
                                     tlv_tags::SIZE => {
                                          if length == 8 {
                                              stat.size = u64::from_le_bytes([
                                                  block_buffer[idx+6], block_buffer[idx+7],
                                                  block_buffer[idx+8], block_buffer[idx+9],
                                                  block_buffer[idx+10], block_buffer[idx+11],
                                                  block_buffer[idx+12], block_buffer[idx+13]
                                              ]);
                                              stat.blocks = (stat.size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
                                          }
                                     }
                                     tlv_tags::MODE => {
                                          if length == 4 {
                                              stat.mode = u32::from_le_bytes([
                                                  block_buffer[idx+6], block_buffer[idx+7],
                                                  block_buffer[idx+8], block_buffer[idx+9]
                                              ]);
                                          }
                                     }
                                     tlv_tags::MTIME => {
                                          if length == 8 {
                                              stat.mtime = i64::from_le_bytes([
                                                  block_buffer[idx+6], block_buffer[idx+7],
                                                  block_buffer[idx+8], block_buffer[idx+9],
                                                  block_buffer[idx+10], block_buffer[idx+11],
                                                  block_buffer[idx+12], block_buffer[idx+13]
                                              ]);
                                          }
                                     }
                                     tlv_tags::CONTENT => {
                                         stat.size = length as u64;
                                         stat.blocks = (stat.size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
                                     }
                                     _ => {}
                                 }
                                 scan_offset += 6 + length;
                                 if scan_offset >= BLOCK_SIZE { break; }
                             }
                        }
                    }
                    Err(_) => {}
                }
                Ok(0)
            }
            OpenFile::Virtual { path, .. } => {
                let path_clone = path.clone();
                drop(open_files);
                let vtmp = VIRTUAL_TMP.lock();
                stat.size = vtmp.get(&path_clone).map(|v| v.len() as u64).unwrap_or(0);
                Ok(0)
            }
            OpenFile::Framebuffer => {
                let fb_info = &crate::boot::get_boot_info().framebuffer;
                stat.size = (fb_info.pixels_per_scan_line * fb_info.height * 4) as u64;
                Ok(0)
            }
        }
    }

    fn mkdir(&self, path: &str, _mode: u32) -> Result<usize, usize> {
        serial::serial_print("[FS-SCHEME] mkdir(");
        serial::serial_print(path);
        serial::serial_print(")\n");
        
        let clean_path = if path.starts_with('/') { &path[1..] } else { path };
        if clean_path.starts_with("tmp/") || clean_path == "tmp" {
            let mut vtmp = VIRTUAL_TMP.lock();
            vtmp.entry(String::from(clean_path)).or_insert_with(alloc::vec::Vec::new);
            return Ok(0);
        }

        // Stub: fail for other directories if not in /tmp
        Err(scheme_error::EINVAL)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        unsafe { serial::serial_print("DEBUG: ioctl entry\n"); }
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        serial::serial_print("[FS-SCHEME] ioctl request: ");
        serial::serial_print_hex(request as u64);
        serial::serial_print(" for ");
        match open_file {
            OpenFile::Framebuffer => {
                serial::serial_print("Framebuffer\n");
                match request as u32 {
                    0x4600 => { // FBIOGET_VSCREENINFO
                        let fb_info = &crate::boot::get_boot_info().framebuffer;
                        let var_info = unsafe { &mut *(arg as *mut fb_var_screeninfo) };
                        var_info.xres = fb_info.width as u32;
                        var_info.yres = fb_info.height as u32;
                        var_info.xres_virtual = fb_info.width as u32;
                        var_info.yres_virtual = fb_info.height as u32;
                        var_info.bits_per_pixel = 32;
                        var_info.red.offset = 16;
                        var_info.red.length = 8;
                        var_info.green.offset = 8;
                        var_info.green.length = 8;
                        var_info.blue.offset = 0;
                        var_info.blue.length = 8;
                        var_info.transp.offset = 24;
                        var_info.transp.length = 8;
                        Ok(0)
                    }
                    0x4601 => { // FBIOPUT_VSCREENINFO
                        crate::serial::serial_print("[FS-SCHEME] FBIOPUT_VSCREENINFO called (stub)\n");
                        Ok(0)
                    }
                    0x4602 => { // FBIOGET_FSCREENINFO
                        let fb_info = &crate::boot::get_boot_info().framebuffer;
                        let fix_info = unsafe { &mut *(arg as *mut fb_fix_screeninfo) };
                        fix_info.smem_start = fb_info.base_address as u64;
                        fix_info.smem_len = (fb_info.pixels_per_scan_line * fb_info.height * 4) as u32;
                        fix_info.line_length = (fb_info.pixels_per_scan_line * 4) as u32;
                        fix_info.visual = 2; // FB_VISUAL_TRUECOLOR
                        Ok(0)
                    }
                    0x4611 => { // FBIOPAN_DISPLAY — stub OK
                        crate::serial::serial_print("[FS-SCHEME] FBIOPAN_DISPLAY (stub OK)\n");
                        Ok(0)
                    }
                    _ => {
                        crate::serial::serial_print("[FS-SCHEME] Unknown FB ioctl: ");
                        crate::serial::serial_print_hex(request as u64);
                        crate::serial::serial_print("\n");
                        Err(scheme_error::EINVAL)
                    }
                }
            }
            _ => Err(scheme_error::ENOSYS),
        }
    }

    fn fmap(&self, id: usize, _offset: usize, _len: usize) -> Result<usize, usize> {
        let open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        
        match open_file {
            OpenFile::Framebuffer => {
                let fb_info = &crate::boot::get_boot_info().framebuffer;
                if fb_info.base_address == 0 {
                    return Err(scheme_error::EIO);
                }
                Ok(fb_info.base_address as usize)
            }
            OpenFile::Virtual { path, .. } => {
                let vtmp = VIRTUAL_TMP.lock();
                if let Some(content) = vtmp.get(path) {
                    let ptr = content.as_ptr() as u64;
                    if content.is_empty() {
                         return Err(scheme_error::EINVAL);
                    }
                    let phys = crate::memory::virt_to_phys(ptr);
                    Ok(phys as usize)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            _ => Err(scheme_error::ENOSYS),
        }
    }
}

// --- Dev Scheme ---

pub struct DevScheme;

impl Scheme for DevScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        // Remove leading slash if present
        let clean_path = if path.starts_with('/') { &path[1..] } else { path };
        
        // dev: or dev:/ → list directory (device names)
        if clean_path.is_empty() || clean_path == "/" {
            return Ok(DEVDIR_LIST_ID);
        }
        if lookup_device(clean_path).is_some() || clean_path == "keyboard" {
            if clean_path == "fb0" {
                return Ok(100); // Magic ID for fb0
            }
            if clean_path == "keyboard" {
                return Ok(101); // Magic ID for keyboard
            }
            Ok(0)
        } else {
            Err(scheme_error::ENOENT)
        }
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        if id == 100 { // fb0
            // Framebuffer is write-only/ioctl, returned 0 on read
             return Ok(0);
        }
        
        if id == 101 { // keyboard
            if buffer.len() == 0 {
                 return Ok(0);
            }
            
            // Non-blocking read from keyboard buffer
            let key = crate::interrupts::read_key();
            if key != 0 {
                buffer[0] = key;
                return Ok(1);
            } else {
                // If non-blocking, return EAGAIN? Or 0?
                // For now, return 0 (EOF behavior) or maybe block?
                // Simpler: non-blocking, return 0 means no data yet.
                // TinyX might poll.
                return Ok(0);
            }
        }
        
        if id == DEVDIR_LIST_ID {
            let names = list_device_names();
            let mut s = alloc::string::String::new();
            for name in &names {
                s.push_str(name);
                s.push('\n');
            }
            let bytes = s.as_bytes();
            let n = core::cmp::min(buffer.len(), bytes.len());
            buffer[..n].copy_from_slice(&bytes[..n]);
            return Ok(n);
        }
        Ok(0) // Placeholder for device handles
    }

    fn write(&self, _id: usize, _buffer: &[u8]) -> Result<usize, usize> {
        Ok(_buffer.len()) // Placeholder
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        if id == 100 { // fb0
            let fb_info = &crate::boot::get_boot_info().framebuffer;
            stat.size = (fb_info.pixels_per_scan_line * fb_info.height * 4) as u64;
            return Ok(0);
        }
        Ok(0)
    }

    fn fmap(&self, id: usize, _offset: usize, _len: usize) -> Result<usize, usize> {
        if id == 100 { // fb0
            let fb_info = &crate::boot::get_boot_info().framebuffer;
            if fb_info.base_address == 0 {
                return Err(scheme_error::EIO);
            }
            // Return physical address
            return Ok(fb_info.base_address as usize);
        }
        Err(scheme_error::ENOSYS)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        if id == 100 { // fb0
            match request {
                0x4600 => { // FBIOGET_VSCREENINFO
                    let fb_info = &crate::boot::get_boot_info().framebuffer;
                    let var_info = unsafe { &mut *(arg as *mut fb_var_screeninfo) };
                    var_info.xres = fb_info.width as u32;
                    var_info.yres = fb_info.height as u32;
                    var_info.xres_virtual = fb_info.width as u32;
                    var_info.yres_virtual = fb_info.height as u32;
                    var_info.bits_per_pixel = 32;
                    var_info.red.offset = 16;
                    var_info.red.length = 8;
                    var_info.green.offset = 8;
                    var_info.green.length = 8;
                    var_info.blue.offset = 0;
                    var_info.blue.length = 8;
                    var_info.transp.offset = 24;
                    var_info.transp.length = 8;
                    return Ok(0);
                }
                0x4602 => { // FBIOGET_FSCREENINFO
                    let fb_info = &crate::boot::get_boot_info().framebuffer;
                    let fix_info = unsafe { &mut *(arg as *mut fb_fix_screeninfo) };
                    fix_info.smem_start = fb_info.base_address as u64;
                    fix_info.smem_len = (fb_info.pixels_per_scan_line * fb_info.height * 4) as u32;
                    fix_info.line_length = (fb_info.pixels_per_scan_line * 4) as u32;
                    fix_info.visual = 2; // FB_VISUAL_TRUECOLOR
                    return Ok(0);
                }
                0x4601 => { // FBIOPUT_VSCREENINFO — accept any mode change
                    serial::serial_print("DevScheme::ioctl: FBIOPUT_VSCREENINFO (stub OK)\n");
                    return Ok(0);
                }
                0x4611 => { // FBIOPAN_DISPLAY — stub OK
                    serial::serial_print("DevScheme::ioctl: FBIOPAN_DISPLAY (stub OK)\n");
                    return Ok(0);
                }
                _ => {
                    serial::serial_print("DevScheme::ioctl: Unknown request: ");
                    serial::serial_print_hex(request as u64);
                    serial::serial_print("\n");
                    return Err(scheme_error::EINVAL);
                }
            }
        }
        Err(scheme_error::EBADF)
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn mkdir(&self, _path: &str, _mode: u32) -> Result<usize, usize> {
        Err(scheme_error::ENOSYS)
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