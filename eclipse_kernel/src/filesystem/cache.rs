//! Cache de archivos para Eclipse OS

use core::sync::atomic::{AtomicU64, Ordering};

// Entrada de cache
#[derive(Debug, Clone, Copy)]
pub struct CacheEntry {
    pub inode: u32,
    pub data: [u8; 4096],
    pub dirty: bool,
    pub last_access: u64,
    pub access_count: u32,
}

impl CacheEntry {
    pub fn new(inode: u32) -> Self {
        Self {
            inode,
            data: [0; 4096],
            dirty: false,
            last_access: 0,
            access_count: 0,
        }
    }

    pub fn update_access(&mut self) {
        self.last_access = get_current_time();
        self.access_count += 1;
    }

    pub fn is_valid(&self) -> bool {
        self.inode != 0
    }
}

// Cache de archivos
pub struct FileCache {
    pub entries: [Option<CacheEntry>; 64], // 64 entradas de cache
    pub hit_count: u64,
    pub miss_count: u64,
}

impl FileCache {
    pub fn new() -> Self {
        Self {
            entries: [None; 64],
            hit_count: 0,
            miss_count: 0,
        }
    }

    pub fn get(&mut self, inode: u32) -> Option<&mut CacheEntry> {
        for entry in &mut self.entries {
            if let Some(ref mut cache_entry) = entry {
                if cache_entry.inode == inode {
                    cache_entry.update_access();
                    self.hit_count += 1;
                    return Some(cache_entry);
                }
            }
        }
        self.miss_count += 1;
        None
    }

    pub fn put(&mut self, inode: u32) -> &mut CacheEntry {
        // Buscar slot libre
        for i in 0..self.entries.len() {
            if self.entries[i].is_none() {
                self.entries[i] = Some(CacheEntry::new(inode));
                return self.entries[i].as_mut().unwrap();
            }
        }

        // Si no hay slots libres, usar algoritmo LRU
        let lru_index = self.find_lru_entry();
        self.entries[lru_index] = Some(CacheEntry::new(inode));
        self.entries[lru_index].as_mut().unwrap()
    }

    /// Encontrar la entrada menos recientemente usada
    fn find_lru_entry(&self) -> usize {
        let mut lru_index = 0;
        let mut oldest_time = u64::MAX;

        for (i, entry) in self.entries.iter().enumerate() {
            if let Some(cache_entry) = entry {
                if cache_entry.last_access < oldest_time {
                    oldest_time = cache_entry.last_access;
                    lru_index = i;
                }
            } else {
                return i; // Slot libre encontrado
            }
        }

        lru_index
    }

    /// Limpiar entradas sucias
    pub fn flush_dirty_entries(&mut self) {
        for entry in &mut self.entries {
            if let Some(ref mut cache_entry) = entry {
                if cache_entry.dirty {
                    // Aquí se escribiría al disco
                    cache_entry.dirty = false;
                }
            }
        }
    }

    /// Obtener estadísticas del cache
    pub fn get_stats(&self) -> (u64, u64, f64) {
        let hit_rate = if self.hit_count + self.miss_count > 0 {
            self.hit_count as f64 / (self.hit_count + self.miss_count) as f64
        } else {
            0.0
        };
        (self.hit_count, self.miss_count, hit_rate)
    }
}

// Instancia global del cache
static mut FILE_CACHE: Option<FileCache> = None;

pub fn init_file_cache() -> Result<(), &'static str> {
    unsafe {
        FILE_CACHE = Some(FileCache::new());
    }
    Ok(())
}

pub fn get_file_cache() -> Option<&'static mut FileCache> {
    unsafe { FILE_CACHE.as_mut() }
}

/// Obtener tiempo actual (simplificado)
fn get_current_time() -> u64 {
    // Implementación simplificada - retorna timestamp fijo
    1640995200 // 2022-01-01 00:00:00 UTC
}
