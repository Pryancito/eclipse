//! Filesystem Service - Manages filesystem operations and VFS
//! 
//! This service provides the Virtual Filesystem (VFS) layer for Eclipse OS.
//! It uses the eclipsefs-lib to parse the filesystem structure from /dev/vda.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

use alloc::vec::Vec;
use alloc::vec;
use alloc::string::String;
use alloc::format;
use core::alloc::Layout;

use eclipse_libc::{
    println, getpid, getppid, sleep_ms, send,
    open, close, read, lseek, 
    O_RDONLY, SEEK_SET,
    mount, get_storage_device_count
};
use eclipsefs_lib::format::{EclipseFSHeader, InodeTableEntry, tlv_tags};
use linked_list_allocator::LockedHeap;

// Heap size (1 MB)
const HEAP_SIZE: usize = 1024 * 1024;
static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    panic!("allocation error: {:?}", layout)
}

/// Block size
const BLOCK_SIZE: usize = 4096;

/// EclipseFS magic bytes — 9 bytes, must match eclipsefs-lib/src/format.rs
const ECLIPSEFS_MAGIC: &[u8] = b"ECLIPSEFS";

/// Offsets to try when GPT scan is unavailable or fails (in 4096-byte blocks).
/// Covers common install layouts: 101 MiB, 256 MiB, 512 MiB, 1 GiB, 2 GiB.
const FALLBACK_OFFSETS: &[u64] = &[
    25856,    // 101 MiB  (EFI ≈ 100 MiB)
    65536,    // 256 MiB  (EFI ≈ 256 MiB)
    131072,   // 512 MiB  (EFI ≈ 512 MiB)
    262144,   // 1 GiB
    524288,   // 2 GiB
    2048,     // 8 MiB    (tiny EFI)
    4096,     // 16 MiB
];

/// Block Device Wrapper
struct BlockDevice {
    fd: i32,
}

impl BlockDevice {
    fn new(path: &str) -> Result<Self, &'static str> {
        let fd = open(path, O_RDONLY, 0);
        if fd < 0 {
            return Err("Failed to open device");
        }
        Ok(Self { fd })
    }
    
    fn read_block(&self, block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if buffer.len() != BLOCK_SIZE {
            return Err("Buffer must be BLOCK_SIZE");
        }
        
        let offset = block_num * BLOCK_SIZE as u64;
        if lseek(self.fd, offset as i64, SEEK_SET) < 0 {
            return Err("Seek failed");
        }
        
        if read(self.fd as u32, buffer) != BLOCK_SIZE as isize {
             return Err("Read failed");
        }
        
        Ok(())
    }
}

impl Drop for BlockDevice {
    fn drop(&mut self) {
        close(self.fd);
    }
}

struct EclipseFS<'a> {
    device: &'a BlockDevice,
    header: EclipseFSHeader,
    inode_table_offset: u64,
    /// Start of the EclipseFS partition in 4096-byte blocks from disk start.
    partition_offset: u64,
}

