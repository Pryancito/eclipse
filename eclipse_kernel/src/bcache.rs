//! Buffer Cache (Block Cache)
//! 
//! Improves I/O performance by caching disk blocks in RAM.
//! - Cache Size: `CACHE_SIZE` blocks (ver constante; p. ej. 1024 × 4 KiB = 4 MiB)
//! - Policy: Write-Back (dirty blocks are written on eviction/sync)
//! - Replacement: LRU (Least Recently Used)

use spin::Mutex;
use alloc::vec::Vec;
use alloc::vec;

/// Entradas de 4 KiB; más entradas = menos misses en lecturas repetidas (coste RAM: × 4096).
const CACHE_SIZE: usize = 1024;
const BLOCK_SIZE: usize = 4096;

#[derive(Clone, Copy, PartialEq)]
struct CacheEntry {
    device_idx: usize,
    block_num: u64,
    valid: bool,
    dirty: bool,
    last_access: u64, // For LRU
}

impl CacheEntry {
    fn new() -> Self {
        Self {
            device_idx: 0,
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
    entries: [CacheEntry { device_idx: 0, block_num: 0, valid: false, dirty: false, last_access: 0 }; CACHE_SIZE],
    data: Vec::new(), 
    access_counter: 0,
});

/// Initialize the buffer cache
pub fn init() {
    let mut cache = CACHE.lock();
    for _ in 0..CACHE_SIZE {
        cache.data.push([0u8; BLOCK_SIZE]);
    }
    crate::serial::serial_printf(format_args!(
        "[BCACHE] Buffer Cache initialized ({} blocks, {} KiB)\n",
        CACHE_SIZE,
        CACHE_SIZE * BLOCK_SIZE / 1024
    ));
}

/// Helper to read from underlying device
fn read_from_device(device_idx: usize, block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if let Some(dev) = crate::storage::get_device(device_idx) {
        dev.read(block_num, buffer)
    } else {
        Err("Device not found")
    }
}

/// Helper to write to underlying device
fn write_to_device(device_idx: usize, block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    if let Some(dev) = crate::storage::get_device(device_idx) {
        dev.write(block_num, buffer)
    } else {
        Err("Device not found")
    }
}


/// Read a block through the cache
pub fn read_block(device_idx: usize, block_num: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() != BLOCK_SIZE {
        return Err("Buffer size must be 4096");
    }

    let mut cache = CACHE.lock();
    
    // 1. Search Cache
    let mut hit_idx = None;
    for i in 0..CACHE_SIZE {
        if cache.entries[i].valid && cache.entries[i].device_idx == device_idx && cache.entries[i].block_num == block_num {
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
    let dev_to_write = cache.entries[victim_idx].device_idx;
    let is_dirty = cache.entries[victim_idx].valid && cache.entries[victim_idx].dirty;
    
    if is_dirty {
        // Write-Back: We must write the dirty block to disk before replacing it
        if let Err(_) = write_to_device(dev_to_write, block_to_write, &cache.data[victim_idx]) {
             crate::serial::serial_print("[BCACHE] WRITE ERROR during eviction\n");
        }
    }

    // 4. Fill from Disk
    match read_from_device(device_idx, block_num, &mut cache.data[victim_idx]) {
        Ok(_) => {
            // Update Metadata
            cache.access_counter += 1;
            cache.entries[victim_idx].device_idx = device_idx;
            cache.entries[victim_idx].block_num = block_num;
            cache.entries[victim_idx].valid = true;
            cache.entries[victim_idx].dirty = false;
            cache.entries[victim_idx].last_access = cache.access_counter;
            
            // Copy to user buffer
            buffer.copy_from_slice(&cache.data[victim_idx]);

            // NOTE: AI pre-fetching was removed from here to prevent a mutual
            // recursion that could exhaust the kernel stack:
            //   read_block → prefetch_ai → read_block → prefetch_ai → …
            // The prefetch path must be driven from a higher-level call site
            // where it is safe to call read_block again.

            Ok(())
        },
        Err(e) => Err(e)
    }
}

/// Write a block through the cache (Write-Back)
pub fn write_block(device_idx: usize, block_num: u64, buffer: &[u8]) -> Result<(), &'static str> {
    if buffer.len() != BLOCK_SIZE {
        return Err("Buffer size must be 4096");
    }

    let mut cache = CACHE.lock();

    // 1. Check if block is in cache
    let mut target_idx = None;
    for i in 0..CACHE_SIZE {
        if cache.entries[i].valid && cache.entries[i].device_idx == device_idx && cache.entries[i].block_num == block_num {
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
            let dirty_dev = cache.entries[victim_idx].device_idx;
             if let Err(_) = write_to_device(dirty_dev, dirty_block, &cache.data[victim_idx]) {
                 crate::serial::serial_print("[BCACHE] WRITE ERROR during eviction\n");
             }
        }
        victim_idx
    };

    // 3. Update Cache (Write-Hit / Write-Allocate)
    cache.access_counter += 1;
    cache.entries[idx].device_idx = device_idx;
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
             let device_idx = cache.entries[i].device_idx;
             if let Ok(_) = write_to_device(device_idx, block_num, &cache.data[i]) {
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

/// AI-Core: Predict and pre-load likely next blocks into the cache
pub fn prefetch_ai(device_idx: usize) {
    if let Some(pid) = crate::process::current_process_id() {
        // We use pid_to_slot_fast + direct PROCESS_TABLE access to avoid expensive cloning
        if let Some(slot) = crate::ipc::pid_to_slot_fast(pid) {
            let table = crate::process::PROCESS_TABLE.lock();
            if let Some(p) = &table[slot] {
                let predictions = p.ai_profile.predict_next_blocks();
                drop(table);

                for block_num in predictions {
                    // Check if already in cache (optimized check)
                    let in_cache = {
                        let cache = CACHE.lock();
                        let mut found = false;
                        for i in 0..CACHE_SIZE {
                            if cache.entries[i].valid && cache.entries[i].device_idx == device_idx && cache.entries[i].block_num == block_num {
                                found = true;
                                break;
                            }
                        }
                        found
                    };

                    if !in_cache {
                        let mut dummy = [0u8; BLOCK_SIZE];
                        // read_block will perform the actual fetch and cache insertion
                        let _ = read_block(device_idx, block_num, &mut dummy);
                    }
                }
            }
        }
    }
}
