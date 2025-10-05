//! Cache de bloques estático para el kernel sin allocación dinámica.
//! Implementa carga bajo demanda para sistemas de archivos grandes.

use crate::drivers::storage_manager::StorageManager;
use core::sync::atomic::{AtomicU64, Ordering};

/// Tamaño de bloque estándar (512 bytes)
pub const BLOCK_SIZE: usize = 512;

/// Número máximo de bloques en cache
const CACHE_SIZE: usize = 64; // 64 bloques = 32KB de cache

/// Cache de bloques estático para el kernel
pub struct StaticBlockCache {
    /// Bloques en cache
    blocks: [Option<[u8; BLOCK_SIZE]>; CACHE_SIZE],
    /// Números de bloque correspondientes
    block_numbers: [Option<u64>; CACHE_SIZE],
    /// Flags de bloques sucios
    dirty_flags: [bool; CACHE_SIZE],
    /// Tiempos de acceso para LRU
    access_times: [u64; CACHE_SIZE],
    /// Contador de tiempo global
    time_counter: AtomicU64,
}

impl StaticBlockCache {
    /// Crear nuevo cache estático
    pub const fn new() -> Self {
        Self {
            blocks: [None; CACHE_SIZE],
            block_numbers: [None; CACHE_SIZE],
            dirty_flags: [false; CACHE_SIZE],
            access_times: [0; CACHE_SIZE],
            time_counter: AtomicU64::new(0),
        }
    }

    /// Obtener bloque del cache o cargarlo del disco
    pub fn get_or_load_block(&mut self, block_num: u64, storage: &mut StorageManager, partition_index: u32) -> Result<&mut [u8; BLOCK_SIZE], &'static str> {
        // Buscar en cache primero
        if let Some(slot) = self.find_cached_block(block_num) {
            self.access_times[slot] = self.get_current_time();
            crate::debug::serial_write_str(&alloc::format!("BLOCK_CACHE: Cache hit para bloque {}\n", block_num));
            return Ok(self.blocks[slot].as_mut().unwrap());
        }

