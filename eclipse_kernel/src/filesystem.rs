use crate::serial;
use core::cmp::min;
use alloc::vec::Vec;
use alloc::{vec, format};
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use eclipsefs_lib::format::{EclipseFSHeader, InodeTableEntry, tlv_tags, constants};
use eclipsefs_lib::NodeKind;

/// Block size for filesystem operations
pub const BLOCK_SIZE: usize = 4096;

/// Maximum record size to prevent OOM (32 MiB)
pub const MAX_RECORD_SIZE: usize = 32 * 1024 * 1024;

const INODE_CACHE_SIZE: usize = 128;

#[derive(Clone, Copy)]
pub struct InodeCacheEntry {
    inode_id: u32,
    offset: u64,
    valid: bool,
    last_access: u64,
}

struct InodeCache {
    entries: [InodeCacheEntry; INODE_CACHE_SIZE],
    access_counter: u64,
}

impl InodeCache {
    const fn new() -> Self {
        Self {
            entries: [InodeCacheEntry { inode_id: 0, offset: 0, valid: false, last_access: 0 }; INODE_CACHE_SIZE],
            access_counter: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Directory content cache
// ---------------------------------------------------------------------------
// Caches raw TLV record data for directory inodes so that `lookup_path` does
// not re-read the same directory from disk on every path traversal.  From the
// logs, the root (inode 1), /bin (inode 3), /lib (inode 12), /etc (inode 5)
// and several others were re-read on every single open() call.
//
// Cache entry layout: (inode_id, last_access_tick, record_bytes).
// Policy: LRU eviction once DIR_CACHE_SIZE entries are in use.
// ---------------------------------------------------------------------------

const DIR_CACHE_SIZE: usize = 32;

struct DirCacheState {
    entries: Vec<(u32, u64, Vec<u8>)>,
    access_counter: u64,
}

impl DirCacheState {
    const fn new() -> Self {
        Self { entries: Vec::new(), access_counter: 0 }
    }

    fn insert(&mut self, inode_id: u32, data: Vec<u8>) {
        if self.entries.iter().any(|e| e.0 == inode_id) {
            return;
        }
        self.access_counter = self.access_counter.wrapping_add(1);
        let ac = self.access_counter;
        if self.entries.len() < DIR_CACHE_SIZE {
            self.entries.push((inode_id, ac, data));
        } else {
            let victim = self.entries.iter().enumerate()
                .min_by_key(|(_, e)| e.1)
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.entries[victim] = (inode_id, ac, data);
        }
    }
}

static DIR_CACHE: spin::Mutex<DirCacheState> = spin::Mutex::new(DirCacheState::new());

/// Global lock for filesystem operations to prevent SMP race conditions.
/// This protects the static `FS` state and ensures atomicity of `lseek` + `read/write` sequences.
static FILESYSTEM_LOCK: crate::sync::ReentrantMutex<()> = crate::sync::ReentrantMutex::new(());

/// Filesystem state
pub struct Filesystem {
    mounted: bool,
    header: Option<EclipseFSHeader>,
    inode_table_offset: u64,
    disk_scheme_id: usize,
    disk_resource_id: usize,
    partition_offset: u64,
    inode_cache: InodeCache,
}

static mut FS: Filesystem = Filesystem {
    mounted: false,
    header: None,
    inode_table_offset: 0,
    disk_scheme_id: 0,
    disk_resource_id: 0,
    partition_offset: 0,
    inode_cache: InodeCache::new(),
};

/// Read a range of bytes directly from disk, potentially spanning block boundaries.
/// Un solo `lseek` + `scheme::read`: `DiskScheme::read` rellena todo el buffer cruzando bloques.
fn read_bytes_at(abs_offset: u64, dest: &mut [u8]) -> Result<(), &'static str> {
    if dest.is_empty() {
        return Ok(());
    }
    let _lock = FILESYSTEM_LOCK.lock();
    unsafe {
        if let Err(e) =
            crate::scheme::lseek(FS.disk_scheme_id, FS.disk_resource_id, abs_offset as isize, 0)
        {
            serial::serial_printf(format_args!(
                "[FS-DEBUG] read_bytes_at lseek failed: off={} err={}\n",
                abs_offset,
                e
            ));
            return Err("Disk seek error");
        }

        if let Some(pid) = crate::process::current_process_id() {
            let start_b = abs_offset / BLOCK_SIZE as u64;
            let end_b = (abs_offset + dest.len() as u64 - 1) / BLOCK_SIZE as u64;
            for block_num in start_b..=end_b {
                if let Some(mut proc) = crate::process::get_process(pid) {
                    proc.ai_profile.record_block_access(block_num);
                    crate::process::update_process(pid, proc);
                }
            }
        }

        match crate::scheme::read(FS.disk_scheme_id, FS.disk_resource_id, dest) {
            Ok(n) if n == dest.len() => Ok(()),
            Ok(n) => {
                serial::serial_printf(format_args!(
                    "[FS-DEBUG] read_bytes_at short read: off={} want={} got={}\n",
                    abs_offset,
                    dest.len(),
                    n
                ));
                Err("Disk read error")
            }
            Err(e) => {
                serial::serial_printf(format_args!(
                    "[FS-DEBUG] read_bytes_at read failed: off={} err={}\n",
                    abs_offset,
                    e
                ));
                Err("Disk read error")
            }
        }
    }
}

/// Escribe un rango contiguo en el disco (cruza límites de bloque).
/// Un solo `lseek` + `scheme::write`: `DiskScheme::write` reparte en trozos de bloque.
fn write_bytes_at(abs_offset: u64, src: &[u8]) -> Result<(), &'static str> {
    if src.is_empty() {
        return Ok(());
    }
    let _lock = FILESYSTEM_LOCK.lock();
    unsafe {
        if let Err(e) =
            crate::scheme::lseek(FS.disk_scheme_id, FS.disk_resource_id, abs_offset as isize, 0)
        {
            serial::serial_printf(format_args!(
                "[FS-DEBUG] write_bytes_at lseek failed: off={} err={}\n",
                abs_offset,
                e
            ));
            return Err("Disk seek error");
        }

        if let Some(pid) = crate::process::current_process_id() {
            let start_b = abs_offset / BLOCK_SIZE as u64;
            let end_b = (abs_offset + src.len() as u64 - 1) / BLOCK_SIZE as u64;
            for block_num in start_b..=end_b {
                if let Some(mut proc) = crate::process::get_process(pid) {
                    proc.ai_profile.record_block_access(block_num);
                    crate::process::update_process(pid, proc);
                }
            }
        }

        match crate::scheme::write(FS.disk_scheme_id, FS.disk_resource_id, src) {
            Ok(n) if n == src.len() => Ok(()),
            Ok(n) => {
                serial::serial_printf(format_args!(
                    "[FS-DEBUG] write_bytes_at short write: off={} want={} got={}\n",
                    abs_offset,
                    src.len(),
                    n
                ));
                Err("Disk write error")
            }
            Err(e) => {
                serial::serial_printf(format_args!(
                    "[FS-DEBUG] write_bytes_at write failed: off={} err={}\n",
                    abs_offset,
                    e
                ));
                Err("Disk write error")
            }
        }
    }
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
        read_bytes_at(
            part_offset.saturating_mul(BLOCK_SIZE as u64),
            &mut superblock,
        )?;
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
        // serial::serial_printf(format_args!("[FS] read_inode_entry({})\n", inode));
        unsafe {
            let header = FS.header.as_ref().ok_or("FS not mounted")?;
            
            if inode < 1 || inode > header.total_inodes {
                return Err("Inode out of range");
            }
            
            // 1. Check Inode Cache
            for i in 0..INODE_CACHE_SIZE {
                if FS.inode_cache.entries[i].valid && FS.inode_cache.entries[i].inode_id == inode {
                    FS.inode_cache.access_counter += 1;
                    FS.inode_cache.entries[i].last_access = FS.inode_cache.access_counter;
                    return Ok(InodeTableEntry::new(inode as u64, FS.inode_cache.entries[i].offset));
                }
            }

            // Calculate sector for inode entry
            // Inode table starts at inode_table_offset
            // Each entry is 8 bytes.
            // inode indices are 1-based, table is 0-indexed (inode 1 is at index 0)
            let index = (inode - 1) as u64;
            let entry_offset = FS.inode_table_offset + (index * constants::INODE_TABLE_ENTRY_SIZE as u64);
            let abs_disk_offset = entry_offset + (FS.partition_offset * BLOCK_SIZE as u64);
            
            let mut entry_buffer = [0u8; 16];
            read_bytes_at(abs_disk_offset, &mut entry_buffer)?;
            
            let inode_num = u64::from_le_bytes([
                entry_buffer[0], entry_buffer[1], 
                entry_buffer[2], entry_buffer[3],
                entry_buffer[4], entry_buffer[5], 
                entry_buffer[6], entry_buffer[7]
            ]);
            
            let rel_offset = u64::from_le_bytes([
                entry_buffer[8], entry_buffer[9], 
                entry_buffer[10], entry_buffer[11],
                entry_buffer[12], entry_buffer[13], 
                entry_buffer[14], entry_buffer[15]
            ]);
             
            let absolute_offset = header.inode_table_offset + header.inode_table_size + rel_offset;
            
            // 2. Update Inode Cache (LRU)
            let mut victim_idx = 0;
            let mut min_access = u64::MAX;
            let mut found_invalid = false;
            for i in 0..INODE_CACHE_SIZE {
                if !FS.inode_cache.entries[i].valid {
                    victim_idx = i;
                    found_invalid = true;
                    break;
                }
                if FS.inode_cache.entries[i].last_access < min_access {
                    min_access = FS.inode_cache.entries[i].last_access;
                    victim_idx = i;
                }
            }
            
            FS.inode_cache.access_counter += 1;
            FS.inode_cache.entries[victim_idx] = InodeCacheEntry {
                inode_id: inode,
                offset: absolute_offset,
                valid: true,
                last_access: FS.inode_cache.access_counter,
            };

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
        let (data_start_abs, content_length) = Self::get_file_metadata(inode)?;
        
        if offset >= content_length {
            return Ok(0); // EOF
        }
        
        let read_len = min(buffer.len(), (content_length - offset) as usize);
        
        // precise byte offset on disk where requested data starts
        let absolute_data_start = data_start_abs + offset;
        read_bytes_at(absolute_data_start, &mut buffer[..read_len])?;
        
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
        let (_, length) = Self::get_file_metadata(inode)?;
        Ok(length as usize)
    }

    /// Write data to file by inode
    /// 
    /// This is a simplified implementation that writes data to an existing file
    /// without extending it. It modifies the file content in-place.
    /// No materializa el registro completo en un `Vec`: recorre TLVs leyendo cabeceras
    /// de 6 bytes desde disco y hace read-modify-write solo en bloques 4 KiB tocados.
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
        
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        
        let mut header_buf = [0u8; 8];
        read_bytes_at(abs_disk_offset, &mut header_buf)?;
        
        let record_size = u32::from_le_bytes([
            header_buf[4], header_buf[5],
            header_buf[6], header_buf[7]
        ]) as usize;
        
        if record_size > MAX_RECORD_SIZE {
            return Err("File record too large (exceeds MAX_RECORD_SIZE)");
        }
        if record_size < 8 {
            return Err("Invalid record size");
        }

        let (content_data_abs, content_length_u64) = Self::scan_content_tlv_disk(
            abs_disk_offset,
            record_size,
        )?
        .ok_or("No CONTENT TLV found in file")?;
        let content_data_start = (content_data_abs - abs_disk_offset) as usize;
        let content_length = usize::try_from(content_length_u64).map_err(|_| "Content size too large")?;
        
        if offset as usize >= content_length {
            return Err("Write offset beyond file content");
        }
        
        let max_write = min(data.len(), content_length - offset as usize);
        
        if max_write == 0 {
            return Ok(0);
        }

        // Byte offset within the EclipseFS record where user data maps.
        let write_start_in_record = content_data_start
            .checked_add(offset as usize)
            .ok_or("Write offset overflow")?;
        if write_start_in_record
            .checked_add(max_write)
            .map(|e| e > record_size)
            .unwrap_or(true)
        {
            return Err("Write spans past record end");
        }

        let disk_write_start = abs_disk_offset
            .checked_add(write_start_in_record as u64)
            .ok_or("Disk offset overflow")?;

        // RMW solo bloques tocados (sin Vec del tamaño del registro).
        let mut cur = disk_write_start;
        let end = disk_write_start + max_write as u64;
        let mut src = 0usize;
        while cur < end {
            let block_base = (cur / BLOCK_SIZE as u64) * BLOCK_SIZE as u64;
            let off_in_blk = (cur % BLOCK_SIZE as u64) as usize;
            let take = min((end - cur) as usize, BLOCK_SIZE - off_in_blk);

            let mut blk = [0u8; BLOCK_SIZE];
            read_bytes_at(block_base, &mut blk)?;
            blk[off_in_blk..off_in_blk + take].copy_from_slice(&data[src..src + take]);
            write_bytes_at(block_base, &blk)?;

            cur += take as u64;
            src += take;
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
                serial::serial_printf(format_args!("[FS] find_child_in_dir: Found DIRECTORY_ENTRIES tag (len={})\n", length));
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

    /// Get absolute disk offset and size of the file's content
    pub fn get_file_metadata(inode: u32) -> Result<(u64, u64), &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        let entry = Self::read_inode_entry(inode)?;
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        
        let mut header_buf = [0u8; 8];
        read_bytes_at(abs_disk_offset, &mut header_buf)?;
        
        let raw_record_size = u32::from_le_bytes([
            header_buf[4], header_buf[5],
            header_buf[6], header_buf[7]
        ]) as usize;

        // Intento rápido desde disco (cabeceras de 6 B), mismo criterio que `write_file_by_inode`.
        // Si devuelve None o Err, NO marcar el fichero como vacío: imágenes reales pueden tener
        // cadenas TLV donde algún paso choca con `record_size` pero el CONTENT sigue siendo
        // localizable con el escaneo en buffer (que acepta CONTENT aunque el payload no quepa
        // en los 8 KiB del scan).
        if (8..=MAX_RECORD_SIZE).contains(&raw_record_size) {
            if let Ok(Some(found)) = Self::scan_content_tlv_disk(abs_disk_offset, raw_record_size) {
                return Ok(found);
            }
        }

        // Legacy: cabecera inválida, registro > MAX_RECORD_SIZE, o fallback tras disco.
        // Read just enough bytes to scan metadata TLVs and find the CONTENT TLV header.
        let mut explicit_record_scan = false;
        let scan_len = if raw_record_size < 8 {
            explicit_record_scan = true;
            BLOCK_SIZE
        } else if raw_record_size > MAX_RECORD_SIZE {
            2 * BLOCK_SIZE
        } else {
            core::cmp::min(raw_record_size, 2 * BLOCK_SIZE)
        };

        let mut scan_buf = vec![0u8; scan_len];
        read_bytes_at(abs_disk_offset, &mut scan_buf)?;

        // Scan TLVs. IMPORTANT: check the tag BEFORE the value-bounds guard so that
        // large-content files (where CONTENT.length >> scan_buf.len()) are found correctly.
        // We only need the 6-byte CONTENT TLV header to learn the file size; the content
        // bytes themselves are read lazily via scheme::read during mmap/read calls.
        let scan_start = if explicit_record_scan { 16usize } else { 8usize };
        let found = Self::scan_for_content_tlv(&scan_buf, scan_start, abs_disk_offset);
        if found.is_some() {
            return Ok(found.unwrap());
        }

        // Suspicious header: retry from offset 8 as a fallback.
        if explicit_record_scan {
            let found = Self::scan_for_content_tlv(&scan_buf, 8, abs_disk_offset);
            if found.is_some() {
                return Ok(found.unwrap());
            }
        }

        // Fallback for empty files (no CONTENT TLV yet)
        Ok((abs_disk_offset + 8, 0))
    }

    /// Scan en buffer para cabeceras anómalas o `record_size` > MAX_RECORD_SIZE.
    /// En registros normales se usa `scan_content_tlv_disk` desde `get_file_metadata`.
    /// El tag CONTENT se comprueba antes del límite del buffer para ficheros muy grandes.
    fn scan_for_content_tlv(buf: &[u8], start_pos: usize, abs_disk_offset: u64) -> Option<(u64, u64)> {
        let mut tlv_pos = start_pos;
        while tlv_pos + 6 <= buf.len() {
            let tag = u16::from_le_bytes([buf[tlv_pos], buf[tlv_pos + 1]]);
            let length = u32::from_le_bytes([
                buf[tlv_pos + 2], buf[tlv_pos + 3],
                buf[tlv_pos + 4], buf[tlv_pos + 5],
            ]) as usize;
            if tag == tlv_tags::CONTENT {
                // Found: return data start and size. The content bytes themselves are
                // not required to be in `buf`; callers read them via scheme::read.
                let data_start_abs = abs_disk_offset + (tlv_pos + 6) as u64;
                return Some((data_start_abs, length as u64));
            }
            // For non-CONTENT TLVs: if the value extends past the scan buffer the
            // record is either corrupt or this TLV is not a known fixed-size type.
            if tlv_pos + 6 + length > buf.len() {
                break;
            }
            tlv_pos += 6 + length;
        }
        None
    }

    /// Recorre la cadena TLV leyendo solo cabeceras de 6 B desde disco (`read_bytes_at`).
    /// `record_size` es el tamaño total del registro según la cabecera de 8 B.
    /// Devuelve posición absoluta del primer byte del payload `CONTENT` y su longitud.
    fn scan_content_tlv_disk(
        abs_record_start: u64,
        record_size: usize,
    ) -> Result<Option<(u64, u64)>, &'static str> {
        if record_size < 8 {
            return Ok(None);
        }
        let mut pos = 8usize;
        while pos + 6 <= record_size {
            let mut hdr = [0u8; 6];
            read_bytes_at(abs_record_start + pos as u64, &mut hdr)?;
            let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
            let length = u32::from_le_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;

            if length == 0 && tag != tlv_tags::DIRECTORY_ENTRIES && tag != tlv_tags::CONTENT {
                serial::serial_print("[FS] scan_content_tlv_disk: Zero-length TLV, stopping\n");
                return Ok(None);
            }

            let next = pos
                .checked_add(6)
                .and_then(|p| p.checked_add(length))
                .ok_or("Corrupt record TLV chain")?;
            if next > record_size {
                return Err("TLV length exceeds record");
            }

            if tag == tlv_tags::CONTENT {
                let data_abs = abs_record_start + (pos + 6) as u64;
                return Ok(Some((data_abs, length as u64)));
            }
            pos = next;
        }
        Ok(None)
    }

