use crate::serial;
use core::cmp::min;
use alloc::vec::Vec;
use alloc::vec;
use alloc::string::String;
use eclipsefs_lib::format::{EclipseFSHeader, InodeTableEntry, tlv_tags, constants};
use eclipsefs_lib::NodeKind;

/// Block size for filesystem operations
pub const BLOCK_SIZE: usize = 4096;

/// Maximum record size to prevent OOM (32 MiB)
pub const MAX_RECORD_SIZE: usize = 32 * 1024 * 1024;

/// Maximum size for an in-kernel virtual file (/tmp, /run).
/// These files are backed by a Vec<u8> on the kernel heap (256 MiB total).
/// Capping at 64 MiB prevents a single ftruncate/write from exhausting the
/// heap and triggering an allocation panic (e.g. a Wayland compositor
/// requesting a 128 MiB shared-memory pool for an 8 K display).
pub const MAX_VIRTUAL_FILE_SIZE: usize = 64 * 1024 * 1024;

/// Tope de bytes de contenido para `read_file_alloc_inode` (comparación con padding del allocator).
/// Debe ser ≤ [`MAX_RECORD_SIZE`]. Publicado para `invariants` y tests host.
pub const READ_FILE_ALLOC_MAX_CONTENT: usize = 32 * 1024 * 1024;

const INODE_CACHE_SIZE: usize = 128;

#[derive(Debug, Clone)]
pub struct NodeMetadata {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, Copy)]
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

/// In-memory map for LFS: inode_id -> disk_offset (bytes)
static INODE_MAP: spin::Mutex<alloc::collections::BTreeMap<u32, u64>> = spin::Mutex::new(alloc::collections::BTreeMap::new());

/// Global lock for filesystem operations to prevent SMP race conditions.
/// This protects the static `FS` state and ensures atomicity of `lseek` + `read/write` sequences.
static FILESYSTEM_LOCK: crate::sync::ReentrantMutex<()> = crate::sync::ReentrantMutex::new(());

/// B-Tree Cache for directory indexing: inode_id -> BTree
static BTREE_CACHE: spin::Mutex<alloc::collections::BTreeMap<u32, alloc::sync::Arc<eclipsefs_lib::btree::BTree>>> = spin::Mutex::new(alloc::collections::BTreeMap::new());

/// Filesystem state
pub struct Filesystem {
    mounted: bool,
    header: Option<EclipseFSHeader>,
    inode_table_offset: u64,
    disk_scheme_id: usize,
    disk_resource_id: usize,
    partition_offset: u64,
    inode_cache: InodeCache,
    log_tail: u64,  // Next available block for append in LFS
    use_lfs: bool,   // Whether LFS mode is enabled
}

static mut FS: Filesystem = Filesystem {
    mounted: false,
    header: None,
    inode_table_offset: 0,
    disk_scheme_id: 0,
    disk_resource_id: 0,
    partition_offset: 0,
    inode_cache: InodeCache::new(),
    log_tail: 0,
    use_lfs: false,
};

