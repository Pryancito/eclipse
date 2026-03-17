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

/// Global lock for filesystem operations to prevent SMP race conditions.
/// This protects the static `FS` state and ensures atomicity of `lseek` + `read/write` sequences.
static FILESYSTEM_LOCK: crate::sync::ReentrantMutex<()> = crate::sync::ReentrantMutex::new(());

/// Read a block from the underlying block device using the scheme system
fn read_block_from_device(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    // The caller of this internal helper should already hold FILESYSTEM_LOCK
    // if it's coordinating multiple related I/O ops, but for safety we also lock here.
    let _lock = FILESYSTEM_LOCK.lock();
    unsafe {
        if !FS.mounted && FS.disk_resource_id == 0 && FS.disk_scheme_id == 0 {
            // During mount, we might not have a handle yet.
        }

        let offset = block_num * 4096;
        if let Err(e) = crate::scheme::lseek(FS.disk_scheme_id, FS.disk_resource_id, offset as isize, 0) {
            serial::serial_printf(format_args!("[FS-DEBUG] read_block lseek failed: block={} err={}\n", block_num, e));
            return Err("Disk seek error");
        }
        
        match crate::scheme::read(FS.disk_scheme_id, FS.disk_resource_id, buffer) {
            Ok(_) => Ok(()),
            Err(e) => {
                serial::serial_printf(format_args!("[FS-DEBUG] read_block read failed: block={} err={}\n", block_num, e));
                Err("Disk read error")
            }
        }
    }
}

/// Write a block to the underlying block device using the scheme system
fn write_block_to_device(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    let _lock = FILESYSTEM_LOCK.lock();
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
    partition_offset: u64,
}

static mut FS: Filesystem = Filesystem {
    mounted: false,
    header: None,
    inode_table_offset: 0,
    disk_scheme_id: 0,
    disk_resource_id: 0,
    partition_offset: 0,
};

/// Read a range of bytes directly from disk, potentially spanning block boundaries.
fn read_bytes_at(abs_offset: u64, dest: &mut [u8]) -> Result<(), &'static str> {
    let mut bytes_read = 0;
    let mut block_buffer = vec![0u8; BLOCK_SIZE];
    
    while bytes_read < dest.len() {
        let current_pos = abs_offset + bytes_read as u64;
        let block_num = current_pos / BLOCK_SIZE as u64;
        let offset_in_block = (current_pos % BLOCK_SIZE as u64) as usize;
        
        read_block_from_device(block_num, &mut block_buffer)?;
        
        let chunk_size = min(dest.len() - bytes_read, BLOCK_SIZE - offset_in_block);
        dest[bytes_read..bytes_read + chunk_size].copy_from_slice(&block_buffer[offset_in_block..offset_in_block + chunk_size]);
        
        bytes_read += chunk_size;
    }
    
    Ok(())
}

impl Filesystem {
    /// Partition offset in 4096-byte blocks.
    /// - When mounted as disk:NpM the DiskScheme already applies the partition
    ///   byte offset, so we must start reading at block 0 of the partition view.
    /// - When mounted as disk:N (whole disk), EclipseFS historically starts at
    ///   block 25856 (101 MiB).
    pub const PARTITION_OFFSET_BLOCKS_DEFAULT: u64 = 25856;
    pub const PARTITION_OFFSET_BLOCKS_PART:    u64 = 0;
    pub fn mount(device_path: &str) -> Result<(), &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        // Determine partition offset:
        //   disk:NpM  → DiskScheme already seeks to the partition start → offset = 0
        //   disk:N@O  → DiskScheme already seeks to block O → offset = 0
        //   disk:N    → whole disk, EclipseFS historically at block 25856
        let has_explicit_offset = (device_path.contains('p') || device_path.contains('@')) &&
            device_path.find(':').map(|i| {
                let suffix = &device_path[i+1..];
                suffix.contains('p') || suffix.contains('@')
            }).unwrap_or(false);
        let part_offset = if has_explicit_offset {
            Self::PARTITION_OFFSET_BLOCKS_PART    // 0
        } else {
            Self::PARTITION_OFFSET_BLOCKS_DEFAULT // 25856
        };