impl<'a> EclipseFS<'a> {
    /// Try to mount EclipseFS at a specific partition offset (in 4096-byte blocks).
    fn mount_at(device: &'a BlockDevice, partition_offset: u64) -> Result<Self, &'static str> {
        let mut superblock = vec![0u8; BLOCK_SIZE];
        device.read_block(partition_offset, &mut superblock)?;
        let header = EclipseFSHeader::from_bytes(&superblock)
            .map_err(|_| "Invalid EclipseFS header")?;
        Ok(Self {
            device,
            inode_table_offset: header.inode_table_offset,
            header,
            partition_offset,
        })
    }

    /// Discover the EclipseFS partition by scanning the GPT and known offsets.
    /// Returns (Self, partition_offset_blocks, partition_index) on success.
    fn mount(device: &'a BlockDevice) -> Result<(Self, u64, Option<usize>), &'static str> {
        // — Step 1: Try GPT discovery —
        if let Some((offset, part_idx)) = find_eclipsefs_in_gpt(device) {
            println!("[FS-SERVICE]   GPT scan found EclipseFS at block {} (partition {})", offset, part_idx);
            if let Ok(fs) = EclipseFS::mount_at(device, offset) {
                return Ok((fs, offset, Some(part_idx)));
            }
        }

        // — Step 2: Try well-known offsets —
        for &offset in FALLBACK_OFFSETS {
            let mut buf = vec![0u8; BLOCK_SIZE];
            if device.read_block(offset, &mut buf).is_err() {
                continue;
            }
            if buf.len() >= ECLIPSEFS_MAGIC.len() && &buf[..ECLIPSEFS_MAGIC.len()] == ECLIPSEFS_MAGIC {
                println!("[FS-SERVICE]   Found EclipseFS magic at fallback offset {} ({} MiB)",
                    offset, (offset * BLOCK_SIZE as u64) / (1024 * 1024));
                if let Ok(fs) = EclipseFS::mount_at(device, offset) {
                    return Ok((fs, offset, None));
                }
            }
        }

        Err("EclipseFS partition not found on this device")
    }
    
    fn read_inode(&self, inode: u32) -> Result<InodeTableEntry, &'static str> {
        if inode < 1 || inode > self.header.total_inodes {
            return Err("Inode out of range");
        }
        
        let index = (inode - 1) as u64;
        let entry_offset = self.inode_table_offset + (index * 8);
        
        let block_num = (entry_offset / BLOCK_SIZE as u64) + self.partition_offset;
        let offset_in_block = (entry_offset % BLOCK_SIZE as u64) as usize;
        
        let mut buffer = vec![0u8; BLOCK_SIZE];
        self.device.read_block(block_num, &mut buffer)?;
        
        let inode_num = u32::from_le_bytes([
            buffer[offset_in_block], buffer[offset_in_block+1], 
            buffer[offset_in_block+2], buffer[offset_in_block+3]
        ]) as u64;
        
        let rel_offset = u32::from_le_bytes([
            buffer[offset_in_block+4], buffer[offset_in_block+5], 
            buffer[offset_in_block+6], buffer[offset_in_block+7]
        ]) as u64;
            
        let absolute_offset = self.header.inode_table_offset + self.header.inode_table_size + rel_offset;
        
        Ok(InodeTableEntry::new(inode_num, absolute_offset))
    }
    
    fn list_dir(&self, inode: u32) -> Result<(), &'static str> {
        let entry = self.read_inode(inode)?;
        
        let block_num = (entry.offset / BLOCK_SIZE as u64) + self.partition_offset;
        let offset_in_block = (entry.offset % BLOCK_SIZE as u64) as usize;
        
        let mut block_buffer = vec![0u8; BLOCK_SIZE];
        self.device.read_block(block_num, &mut block_buffer)?;
        
        // Parse TLVs
        let mut offset = offset_in_block + 8; // Skip header
        let end = block_buffer.len(); 
        
        if offset >= end {
             return Ok(());
        }

        while offset + 6 <= end {
            let tag = u16::from_le_bytes([block_buffer[offset], block_buffer[offset+1]]);
            let length = u32::from_le_bytes([
                block_buffer[offset+2], block_buffer[offset+3], 
                block_buffer[offset+4], block_buffer[offset+5]
            ]) as usize;
            
            offset += 6;
            
            if offset + length > end {
                break;
            }

            if tag == tlv_tags::DIRECTORY_ENTRIES {
                let dir_data = &block_buffer[offset..offset+length];
                let mut dir_offset = 0;
                
                println!("[FS-SERVICE] Directory listing:");
                
                while dir_offset + 8 <= dir_data.len() {
                    let name_len = u32::from_le_bytes([
                        dir_data[dir_offset], dir_data[dir_offset+1],
                        dir_data[dir_offset+2], dir_data[dir_offset+3]
                    ]) as usize;
                    
                    let child_inode = u32::from_le_bytes([
                        dir_data[dir_offset+4], dir_data[dir_offset+5],
                        dir_data[dir_offset+6], dir_data[dir_offset+7]
                    ]);
                    
                    if dir_offset + 8 + name_len <= dir_data.len() {
                        if let Ok(name) = core::str::from_utf8(&dir_data[dir_offset+8..dir_offset+8+name_len]) {
                             println!("  - {} (inode {})", name, child_inode);
                        }
                    }
                    
                    dir_offset += 8 + name_len;
                }
            }
            
            offset += length;
        }
        
        Ok(())
    }
}

