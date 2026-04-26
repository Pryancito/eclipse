//! Page Cache (File-level Cache)
//!
//! Replaces the primitive block-level bcache with an inode-aware page cache.
//! This allows mapping file pages directly into user address space (Zero-Copy).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;
use core::sync::atomic::{AtomicU64, Ordering};

/// Page size (must be 4096 to match hardware pages)
pub const PAGE_SIZE: usize = 4096;

/// A single cached page
pub struct CachedPage {
    /// Physical address of the 4KB frame
    pub phys_addr: u64,
    /// Whether the page has been modified and needs sync to disk
    pub dirty: bool,
    /// Whether the page contains valid data from disk
    pub valid: bool,
    /// Last access timestamp (for LRU eviction)
    pub last_access: u64,
}

impl CachedPage {
    /// Create a new cached page from an existing physical frame
    pub fn new(phys_addr: u64, last_access: u64) -> Self {
        Self {
            phys_addr,
            dirty: false,
            valid: false,
            last_access,
        }
    }

    /// Access the raw data of the page via the Higher-Half Direct Map
    pub fn as_slice(&self) -> &[u8] {
        let virt = crate::memory::phys_to_virt(self.phys_addr);
        unsafe { core::slice::from_raw_parts(virt as *const u8, PAGE_SIZE) }
    }

    /// Access the raw data mutably
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        self.dirty = true;
        let virt = crate::memory::phys_to_virt(self.phys_addr);
        unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, PAGE_SIZE) }
    }
}

/// Global Page Cache Manager
pub struct PageCacheManager {
    /// Mapping: (device_id, inode_id, page_index) -> CachedPage
    /// page_index is the 4KB-aligned offset within the file.
    pages: BTreeMap<(usize, u32, u64), Arc<Mutex<CachedPage>>>,
    access_counter: AtomicU64,
}

impl PageCacheManager {
    pub const fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            access_counter: AtomicU64::new(0),
        }
    }

    /// Get or create a page in the cache. 
    /// If it doesn't exist, it allocates a new frame but does NOT fill it from disk.
    pub fn get_or_create(&mut self, device_id: usize, inode_id: u32, page_index: u64) -> Arc<Mutex<CachedPage>> {
        let key = (device_id, inode_id, page_index);
        let now = self.access_counter.fetch_add(1, Ordering::Relaxed);

        if let Some(page) = self.pages.get(&key) {
            let mut p = page.lock();
            p.last_access = now;
            return Arc::clone(page);
        }

        // Allocate a new physical frame for this page
        // In a real kernel, we would use the frame allocator.
        // For now, we use the anonymous pool to ensure it's mappable to userspace.
        let phys_addr = crate::memory::alloc_phys_frame_for_anon_mmap()
            .expect("PageCache: Out of physical memory");
        
        let page = Arc::new(Mutex::new(CachedPage::new(phys_addr, now)));
        self.pages.insert(key, Arc::clone(&page));
        page
    }

    /// Remove a page from the cache (e.g. on file deletion or eviction)
    pub fn evict(&mut self, device_id: usize, inode_id: u32, page_index: u64) -> bool {
        if let Some(page_arc) = self.pages.remove(&(device_id, inode_id, page_index)) {
            let page = page_arc.lock();
            if page.dirty {
                // TODO: Trigger asynchronous write-back before freeing
                crate::serial::serial_print("[PAGE-CACHE] WARNING: Evicting dirty page without flush!\n");
            }
            // Free the physical frame
            // crate::memory::free_phys_frame(page.phys_addr);
            return true;
        }
        false
    }

    /// Flush all dirty pages for a specific file
    pub fn flush_file(&self, device_id: usize, inode_id: u32) {
        let start = (device_id, inode_id, 0);
        let end = (device_id, inode_id, u64::MAX);
        
        for ((_, _, page_index), page_arc) in self.pages.range(start..=end) {
            let mut page = page_arc.lock();
            if page.dirty {
                // Actual write-back to disk
                let _ = crate::filesystem::write_page_to_disk(inode_id, page.as_slice(), *page_index);
                page.dirty = false;
            }
        }
    }

    /// Flush all dirty pages in the entire cache
    pub fn flush_all(&self) {
        for ((_, ino, page_index), page_arc) in self.pages.iter() {
            let mut page = page_arc.lock();
            if page.dirty {
                // Actual write-back to disk
                let _ = crate::filesystem::write_page_to_disk(*ino, page.as_slice(), *page_index);
                page.dirty = false;
            }
        }
    }
}

/// Global instance of the Page Cache
pub static PAGE_CACHE: Mutex<PageCacheManager> = Mutex::new(PageCacheManager::new());

/// High-level API to read a file through the Page Cache
pub fn read_page(device_id: usize, inode_id: u32, page_index: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
    if buffer.len() != PAGE_SIZE {
        return Err("Buffer must be 4096 bytes");
    }

    let page_arc = PAGE_CACHE.lock().get_or_create(device_id, inode_id, page_index);
    let page = page_arc.lock();

    // If it's a new page (e.g. just allocated), we should ideally fill it from disk.
    // However, get_or_create doesn't know if it was just created.
    // For now, we rely on the caller to check if the page is "fresh" or we add a 'valid' flag.
    
    // Simplified: Always return the data. Inode-level filling logic will be in filesystem.rs.
    buffer.copy_from_slice(page.as_slice());
    Ok(())
}

/// High-level API to write a file through the Page Cache (Write-Back)
pub fn write_page(device_id: usize, inode_id: u32, page_index: u64, data: &[u8]) -> Result<(), &'static str> {
    if data.len() != PAGE_SIZE {
        return Err("Data must be 4096 bytes");
    }

    let page_arc = PAGE_CACHE.lock().get_or_create(device_id, inode_id, page_index);
    let mut page = page_arc.lock();
    
    page.as_slice_mut().copy_from_slice(data);
    page.dirty = true;
    
    Ok(())
}
