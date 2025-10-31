//! Gestión de bloques para Eclipse OS

use crate::filesystem::BLOCK_SIZE;
use core::sync::atomic::{AtomicU64, Ordering};

// Dispositivo de bloques
pub struct BlockDevice {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub block_size: usize,
    pub device_id: u32,
    pub read_count: AtomicU64,
    pub write_count: AtomicU64,
}

impl BlockDevice {
    pub fn new() -> Self {
        Self {
            total_blocks: 0,
            free_blocks: 0,
            block_size: BLOCK_SIZE,
            device_id: 1,
            read_count: AtomicU64::new(0),
            write_count: AtomicU64::new(0),
        }
    }

    pub fn init(&mut self, total_blocks: u64) {
        self.total_blocks = total_blocks;
        self.free_blocks = total_blocks;
    }

    pub fn allocate_block(&mut self) -> Option<u64> {
        if self.free_blocks > 0 {
            self.free_blocks -= 1;
            Some(self.total_blocks - self.free_blocks - 1)
        } else {
            None
        }
    }

    pub fn free_block(&mut self, _block: u64) {
        if self.free_blocks < self.total_blocks {
            self.free_blocks += 1;
        }
    }

    /// Leer un bloque del disco
    pub fn read_block(
        &mut self,
        block_num: u64,
        buffer: &mut [u8; BLOCK_SIZE],
    ) -> Result<(), &'static str> {
        if block_num >= self.total_blocks {
            return Err("Block number out of range");
        }

        // Simular lectura del disco (en un sistema real, esto sería una llamada al driver de disco)
        // Por ahora, llenar con datos de ejemplo
        let pattern = (block_num % 256) as u8;
        for i in 0..BLOCK_SIZE {
            buffer[i] = pattern.wrapping_add(i as u8);
        }

        self.read_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Escribir un bloque al disco
    pub fn write_block(
        &mut self,
        block_num: u64,
        buffer: &[u8; BLOCK_SIZE],
    ) -> Result<(), &'static str> {
        if block_num >= self.total_blocks {
            return Err("Block number out of range");
        }

        // Simular escritura al disco (en un sistema real, esto sería una llamada al driver de disco)
        // Por ahora, solo incrementar el contador
        self.write_count.fetch_add(1, Ordering::Relaxed);

        // En un sistema real, aquí se escribiría al disco físico
        // Por ahora, solo simulamos la operación

        Ok(())
    }

    /// Sincronizar todos los bloques pendientes
    pub fn sync(&mut self) -> Result<(), &'static str> {
        // Simular sincronización con el disco
        // En un sistema real, esto forzaría la escritura de todos los buffers pendientes
        Ok(())
    }

    /// Obtener estadísticas del dispositivo
    pub fn get_stats(&self) -> (u64, u64) {
        (
            self.read_count.load(Ordering::Relaxed),
            self.write_count.load(Ordering::Relaxed),
        )
    }
}

// Cache de bloques
pub struct BlockCache {
    pub blocks: [Option<[u8; BLOCK_SIZE]>; 32], // 32 bloques en cache
    pub block_numbers: [Option<u64>; 32],
    pub dirty_flags: [bool; 32], // Marcar bloques sucios
    pub access_times: [u64; 32], // Para algoritmo LRU
}

impl BlockCache {
    pub fn new() -> Self {
        Self {
            blocks: [None; 32],
            block_numbers: [None; 32],
            dirty_flags: [false; 32],
            access_times: [0; 32],
        }
    }

    pub fn get_block(&mut self, block_num: u64) -> Option<&mut [u8; BLOCK_SIZE]> {
        for i in 0..32 {
            if self.block_numbers[i] == Some(block_num) {
                self.access_times[i] = get_current_time();
                return self.blocks[i].as_mut();
            }
        }
        None
    }

    /// Obtener bloque del cache o cargarlo del disco
    pub fn get_or_load_block(
        &mut self,
        block_num: u64,
    ) -> Result<&mut [u8; BLOCK_SIZE], &'static str> {
        // Buscar en cache primero
        if self.is_block_cached(block_num) {
            return self.get_block(block_num).ok_or("Block not found in cache");
        }