/// Read a range of bytes directly from disk, potentially spanning block boundaries.
/// Un solo `lseek` + `scheme::read`: `DiskScheme::read` rellena todo el buffer cruzando bloques.
fn read_bytes_at(abs_offset: u64, dest: &mut [u8]) -> Result<(), &'static str> {
    if dest.is_empty() {
        return Ok(());
    }
    let _lock = FILESYSTEM_LOCK.lock();
    unsafe {
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

        match crate::scheme::read(FS.disk_scheme_id, FS.disk_resource_id, dest, abs_offset) {
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

        match crate::scheme::write(FS.disk_scheme_id, FS.disk_resource_id, src, abs_offset) {
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

/// Escribe una página (4KB) de un archivo al disco.
/// Usado por el PageCache para el write-back.
pub fn write_page_to_disk(inode: u32, data: &[u8], page_index: u64) -> Result<(), &'static str> {
    // 1. Obtener el offset del archivo
    // NOTA: En este LFS simplificado, buscamos el offset base del inodo y sumamos el indice de página.
    let inode_info = Filesystem::read_inode_entry(inode)?;
    let abs_offset = inode_info.offset + page_index;
    
    write_bytes_at(abs_offset, data)
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
                
                FS.header = Some(header.clone());
                FS.mounted = true;
                
                // Initialize LFS: find the highest offset to set log_tail correctly
                FS.use_lfs = true;
                let mut highest_offset = header.inode_table_offset + header.inode_table_size;
                
                serial::serial_print("[FS] Scanning inode table for log_tail (optimized)...\n");
                let chunk_size = 64 * 1024; // 64KB per read (4096 inodes)
                let entries_per_chunk = chunk_size / constants::INODE_TABLE_ENTRY_SIZE;
                let mut chunk_buf = vec![0u8; chunk_size];

                for chunk_start in (0..header.total_inodes).step_by(entries_per_chunk) {
                    let entries_in_chunk = core::cmp::min(entries_per_chunk as u32, header.total_inodes - chunk_start);
                    let bytes_to_read = entries_in_chunk as usize * constants::INODE_TABLE_ENTRY_SIZE;
                    
                    let table_offset = header.inode_table_offset + (chunk_start as u64 * constants::INODE_TABLE_ENTRY_SIZE as u64);
                    let abs_disk_offset = table_offset + (part_offset * BLOCK_SIZE as u64);
                    
                    if read_bytes_at(abs_disk_offset, &mut chunk_buf[..bytes_to_read]).is_ok() {
                        for i in 0..entries_in_chunk {
                            let entry_ptr = i as usize * constants::INODE_TABLE_ENTRY_SIZE;
                            let offset = u64::from_le_bytes([
                                chunk_buf[entry_ptr + 8], chunk_buf[entry_ptr + 9],
                                chunk_buf[entry_ptr + 10], chunk_buf[entry_ptr + 11],
                                chunk_buf[entry_ptr + 12], chunk_buf[entry_ptr + 13],
                                chunk_buf[entry_ptr + 14], chunk_buf[entry_ptr + 15]
                            ]);
                            
                            if offset != 0 {
                                let abs_record_offset = header.inode_table_offset + header.inode_table_size + offset;
                                // Need to know the record size to correctly set the tail.
                                // We read the record header (first 8 bytes).
                                let mut rec_header = [0u8; 8];
                                let rec_abs_disk_offset = abs_record_offset + (part_offset * BLOCK_SIZE as u64);
                                if read_bytes_at(rec_abs_disk_offset, &mut rec_header).is_ok() {
                                    let record_size = u32::from_le_bytes([
                                        rec_header[4], rec_header[5],
                                        rec_header[6], rec_header[7]
                                    ]) as u64;
                                    
                                    let end_offset = abs_record_offset + record_size;
                                    if end_offset > highest_offset {
                                        highest_offset = end_offset;
                                    }
                                }
                            }
                        }
                    }
                }

                FS.log_tail = (highest_offset + 4095) / 4096;
                serial::serial_print("[FS] LFS Mode Enabled. Log Tail found at block ");
                serial::serial_print_dec(FS.log_tail);
                serial::serial_print("\n");

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

            // 0. Check LFS Inode Map
            if FS.use_lfs {
                if let Some(&offset) = INODE_MAP.lock().get(&inode) {
                    return Ok(InodeTableEntry::new(inode as u64, offset));
                }
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
             
            // Use checked arithmetic: these three values come from an on-disk header and
            // could be crafted to overflow when added together, producing an incorrect
            // disk seek that corrupts arbitrary memory via the HHDM mapping.
            let absolute_offset = header.inode_table_offset
                .checked_add(header.inode_table_size)
                .and_then(|s| s.checked_add(rel_offset))
                .ok_or("Inode offset arithmetic overflow (corrupt filesystem header)")?;
            
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

    pub fn get_node_metadata(inode: u32) -> Result<NodeMetadata, &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        let (data_start, _) = Self::get_file_metadata(inode)?;
        
        // Read Record header (8 bytes: u32 inode, u32 size)
        let mut header = [0u8; 8];
        read_bytes_at(data_start, &mut header)?;
        
        let record_inode = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let record_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        
        if record_inode != inode { return Err("Record inode mismatch"); }
        
        let mut data = vec![0u8; record_size as usize];
        read_bytes_at(data_start + 8, &mut data)?;
        
        let mut meta = NodeMetadata {
            mode: 0, uid: 0, gid: 0, size: 0, kind: NodeKind::File,
        };
        
        // Parse metadata TLVs
        let mut pos = 0;
        while pos + 6 <= data.len() {
            let t = u16::from_le_bytes([data[pos], data[pos+1]]);
            let l = u32::from_le_bytes([data[pos+2], data[pos+3], data[pos+4], data[pos+5]]) as usize;
            if pos + 6 + l > data.len() { break; }
            let v = &data[pos+6 .. pos+6 + l];
            match t {
                tlv_tags::NODE_TYPE => meta.kind = match v[0] { 1 => NodeKind::File, 2 => NodeKind::Directory, 3 => NodeKind::Symlink, _ => NodeKind::File },
                tlv_tags::MODE => meta.mode = u16::from_le_bytes([v[0], v[1]]),
                tlv_tags::UID => meta.uid = u32::from_le_bytes([v[0], v[1], v[2], v[3]]),
                tlv_tags::GID => meta.gid = u32::from_le_bytes([v[0], v[1], v[2], v[3]]),
                tlv_tags::SIZE => meta.size = u64::from_le_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]]),
                _ => {}
            }
            pos += 6 + l;
        }
        // Default modes if missing
        if meta.mode == 0 {
            meta.mode = match meta.kind {
                NodeKind::Directory => 0o755,
                NodeKind::Symlink => 0o777,
                _ => 0o644,
            };
        }
        Ok(meta)
    }

    /// Update a specific metadata tag for an inode (LFS only)
    pub fn update_metadata(inode: u32, target_tag: u32, new_value: &[u8]) -> Result<(), &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        unsafe {
            if !FS.use_lfs { return Err("Metadata update only supported in LFS mode"); }
        }
        
        let (old_data_start, _) = Self::get_file_metadata(inode)?;
        
        // Read existing record
        let mut header = [0u8; 8];
        read_bytes_at(old_data_start, &mut header)?;
        let record_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let mut data = vec![0u8; record_size as usize];
        read_bytes_at(old_data_start + 8, &mut data)?;
        
        // Build new metadata buffer
        let mut new_data = Vec::new();
        let mut pos = 0;
        let mut updated = false;
        let target_tag_u16 = target_tag as u16;
        
        while pos + 6 <= data.len() {
            let t = u16::from_le_bytes([data[pos], data[pos+1]]);
            let l = u32::from_le_bytes([data[pos+2], data[pos+3], data[pos+4], data[pos+5]]) as usize;
            if pos + 6 + l > data.len() { break; }
            let v = &data[pos+6 .. pos+6 + l];
            
            new_data.extend_from_slice(&t.to_le_bytes());
            if t == target_tag_u16 {
                new_data.extend_from_slice(&(new_value.len() as u32).to_le_bytes());
                new_data.extend_from_slice(new_value);
                updated = true;
            } else {
                new_data.extend_from_slice(&(l as u32).to_le_bytes());
                new_data.extend_from_slice(v);
            }
            pos += 6 + l;
        }
        
        if !updated {
            new_data.extend_from_slice(&target_tag_u16.to_le_bytes());
            new_data.extend_from_slice(&(new_value.len() as u32).to_le_bytes());
            new_data.extend_from_slice(new_value);
        }
        
        // Write new record to log
        let total_len = new_data.len() as u32;
        let mut record = Vec::new();
        record.extend_from_slice(&inode.to_le_bytes());
        record.extend_from_slice(&total_len.to_le_bytes());
        record.extend_from_slice(&new_data);
        
        unsafe {
            let new_off = FS.log_tail;
            write_bytes_at(new_off, &record)?;
            FS.log_tail += record.len() as u64;
            INODE_MAP.lock().insert(inode, new_off);
        }
        
        Ok(())
    }

    /// Comprobar permisos de acceso para el proceso actual.
    /// mask: 4=R, 2=W, 1=X
    pub fn check_access(inode: u32, mask: u8) -> Result<(), &'static str> {
        let pid = crate::process::current_process_id().ok_or("No current process")?;
        let p = crate::process::get_process(pid).ok_or("Process not found")?;
        let proc = p.proc.lock();
        
        // Root bypasses everything
        if proc.euid == 0 {
            return Ok(());
        }

        let meta = Self::get_node_metadata(inode)?;
        
        let mode = meta.mode;
        
        let mut granted = 0;
        if proc.euid == meta.uid {
            granted = (mode >> 6) & 0x7;
        } else if proc.egid == meta.gid {
            granted = (mode >> 3) & 0x7;
        } else {
            // Check supplementary groups
            let mut in_group = false;
            for i in 0..proc.supplementary_groups_len {
                if proc.supplementary_groups[i] == meta.gid {
                    in_group = true;
                    break;
                }
            }
            if in_group {
                granted = (mode >> 3) & 0x7;
            } else {
                granted = mode & 0x7;
            }
        }

        if (granted as u8 & mask) == mask {
            Ok(())
        } else {
            Err("Permission denied")
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
                if length > MAX_RECORD_SIZE {
                    return Err("CONTENT TLV exceeds MAX_RECORD_SIZE");
                }
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
    /// Write file by inode (handles LFS redirection)
    pub fn write_file_by_inode(inode: u32, data: &[u8], offset: u64) -> Result<usize, &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        if unsafe { FS.use_lfs } {
            return Self::write_file_lfs(inode, data, offset);
        }
        
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

    /// Flush all in-memory LFS redirections to the on-disk inode table.
    /// This makes the current state persistent across reboots.
    pub fn sync() -> Result<(), &'static str> {
        let _lock = FILESYSTEM_LOCK.lock();
        unsafe {
            if !FS.mounted { return Ok(()); }
        }
        
        serial::serial_print("[FS] Flushing Page Cache...\n");
        crate::page_cache::PAGE_CACHE.lock().flush_all();
        
        serial::serial_print("[FS] Syncing LFS inode map to disk...\n");
        
        let map = INODE_MAP.lock();
        for (&inode, &new_offset) in map.iter() {
            // Update the entry in the original inode table
            let index = (inode - 1) as u64;
            let entry_offset = unsafe { FS.inode_table_offset } + (index * constants::INODE_TABLE_ENTRY_SIZE as u64);
            let abs_disk_offset = entry_offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
            
            let mut entry_buffer = [0u8; 16];
            entry_buffer[0..8].copy_from_slice(&(inode as u64).to_le_bytes());
            
            let header = unsafe { FS.header.as_ref().unwrap() };
            let log_base = header.inode_table_offset + header.inode_table_size;
            let rel_offset = new_offset.saturating_sub(log_base);
            
            entry_buffer[8..16].copy_from_slice(&rel_offset.to_le_bytes());
            
            write_bytes_at(abs_disk_offset, &entry_buffer)?;
        }
        
        serial::serial_print("[FS] Sync complete\n");
        Ok(())
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
                // serial::serial_printf(format_args!("[FS] get_file_metadata(ino={}): found size={} at offset={:#x}\n", inode, found.1, found.0));
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
        let clean_path = path.trim_start_matches("file:");
        Self::lookup_path_recursive(clean_path, 0)
    }

    fn lookup_path_recursive(path: &str, depth: usize) -> Result<u32, &'static str> {
        if depth > 32 { return Err("Too many symlink levels"); }
        let _lock = FILESYSTEM_LOCK.lock();
        
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current_inode = 1;
        
        for (i, part) in parts.iter().enumerate() {
            let is_last = i == parts.len() - 1;
            
            // Standard lookup in current_inode (directory)
            let next_inode = {
                // Reusing the B-Tree search logic...
                // (I'll extract it to a helper find_in_dir_btree)
                Self::find_child_in_dir_btree(current_inode, part)?
            };

            // Check if next_inode is a symlink
            match Self::inode_kind(next_inode)? {
                NodeKind::Symlink => {
                    let target = Self::read_symlink_target(next_inode)?;
                    // Resolve relative/absolute
                    let _link_parent_path = if i == 0 { "/" } else {
                        // Reconstruct path up to here
                        // This is a bit inefficient but safe
                        "" // TODO: implement proper parent path reconstruction if needed
                    };
                    // Actually, resolve_symlink_path handles it
                    let mut full_path_so_far = String::from("/");
                    for j in 0..i { full_path_so_far.push_str(parts[j]); full_path_so_far.push('/'); }
                    full_path_so_far.push_str(part);
                    
                    let resolved = resolve_symlink_path(&full_path_so_far, &target);
                    
                    if is_last {
                        // Restart lookup for the resolved path
                        return Self::lookup_path_recursive(&resolved, depth + 1);
                    } else {
                        // Resolve the symlink and continue traversal
                        current_inode = Self::lookup_path_recursive(&resolved, depth + 1)?;
                    }
                }
                NodeKind::Directory => {
                    current_inode = next_inode;
                }
                NodeKind::File => {
                    if !is_last { return Err("Not a directory"); }
                    current_inode = next_inode;
                }
            }
        }
        Ok(current_inode)
    }

    fn find_child_in_dir_btree(dir_inode: u32, name: &str) -> Result<u32, &'static str> {
        // (Copied from existing lookup_path logic)
        let cached = {
            let mut dc = DIR_CACHE.lock();
            if let Some(idx) = dc.entries.iter().position(|e| e.0 == dir_inode) {
                dc.access_counter = dc.access_counter.wrapping_add(1);
                dc.entries[idx].1 = dc.access_counter;
                Some(dc.entries[idx].2.clone())
            } else { None }
        };
        let record_data = if let Some(data) = cached { data } else {
            let entry = Self::read_inode_entry(dir_inode)?;
            let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
            let mut header_buf = [0u8; 8];
            read_bytes_at(abs_disk_offset, &mut header_buf)?;
            let record_size = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]) as usize;
            if record_size < 8 || record_size > MAX_RECORD_SIZE { return Err("Invalid directory record"); }
            let mut data = vec![0u8; record_size];
            read_bytes_at(abs_disk_offset, &mut data)?;
            DIR_CACHE.lock().insert(dir_inode, data.clone());
            data
        };
        let mut cache = BTREE_CACHE.lock();
        let btree = if let Some(bt) = cache.get(&dir_inode) {
            alloc::sync::Arc::clone(bt)
        } else {
            let mut bt = eclipsefs_lib::btree::BTree::new();
            let data = &record_data[8..];
            let mut tlv_pos = 0usize;
            while tlv_pos + 6 <= data.len() {
                let tag = u16::from_le_bytes([data[tlv_pos], data[tlv_pos + 1]]);
                let length = u32::from_le_bytes([data[tlv_pos+2], data[tlv_pos+3], data[tlv_pos+4], data[tlv_pos+5]]) as usize;
                if tag == tlv_tags::DIRECTORY_ENTRIES && tlv_pos + 6 + length <= data.len() {
                    let dir_data = &data[tlv_pos + 6..tlv_pos + 6 + length];
                    let mut dir_off = 0usize;
                    while dir_off + 8 <= dir_data.len() {
                        let name_len = u32::from_le_bytes([dir_data[dir_off], dir_data[dir_off+1], dir_data[dir_off+2], dir_data[dir_off+3]]) as usize;
                        let child_inode = u32::from_le_bytes([dir_data[dir_off+4], dir_data[dir_off+5], dir_data[dir_off+6], dir_data[dir_off+7]]);
                        if name_len > 0 && dir_off + 8 + name_len <= dir_data.len() {
                            let name_bytes = &dir_data[dir_off + 8..dir_off + 8 + name_len];
                            if let Ok(name) = core::str::from_utf8(name_bytes) {
                                let _ = bt.insert(alloc::string::String::from(name), child_inode);
                            }
                        }
                        dir_off += 8 + name_len;
                    }
                }
                tlv_pos += 6 + length;
            }
            let arc_bt = alloc::sync::Arc::new(bt);
            cache.insert(dir_inode, alloc::sync::Arc::clone(&arc_bt));
            arc_bt
        };
        btree.search(name).ok_or("File not found")
    }

    pub fn lookup_path_no_follow(path: &str) -> Result<u32, &'static str> {
        let clean_path = path.trim_start_matches("file:");
        let _lock = FILESYSTEM_LOCK.lock();
        unsafe {
            if !FS.mounted {
                return Err("Filesystem not mounted during lookup");
            }
        }
        
        serial::serial_printf(format_args!("[FS] lookup_path('{}')\n", clean_path));

        if clean_path == "/" || clean_path == "" {
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
         
            // Use B-Tree for lookup
            let next_inode = {
                let mut cache = BTREE_CACHE.lock();
                let btree = if let Some(bt) = cache.get(&current_inode) {
                    alloc::sync::Arc::clone(bt)
                } else {
                    // Build BTree from record_data
                    let mut bt = eclipsefs_lib::btree::BTree::new();
                    // Simple parse to fill BTree (reusing logic from find_child_in_dir)
                    let data = &record_data[8..];
                    let mut tlv_pos = 0usize;
                    while tlv_pos + 6 <= data.len() {
                        let tag = u16::from_le_bytes([data[tlv_pos], data[tlv_pos + 1]]);
                        let length = u32::from_le_bytes([data[tlv_pos+2], data[tlv_pos+3], data[tlv_pos+4], data[tlv_pos+5]]) as usize;
                        if tag == tlv_tags::DIRECTORY_ENTRIES && tlv_pos + 6 + length <= data.len() {
                            let dir_data = &data[tlv_pos + 6..tlv_pos + 6 + length];
                            let mut dir_off = 0usize;
                            while dir_off + 8 <= dir_data.len() {
                                let name_len = u32::from_le_bytes([dir_data[dir_off], dir_data[dir_off+1], dir_data[dir_off+2], dir_data[dir_off+3]]) as usize;
                                let child_inode = u32::from_le_bytes([dir_data[dir_off+4], dir_data[dir_off+5], dir_data[dir_off+6], dir_data[dir_off+7]]);
                                if name_len > 0 && dir_off + 8 + name_len <= dir_data.len() {
                                    let name_bytes = &dir_data[dir_off + 8..dir_off + 8 + name_len];
                                    if let Ok(name) = core::str::from_utf8(name_bytes) {
                                        let _ = bt.insert(alloc::string::String::from(name), child_inode);
                                    }
                                }
                                dir_off += 8 + name_len;
                            }
                        }
                        tlv_pos += 6 + length;
                    }
                    let arc_bt = alloc::sync::Arc::new(bt);
                    cache.insert(current_inode, alloc::sync::Arc::clone(&arc_bt));
                    arc_bt
                };
                btree.search(part)
            };

            if let Some(ino) = next_inode {
                serial::serial_printf(format_args!("[FS] Found '{}' -> inode {} (via B-Tree)\n", part, ino));
                current_inode = ino;
            } else {
                serial::serial_printf(format_args!("[FS] Child '{}' not found in directory\n", part));
                return Err("File not found");
            }
        }
        
        Ok(current_inode)
    }

    /// Igual que [`lookup_path`], pero si el último componente es un symlink lo sigue hasta un
    /// inode de **fichero** regular. Necesario para `open(2)` y para cargar `.so`/`PT_INTERP`:
    /// sin esto, `open` devolvía el TLV del symlink (~20 bytes) y musl fallaba con «Exec format error».
    pub fn lookup_path_resolve_file_inode(path: &str) -> Result<u32, &'static str> {
        use alloc::string::String;
        let mut cur = String::from(path.trim_start_matches('/'));
        if cur.is_empty() {
            return Err("Empty path");
        }
        const MAX: usize = 32;
        for _ in 0..MAX {
            let inode = Self::lookup_path(&cur)?;
            match Self::inode_kind(inode)? {
                NodeKind::Symlink => {
                    let target = Self::read_symlink_target(inode)?;
                    let slash_path = alloc::format!("/{}", cur.trim_start_matches('/'));
                    let link_for_resolve = slash_path.trim_end_matches('/');
                    let resolved = resolve_symlink_path(link_for_resolve, target.as_str());
                    cur.clear();
                    cur.push_str(resolved.trim_start_matches('/'));
                    if cur.is_empty() {
                        return Err("Symlink resolved to empty path");
                    }
                }
                NodeKind::Directory => return Err("Path resolves to a directory"),
                NodeKind::File => return Ok(inode),
            }
        }
        Err("Too many symlink levels")
    }

    /// LFS Write: Append new version of the record to the log
    fn write_file_lfs(inode: u32, data: &[u8], offset: u64) -> Result<usize, &'static str> {
        let entry = Self::read_inode_entry(inode)?;
        let abs_disk_offset = entry.offset + (unsafe { FS.partition_offset } * BLOCK_SIZE as u64);
        
        // Read old record
        let mut header_buf = [0u8; 8];
        read_bytes_at(abs_disk_offset, &mut header_buf)?;
        let record_size = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]) as usize;
        
        if record_size > MAX_RECORD_SIZE { return Err("Record too large"); }
        let mut record_data = vec![0u8; record_size];
        read_bytes_at(abs_disk_offset, &mut record_data)?;
        
        // Find CONTENT TLV
        let mut tlv_offset = 8; // skip 8-byte record header
        let mut found = false;
        while tlv_offset + 6 <= record_size {
            let tag = u16::from_le_bytes([record_data[tlv_offset], record_data[tlv_offset+1]]);
            let length = u32::from_le_bytes([record_data[tlv_offset+2], record_data[tlv_offset+3], record_data[tlv_offset+4], record_data[tlv_offset+5]]) as usize;
            
            if tag == tlv_tags::CONTENT {
                if offset as usize + data.len() > length {
                    return Err("LFS Write: Expansion not yet supported");
                }
                record_data[tlv_offset + 6 + offset as usize .. tlv_offset + 6 + offset as usize + data.len()].copy_from_slice(data);
                found = true;
                break;
            }
            tlv_offset += 6 + length;
        }
        
        if !found { return Err("No CONTENT TLV found"); }
        
        // Write new version to log tail
        let new_abs_offset = unsafe { (FS.log_tail + FS.partition_offset) * BLOCK_SIZE as u64 };
        write_bytes_at(new_abs_offset, &record_data)?;
        
        // Update state
        unsafe {
            let blocks_used = (record_size as u64 + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64;
            FS.log_tail += blocks_used;
        }
        INODE_MAP.lock().insert(inode, new_abs_offset / BLOCK_SIZE as u64 * BLOCK_SIZE as u64 - (unsafe { FS.partition_offset } * BLOCK_SIZE as u64));
        
        serial::serial_printf(format_args!("[LFS] Inode {} updated at log_tail={}, new_offset={}\n", inode, unsafe { FS.log_tail }, new_abs_offset));
        
        Ok(data.len())
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
    let fs_scheme = alloc::sync::Arc::new(FileSystemScheme);
    crate::scheme::register_scheme("file", fs_scheme.clone());
    crate::scheme::register_scheme("dev", fs_scheme);

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

/// Elimina un archivo.  Actualmente soportado para rutas /tmp/* y /run/*.
pub fn unlink_path(path: &str) -> Result<(), usize> {
    use crate::scheme::error;
    let clean = if path.starts_with('/') { &path[1..] } else { path };
    if is_virtual_tmp_path(clean) {
        let mut vtmp = VIRTUAL_TMP.lock();
        if vtmp.remove(&String::from(clean)).is_some() {
            Ok(())
        } else {
            Err(error::ENOENT)
        }
    } else if is_virtual_run_path(clean) {
        let mut vrun = VIRTUAL_RUN.lock();
        if vrun.remove(&String::from(clean)).is_some() {
            Ok(())
        } else {
            Err(error::ENOENT)
        }
    } else {
        Err(error::ENOSYS)
    }
}

/// Crea un directorio.  Actualmente soportado bajo /tmp/ y /run/.

pub fn create_virtual_symlink(path: &str, target: &str) -> Result<(), &'static str> {
    let mut vtmp = VIRTUAL_TMP.lock();
    let mut vrun = VIRTUAL_RUN.lock();
    let mut vkinds = VIRTUAL_KINDS.lock();
    
    let (map, key) = if path.starts_with("tmp/") {
        (&mut *vtmp, path)
    } else if path.starts_with("run/") {
        (&mut *vrun, path)
    } else {
        return Err("Not a virtual path");
    };
    
    map.insert(String::from(key), target.as_bytes().to_vec());
    vkinds.insert(String::from(key), NodeKind::Symlink);
    Ok(())
}