        // Si no está en cache, cargarlo del disco
        crate::debug::serial_write_str(&alloc::format!("BLOCK_CACHE: Cache miss para bloque {}, cargando del disco\n", block_num));
        self.load_block_from_disk(block_num, storage, partition_index)
    }

    /// Buscar bloque en cache
    fn find_cached_block(&self, block_num: u64) -> Option<usize> {
        for i in 0..CACHE_SIZE {
            if self.block_numbers[i] == Some(block_num) {
                return Some(i);
            }
        }
        None
    }

    /// Cargar bloque del disco al cache
    fn load_block_from_disk(&mut self, block_num: u64, storage: &mut StorageManager, partition_index: u32) -> Result<&mut [u8; BLOCK_SIZE], &'static str> {
        // Buscar slot libre o usar LRU
        let slot = self.find_free_slot().unwrap_or_else(|| self.find_lru_slot());

        // Escribir bloque sucio si es necesario
        if self.dirty_flags[slot] {
            if let Some(dirty_block_num) = self.block_numbers[slot] {
                self.write_block_to_disk(dirty_block_num, &self.blocks[slot].unwrap(), storage, partition_index)?;
            }
        }

        // Inicializar slot para nuevo bloque
        self.blocks[slot] = Some([0; BLOCK_SIZE]);
        self.block_numbers[slot] = Some(block_num);
        self.dirty_flags[slot] = false;
        self.access_times[slot] = self.get_current_time();

        // Leer del disco usando el storage manager
        let buffer = self.blocks[slot].as_mut().unwrap();
        storage.read_from_partition(partition_index, block_num, buffer)
            .map_err(|_| "Error leyendo bloque del disco")?;

        crate::debug::serial_write_str(&alloc::format!("BLOCK_CACHE: Bloque {} cargado exitosamente en slot {}\n", block_num, slot));
        Ok(buffer)
    }

    /// Escribir bloque al disco
    fn write_block_to_disk(&self, block_num: u64, data: &[u8; BLOCK_SIZE], storage: &mut StorageManager, partition_index: u32) -> Result<(), &'static str> {
        storage.write_to_partition(partition_index, block_num, data)
            .map_err(|_| "Error escribiendo bloque al disco")?;
        Ok(())
    }

    /// Encontrar slot libre
    fn find_free_slot(&self) -> Option<usize> {
        for i in 0..CACHE_SIZE {
            if self.blocks[i].is_none() {
                return Some(i);
            }
        }
        None
    }

    /// Encontrar slot LRU (Least Recently Used)
    fn find_lru_slot(&self) -> usize {
        let mut lru_slot = 0;
        let mut oldest_time = u64::MAX;

        for i in 0..CACHE_SIZE {
            if self.access_times[i] < oldest_time {
                oldest_time = self.access_times[i];
                lru_slot = i;
            }
        }

        lru_slot
    }

    /// Marcar bloque como sucio
    pub fn mark_dirty(&mut self, block_num: u64) {
        for i in 0..CACHE_SIZE {
            if self.block_numbers[i] == Some(block_num) {
                self.dirty_flags[i] = true;
                crate::debug::serial_write_str(&alloc::format!("BLOCK_CACHE: Bloque {} marcado como sucio\n", block_num));
                break;
            }
        }
    }

    /// Sincronizar todos los bloques sucios
    pub fn sync(&mut self, storage: &mut StorageManager, partition_index: u32) -> Result<(), &'static str> {
        crate::debug::serial_write_str("BLOCK_CACHE: Sincronizando bloques sucios...\n");
        
        for i in 0..CACHE_SIZE {
            if self.dirty_flags[i] {
                if let Some(block_num) = self.block_numbers[i] {
                    self.write_block_to_disk(block_num, &self.blocks[i].unwrap(), storage, partition_index)?;
                    self.dirty_flags[i] = false;
                    crate::debug::serial_write_str(&alloc::format!("BLOCK_CACHE: Bloque {} sincronizado\n", block_num));
                }
            }
        }

        crate::debug::serial_write_str("BLOCK_CACHE: Sincronización completada\n");
        Ok(())
    }

    /// Obtener estadísticas del cache
    pub fn get_stats(&self) -> (usize, usize, usize) {
        let total_blocks = self.blocks.iter().filter(|b| b.is_some()).count();
        let dirty_blocks = self.dirty_flags.iter().filter(|&&dirty| dirty).count();
        let free_slots = CACHE_SIZE - total_blocks;
        (total_blocks, dirty_blocks, free_slots)
    }

    /// Obtener tiempo actual (contador simple)
    fn get_current_time(&self) -> u64 {
        self.time_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Invalidar cache completo
    pub fn invalidate_all(&mut self) {
        crate::debug::serial_write_str("BLOCK_CACHE: Invalidando cache completo\n");
        for i in 0..CACHE_SIZE {
            self.blocks[i] = None;
            self.block_numbers[i] = None;
            self.dirty_flags[i] = false;
            self.access_times[i] = 0;
        }
    }
}

/// Instancia global del cache de bloques
static mut BLOCK_CACHE: StaticBlockCache = StaticBlockCache::new();

/// Obtener referencia mutable al cache global
pub fn get_block_cache() -> &'static mut StaticBlockCache {
    unsafe { &mut BLOCK_CACHE }
}

/// Función de utilidad para leer datos desde un offset específico
pub fn read_data_from_offset(
    cache: &mut StaticBlockCache,
    storage: &mut StorageManager,
    partition_index: u32,
    offset: u64,
    buffer: &mut [u8]
) -> Result<usize, &'static str> {
    let block_size = BLOCK_SIZE as u64;
    let start_block = offset / block_size;
    let block_offset = (offset % block_size) as usize;
    
    let mut bytes_read = 0;
    let mut remaining = buffer.len();
    let mut current_offset = block_offset;
    let mut current_block = start_block;
    
    while remaining > 0 && bytes_read < buffer.len() {
        let block_data = cache.get_or_load_block(current_block, storage, partition_index)?;
        let available = block_data.len() - current_offset;
        let to_copy = remaining.min(available);
        
        buffer[bytes_read..bytes_read + to_copy]
            .copy_from_slice(&block_data[current_offset..current_offset + to_copy]);
        
        bytes_read += to_copy;
        remaining -= to_copy;
        current_offset = 0;
        current_block += 1;
    }
    
    Ok(bytes_read)
}