        // Si no está en cache, cargarlo del disco
        self.load_block_from_disk(block_num)
    }

    /// Verificar si un bloque está en cache
    fn is_block_cached(&self, block_num: u64) -> bool {
        for i in 0..self.block_numbers.len() {
            if let Some(cached_block) = self.block_numbers[i] {
                if cached_block == block_num {
                    return true;
                }
            }
        }
        false
    }

    /// Cargar bloque del disco al cache
    fn load_block_from_disk(
        &mut self,
        block_num: u64,
    ) -> Result<&mut [u8; BLOCK_SIZE], &'static str> {
        // Buscar slot libre o usar LRU
        let slot = self
            .find_free_slot()
            .unwrap_or_else(|| self.find_lru_slot());

        // Escribir bloque sucio si es necesario
        if self.dirty_flags[slot] {
            if let Some(block_num) = self.block_numbers[slot] {
                self.write_block_to_disk(block_num, &self.blocks[slot].unwrap())?;
            }
        }

        // Cargar nuevo bloque
        self.blocks[slot] = Some([0; BLOCK_SIZE]);
        self.block_numbers[slot] = Some(block_num);
        self.dirty_flags[slot] = false;
        self.access_times[slot] = get_current_time();

        // Leer del disco
        if let Some(device) = get_block_device() {
            device.read_block(block_num, self.blocks[slot].as_mut().unwrap())?;
        }

        Ok(self.blocks[slot].as_mut().unwrap())
    }

    /// Escribir bloque al disco
    fn write_block_to_disk(
        &self,
        block_num: u64,
        data: &[u8; BLOCK_SIZE],
    ) -> Result<(), &'static str> {
        if let Some(device) = get_block_device() {
            device.write_block(block_num, data)
        } else {
            Err("Block device not available")
        }
    }

    /// Encontrar slot libre
    fn find_free_slot(&self) -> Option<usize> {
        for i in 0..32 {
            if self.blocks[i].is_none() {
                return Some(i);
            }
        }
        None
    }

    /// Encontrar slot LRU
    fn find_lru_slot(&self) -> usize {
        let mut lru_slot = 0;
        let mut oldest_time = u64::MAX;

        for i in 0..32 {
            if self.access_times[i] < oldest_time {
                oldest_time = self.access_times[i];
                lru_slot = i;
            }
        }

        lru_slot
    }

    pub fn put_block(&mut self, block_num: u64) -> &mut [u8; BLOCK_SIZE] {
        // Buscar slot libre
        for i in 0..32 {
            if self.blocks[i].is_none() {
                self.blocks[i] = Some([0; BLOCK_SIZE]);
                self.block_numbers[i] = Some(block_num);
                self.dirty_flags[i] = false;
                self.access_times[i] = get_current_time();
                return self.blocks[i].as_mut().unwrap();
            }
        }

        // Si no hay slots libres, usar LRU
        let slot = self.find_lru_slot();

        // Escribir bloque sucio si es necesario
        if self.dirty_flags[slot] {
            if let Some(block_num) = self.block_numbers[slot] {
                let _ = self.write_block_to_disk(block_num, &self.blocks[slot].unwrap());
            }
        }

        self.blocks[slot] = Some([0; BLOCK_SIZE]);
        self.block_numbers[slot] = Some(block_num);
        self.dirty_flags[slot] = false;
        self.access_times[slot] = get_current_time();
        self.blocks[slot].as_mut().unwrap()
    }

    /// Marcar bloque como sucio
    pub fn mark_dirty(&mut self, block_num: u64) {
        for i in 0..32 {
            if self.block_numbers[i] == Some(block_num) {
                self.dirty_flags[i] = true;
                break;
            }
        }
    }

    /// Sincronizar todos los bloques sucios
    pub fn sync(&mut self) -> Result<(), &'static str> {
        for i in 0..32 {
            if self.dirty_flags[i] {
                if let Some(block_num) = self.block_numbers[i] {
                    self.write_block_to_disk(block_num, &self.blocks[i].unwrap())?;
                    self.dirty_flags[i] = false;
                }
            }
        }

        // Sincronizar dispositivo
        if let Some(device) = get_block_device() {
            device.sync()?;
        }

        Ok(())
    }

    /// Obtener estadísticas del cache
    pub fn get_stats(&self) -> (usize, usize, usize) {
        let total_blocks = self.blocks.iter().filter(|b| b.is_some()).count();
        let dirty_blocks = self.dirty_flags.iter().filter(|&&d| d).count();
        let free_blocks = self.blocks.iter().filter(|b| b.is_none()).count();
        (total_blocks, dirty_blocks, free_blocks)
    }
}

// Instancia global del dispositivo de bloques
static mut BLOCK_DEVICE: Option<BlockDevice> = None;
static mut BLOCK_CACHE: Option<BlockCache> = None;

pub fn init_block_device() -> Result<(), &'static str> {
    unsafe {
        BLOCK_DEVICE = Some(BlockDevice::new());
        if let Some(ref mut device) = BLOCK_DEVICE {
            device.init(1024); // 1024 bloques por defecto
        }

        BLOCK_CACHE = Some(BlockCache::new());
    }
    Ok(())
}

pub fn get_block_device() -> Option<&'static mut BlockDevice> {
    unsafe { BLOCK_DEVICE.as_mut() }
}

pub fn get_block_cache() -> Option<&'static mut BlockCache> {
    unsafe { BLOCK_CACHE.as_mut() }
}

/// Obtener tiempo actual (simplificado)
fn get_current_time() -> u64 {
    // Implementación simplificada - retorna timestamp fijo
    1640995200 // 2022-01-01 00:00:00 UTC
}