    /// Lee el TLV NODE_TYPE (1=file, 2=directorio, 3=symlink). Sin TLV se asume fichero.
    fn scan_node_type_from_buf(buf: &[u8], start_pos: usize) -> Option<u8> {
        let mut tlv_pos = start_pos;
        while tlv_pos + 6 <= buf.len() {
            let tag = u16::from_le_bytes([buf[tlv_pos], buf[tlv_pos + 1]]);
            let length = u32::from_le_bytes([
                buf[tlv_pos + 2],
                buf[tlv_pos + 3],
                buf[tlv_pos + 4],
                buf[tlv_pos + 5],
            ]) as usize;
            if tag == tlv_tags::NODE_TYPE && length >= 1 && tlv_pos + 6 + length <= buf.len() {
                return Some(buf[tlv_pos + 6]);
            }
            if tlv_pos + 6 + length > buf.len() {
                break;
            }
            tlv_pos += 6 + length;
        }
        None
    }

    pub fn inode_kind(inode: u32) -> Result<NodeKind, &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        let entry = Self::read_inode_entry(inode)?;
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        let mut header_buf = [0u8; 8];
        read_bytes_at(abs_disk_offset, &mut header_buf)?;
        let raw_record_size = u32::from_le_bytes([
            header_buf[4], header_buf[5], header_buf[6], header_buf[7],
        ]) as usize;
        let scan_len = if raw_record_size < 8 {
            BLOCK_SIZE
        } else if raw_record_size > MAX_RECORD_SIZE {
            2 * BLOCK_SIZE
        } else {
            core::cmp::min(raw_record_size, 2 * BLOCK_SIZE)
        };
        let mut scan_buf = vec![0u8; scan_len];
        read_bytes_at(abs_disk_offset, &mut scan_buf)?;
        let start = if raw_record_size < 8 { 16usize } else { 8usize };
        if let Some(k) = Self::scan_node_type_from_buf(&scan_buf, start) {
            return Ok(match k {
                1 => NodeKind::File,
                2 => NodeKind::Directory,
                3 => NodeKind::Symlink,
                _ => NodeKind::File,
            });
        }
        if raw_record_size < 8 {
            if let Some(k) = Self::scan_node_type_from_buf(&scan_buf, 8) {
                return Ok(match k {
                    1 => NodeKind::File,
                    2 => NodeKind::Directory,
                    3 => NodeKind::Symlink,
                    _ => NodeKind::File,
                });
            }
        }
        Ok(NodeKind::File)
    }

    /// Destino UTF-8 del symlink (TLV CONTENT del nodo).
    pub fn read_symlink_target(inode: u32) -> Result<String, &'static str> {
        let len = Self::content_len_by_inode(inode)?;
        if len == 0 {
            return Err("Empty symlink");
        }
        if len > 4096 {
            return Err("Symlink target too long");
        }
        let mut buf = vec![0u8; len];
        let n = Self::read_file_by_inode_at(inode, &mut buf, 0)?;
        buf.truncate(n);
        String::from_utf8(buf).map_err(|_| "Symlink target not UTF-8")
    }

    /// Longitud del payload `CONTENT` (misma ruta que `get_file_metadata`).
    pub fn get_file_size(inode: u32) -> Result<u64, &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        unsafe {
            let _ = FS.header.as_ref().ok_or("FS not mounted")?;
        }
        let (_data_abs, size) = Self::get_file_metadata(inode)?;
        Ok(size)
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

            // Check the directory content cache first to avoid a disk read.
            let cached = {
                let mut dc = DIR_CACHE.lock();
                if let Some(idx) = dc.entries.iter().position(|e| e.0 == current_inode) {
                    dc.access_counter = dc.access_counter.wrapping_add(1);
                    dc.entries[idx].1 = dc.access_counter;
                    Some(dc.entries[idx].2.clone())
                } else {
                    None
                }
            };

            let record_data = if let Some(data) = cached {
                data
            } else {
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

                let mut data = vec![0u8; record_size];
                serial::serial_printf(format_args!("[FS] Reading directory data ({} bytes)...\n", record_size));
                read_bytes_at(abs_disk_offset, &mut data)?;
                serial::serial_print("[FS] Data read complete\n");

                // Cache the record for subsequent lookups of siblings/the same directory.
                DIR_CACHE.lock().insert(current_inode, data.clone());

                data
            };
         
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