    unsafe {
        if FS.mounted {
            return Err("Filesystem already mounted");
        }
        
        serial::serial_print("[FS] Attempting to mount eclipsefs...\n");

        // Open device via scheme registry
        serial::serial_print("[FS] Opening ");
        serial::serial_print(device_path);
        serial::serial_print(" via scheme registry...\n");
        match crate::scheme::open(device_path, 0, 0) {
            Ok((s_id, r_id)) => {
                FS.disk_scheme_id = s_id;
                FS.disk_resource_id = r_id;
                FS.partition_offset = part_offset;
                serial::serial_print("[FS] Disk handle opened successfully\n");
            }
            Err(e) => {
                serial::serial_print("[FS] Failed to open device: ");
                serial::serial_print(device_path);
                serial::serial_print(". Error code: ");
                serial::serial_print_dec(e as u64);
                serial::serial_print("\n");
                return Err("Failed to open disk device");
            }
        }
        
        serial::serial_print("[FS] Allocating superblock buffer...\n");
        // Use heap to avoid stack overflow
        let mut superblock = vec![0u8; 4096];
        
        serial::serial_print("[FS] Reading superblock from block device...\n");
        read_block_from_device(part_offset, &mut superblock)?;
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
                
                serial::serial_print("[FS] Filesystem mounted successfully.\n");

                Ok(())
            },
            Err(e) => {
                serial::serial_print("[FS] Invalid EclipseFS header: ");
                serial::serial_print_dec(e as u64);
                serial::serial_print("\n");
                // Close the handle we just opened so the caller can retry on
                // the next disk without leaking open-file slots.
                let _ = crate::scheme::close(FS.disk_scheme_id, FS.disk_resource_id);
                FS.disk_scheme_id = 0;
                FS.disk_resource_id = 0;
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
            let abs_disk_offset = entry_offset + (FS.partition_offset * BLOCK_SIZE as u64);
            
            let mut entry_buffer = [0u8; 8];
            read_bytes_at(abs_disk_offset, &mut entry_buffer)?;
            
            let inode_num = u32::from_le_bytes([
                entry_buffer[0], entry_buffer[1], 
                entry_buffer[2], entry_buffer[3]
            ]) as u64;
            
            let rel_offset = u32::from_le_bytes([
                entry_buffer[4], entry_buffer[5], 
                entry_buffer[6], entry_buffer[7]
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
        let _lock = FILESYSTEM_LOCK.lock();
        let entry = Self::read_inode_entry(inode).map_err(|e| {
            serial::serial_print("[FS-DEBUG] read_file_by_inode_at: read_inode_entry failed inode=");
            serial::serial_print_dec(inode as u64);
            serial::serial_print("\n");
            e
        })?;
        
        // Read the first 8 bytes of the node record to find record_size
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        
        let mut header_buf = [0u8; 8];
        read_bytes_at(abs_disk_offset, &mut header_buf)?;
        
        let record_size = u32::from_le_bytes([
            header_buf[4], header_buf[5],
            header_buf[6], header_buf[7]
        ]) as usize;

        // Parse TLVs to find CONTENT.
        // TLV metadata we care about usually fits in the first few blocks.
        let scan_len = min(record_size, 2 * BLOCK_SIZE);
        let mut scan_buf = vec![0u8; scan_len];
        read_bytes_at(abs_disk_offset, &mut scan_buf)?;
        
        let record_data = &scan_buf;
        let mut tlv_pos = 8usize; // Skip 8-byte record header

        let mut content_start_offset_rel = 0usize;
        let mut content_length = 0usize;
        let mut found = false;

        while tlv_pos + 6 <= record_size && tlv_pos + 6 <= record_data.len() {
            let tag = u16::from_le_bytes([record_data[tlv_pos], record_data[tlv_pos + 1]]);
            let length = u32::from_le_bytes([
                record_data[tlv_pos + 2], record_data[tlv_pos + 3],
                record_data[tlv_pos + 4], record_data[tlv_pos + 5],
            ]) as usize;
            if tag == tlv_tags::CONTENT {
                content_start_offset_rel = (tlv_pos + 6) as usize;
                content_length = length;
                found = true;
                break;
            }
            tlv_pos += 6 + length;
        }
        
        if !found {
            serial::serial_print("[FS-DEBUG] read_file_by_inode_at: CONTENT TLV not found inode=");
            serial::serial_print_dec(inode as u64);
            serial::serial_print("\n");
            return Err("CONTENT TLV not found");
        }
        
        if offset >= content_length as u64 {
            return Ok(0); // EOF
        }
        
        let read_len = min(buffer.len(), content_length - offset as usize);
        
        // precise byte offset on disk where requested data starts
        let absolute_data_start = entry.offset + content_start_offset_rel as u64 + offset;
        
        // Read data from disk
        let abs_disk_data_offset = absolute_data_start + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        read_bytes_at(abs_disk_data_offset, &mut buffer[..read_len])?;
        
        Ok(read_len)
    }

    /// Backwards compatibility wrapper
    pub fn read_file_by_inode(inode: u32, buffer: &mut [u8]) -> Result<usize, &'static str> {
        Self::read_file_by_inode_at(inode, buffer, 0)
    }

    /// Obtener la longitud (en bytes) del TLV `CONTENT` de un inode.
    ///
    /// Útil para cargar binarios completos (p.ej. servicios en /sbin) sin
    /// depender de binarios embebidos en el kernel.
    pub fn content_len_by_inode(inode: u32) -> Result<usize, &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        let entry = Self::read_inode_entry(inode)?;

        // Read the first 8 bytes of the node record to find record_size
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        
        let mut header_buf = [0u8; 8];
        read_bytes_at(abs_disk_offset, &mut header_buf)?;
        
        let record_size = u32::from_le_bytes([
            header_buf[4], header_buf[5],
            header_buf[6], header_buf[7]
        ]) as usize;

        // Escaneo robusto: leer hasta dos bloques para TLVs.
        let scan_len = min(record_size, 2 * BLOCK_SIZE);
        let mut scan_buf = vec![0u8; scan_len];
        read_bytes_at(abs_disk_offset, &mut scan_buf)?;

        let record_data = &scan_buf;
        let mut tlv_pos = 8usize;
        while tlv_pos + 6 <= record_size && tlv_pos + 6 <= record_data.len() {
            let tag = u16::from_le_bytes([record_data[tlv_pos], record_data[tlv_pos + 1]]);
            let length = u32::from_le_bytes([
                record_data[tlv_pos + 2],
                record_data[tlv_pos + 3],
                record_data[tlv_pos + 4],
                record_data[tlv_pos + 5],
            ]) as usize;
            if tag == tlv_tags::CONTENT {
                return Ok(length);
            }
            tlv_pos += 6 + length;
        }

        Err("CONTENT TLV not found")
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
        let _lock = FILESYSTEM_LOCK.lock();
        let entry = Self::read_inode_entry(inode)?;
        
        // Read the full node record
        let block_num = (entry.offset / BLOCK_SIZE as u64) + unsafe { FS.partition_offset };
        let offset_in_block = (entry.offset % BLOCK_SIZE as u64) as usize;
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        
        let mut header_buf = [0u8; 8];
        read_bytes_at(abs_disk_offset, &mut header_buf)?;
        
        let record_size = u32::from_le_bytes([
            header_buf[4], header_buf[5],
            header_buf[6], header_buf[7]
        ]) as usize;
        
        // OOM Protection
        if record_size > MAX_RECORD_SIZE {
            return Err("File record too large (exceeds MAX_RECORD_SIZE)");
        }
        
        let mut record_data = vec![0u8; record_size];
        read_bytes_at(abs_disk_offset, &mut record_data)?;
        
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
        let mut records_scanned = 0;
        
        while offset + 6 <= data.len() {
             let tag = u16::from_le_bytes([data[offset], data[offset+1]]);
             let length = u32::from_le_bytes([
                data[offset+2], data[offset+3], data[offset+4], data[offset+5]
            ]) as usize;
            
            // Loop Guard: Prevent infinite loops on corrupted disk data
            if length == 0 && tag != tlv_tags::DIRECTORY_ENTRIES {
                serial::serial_print("[FS] find_child_in_dir: Zero-length TLV found, stopping scan\n");
                break;
            }

            offset += 6;
            
            if offset + length > data.len() {
                serial::serial_printf(format_args!("[FS] find_child_in_dir: TLV length {} exceeds remaining data size {}\n", length, data.len() - (offset - 6)));
                break; 
            }

            if tag == tlv_tags::DIRECTORY_ENTRIES {
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
                        if name == target_name {
                            serial::serial_printf(format_args!("[FS] find_child_in_dir: Found '{}' after {} records\n", target_name, records_scanned));
                            return Some(child_inode);
                        }
                    }
                    
                    if name_len == 0 && dir_data.len() > dir_offset + 8 {
                        // Avoid infinite loop if name_len is 0 but we aren't at the end
                        dir_offset += 8;
                    } else {
                        dir_offset += 8 + name_len;
                    }
                    records_scanned += 1;
                }
            }
            offset += length;
        }
        None
    }

    /// Returns the content length from the CONTENT TLV
    pub fn get_file_size(inode: u32) -> Result<u64, &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        unsafe {
            let _ = FS.header.as_ref().ok_or("FS not mounted")?;
        }
        
        // Read the inode entry from the inode table
        let entry = Self::read_inode_entry(inode)?;
        // Calculate record location
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        
        let mut header_buf = [0u8; 8];
        if let Err(e) = read_bytes_at(abs_disk_offset, &mut header_buf) {
            return Err(e);
        }
        
        // Get record total size
        let mut record_size = u32::from_le_bytes([
            header_buf[4], header_buf[5],
            header_buf[6], header_buf[7]
        ]) as usize;
        
        let mut explicit_record_scan = false;

        if record_size < 8 || record_size > MAX_RECORD_SIZE {
             crate::serial::serial_print("[FS] get_file_size: Suspicious record size (");
             crate::serial::serial_print_dec(record_size as u64);
             crate::serial::serial_print("), enabling fallback scan\n");
             // Force a scan of the first block size, minus header
             record_size = BLOCK_SIZE; 
             explicit_record_scan = true;
        }
        
        // Read the record data
        let mut record_data = vec![0u8; record_size];
        if let Err(e) = read_bytes_at(abs_disk_offset, &mut record_data) {
            return Err(e);
        }
        
        // Parse TLVs
        // If explicit scan, use record_data directly, starting after 16 bytes (suspected header)
        // If normal, use record_data, starting after 8 bytes (standard header)
        
        let (buffer_to_scan, buffer_offset) = if explicit_record_scan {
            (&record_data, 0) // read_bytes_at already read from abs_disk_offset
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
        let _lock = FILESYSTEM_LOCK.lock();
        unsafe {
            if !FS.mounted {
                return Err("Filesystem not mounted during lookup");
            }
        }
        
        serial::serial_printf(format_args!("[FS] lookup_path('{}')\n", path));

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
            serial::serial_printf(format_args!("[FS] Looking up '{}' in inode {}\n", part, current_inode));

            let entry = Self::read_inode_entry(current_inode)?;
            let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
            
            let mut header_buf = [0u8; 8];
            read_bytes_at(abs_disk_offset, &mut header_buf)?;
            
            let record_size = u32::from_le_bytes([
                header_buf[4], header_buf[5],
                header_buf[6], header_buf[7]
            ]) as usize;
            
            serial::serial_printf(format_args!("[FS] Dir record={} offset=0x{:x} size={}\n", current_inode, abs_disk_offset, record_size));

            if record_size < 8 {
                serial::serial_printf(format_args!("[FS] Error: Record {} too small ({})\n", current_inode, record_size));
                return Err("Invalid directory record size (too small)");
            }
            
            if record_size > MAX_RECORD_SIZE {
                 serial::serial_printf(format_args!("[FS] Error: Record {} too large ({})\n", current_inode, record_size));
                 return Err("Directory record too large");
            }
            
            let mut record_data = vec![0u8; record_size];
            serial::serial_printf(format_args!("[FS] Reading directory data ({} bytes)...\n", record_size));
            read_bytes_at(abs_disk_offset, &mut record_data)?;
            serial::serial_print("[FS] Data read complete\n");
         
            if let Some(inode) = Self::find_child_in_dir(&record_data[8..], part) {
                serial::serial_printf(format_args!("[FS] Found '{}' -> inode {}\n", part, inode));
                current_inode = inode;
            } else {
                serial::serial_printf(format_args!("[FS] Child '{}' not found in directory\n", part));
                return Err("File not found");
            }
        }
        
        Ok(current_inode)
    }
}