/// Attempt to locate the EclipseFS partition via the GPT partition table.
///
/// The GPT header lives at LBA 1 (512-byte sectors = byte offset 512 on disk).
/// Since our BlockDevice reads 4096-byte blocks, the GPT header is at byte 512
/// inside block 0 of the disk.
///
/// Returns the start offset in 4096-byte blocks, or None.
fn find_eclipsefs_in_gpt(device: &BlockDevice) -> Option<(u64, usize)> {
    // Read block 0 — contains MBR/protective-MBR + beginning of the GPT header.
    let mut block0 = vec![0u8; BLOCK_SIZE];
    device.read_block(0, &mut block0).ok()?;

    // GPT signature at byte 512: "EFI PART"
    const GPT_SIG: &[u8] = b"EFI PART";
    let gpt_start = 512usize;
    if block0.len() < gpt_start + 8 || &block0[gpt_start..gpt_start + 8] != GPT_SIG {
        println!("[FS-SERVICE]   No GPT header at byte 512 — skipping GPT scan");
        return None;
    }

    // GPT header: PartitionEntryLBA at byte +72, NumEntries at +80, EntrySize at +84.
    let h = &block0[gpt_start..];
    let part_entry_lba = u64::from_le_bytes([h[72],h[73],h[74],h[75],h[76],h[77],h[78],h[79]]);
    let num_entries    = u32::from_le_bytes([h[80],h[81],h[82],h[83]]) as usize;
    let entry_size     = u32::from_le_bytes([h[84],h[85],h[86],h[87]]) as usize;

    if entry_size < 128 || num_entries == 0 || num_entries > 128 {
        return None;
    }

    // Convert 512-byte LBA to 4096-byte block index.
    let part_table_block       = part_entry_lba / 8;
    let part_table_byte_offset = ((part_entry_lba % 8) * 512) as usize;

    let total_bytes   = num_entries * entry_size;
    let blocks_needed = ((part_table_byte_offset + total_bytes + BLOCK_SIZE - 1) / BLOCK_SIZE).min(8);

    let mut part_buf = vec![0u8; blocks_needed * BLOCK_SIZE];
    for b in 0..blocks_needed {
        let blk_slice = &mut part_buf[b * BLOCK_SIZE..(b + 1) * BLOCK_SIZE];
        if device.read_block(part_table_block + b as u64, blk_slice).is_err() {
            break;
        }
    }

    println!("[FS-SERVICE]   GPT: {} entries, entry_size={}, first_entry_lba={}",
        num_entries, entry_size, part_entry_lba);

    for i in 0..num_entries {
        let entry_start = part_table_byte_offset + i * entry_size;
        if entry_start + 128 > part_buf.len() { break; }
        let e = &part_buf[entry_start..];

        // Type GUID is at offset 0 (16 bytes); all zeros = unused entry.
        if e[0..16].iter().all(|&b| b == 0) { continue; }

        // StartingLBA at offset 32 (8 bytes).
        let start_lba   = u64::from_le_bytes([e[32],e[33],e[34],e[35],e[36],e[37],e[38],e[39]]);
        let start_block = start_lba / 8;

        // Check for EclipseFS magic at the start of this partition.
        let mut test_buf = vec![0u8; BLOCK_SIZE];
        if device.read_block(start_block, &mut test_buf).is_err() { continue; }
        if test_buf.len() >= ECLIPSEFS_MAGIC.len() && &test_buf[..ECLIPSEFS_MAGIC.len()] == ECLIPSEFS_MAGIC {
            println!("[FS-SERVICE]   GPT entry {}: EclipseFS at LBA {} (block {})",
                i, start_lba, start_block);
            return Some((start_block, i + 1));
        }
    }

    None
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Initialize Allocator
    unsafe {
        ALLOCATOR.lock().init(HEAP_MEM.as_mut_ptr(), HEAP_SIZE);
    }

    let pid = getpid();
    
    println!("+--------------------------------------------------------------+");
    println!("|              FILESYSTEM SERVICE (VFS)                        |");
    println!("+--------------------------------------------------------------+");
    println!("[FS-SERVICE] Starting (PID: {})", pid);
    println!("[FS-SERVICE] Will probe disk:0..disk:N via GPT scan + known offsets.");

    let mut found = false;
    println!("[FS-SERVICE] Probing for root filesystem...");
    
    let device_count = get_storage_device_count();
    println!("[FS-SERVICE] Found {} storage device(s)", device_count);
    if device_count == 0 {
        println!("[FS-SERVICE] CRITICAL: AHCI/NVMe registered zero block devices.");
        println!("[FS-SERVICE] Check that the AHCI controller is detected by the kernel (look for [AHCI] lines above).");
    }

    for i in 0..device_count {
        let device_name = format!("disk:{}", i);
        println!("[FS-SERVICE] Probing {}...", device_name);
        
        match BlockDevice::new(&device_name) {
            Ok(device) => {
                println!("[FS-SERVICE]   {} opened, scanning for EclipseFS...", device_name);
                match EclipseFS::mount(&device) {
                    Ok((fs, partition_offset, part_idx)) => {
                        println!("[FS-SERVICE] Valid filesystem found on {} at block {} ({} MiB)!",
                            device_name, partition_offset,
                            (partition_offset * BLOCK_SIZE as u64) / (1024 * 1024));
                        
                        // Construct the specific mount path for the kernel
                        let mount_path = if let Some(idx) = part_idx {
                            format!("disk:{}p{}", i, idx)
                        } else {
                            format!("disk:{}@{}", i, partition_offset)
                        };

                        println!("[FS-SERVICE] Version: {}.{}",
                            fs.header.version >> 16, fs.header.version & 0xFFFF);

                        // Notify kernel to mount root with this specific device string
                        println!("[FS-SERVICE] Notifying kernel to mount {} as root...", mount_path);
                        if mount(&mount_path) == 0 {
                            println!("[FS-SERVICE] Kernel root mount successful!");

                            // List root directory from our side to verify
                            if let Err(e) = fs.list_dir(1) {
                                println!("[FS-SERVICE] Failed to list root: {}", e);
                            }

                            found = true;
                            break;
                        } else {
                            println!("[FS-SERVICE] Kernel root mount FAILED for {}!", device_name);
                        }
                    },
                    Err(e) => {
                        println!("[FS-SERVICE]   {} — {}", device_name, e);
                    }
                }
            },
            Err(e) => {
                println!("[FS-SERVICE]   {} — could not open device ({})", device_name, e);
            }
        }
    }

    if !found {
        println!("[FS-SERVICE] CRITICAL: No EclipseFS root partition found on any disk!");
        println!("[FS-SERVICE] Tried GPT scan + offsets: {:?}", FALLBACK_OFFSETS);
        // Do NOT signal READY — the rest of the system must not start without a filesystem.
        println!("[FS-SERVICE] Halting — filesystem not mounted.");
        loop {
            sleep_ms(100);
        }
    }

    println!("[FS-SERVICE] Entering main loop...");
    let ppid = getppid();
    if ppid > 0 {
        let _ = send(ppid, 255, b"READY");
    }
    loop {
        sleep_ms(100);
    }
}
