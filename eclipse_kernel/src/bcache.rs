//! Buffer Cache (Block Cache)
//! 
//! Improves I/O performance by caching disk blocks in RAM.
//! - Cache Size: 128 blocks (512 KB)
//! - Policy: Write-Back (dirty blocks are written on eviction/sync)
//! - Replacement: LRU (Least Recently Used)

use spin::Mutex;
use alloc::vec::Vec;
use alloc::vec;

const CACHE_SIZE: usize = 128; // 128 * 4KB = 512KB cache
const BLOCK_SIZE: usize = 4096;

#[derive(Clone, Copy, PartialEq)]
struct CacheEntry {
    block_num: u64,
    valid: bool,
    dirty: bool,
    last_access: u64, // For LRU
}

impl CacheEntry {
    fn new() -> Self {
        Self {
            block_num: 0,
            valid: false,
            dirty: false,
            last_access: 0,
        }
    }
}

pub struct BufferCache {
    entries: [CacheEntry; CACHE_SIZE],
    data: Vec<[u8; BLOCK_SIZE]>,
    access_counter: u64,
}

struct GlobalCache {
    inner: Mutex<BufferCache>,
}

// We need a wrapper because Vec::new() is not const fn in no_std context typically?
// Actually specific Vec::new IS const safe now but let's be careful with static initialization.
// We'll initialize Empty and reserve in init().

static CACHE: Mutex<BufferCache> = Mutex::new(BufferCache {
    entries: [CacheEntry { block_num: 0, valid: false, dirty: false, last_access: 0 }; CACHE_SIZE],
    data: Vec::new(), 
    access_counter: 0,
});

/// Initialize the buffer cache
pub fn init() {
    let mut cache = CACHE.lock();
    // Pre-allocate the data buffer (512KB)
    // We explicitly push 128 blocks of 4096 bytes.
    for _ in 0..CACHE_SIZE {
        cache.data.push([0u8; BLOCK_SIZE]);
    }
    crate::serial::serial_print("[BCACHE] Buffer Cache initialized (512KB)\n");
}

/// Helper to read from underlying device
fn read_from_device(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    // Try VirtIO first
    if let Ok(_) = crate::virtio::read_block(block_num, buffer) {
        return Ok(());
    }
    // Fallback to ATA
    crate::ata::read_block(block_num, buffer)
}

/// Helper to write to underlying device
fn write_to_device(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    // Try VirtIO first
    if let Ok(_) = crate::virtio::write_block(block_num, buffer) {
        return Ok(());
    }
    // ATA write not implemented
    Err("Device write failed")
}


/// Read a block through the cache
pub fn read_block(block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() != BLOCK_SIZE {
        return Err("Buffer size must be 4096");
    }

    let mut cache = CACHE.lock();
    
    // 1. Search Cache
    let mut hit_idx = None;
    for i in 0..CACHE_SIZE {
        if cache.entries[i].valid && cache.entries[i].block_num == block_num {
            hit_idx = Some(i);
            break;
        }
    }

    if let Some(idx) = hit_idx {
        // HIT
        cache.access_counter += 1;
        cache.entries[idx].last_access = cache.access_counter;
        buffer.copy_from_slice(&cache.data[idx]);
        return Ok(());
    }

    // 2. MISS - Find Victim (LRU)
    let mut victim_idx = 0;
    let mut min_access = u64::MAX;

    // First pass: invalid entries
    let mut found_invalid = false;
    for i in 0..CACHE_SIZE {
        if !cache.entries[i].valid {
            victim_idx = i;
            found_invalid = true;
            break;
        }
    }

    if !found_invalid {
         // Second pass: LRU
         for i in 0..CACHE_SIZE {
            if cache.entries[i].last_access < min_access {
                min_access = cache.entries[i].last_access;
                victim_idx = i;
            }
        }
    }

    // 3. Evict Victim
    let block_to_write = cache.entries[victim_idx].block_num;
    let is_dirty = cache.entries[victim_idx].valid && cache.entries[victim_idx].dirty;
    
    if is_dirty {
        // Write-Back: We must write the dirty block to disk before replacing it
        // We clone the data to release the lock? No, keep it simple.
        // NOTE: This blocks the whole cache during device write. 
        if let Err(_) = write_to_device(block_to_write, &cache.data[victim_idx]) {
             crate::serial::serial_print("[BCACHE] WRITE ERROR during eviction\n");
             // We can't do much here except log it.
        }
    }

    // 4. Fill from Disk
    // Release lock logic would be complex here, so we hold it.
    // Read directly into the cache slot
    match read_from_device(block_num, &mut cache.data[victim_idx]) {
        Ok(_) => {
            // Update Metadata
            cache.access_counter += 1;
            cache.entries[victim_idx].block_num = block_num;
            cache.entries[victim_idx].valid = true;
            cache.entries[victim_idx].dirty = false;
            cache.entries[victim_idx].last_access = cache.access_counter;
            
            // Copy to user buffer
            buffer.copy_from_slice(&cache.data[victim_idx]);
            Ok(())
        },
        Err(e) => Err(e)
    }
}