/// Initialize the filesystem subsystem and mount the root filesystem.
///
/// Auto-scans **all** registered block devices (disk:0, disk:1, …) and mounts
/// the first one that contains a valid EclipseFS superblock.  This makes the
/// kernel work correctly in QEMU (typically disk:0), on real hardware (typically
/// disk:1 when disk:0 is the EFI system partition), and in any other disk layout
/// without any hard-coded index.
pub fn init() {
    serial::serial_print("[FS] Initializing filesystem subsystem...\n");
    crate::bcache::init();
    crate::scheme::register_scheme("file", alloc::sync::Arc::new(FileSystemScheme));
    crate::scheme::register_scheme("dev", alloc::sync::Arc::new(DevScheme));

    // The disk: scheme must already be registered before we reach here.
    let device_count = crate::storage::device_count();
    serial::serial_print("[FS] Available storage devices: ");
    serial::serial_print_dec(device_count as u64);
    serial::serial_print("\n");

    if device_count == 0 {
        serial::serial_print("[FS] WARNING: No storage devices found. Root filesystem NOT mounted.\n");
        return;
    }

    // Try each registered disk in order until we find one with a valid EclipseFS
    // header.  We build the path on the stack as a fixed-size byte array so we
    // don't need alloc at this early stage.
    // Try each registered disk and its partitions in order until we find a root.
    let mut mounted = false;
    for idx in 0..device_count {
        // We will try these patterns for each disk:
        // 1. "disk:N" (Legacy offset 101MiB)
        // 2. "disk:Np1", "disk:Np2", "disk:Np3", "disk:Np4" (GPT partitions)
        
        for variant in 0..=4 {
            let mut path_buf = [0u8; 12]; // "disk:Npx" + NUL
            let path_str = if variant == 0 {
                // Case 0: disk:N
                format_into_buf(&mut path_buf, "disk:", idx as u64, None)
            } else {
                // Case 1..4: disk:NpM
                format_into_buf(&mut path_buf, "disk:", idx as u64, Some(variant as u64))
            };

            serial::serial_print("[FS] Probing ");
            serial::serial_print(path_str);
            serial::serial_print("... ");

            match mount_root(path_str) {
                Ok(()) => {
                    mounted = true;
                    break;
                }
                Err(_) => {
                }
            }
        }
        
        if mounted { break; }
    }

    if !mounted {
        serial::serial_print("[FS] ERROR: System will continue without a root filesystem.\n");
    }
}

