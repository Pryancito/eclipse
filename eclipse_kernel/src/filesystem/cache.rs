//! Cache de archivos para Eclipse OS


// Entrada de cache
#[derive(Debug, Clone, Copy)]
pub struct CacheEntry {
    pub inode: u32,
    pub data: [u8; 4096],
    pub dirty: bool,
    pub last_access: u64,
}

impl CacheEntry {
    pub fn new(inode: u32) -> Self {
        Self {
            inode,
            data: [0; 4096],
            dirty: false,
            last_access: 0,
        }
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
        
        // Si no hay slots libres, usar el primero (simplificado)
        self.entries[0] = Some(CacheEntry::new(inode));
        self.entries[0].as_mut().unwrap()
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
