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
    println, getpid, yield_cpu, 
    open, close, read, lseek, 
    O_RDONLY, SEEK_SET
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
/// Partition offset (same as kernel for now)
const PARTITION_OFFSET_BLOCKS: u64 = 131328;

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
}

impl<'a> EclipseFS<'a> {
    fn mount(device: &'a BlockDevice) -> Result<Self, &'static str> {
        let mut superblock = vec![0u8; BLOCK_SIZE];
        
        // Read superblock
        device.read_block(PARTITION_OFFSET_BLOCKS, &mut superblock)?;
        
        // Parse header
        let header = EclipseFSHeader::from_bytes(&superblock)
            .map_err(|_| "Invalid EclipseFS header")?;
            
        Ok(Self {
            device,
            inode_table_offset: header.inode_table_offset,
            header,
        })
    }
    
    fn read_inode(&self, inode: u32) -> Result<InodeTableEntry, &'static str> {
        if inode < 1 || inode > self.header.total_inodes {
            return Err("Inode out of range");
        }
        
        let index = (inode - 1) as u64;
        let entry_offset = self.inode_table_offset + (index * 8);
        
        let block_num = (entry_offset / BLOCK_SIZE as u64) + PARTITION_OFFSET_BLOCKS;
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
        
        let block_num = (entry.offset / BLOCK_SIZE as u64) + PARTITION_OFFSET_BLOCKS;
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

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Initialize Allocator
    unsafe {
        ALLOCATOR.lock().init(HEAP_MEM.as_mut_ptr(), HEAP_SIZE);
    }

    let pid = getpid();
    
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║              FILESYSTEM SERVICE (VFS)                        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!("[FS-SERVICE] Starting (PID: {})", pid);
    
    println!("[FS-SERVICE] Opening /dev/vda...");
    match BlockDevice::new("/dev/vda") {
        Ok(device) => {
            println!("[FS-SERVICE] Device opened successfully");
            
            match EclipseFS::mount(&device) {
                Ok(fs) => {
                    println!("[FS-SERVICE] Filesystem mounted!");
                    println!("[FS-SERVICE] Version: {}.{}", 
                        fs.header.version >> 16, fs.header.version & 0xFFFF);
                        
                    // List root directory
                    if let Err(e) = fs.list_dir(1) {
                         println!("[FS-SERVICE] Failed to list root: {}", e);
                    }
                },
                Err(e) => println!("[FS-SERVICE] Mount failed: {}", e),
            }
        },
        Err(e) => println!("[FS-SERVICE] Failed to open /dev/vda: {}", e),
    }

    println!("[FS-SERVICE] Entering main loop...");
    loop {
        yield_cpu();
    }
}