/// Helper to format "disk:N" or "disk:NpM" without heavy formatting machinery
fn format_into_buf<'a>(buf: &'a mut [u8], prefix: &str, n: u64, p: Option<u64>) -> &'a str {
    let mut pos = 0;
    // Copy prefix
    for b in prefix.as_bytes() {
        buf[pos] = *b;
        pos += 1;
    }
    // Copy N
    pos = write_decimal(buf, pos, n);
    // Copy pM if present
    if let Some(p_val) = p {
        buf[pos] = b'p';
        pos += 1;
        pos = write_decimal(buf, pos, p_val);
    }
    core::str::from_utf8(&buf[..pos]).unwrap_or("?")
}

fn write_decimal(buf: &mut [u8], mut pos: usize, mut n: u64) -> usize {
    if n == 0 {
        buf[pos] = b'0';
        return pos + 1;
    }
    let start = pos;
    while n > 0 {
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        pos += 1;
    }
    // Reverse digits
    let end = pos;
    let mut i = start;
    let mut j = end - 1;
    while i < j {
        buf.swap(i, j);
        i += 1;
        j -= 1;
    }
    end
}


/// Mount the root filesystem
pub fn mount_root(device_path: &str) -> Result<(), &'static str> {
    Filesystem::mount(device_path)
}