pub fn mkdir_path(path: &str, _mode: u32) -> Result<(), usize> {
    use crate::scheme::error;
    let clean = if path.starts_with('/') { &path[1..] } else { path };
    if is_virtual_tmp_path(clean) {
        // Guardamos el directorio como entrada vacía en VIRTUAL_TMP.
        let mut vtmp = VIRTUAL_TMP.lock();
        vtmp.entry(String::from(clean)).or_insert_with(alloc::vec::Vec::new);
        Ok(())
    } else if is_virtual_run_path(clean) {
        let mut vrun = VIRTUAL_RUN.lock();
        vrun.entry(String::from(clean)).or_insert_with(alloc::vec::Vec::new);
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
    // The linked_list_allocator pads every allocation size up to the next multiple of
    // size_of::<usize>() == 8, so a file whose length is in [READ_FILE_ALLOC_MAX_CONTENT - 7,
    // READ_FILE_ALLOC_MAX_CONTENT - 1] would be rounded up to READ_FILE_ALLOC_MAX_CONTENT by the allocator,
    // producing Layout { size: 134217728, align: 8 } which exhausts the static kernel
    // heap.  Use saturating_add to avoid wrapping and compare against the limit.
    const ALLOC_ALIGN: usize = core::mem::size_of::<usize>();
    if len.saturating_add(ALLOC_ALIGN - 1) >= READ_FILE_ALLOC_MAX_CONTENT {
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

/// Virtual files under /run (in-memory overlay for socket files, PID files, etc.)
/// This allows programs like seatd to bind()/unlink() socket files under /run even though
/// the real root filesystem is read-only.
static VIRTUAL_KINDS: Mutex<BTreeMap<String, NodeKind>> = Mutex::new(BTreeMap::new());
static VIRTUAL_RUN: Mutex<BTreeMap<String, alloc::vec::Vec<u8>>> = Mutex::new(BTreeMap::new());

/// Returns true when a clean (leading-slash-stripped) path belongs to the virtual /run overlay.
#[inline(always)]
fn is_virtual_run_path(clean_path: &str) -> bool {
    clean_path == "run" || clean_path.starts_with("run/")
}

/// Returns true when a clean (leading-slash-stripped) path belongs to the virtual /tmp overlay.
#[inline(always)]
fn is_virtual_tmp_path(clean_path: &str) -> bool {
    clean_path == "tmp" || clean_path.starts_with("tmp/")
}

#[derive(Clone)]
enum OpenFile {
    Real { 
        inode: u32, 
        data_start_abs: u64,
        size: u64,
    },
    Virtual { path: String },
    Framebuffer,
    Drm { resource_id: usize },
    Input { resource_id: usize },
    Null,
    Zero,
    Tty,
    Random,
    DeviceList,
}

static OPEN_FILES_SCHEME: Mutex<alloc::vec::Vec<Option<OpenFile>>> = Mutex::new(alloc::vec::Vec::new());

pub struct FileSystemScheme;

impl Scheme for FileSystemScheme {
    fn open(&self, path: &str, flags: usize, _mode: u32) -> Result<usize, usize> {
        
        // serial::serial_printf(format_args!("[FS-SCHEME] open({})\n", path));

        // Strip *all* leading slashes. `file://foo` yields relative_path `//foo`; removing only
        // one slash left `/foo` and lookup treated `foo` as a child of root. Also, musl may
        // open `//libfoo.so` (bad search-dir join); trimming yields `libfoo.so` so we can retry
        // under `lib/` / `usr/lib/` below.
        let mut clean_path = path.trim_start_matches('/');

        // Handle hardcoded build-time paths from relocated libraries (e.g. Fontconfig)
        let hardcoded_prefix = "home/moebius/eclipse/eclipse-os-build/";
        if clean_path.starts_with(hardcoded_prefix) {
            let _old_path = clean_path;
            clean_path = &clean_path[hardcoded_prefix.len()..];
            // serial::serial_printf(format_args!("[FS-SCHEME] fontconfig: redirected {} to: {}\n", old_path, clean_path));
        }

        match clean_path {
            p if p == "" || p == "dev" || p == "dev/" => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::DeviceList));
                Ok(id)
            },
            p if p == "dev/fb0" || p == "fb0" => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::Framebuffer));
                Ok(id)
            },
            p if p == "dev/dri/card0" || p == "card0" || p == "dev/card0" => {
                let drm = crate::drm_scheme::DrmScheme;
                let res_id = drm.open("card0", flags, _mode)?;
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::Drm { resource_id: res_id }));
                Ok(id)
            },
            p if p == "dev/keyboard" || p == "keyboard" 
              || p == "dev/input/event0" || p == "dev/input/event1" 
              || p == "input/event0" || p == "input/event1" => {
                let input_path = if p.contains("keyboard") { "keyboard" }
                                else if p.contains("event0") { "event0" } 
                                else { "event1" };
                let (_, res_id) = crate::scheme::open(&alloc::format!("input:{}", input_path), flags, _mode)?;
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::Input { resource_id: res_id }));
                Ok(id)
            },
            p if p == "dev/null" || p == "null" => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::Null));
                Ok(id)
            },
            p if p == "dev/zero" || p == "zero" => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::Zero));
                Ok(id)
            },
            p if p == "dev/tty" || p == "tty" 
                || (p.starts_with("dev/tty") && p.len() > 7)
                || (p.starts_with("tty") && p.len() > 3) => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::Tty));
                Ok(id)
            },
            p if p == "dev/random" || p == "dev/urandom" || p == "random" || p == "urandom" => {
                let mut open_files = OPEN_FILES_SCHEME.lock();
                let id = open_files.len();
                open_files.push(Some(OpenFile::Random));
                Ok(id)
            },
            p if is_virtual_tmp_path(p) => {
                let key = String::from(clean_path);
                let mut vtmp = VIRTUAL_TMP.lock();
                if (flags & O_CREAT) != 0 {
                    if (flags & O_EXCL) != 0 && vtmp.contains_key(&key) {
                        return Err(scheme_error::EEXIST);
                    }
                    vtmp.entry(key.clone()).or_insert_with(alloc::vec::Vec::new);
                }

                if vtmp.contains_key(&key) {
                    drop(vtmp);
                    let mut open_files = OPEN_FILES_SCHEME.lock();
                    let id = open_files.len();
                    open_files.push(Some(OpenFile::Virtual { path: key }));
                    Ok(id)
                } else {
                    Err(scheme_error::ENOENT)
                }
            },
            p if is_virtual_run_path(p) => {
                let key = String::from(clean_path);
                let mut vrun = VIRTUAL_RUN.lock();
                if (flags & O_CREAT) != 0 {
                    if (flags & O_EXCL) != 0 && vrun.contains_key(&key) {
                        return Err(scheme_error::EEXIST);
                    }
                    vrun.entry(key.clone()).or_insert_with(alloc::vec::Vec::new);
                }

                if vrun.contains_key(&key) {
                    drop(vrun);
                    let mut open_files = OPEN_FILES_SCHEME.lock();
                    let id = open_files.len();
                    open_files.push(Some(OpenFile::Virtual { path: key }));
                    Ok(id)
                } else {
                    Err(scheme_error::ENOENT)
                }
            },
            _ => {
                if !is_mounted() {
                    return Err(scheme_error::EIO);
                }
                
                                let mut access_mask = 0u8;
                if (flags & 3) == 0 { access_mask |= 4; } // O_RDONLY
                if (flags & 3) == 1 { access_mask |= 2; } // O_WRONLY
                if (flags & 3) == 2 { access_mask |= 6; } // O_RDWR
                
                let res = Filesystem::lookup_path_resolve_file_inode(clean_path);
                let (ino, size, id) = match res {
                    Ok(ino) => {
                        // Check access
                        if let Err(_e) = Filesystem::check_access(ino, access_mask) {
                            return Err(scheme_error::EACCES);
                        }
                        
                        let (_data_start, _size) = Filesystem::get_file_metadata(ino).map_err(|_| scheme_error::EIO)?;
                        let (data_start, size) = Filesystem::get_file_metadata(ino).map_err(|_| scheme_error::EIO)?;
                        let mut open_files = OPEN_FILES_SCHEME.lock();
                        let id = open_files.len();
                        open_files.push(Some(OpenFile::Real {
                            inode: ino,
                            data_start_abs: data_start,
                            size,
                        }));
                        (ino, size, id)
                    }
                    Err(_) => return Err(scheme_error::ENOENT),
                };
                serial::serial_printf(format_args!("[FS-SCHEME] open Real: inode={} size={} fd={}\n", ino, size, id));
                Ok(id)
            }
        }
    }

    
    fn read(&self, id: usize, buffer: &mut [u8], offset: u64) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        
        match open_file {
            OpenFile::Real { inode, size, .. } => {
                let current_off = offset;
                let file_size = *size;
                let inode_id = *inode;
                let device_id = unsafe { FS.disk_scheme_id };

                if current_off >= file_size {
                    return Ok(0);
                }

                let max_read = core::cmp::min(buffer.len(), (file_size - current_off) as usize);
                let mut total_read = 0usize;

                while total_read < max_read {
                    let file_off = current_off + total_read as u64;
                    let page_idx = file_off / 4096;
                    let off_in_page = (file_off % 4096) as usize;
                    let take = core::cmp::min(max_read - total_read, 4096 - off_in_page);

                    let page_arc = crate::page_cache::PAGE_CACHE.lock().get_or_create(device_id, inode_id, page_idx * 4096);
                    let mut page = page_arc.lock();

                    if !page.valid {
                         let mut temp = [0u8; 4096];
                         let (data_start, _) = Filesystem::get_file_metadata(inode_id).map_err(|_| scheme_error::EIO)?;
                         let abs_disk_off = data_start + (page_idx * 4096);
                         if read_bytes_at(abs_disk_off, &mut temp).is_ok() {
                             page.as_slice_mut().copy_from_slice(&temp);
                             page.valid = true;
                             page.dirty = false;
                         }
                    }

                    buffer[total_read..total_read + take].copy_from_slice(&page.as_slice()[off_in_page..off_in_page + take]);
                    total_read += take;
                }
                Ok(total_read)
            }
            OpenFile::Virtual { path } => {
                let path_clone = path.clone();
                let off = offset;
                drop(open_files);
                if is_virtual_run_path(&path_clone) {
                    let vrun = VIRTUAL_RUN.lock();
                    let content = vrun.get(&path_clone).ok_or(scheme_error::EIO)?;
                    let start = off as usize;
                    if start >= content.len() { return Ok(0); }
                    let len = core::cmp::min(buffer.len(), content.len() - start);
                    buffer[..len].copy_from_slice(&content[start..start + len]);
                    Ok(len)
                } else {
                    let vtmp = VIRTUAL_TMP.lock();
                    let content = vtmp.get(&path_clone).ok_or(scheme_error::EIO)?;
                    let start = off as usize;
                    if start >= content.len() { return Ok(0); }
                    let len = core::cmp::min(buffer.len(), content.len() - start);
                    buffer[..len].copy_from_slice(&content[start..start + len]);
                    Ok(len)
                }
            }
            OpenFile::DeviceList => {
                let names = list_device_names();
                let mut data = alloc::string::String::new();
                for name in names { data.push_str(&name); data.push('\n'); }
                let data_bytes = data.as_bytes();
                if offset >= data_bytes.len() as u64 { return Ok(0); }
                let count = core::cmp::min(buffer.len(), data_bytes.len() - offset as usize);
                buffer[..count].copy_from_slice(&data_bytes[offset as usize .. offset as usize + count]);
                Ok(count)
            }
            OpenFile::Framebuffer => Ok(0),
            OpenFile::Drm { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                crate::drm_scheme::DrmScheme.read(res_id, buffer, offset)
            }
            OpenFile::Input { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                if let Some(scheme_id) = crate::scheme::get_scheme_id("input") {
                    crate::scheme::read(scheme_id, res_id, buffer, offset)
                } else { Err(scheme_error::ENODEV) }
            }
            OpenFile::Null => Ok(0),
            OpenFile::Zero => { for b in buffer.iter_mut() { *b = 0; } Ok(buffer.len()) }
            OpenFile::Tty => Ok(0),
            OpenFile::Random => {
                let mut tsc: u64;
                for b in buffer.iter_mut() {
                    unsafe { core::arch::asm!("rdtsc", out("rax") tsc, out("rdx") _, options(nomem, nostack)); }
                    *b = (tsc & 0xFF) as u8 ^ ((tsc >> 8) & 0xFF) as u8;
                }
                Ok(buffer.len())
            }
        }
    }

    fn write(&self, id: usize, buffer: &[u8], offset: u64) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Real { inode, .. } => {
                Filesystem::write_file_by_inode(*inode, buffer, offset).map_err(|_| scheme_error::EIO)
            }
            OpenFile::Virtual { path } => {
                let path_clone = path.clone();
                let end = (offset as usize).checked_add(buffer.len()).ok_or(scheme_error::EINVAL)?;
                if end > MAX_VIRTUAL_FILE_SIZE { return Err(scheme_error::EINVAL); }
                if is_virtual_run_path(&path_clone) {
                    let mut vrun = VIRTUAL_RUN.lock();
                    let content = vrun.entry(path_clone).or_insert_with(alloc::vec::Vec::new);
                    if end > content.len() { content.resize(end, 0); }
                    content[offset as usize..end].copy_from_slice(buffer);
                } else if is_virtual_tmp_path(&path_clone) {
                    let mut vtmp = VIRTUAL_TMP.lock();
                    let content = vtmp.entry(path_clone).or_insert_with(alloc::vec::Vec::new);
                    if end > content.len() { content.resize(end, 0); }
                    content[offset as usize..end].copy_from_slice(buffer);
                } else { return Err(scheme_error::EROFS); }
                Ok(buffer.len())
            }
            OpenFile::Drm { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                crate::drm_scheme::DrmScheme.write(res_id, buffer, offset)
            }
            OpenFile::Input { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                if let Some(scheme_id) = crate::scheme::get_scheme_id("input") {
                    crate::scheme::write(scheme_id, res_id, buffer, offset)
                } else { Err(scheme_error::ENODEV) }
            }
            OpenFile::Tty => {
                if let Ok(s) = core::str::from_utf8(buffer) { crate::serial::serial_print(s); Ok(buffer.len()) }
                else { Err(scheme_error::EINVAL) }
            }
            _ => Ok(buffer.len()),
        }
    }

    fn lseek(&self, id: usize, seek_offset: isize, whence: usize, current_offset: u64) -> Result<usize, usize> {
        let open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        let new_offset = match open_file {
            OpenFile::Real { size, .. } => {
                match whence {
                    0 => seek_offset as u64,
                    1 => (current_offset as i128 + seek_offset as i128).max(0) as u64,
                    2 => (*size as i128 + seek_offset as i128).max(0) as u64,
                    _ => return Err(scheme_error::EINVAL),
                }
            }
            OpenFile::Virtual { path } => {
                let path_clone = path.clone();
                let len = if is_virtual_run_path(&path_clone) {
                    VIRTUAL_RUN.lock().get(&path_clone).map(|v| v.len() as u64).unwrap_or(0)
                } else {
                    VIRTUAL_TMP.lock().get(&path_clone).map(|v| v.len() as u64).unwrap_or(0)
                };
                match whence {
                    0 => seek_offset as u64,
                    1 => (current_offset as i128 + seek_offset as i128).max(0) as u64,
                    2 => (len as i128 + seek_offset as i128).max(0) as u64,
                    _ => return Err(scheme_error::EINVAL),
                }
            }
            _ => current_offset,
        };
        Ok(new_offset as usize)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Real { inode: _, size, .. } => {
                stat.size = *size;
                stat.mode = 0o100644;
                Ok(0)
            }
            OpenFile::Virtual { path, .. } => {
                let path_clone = path.clone();
                let kind = VIRTUAL_KINDS.lock().get(&path_clone).cloned().unwrap_or(NodeKind::File);
                stat.mode = match kind {
                    NodeKind::File => 0o100644,
                    NodeKind::Directory => 0o040755,
                    NodeKind::Symlink => 0o120777,
                };
                stat.size = if is_virtual_run_path(&path_clone) {
                    VIRTUAL_RUN.lock().get(&path_clone).map(|v| v.len() as u64).unwrap_or(0)
                } else {
                    VIRTUAL_TMP.lock().get(&path_clone).map(|v| v.len() as u64).unwrap_or(0)
                };
                Ok(0)
            }
            OpenFile::Framebuffer => {
                let fb_info = &crate::boot::get_boot_info().framebuffer;
                stat.size = (fb_info.pixels_per_scan_line * fb_info.height * 4) as u64;
                stat.mode = 0o020666;
                Ok(0)
            }
            OpenFile::DeviceList => {
                stat.mode = 0o444 | 0x4000;
                stat.size = list_device_names().iter().map(|n| n.len() + 1).sum::<usize>() as u64;
                Ok(0)
            }
            OpenFile::Input { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                if let Some(scheme_id) = crate::scheme::get_scheme_id("input") {
                    crate::scheme::fstat(scheme_id, res_id, stat)
                } else { Err(scheme_error::ENODEV) }
            }
            _ => { stat.mode = 0o020666; Ok(0) }
        }
    }

    fn ftruncate(&self, id: usize, len: usize) -> Result<usize, usize> {
        if len > MAX_VIRTUAL_FILE_SIZE { return Err(scheme_error::EINVAL); }
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Virtual { path, .. } => {
                let path_clone = path.clone();
                if is_virtual_run_path(&path_clone) {
                    let mut vrun = VIRTUAL_RUN.lock();
                    if let Some(content) = vrun.get_mut(&path_clone) { content.resize(len, 0); Ok(0) }
                    else { Err(scheme_error::ENOENT) }
                } else {
                    let mut vtmp = VIRTUAL_TMP.lock();
                    if let Some(content) = vtmp.get_mut(&path_clone) { content.resize(len, 0); Ok(0) }
                    else { Err(scheme_error::ENOENT) }
                }
            }
            _ => Err(scheme_error::ENOSYS),
        }
    }

    fn ioctl(&self, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Drm { resource_id } => {
                let res_id = *resource_id; drop(open_files);
                crate::drm_scheme::DrmScheme.ioctl(res_id, request, arg)
            }
            OpenFile::Framebuffer => {
                match request as u32 {
                    0x4600 => { // FBIOGET_VSCREENINFO
                        let fb_info = &crate::boot::get_boot_info().framebuffer;
                        let var_info = unsafe { &mut *(arg as *mut fb_var_screeninfo) };
                        var_info.xres = fb_info.width as u32; var_info.yres = fb_info.height as u32;
                        var_info.xres_virtual = fb_info.width as u32; var_info.yres_virtual = fb_info.height as u32;
                        var_info.bits_per_pixel = 32;
                        var_info.red.offset = 16; var_info.red.length = 8;
                        var_info.green.offset = 8; var_info.green.length = 8;
                        var_info.blue.offset = 0; var_info.blue.length = 8;
                        var_info.transp.offset = 24; var_info.transp.length = 8;
                        Ok(0)
                    }
                    0x4602 => { // FBIOGET_FSCREENINFO
                        let fb_info = &crate::boot::get_boot_info().framebuffer;
                        let fix_info = unsafe { &mut *(arg as *mut fb_fix_screeninfo) };
                        fix_info.smem_start = fb_info.base_address as u64;
                        fix_info.smem_len = (fb_info.pixels_per_scan_line * fb_info.height * 4) as u32;
                        fix_info.line_length = (fb_info.pixels_per_scan_line * 4) as u32;
                        fix_info.visual = 2; Ok(0)
                    }
                    _ => Ok(0)
                }
            }
            OpenFile::Tty => {
                match request {
                    0x5603 => { // VT_GETSTATE
                        #[repr(C)] struct VtStat { v_active: u16, v_signal: u16, v_state: u16 }
                        let stat = unsafe { &mut *(arg as *mut VtStat) };
                        stat.v_active = 1; stat.v_state = 2; Ok(0)
                    }
                    0x5600 => { // VT_OPENQRY
                        unsafe { *(arg as *mut u32) = 1; } Ok(0)
                    }
                    _ => Ok(0)
                }
            }
            OpenFile::Input { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                if let Some(scheme_id) = crate::scheme::get_scheme_id("input") {
                    crate::scheme::ioctl(scheme_id, res_id, request, arg)
                } else { Err(scheme_error::ENODEV) }
            }
            _ => Err(scheme_error::ENOSYS),
        }
    }

    fn fmap(&self, id: usize, offset: usize, len: usize) -> Result<usize, usize> {
        let open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Framebuffer => {
                let fb_info = &crate::boot::get_boot_info().framebuffer;
                if fb_info.base_address == 0 { return Err(scheme_error::EIO); }
                Ok(fb_info.base_address as usize)
            }
            OpenFile::Drm { resource_id } => {
                let res_id = *resource_id; drop(open_files);
                crate::drm_scheme::DrmScheme.fmap(res_id, offset, len)
            }
            _ => Err(scheme_error::ENOSYS),
        }
    }

    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get_mut(id).and_then(|s| s.as_mut()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Drm { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                crate::drm_scheme::DrmScheme.poll(res_id, events)
            }
            OpenFile::Input { resource_id } => {
                let res_id = *resource_id;
                drop(open_files);
                if let Some(scheme_id) = crate::scheme::get_scheme_id("input") {
                    crate::scheme::poll(scheme_id, res_id, events)
                } else { Err(scheme_error::ENODEV) }
            }
            _ => Ok(events),
        }
    }

    fn dup_independent(&self, id: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        let existing = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?.clone();
        for (i, slot) in open_files.iter_mut().enumerate() {
            if i != id && slot.is_none() { *slot = Some(existing); return Ok(i); }
        }
        let new_id = open_files.len(); open_files.push(Some(existing)); Ok(new_id)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut open_files = OPEN_FILES_SCHEME.lock();
        if let Some(slot) = open_files.get_mut(id) {
            if let Some(file) = slot.take() {
                match file {
                    OpenFile::Drm { resource_id } => {
                        drop(open_files);
                        return crate::drm_scheme::DrmScheme.close(resource_id);
                    }
                    OpenFile::Input { resource_id } => {
                        let res_id = resource_id;
                        drop(open_files);
                        if let Some(scheme_id) = crate::scheme::get_scheme_id("input") {
                            return crate::scheme::close(scheme_id, res_id);
                        }
                        return Err(scheme_error::ENODEV);
                    }
                    _ => {}
                }
            }
            return Ok(0);
        }
        Err(scheme_error::EBADF)
    }

    fn mkdir(&self, path: &str, _mode: u32) -> Result<usize, usize> {
        let clean_path = path.trim_start_matches('/');
        if is_virtual_tmp_path(clean_path) {
            VIRTUAL_TMP.lock().entry(String::from(clean_path)).or_insert_with(alloc::vec::Vec::new);
            Ok(0)
        } else if is_virtual_run_path(clean_path) {
            VIRTUAL_RUN.lock().entry(String::from(clean_path)).or_insert_with(alloc::vec::Vec::new);
            Ok(0)
        } else { Err(scheme_error::ENOSYS) }
    }

    fn unlink(&self, path: &str) -> Result<usize, usize> {
        let clean_path = path.trim_start_matches('/');
        if is_virtual_tmp_path(clean_path) {
            if VIRTUAL_TMP.lock().remove(&String::from(clean_path)).is_some() { Ok(0) }
            else { Err(scheme_error::ENOENT) }
        } else if is_virtual_run_path(clean_path) {
            if VIRTUAL_RUN.lock().remove(&String::from(clean_path)).is_some() { Ok(0) }
            else { Err(scheme_error::ENOENT) }
        } else { Err(scheme_error::ENOSYS) }
    }

    fn rename(&self, old_path: &str, new_path: &str) -> Result<usize, usize> {
        let old_key = old_path.trim_start_matches('/');
        let new_key = new_path.trim_start_matches('/');
        if is_virtual_tmp_path(old_key) && is_virtual_tmp_path(new_key) {
            let mut vtmp = VIRTUAL_TMP.lock();
            if let Some(data) = vtmp.remove(&String::from(old_key)) {
                vtmp.insert(String::from(new_key), data); Ok(0)
            } else { Err(scheme_error::ENOENT) }
        } else { Err(scheme_error::ENOSYS) }
    }

    fn check_access(&self, id: usize, mask: u8) -> Result<(), usize> {
        let open_files = OPEN_FILES_SCHEME.lock();
        let open_file = open_files.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        match open_file {
            OpenFile::Real { inode, .. } => {
                let ino = *inode;
                drop(open_files);
                Filesystem::check_access(ino, mask).map_err(|_| scheme_error::EACCES)
            },
            _ => Ok(()),
        }
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