/// Lista los nombres de los hijos de un directorio dado por inode.
/// Devuelve un Vec<String> con los nombres (sin ruta), o error si el inode no es directorio.
pub fn list_dir_children_by_inode(inode: u32) -> Result<Vec<String>, &'static str> {
    let _lock = FILESYSTEM_LOCK.lock();
    let entry = Filesystem::read_inode_entry(inode)?;
    let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);

    let mut header_buf = [0u8; 8];
    read_bytes_at(abs_disk_offset, &mut header_buf)?;
    let record_size = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]) as usize;
    if record_size < 8 || record_size > MAX_RECORD_SIZE {
        return Err("Invalid directory record size");
    }

    let mut record_data = vec![0u8; record_size];
    read_bytes_at(abs_disk_offset, &mut record_data)?;

    // Recorrer TLVs buscando DIRECTORY_ENTRIES
    let mut names = Vec::new();
    let data = &record_data[8..]; // saltar cabecera de 8 bytes del record
    let mut tlv_pos = 0usize;
    while tlv_pos + 6 <= data.len() {
        let tag = u16::from_le_bytes([data[tlv_pos], data[tlv_pos + 1]]);
        let length = u32::from_le_bytes([data[tlv_pos+2], data[tlv_pos+3], data[tlv_pos+4], data[tlv_pos+5]]) as usize;
        if tag == tlv_tags::DIRECTORY_ENTRIES && tlv_pos + 6 + length <= data.len() {
            let dir_data = &data[tlv_pos + 6..tlv_pos + 6 + length];
            let mut dir_off = 0usize;
            while dir_off + 8 <= dir_data.len() {
                let name_len = u32::from_le_bytes([
                    dir_data[dir_off], dir_data[dir_off+1],
                    dir_data[dir_off+2], dir_data[dir_off+3]
                ]) as usize;
                // Saltar inode (4 bytes en dir_off+4)
                if name_len == 0 || dir_off + 8 + name_len > dir_data.len() { break; }
                let name_bytes = &dir_data[dir_off + 8..dir_off + 8 + name_len];
                if let Ok(name) = core::str::from_utf8(name_bytes) {
                    names.push(String::from(name));
                }
                dir_off += 8 + name_len;
            }
            return Ok(names);
        }
        if length == 0 { break; }
        tlv_pos += 6 + length;
    }
    Err("Not a directory (no DIRECTORY_ENTRIES TLV found)")
}