/// Read a file from the filesystem
pub fn read_file(path: &str, buffer: &mut [u8]) -> Result<usize, &'static str> {
    // 1. Lookup inode
    let inode = Filesystem::lookup_path(path)?;
    // 2. Read file
    Filesystem::read_file_by_inode(inode, buffer)
}

/// Leer un fichero completo asignando un buffer del tamaño exacto del TLV `CONTENT`.
pub fn read_file_alloc(path: &str) -> Result<Vec<u8>, &'static str> {
    let inode = Filesystem::lookup_path(path)?;
    let len = Filesystem::content_len_by_inode(inode)?;
    if len == 0 {
        return Err("Empty file");
    }
    if len > MAX_RECORD_SIZE {
        return Err("File too large (exceeds MAX_RECORD_SIZE)");
    }
    let mut buf = vec![0u8; len];
    let n = Filesystem::read_file_by_inode_at(inode, &mut buf, 0)?;
    buf.truncate(n);
    Ok(buf)
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
        
        serial::serial_printf(format_args!("[FS-SCHEME] open({})\n", path));

        // Clean path to remove leading slash if present
        let clean_path = if path.starts_with('/') { &path[1..] } else { path };

        match clean_path {
            p if p == "dev/fb0" => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                for (i, slot) in open_files.iter_mut().enumerate() {
                    if slot.is_none() {
                        *slot = Some(OpenFile::Framebuffer);
                        return Ok(i);
                    }
                }
                let id = open_files.len();
                open_files.push(Some(OpenFile::Framebuffer));
                Ok(id)
            },
            p if p == "tmp" || p.starts_with("tmp/") => {
                let key = String::from(clean_path);
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
                    Ok(id)
                } else {
                    Err(scheme_error::ENOENT)
                }
            },
            _ => {
                // Real filesystem path - requires mount
                if !is_mounted() {
                    serial::serial_print("[FS-SCHEME] open() failed: physical path requires mount\n");
                    return Err(scheme_error::EIO);
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
                    Err(_) => Err(scheme_error::ENOENT)
                }
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
                            FS.partition_offset + (entry.offset / BLOCK_SIZE as u64)
                        };
                        let offset_in_first_block = (entry.offset % BLOCK_SIZE as u64) as usize;
                        let mut block_buffer = vec![0u8; BLOCK_SIZE];
                        
                        if let Ok(_) = read_block_from_device(record_block_start, &mut block_buffer) {
                             // Read two blocks into a contiguous buffer to handle TLV headers
                             // that may span a block boundary.
                             let mut scan_buf = vec![0u8; 2 * BLOCK_SIZE];
                             scan_buf[..BLOCK_SIZE].copy_from_slice(&block_buffer);
                             if let Err(_) = read_block_from_device(record_block_start + 1, &mut scan_buf[BLOCK_SIZE..]) {
                                 serial::serial_print("[FS-DEBUG] fstat: read_block(second) failed, TLV scan limited to first block\n");
                             }
                             // Record data begins at offset_in_first_block within scan_buf.
                             let record_data = &scan_buf[offset_in_first_block..];
                             let mut scan_offset = 8usize; // Skip record header
                             while scan_offset + 6 <= record_data.len() {
                                 let tag = u16::from_le_bytes([record_data[scan_offset], record_data[scan_offset+1]]);
                                 let length = u32::from_le_bytes([
                                     record_data[scan_offset+2], record_data[scan_offset+3],
                                     record_data[scan_offset+4], record_data[scan_offset+5]
                                 ]) as usize;

                                 match tag {
                                     tlv_tags::SIZE => {
                                          if scan_offset + 6 + 8 <= record_data.len() {
                                              stat.size = u64::from_le_bytes([
                                                  record_data[scan_offset+6], record_data[scan_offset+7],
                                                  record_data[scan_offset+8], record_data[scan_offset+9],
                                                  record_data[scan_offset+10], record_data[scan_offset+11],
                                                  record_data[scan_offset+12], record_data[scan_offset+13]
                                              ]);
                                              stat.blocks = (stat.size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
                                          }
                                     }
                                     tlv_tags::MODE => {
                                          if scan_offset + 6 + 4 <= record_data.len() {
                                              stat.mode = u32::from_le_bytes([
                                                  record_data[scan_offset+6], record_data[scan_offset+7],
                                                  record_data[scan_offset+8], record_data[scan_offset+9]
                                              ]);
                                          }
                                     }
                                     tlv_tags::MTIME => {
                                          if scan_offset + 6 + 8 <= record_data.len() {
                                              stat.mtime = i64::from_le_bytes([
                                                  record_data[scan_offset+6], record_data[scan_offset+7],
                                                  record_data[scan_offset+8], record_data[scan_offset+9],
                                                  record_data[scan_offset+10], record_data[scan_offset+11],
                                                  record_data[scan_offset+12], record_data[scan_offset+13]
                                              ]);
                                          }
                                     }
                                     tlv_tags::CONTENT => {
                                         stat.size = length as u64;
                                         stat.blocks = (stat.size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
                                         break; // CONTENT is the last TLV we care about
                                     }
                                     _ => {}
                                 }
                                 scan_offset += 6 + length;
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