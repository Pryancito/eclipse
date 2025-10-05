//! Sistema de caché inteligente para EclipseFS (inspirado en RedoxFS)

#[cfg(feature = "std")]
use std::{collections::HashMap, sync::RwLock};

#[cfg(not(feature = "std"))]
use {
    heapless::{FnvIndexMap, Vec as HeaplessVec},
    spin::RwLock,
};

use crate::EclipseFSResult;

/// Configuración de caché
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub max_entries: usize,
    pub max_memory_mb: usize,
    pub read_ahead_size: usize,
    pub write_behind_size: usize,
    pub prefetch_enabled: bool,
    pub compression_enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1024,
            max_memory_mb: 64,
            read_ahead_size: 4096,
            write_behind_size: 8192,
            prefetch_enabled: true,
            compression_enabled: false,
        }
    }
}

/// Entrada de caché con metadatos
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub key: u64,
    pub data: Vec<u8>,
    pub access_count: u32,
    pub last_access: u64,
    pub creation_time: u64,
    pub size: usize,
    pub is_dirty: bool,
    pub is_prefetched: bool,
    pub compression_ratio: f32,
}

impl CacheEntry {
    pub fn new(key: u64, data: Vec<u8>) -> Self {
        let now = Self::current_timestamp();
        Self {
            key,
            size: data.len(),
            data,
            access_count: 1,
            last_access: now,
            creation_time: now,
            is_dirty: false,
            is_prefetched: false,
            compression_ratio: 1.0,
        }
    }
    
    fn current_timestamp() -> u64 {
        // En un sistema real, esto vendría del kernel o RTC
        1640995200 // 2022-01-01 00:00:00 UTC
    }
    
    pub fn access(&mut self) {
        self.access_count += 1;
        self.last_access = Self::current_timestamp();
    }
    
    pub fn mark_dirty(&mut self) {
        self.is_dirty = true;
    }
    
    pub fn mark_clean(&mut self) {
        self.is_dirty = false;
    }
    
    pub fn calculate_score(&self) -> f64 {
        // Algoritmo de scoring basado en frecuencia y recencia (LFU + LRU)
        let frequency_score = (self.access_count as f64).ln() + 1.0;
        let recency_score = 1.0 / ((Self::current_timestamp() - self.last_access + 1) as f64);
        let size_penalty = 1.0 / ((self.size as f64).ln() + 1.0);
        
        frequency_score * recency_score * size_penalty
    }
}

/// Sistema de caché inteligente (inspirado en RedoxFS)
#[cfg(feature = "std")]
pub struct IntelligentCache {
    entries: RwLock<HashMap<u64, CacheEntry>>,
    config: CacheConfig,
    total_memory_used: RwLock<usize>,
    hit_count: RwLock<u64>,
    miss_count: RwLock<u64>,
}

#[cfg(not(feature = "std"))]
pub struct IntelligentCache {
    entries: RwLock<FnvIndexMap<u64, CacheEntry, 1024>>,
    config: CacheConfig,
    total_memory_used: RwLock<usize>,
    hit_count: RwLock<u64>,
    miss_count: RwLock<u64>,
}

impl IntelligentCache {
    pub fn new(config: CacheConfig) -> Self {
        Self {
            #[cfg(feature = "std")]
            entries: RwLock::new(HashMap::new()),
            #[cfg(not(feature = "std"))]
            entries: RwLock::new(FnvIndexMap::new()),
            config,
            total_memory_used: RwLock::new(0),
            hit_count: RwLock::new(0),
            miss_count: RwLock::new(0),
        }
    }
    
    /// Obtener entrada de caché
    pub fn get(&self, key: u64) -> Option<Vec<u8>> {
        let mut entries = self.entries.write().unwrap();
        
        if let Some(entry) = entries.get_mut(&key) {
            entry.access();
            *self.hit_count.write().unwrap() += 1;
            Some(entry.data.clone())
        } else {
            *self.miss_count.write().unwrap() += 1;
            None
        }
    }
    
    /// Insertar entrada en caché
    pub fn put(&self, key: u64, data: Vec<u8>) -> EclipseFSResult<()> {
        let mut entries = self.entries.write().unwrap();
        let mut total_memory = self.total_memory_used.write().unwrap();
        
        // Verificar límites de memoria
        if *total_memory + data.len() > self.config.max_memory_mb * 1024 * 1024 {
            #[cfg(feature = "std")]
            self.evict_entries(&mut *entries, data.len())?;
            #[cfg(not(feature = "std"))]
            self.evict_entries(&mut *entries, data.len())?;
        }
        
        // Verificar límite de entradas
        if entries.len() >= self.config.max_entries {
            #[cfg(feature = "std")]
            self.evict_entries(&mut *entries, 0)?;
            #[cfg(not(feature = "std"))]
            self.evict_entries(&mut *entries, 0)?;
        }
        
        let entry = CacheEntry::new(key, data);
        *total_memory += entry.size;
        
        #[cfg(feature = "std")]
        {
            entries.insert(key, entry);
        }
        
        #[cfg(not(feature = "std"))]
        {
            entries.insert(key, entry).map_err(|_| EclipseFSError::InvalidOperation)?;
        }
        
        Ok(())
    }
    