/// Lista los hijos de un directorio dado por ruta.
pub fn list_dir_children(path: &str) -> Result<Vec<String>, &'static str> {
    let inode = Filesystem::lookup_path(path)?;
    list_dir_children_by_inode(inode)
}

/// Elimina un archivo.  Actualmente soportado solo para rutas /tmp/*.
pub fn unlink_path(path: &str) -> Result<(), usize> {
    use crate::scheme::error;
    let clean = if path.starts_with('/') { &path[1..] } else { path };
    if clean.starts_with("tmp/") || clean == "tmp" {
        let mut vtmp = VIRTUAL_TMP.lock();
        if vtmp.remove(&String::from(clean)).is_some() {
            Ok(())
        } else {
            Err(error::ENOENT)
        }
    } else {
        Err(error::ENOSYS)
    }
}

/// Crea un directorio.  Actualmente soportado solo bajo /tmp/.
pub fn mkdir_path(path: &str, _mode: u32) -> Result<(), usize> {
    use crate::scheme::error;
    let clean = if path.starts_with('/') { &path[1..] } else { path };
    if clean.starts_with("tmp/") || clean == "tmp" {
        // Guardamos el directorio como entrada vacía en VIRTUAL_TMP.
        let mut vtmp = VIRTUAL_TMP.lock();
        vtmp.entry(String::from(clean)).or_insert_with(alloc::vec::Vec::new);
        Ok(())
    } else {
        Err(error::ENOSYS)
    }
}