/// Write a block through the cache (Write-Back)
pub fn write_block(block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    if buffer.len() != BLOCK_SIZE {
        return Err("Buffer size must be 4096");
    }

    let mut cache = CACHE.lock();

    // 1. Check if block is in cache
    let mut target_idx = None;
    for i in 0..CACHE_SIZE {
        if cache.entries[i].valid && cache.entries[i].block_num == block_num {
            target_idx = Some(i);
            break;
        }
    }

    // 2. If not in cache, allocate a slot (Eviction Logic)
    let idx = if let Some(i) = target_idx {
        i
    } else {
        // Find Victim
        let mut victim_idx = 0;
        let mut min_access = u64::MAX;
        let mut found_invalid = false;
        
        for i in 0..CACHE_SIZE {
            if !cache.entries[i].valid {
                victim_idx = i;
                found_invalid = true;
                break;
            }
        }

        if !found_invalid {
             for i in 0..CACHE_SIZE {
                if cache.entries[i].last_access < min_access {
                    min_access = cache.entries[i].last_access;
                    victim_idx = i;
                }
            }
        }
        
        // Evict if dirty
        if cache.entries[victim_idx].valid && cache.entries[victim_idx].dirty {
            let dirty_block = cache.entries[victim_idx].block_num;
             if let Err(_) = write_to_device(dirty_block, &cache.data[victim_idx]) {
                 crate::serial::serial_print("[BCACHE] WRITE ERROR during eviction\n");
             }
        }
        victim_idx
    };

    // 3. Update Cache (Write-Hit / Write-Allocate)
    // We simply overwrite the cache slot with new data and mark it dirty.
    // We do NOT need to read from disk if we are overwriting the whole block (4096 bytes).
    // The buffer size check at the top ensures we have a full block.
    
    cache.access_counter += 1;
    cache.entries[idx].block_num = block_num;
    cache.entries[idx].valid = true;
    cache.entries[idx].dirty = true; // Cached Copy is now newer than Disk
    cache.entries[idx].last_access = cache.access_counter; // MRU
    
    cache.data[idx].copy_from_slice(buffer);

    Ok(())
}

/// Flush all dirty blocks to disk
pub fn flush() {
    let mut cache = CACHE.lock();
    crate::serial::serial_print("[BCACHE] Flushing dirty blocks...\n");
    
    for i in 0..CACHE_SIZE {
        if cache.entries[i].valid && cache.entries[i].dirty {
             let block_num = cache.entries[i].block_num;
             if let Ok(_) = write_to_device(block_num, &cache.data[i]) {
                 cache.entries[i].dirty = false;
             } else {
                 crate::serial::serial_print("[BCACHE] Failed to flush block ");
                 crate::serial::serial_print_dec(block_num);
                 crate::serial::serial_print("\n");
             }
        }
    }
    crate::serial::serial_print("[BCACHE] Flush complete\n");
}