    /// Marcar entrada como modificada
    pub fn mark_dirty(&self, key: u64) -> EclipseFSResult<()> {
        let mut entries = self.entries.write().unwrap();
        
        if let Some(entry) = entries.get_mut(&key) {
            entry.mark_dirty();
        }
        
        Ok(())
    }
    
    /// Prefetch de datos (inspirado en RedoxFS)
    pub fn prefetch(&self, keys: &[u64]) -> EclipseFSResult<()> {
        if !self.config.prefetch_enabled {
            return Ok(());
        }
        
        // Simular prefetch - en un sistema real, esto cargaría datos del disco
        for &key in keys {
            let prefetch_data = vec![0u8; self.config.read_ahead_size];
            let mut entry = CacheEntry::new(key, prefetch_data);
            entry.is_prefetched = true;
            
            // Insertar en caché sin contar como hit
            let mut entries = self.entries.write().unwrap();
            #[cfg(feature = "std")]
            {
                entries.insert(key, entry);
            }
            #[cfg(not(feature = "std"))]
            {
                let _ = entries.insert(key, entry);
            }
        }
        
        Ok(())
    }
    
    /// Evictar entradas usando algoritmo inteligente
    #[cfg(feature = "std")]
    fn evict_entries(&self, entries: &mut HashMap<u64, CacheEntry>, needed_space: usize) -> EclipseFSResult<()> {
        let mut scores: Vec<(u64, f64)> = Vec::new();
        
        // Calcular scores para todas las entradas
        for (key, entry) in entries.iter() {
            if !entry.is_dirty {
                scores.push((*key, entry.calculate_score()));
            }
        }
        
        // Ordenar por score (menor score = más probable de ser evictado)
        scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        
        let mut freed_space = 0;
        let mut total_memory = self.total_memory_used.write().unwrap();
        
        // Evictar entradas hasta liberar suficiente espacio
        for (key, _) in scores {
            if let Some(entry) = entries.remove(&key) {
                freed_space += entry.size;
                *total_memory -= entry.size;
                
                if freed_space >= needed_space {
                    break;
                }
            }
        }
        
        Ok(())
    }
    
    /// Evictar entradas usando algoritmo inteligente (no_std)
    #[cfg(not(feature = "std"))]
    fn evict_entries(&self, entries: &mut FnvIndexMap<u64, CacheEntry, 1024>, needed_space: usize) -> EclipseFSResult<()> {
        let mut scores: Vec<(u64, f64)> = Vec::new();
        
        // Calcular scores para todas las entradas
        for (key, entry) in entries.iter() {
            if !entry.is_dirty {
                scores.push((*key, entry.calculate_score()));
            }
        }
        
        // Ordenar por score (menor score = más probable de ser evictado)
        scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        
        let mut freed_space = 0;
        let mut total_memory = self.total_memory_used.write().unwrap();
        
        // Evictar entradas hasta liberar suficiente espacio
        for (key, _) in scores {
            if let Some(entry) = entries.remove(&key) {
                freed_space += entry.size;
                *total_memory -= entry.size;
                
                if freed_space >= needed_space {
                    break;
                }
            }
        }
        
        Ok(())
    }
    
    /// Obtener estadísticas de caché
    pub fn get_stats(&self) -> CacheStats {
        let entries = self.entries.read().unwrap();
        let total_memory = *self.total_memory_used.read().unwrap();
        let hits = *self.hit_count.read().unwrap();
        let misses = *self.miss_count.read().unwrap();
        
        CacheStats {
            total_entries: entries.len(),
            total_memory_mb: total_memory / (1024 * 1024),
            hit_ratio: if hits + misses > 0 {
                hits as f64 / (hits + misses) as f64
            } else {
                0.0
            },
            dirty_entries: entries.values().filter(|e| e.is_dirty).count(),
            prefetched_entries: entries.values().filter(|e| e.is_prefetched).count(),
        }
    }
    
    /// Limpiar caché
    pub fn clear(&self) {
        let mut entries = self.entries.write().unwrap();
        entries.clear();
        *self.total_memory_used.write().unwrap() = 0;
        *self.hit_count.write().unwrap() = 0;
        *self.miss_count.write().unwrap() = 0;
    }
}


/// Estadísticas de caché
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub total_entries: usize,
    pub total_memory_mb: usize,
    pub hit_ratio: f64,
    pub dirty_entries: usize,
    pub prefetched_entries: usize,
}

impl CacheStats {
    pub fn print_summary(&self) {
        println!("Cache Stats:");
        println!("  Entries: {}", self.total_entries);
        println!("  Memory: {} MB", self.total_memory_mb);
        println!("  Hit Ratio: {:.2}%", self.hit_ratio * 100.0);
        println!("  Dirty Entries: {}", self.dirty_entries);
        println!("  Prefetched Entries: {}", self.prefetched_entries);
    }
}