/// Resuelve ruta de symlink relativo/absoluto (comportamiento tipo open(2)).
fn resolve_symlink_path(link_path: &str, target: &str) -> String {
    if target.starts_with('/') {
        return String::from(target);
    }
    let link_path = link_path.trim_end_matches('/');
    let parent = if let Some((p, _name)) = link_path.rsplit_once('/') {
        if p.is_empty() {
            "/"
        } else {
            p
        }
    } else {
        "/"
    };
    if parent == "/" {
        alloc::format!("/{}", target)
    } else {
        alloc::format!("{}/{}", parent, target)
    }
}

fn read_file_alloc_inode(inode: u32) -> Result<Vec<u8>, &'static str> {
    let len = Filesystem::content_len_by_inode(inode)?;
    if len == 0 {
        return Err("Empty file");
    }
    // Binarios grandes (p. ej. cargo ~50 MiB): límite propio del contenido, no del TLV de metadatos.
    const MAX_WHOLE_FILE_READ: usize = 128 * 1024 * 1024;
    if len > MAX_WHOLE_FILE_READ {
        return Err("File too large");
    }
    let mut buf = vec![0u8; len];
    let n = Filesystem::read_file_by_inode_at(inode, &mut buf, 0)?;
    buf.truncate(n);
    Ok(buf)
}

/// Leer un fichero completo; sigue cadenas de symlinks (p. ej. `/lib/ld-musl-x86_64.so.1`).
pub fn read_file_alloc(path: &str) -> Result<Vec<u8>, &'static str> {
    read_file_alloc_follow(path, 0)
}

fn read_file_alloc_follow(path: &str, depth: usize) -> Result<Vec<u8>, &'static str> {
    const MAX_SYMLINK_DEPTH: usize = 16;
    if depth >= MAX_SYMLINK_DEPTH {
        return Err("Too many symlink levels");
    }
    let inode = Filesystem::lookup_path(path)?;
    match Filesystem::inode_kind(inode)? {
        NodeKind::Symlink => {
            let target = Filesystem::read_symlink_target(inode)?;
            let next = resolve_symlink_path(path, target.as_str());
            read_file_alloc_follow(next.as_str(), depth + 1)
        }
        NodeKind::Directory => Err("Path is a directory"),
        NodeKind::File => read_file_alloc_inode(inode),
    }
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

#[derive(Clone)]
enum OpenFile {
    Real { 
        inode: u32, 
        offset: u64,
        data_start_abs: u64,
        size: u64,
    },
    Virtual { path: String, offset: u64 },
    Framebuffer,
}

static OPEN_FILES_SCHEME: Mutex<alloc::vec::Vec<Option<OpenFile>>> = Mutex::new(alloc::vec::Vec::new());

pub struct FileSystemScheme;

impl Scheme for FileSystemScheme {
    fn open(&self, path: &str, flags: usize, _mode: u32) -> Result<usize, usize> {
        
        serial::serial_printf(format_args!("[FS-SCHEME] open({})\n", path));

        // Strip *all* leading slashes. `file://foo` yields relative_path `//foo`; removing only
        // one slash left `/foo` and lookup treated `foo` as a child of root. Also, musl may
        // open `//libfoo.so` (bad search-dir join); trimming yields `libfoo.so` so we can retry
        // under `lib/` / `usr/lib/` below.
        let clean_path = path.trim_start_matches('/');

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
                
                // Bootstrap: si /etc/termcap o /etc/inputrc no existen, proveemos versiones sintéticas
                // para que bash y musl funcionen correctamente.
                let mut res = Filesystem::lookup_path(clean_path);
                if res.is_err() {
                    if path == "etc/termcap" || path == "/etc/termcap" || 
                       path == "etc/inputrc" || path == "/etc/inputrc" ||
                       path == "dev/null"    || path == "/dev/null"    ||
                       path == "dev/zero"    || path == "/dev/zero"     {
                        
                        let content = match path {
                            p if p.contains("termcap") => "xterm|xterm-256color:am:bs:km:mi:ms:co#80:li#24:it#8:cl=\\E[H\\E[J:cm=\\E[%i%d;%dH:nd=\\E[C:up=\\E[A:ce=\\E[K:cd=\\E[J:so=\\E[7m:se=\\E[m:md=\\E[1m:me=\\E[m:ti=\\E[?1049h:te=\\E[?1049l:ks=\\E[?1;2l:ke=\\E[?1;2h:kb=\\x08:ku=\\E[A:kd=\\E[B:kl=\\E[D:kr=\\E[C:ho=\\E[H:ks=\\E[?1h\\E=:ke=\\E[?1l\\E>:sc=\\E7:rc=\\E8:al=\\E[L:dl=\\E[M:AL=\\E[%dL:DL=\\E[%dM:ic=\\E[@:dc=\\E[P:IC=\\E[%d@:DC=\\E[%dP:us=\\E[4m:ue=\\E[24m:so=\\E[7m:se=\\E[27m:op=\\E[39;49m:kb=\\x08:kd=\\E[B:kl=\\E[D:kr=\\E[C:ku=\\E[A:le=^H:nd=\\E[C:up=\\E[A:upn=\\E[%dA:dn=\\E[B:dnn=\\E[%dB:le=\\E[D:len=\\E[%dD:ri=\\E[C:rin=\\E[%dC:ho=\\E[H:cr=^M:nl=^J:bl=^G:ta=^I:",
                            p if p.contains("inputrc") => "set bell-style none\nset editing-mode emacs\n",
                            p if p.contains("null")    => "",
                            p if p.contains("zero")    => "\0",
                            _ => ""
                        };
                        
                        let synthetic_path = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
                        let mut vtmp = VIRTUAL_TMP.lock();
                        vtmp.insert(synthetic_path.clone(), content.as_bytes().to_vec());
                        drop(vtmp);
                        
                        // Encontrar un slot libre en OPEN_FILES_SCHEME y devolver su ID
                        let mut open_files = OPEN_FILES_SCHEME.lock();
                        for (i, slot) in open_files.iter_mut().enumerate() {
                            if slot.is_none() {
                                *slot = Some(OpenFile::Virtual { path: synthetic_path, offset: 0 });
                                return Ok(i);
                            }
                        }
                        let id = open_files.len();
                        open_files.push(Some(OpenFile::Virtual { path: synthetic_path, offset: 0 }));
                        return Ok(id);
                    }
                }
                
                let mut inode = match res {
                    Ok(i) => Some(i),
                    Err(_) => None,
                };
                
                // If it's one of our synthetic files, redirect to Virtual
                if inode.is_none() {
                    let vtmp = VIRTUAL_TMP.lock();
                    if vtmp.contains_key(clean_path) {
                        drop(vtmp);
                        let key = String::from(clean_path);
                        let mut open_files = OPEN_FILES_SCHEME.lock();
                        for (i, slot) in open_files.iter_mut().enumerate() {
                            if slot.is_none() {
                                *slot = Some(OpenFile::Virtual { path: key, offset: 0 });
                                return Ok(i);
                            }
                        }
                    }
                }

                if inode.is_none()
                    && !clean_path.contains('/')
                    && clean_path.contains(".so")
                {
                    // e.g. musl opened `//libfoo.so.N` → trimmed `libfoo.so.N`; or missing prefix.
                    if let Ok(i) = Filesystem::lookup_path(&alloc::format!("lib/{clean_path}")) {
                        inode = Some(i);
                    } else if let Ok(i) =
                        Filesystem::lookup_path(&alloc::format!("usr/lib/{clean_path}"))
                    {
                        inode = Some(i);
                    }
                }
                match inode {
                    Some(ino) => {
                        let (data_start, size) =
                            Filesystem::get_file_metadata(ino).map_err(|_| scheme_error::EIO)?;
                        let mut open_files = OPEN_FILES_SCHEME.lock();
                        for (i, slot) in open_files.iter_mut().enumerate() {
                            if slot.is_none() {
                                *slot = Some(OpenFile::Real {
                                    inode: ino,
                                    offset: 0,
                                    data_start_abs: data_start,
                                    size,
                                });
                                return Ok(i);
                            }
                        }
                        let id = open_files.len();
                        open_files.push(Some(OpenFile::Real {
                            inode: ino,
                            offset: 0,
                            data_start_abs: data_start,
                            size,
                        }));
                        Ok(id)
                    }
                    None => Err(scheme_error::ENOENT),
                }
            }
        }
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        match open_file {
            OpenFile::Real { inode: _, offset, data_start_abs, size } => {
                let current_off = *offset;
                let file_size = *size;
                
                if current_off >= file_size {
                    return Ok(0);
                }
                
                let read_len = core::cmp::min(buffer.len(), (file_size - current_off) as usize);
                let abs_disk_read_offset = *data_start_abs + current_off;
                
                // Read directly bypassing TLV scan
                match read_bytes_at(abs_disk_read_offset, &mut buffer[..read_len]) {
                    Ok(_) => {
                        *offset += read_len as u64;
                        Ok(read_len)
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
            OpenFile::Real { inode, offset, .. } => {
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
            OpenFile::Real { inode: _, offset, data_start_abs: _, size } => {
                let file_size = *size;
                let no = match whence {
                    0 => seek_offset as u64,
                    1 => (*offset as isize + seek_offset) as u64,
                    2 => (file_size as isize + seek_offset) as u64,
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

    fn ftruncate(&self, id: usize, len: usize) -> Result<usize, usize> {
        // Resize the in-memory content vector for virtual /tmp files so that
        // subsequent fmap() calls return a non-empty physical address, enabling
        // true MAP_SHARED cross-process shared memory for the SideWind compositor
        // protocol. Without this, the terminal's framebuffer remains 0 bytes
        // and fmap() returns EINVAL, causing every mmap(MAP_SHARED) to fall back
        // to private anonymous frames that are invisible to other processes.
        let open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Virtual { path, .. } => {
                let path_clone = path.clone();
                drop(open_files);
                let mut vtmp = VIRTUAL_TMP.lock();
                if let Some(content) = vtmp.get_mut(&path_clone) {
                    content.resize(len, 0);
                    Ok(0)
                } else {
                    Err(scheme_error::ENOENT)
                }
            }
            _ => Err(scheme_error::ENOSYS),
        }
    }

    fn dup_independent(&self, id: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let existing = open_files
            .get(id)
            .and_then(|s| s.as_ref())
            .ok_or(scheme_error::EBADF)?
            .clone();
        // Reuse the first free slot (but not the same slot as id).
        for (i, slot) in open_files.iter_mut().enumerate() {
            if i != id && slot.is_none() {
                *slot = Some(existing);
                return Ok(i);
            }
        }
        // No free slot found; grow the vector.
        let new_id = open_files.len();
        open_files.push(Some(existing));
        Ok(new_id)
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
            OpenFile::Real { inode, offset: _, data_start_abs: _, size } => {
                stat.ino = *inode as u64;
                stat.size = *size;
                stat.mode = 0o100644; // Default regular file
                stat.blksize = BLOCK_SIZE as u32;
                stat.blocks = (stat.size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
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

    fn unlink(&self, path: &str) -> Result<usize, usize> {
        let clean_path = if path.starts_with('/') { &path[1..] } else { path };
        if clean_path.starts_with("tmp/") || clean_path == "tmp" {
            let mut vtmp = VIRTUAL_TMP.lock();
            if vtmp.remove(&String::from(clean_path)).is_some() {
                return Ok(0);
            }
            return Err(scheme_error::ENOENT);
        }
        Err(scheme_error::ENOSYS)
    }

    fn rename(&self, old_path: &str, new_path: &str) -> Result<usize, usize> {
        let old_key = if old_path.starts_with('/') {
            &old_path[1..]
        } else {
            old_path
        };
        let new_key = if new_path.starts_with('/') {
            &new_path[1..]
        } else {
            new_path
        };
        if !(old_key.starts_with("tmp/") || old_key == "tmp") {
            return Err(scheme_error::ENOSYS);
        }
        if !(new_key.starts_with("tmp/") || new_key == "tmp") {
            return Err(scheme_error::ENOSYS);
        }
        let old_s = String::from(old_key);
        let new_s = String::from(new_key);
        let mut vtmp = VIRTUAL_TMP.lock();
        let data = match vtmp.remove(&old_s) {
            Some(d) => d,
            None => return Err(scheme_error::ENOENT),
        };
        vtmp.insert(new_s.clone(), data);
        drop(vtmp);
        let mut open_files = OPEN_FILES_SCHEME.lock();
        for slot in open_files.iter_mut() {
            if let Some(OpenFile::Virtual { path, .. }) = slot {
                if *path == old_s {
                    *path = new_s.clone();
                }
            }
        }
        Ok(0)
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;

        match open_file {
            OpenFile::Framebuffer => {
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
                        Ok(0)
                    }
                    _ => {
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
        let is_dev = lookup_device(clean_path).is_some() || clean_path == "keyboard" 
            || clean_path == "null" || clean_path == "zero" || clean_path == "tty" 
            || (clean_path.starts_with("tty") && clean_path.len() > 3 && clean_path[3..].chars().all(|c| c.is_ascii_digit()))
            || clean_path == "random" || clean_path == "urandom";

        if is_dev {
            if clean_path == "fb0" {
                return Ok(100); // Magic ID for fb0
            }
            if clean_path == "keyboard" {
                return Ok(101); // Magic ID for keyboard
            }
            if clean_path == "null" {
                return Ok(102);
            }
            if clean_path == "zero" {
                return Ok(103);
            }
            if clean_path == "tty" || (clean_path.starts_with("tty") && clean_path.len() > 3 && clean_path[3..].chars().all(|c| c.is_ascii_digit())) {
                return Ok(104);
            }
            if clean_path == "random" || clean_path == "urandom" {
                return Ok(105);
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
        
        if id == 102 { // null
            return Ok(0);
        }
        
        if id == 103 { // zero
            for b in buffer.iter_mut() {
                *b = 0;
            }
            return Ok(buffer.len());
        }

        if id == 104 { // tty
            // Map /dev/tty roughly to stdin for reading (not quite POSIX, but works for most)
            return Ok(0); // EOF for now
        }

        if id == 105 { // random / urandom
            // Use TSC as a simple entropy source
            let mut tsc: u64;
            for b in buffer.iter_mut() {
                unsafe {
                    core::arch::asm!("rdtsc", out("rax") tsc, out("rdx") _, options(nomem, nostack));
                }
                *b = (tsc & 0xFF) as u8 ^ ((tsc >> 8) & 0xFF) as u8;
            }
            return Ok(buffer.len());
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

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        if id == 102 || id == 103 || id == 105 { // null, zero, random
            return Ok(buffer.len());
        }
        if id == 104 { // tty
            if let Ok(s) = core::str::from_utf8(buffer) {
                crate::serial::serial_print(s);
                return Ok(buffer.len());
            }
        }
        Ok(buffer.len()) // Placeholder for others
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
        if id == 104 { // tty / ttyN  (e.g. /dev/tty0, /dev/tty1)
            // Provide the VT (virtual terminal) ioctls that seatd requires to
            // determine and manage the active VT.  This is a single-VT system so
            // we always report VT 1 as the only active terminal.
            match request {
                0x5603 => {
                    // VT_GETSTATE – fill struct vt_stat { u16 v_active, v_signal, v_state }
                    #[repr(C)]
                    struct VtStat {
                        v_active: u16,
                        v_signal: u16,
                        v_state:  u16,
                    }
                    if arg == 0 { return Err(scheme_error::EFAULT); }
                    let stat = unsafe { &mut *(arg as *mut VtStat) };
                    stat.v_active = 1; // VT 1 is the active terminal
                    stat.v_signal = 0;
                    stat.v_state  = 2; // bitmask: bit 1 = VT 1 open
                    return Ok(0);
                }
                0x5602 => return Ok(0), // VT_SETMODE   – accept silently
                0x5605 => return Ok(0), // VT_RELDISP   – release display, stub ok
                0x5606 => return Ok(0), // VT_ACTIVATE  – activate a VT, stub ok
                0x5607 => return Ok(0), // VT_WAITACTIVE – wait for VT, stub ok
                0x4B3A => {
                    // KDGETMODE – return KD_TEXT = 0
                    if arg == 0 { return Err(scheme_error::EFAULT); }
                    let mode = unsafe { &mut *(arg as *mut u32) };
                    *mode = 0;
                    return Ok(0);
                }
                0x4B3B => return Ok(0), // KDSETMODE   – set KD mode, stub ok
                0x4B44 => {
                    // KDGKBMODE – return K_UNICODE = 3
                    if arg == 0 { return Err(scheme_error::EFAULT); }
                    let mode = unsafe { &mut *(arg as *mut u32) };
                    *mode = 3;
                    return Ok(0);
                }
                0x4B45 => return Ok(0), // KDSKBMODE   – set keyboard mode, stub ok
                0x540E => return Ok(0), // TIOCSCTTY   – set controlling tty, stub ok
                _ => return Err(scheme_error::ENOSYS),
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
/// Get directory children by filesystem scheme resource_id.
/// Returns the list of child names if the resource is a directory, or Err otherwise.
pub fn get_dir_children_by_resource(resource_id: usize) -> Result<alloc::vec::Vec<alloc::string::String>, &'static str> {
    let open_files = OPEN_FILES_SCHEME.lock();
    match open_files.get(resource_id).and_then(|s| s.as_ref()) {
        Some(OpenFile::Real { inode, .. }) => {
            let ino = *inode;
            drop(open_files);
            list_dir_children_by_inode(ino)
        }
        Some(OpenFile::Virtual { path, .. }) => {
            // Virtual paths under /tmp: list VIRTUAL_TMP keys with this prefix.
            let prefix = path.clone();
            drop(open_files);
            let vtmp = VIRTUAL_TMP.lock();
            let children: alloc::vec::Vec<alloc::string::String> = vtmp.keys()
                .filter(|k| {
                    if prefix == "tmp" {
                        // List direct children of /tmp
                        let rest = k.strip_prefix("tmp/").unwrap_or("");
                        !rest.is_empty() && !rest.contains('/')
                    } else {
                        k.starts_with(&alloc::format!("{}/", prefix))
                            && !k[prefix.len()+1..].contains('/')
                    }
                })
                .map(|k| {
                    let slash_pos = k.rfind('/').map(|p| p + 1).unwrap_or(0);
                    alloc::string::String::from(&k[slash_pos..])
                })
                .collect();
            Ok(children)
        }
        _ => Err("not a directory or not open"),
    }
}

/// Get the inode for a filesystem scheme resource, if it's a Real file.
pub fn get_resource_inode(resource_id: usize) -> Option<u32> {
    let open_files = OPEN_FILES_SCHEME.lock();
    match open_files.get(resource_id).and_then(|s| s.as_ref()) {
        Some(OpenFile::Real { inode, .. }) => Some(*inode),
        _ => None,
    }
}
