//! Algoritmo ARC (Adaptive Replacement Cache) - "Arquera"
//! Implementación inspirada en ZFS para caché adaptativa de alto rendimiento
//!
//! El algoritmo ARC mantiene dos listas principales:
//! - T1: Entradas recientes (LRU)
//! - T2: Entradas frecuentes (LRU de entradas que han sido accedidas múltiples veces)
//!
//! Y dos listas "fantasma" que rastrean entradas evictadas:
//! - B1: Entradas evictadas de T1
//! - B2: Entradas evictadas de T2
//!
//! El algoritmo se adapta dinámicamente ajustando el tamaño de T1 vs T2 basado en el patrón de acceso

use std::collections::{HashMap, VecDeque};
use crate::EclipseFSNode;

/// Tamaño máximo total del cache ARC (debe ser par)
const ARC_CACHE_SIZE: usize = 1024;

/// Implementación del cache ARC (Adaptive Replacement Cache)
/// Este es el algoritmo "arquera" que optimiza dinámicamente entre
/// datos recientes y datos frecuentes
pub struct AdaptiveReplacementCache {
    /// T1: Lista de páginas recientes (accedidas una vez)
    t1: VecDeque<u32>,
    
    /// T2: Lista de páginas frecuentes (accedidas múltiples veces)
    t2: VecDeque<u32>,
    
    /// B1: Lista fantasma de T1 (rastreo de evictados recientes)
    b1: VecDeque<u32>,
    
    /// B2: Lista fantasma de T2 (rastreo de evictados frecuentes)
    b2: VecDeque<u32>,
    
    /// Almacenamiento real de datos
    cache_data: HashMap<u32, EclipseFSNode>,
    
    /// Contador de accesos para determinar T1 vs T2
    access_count: HashMap<u32, u32>,
    
    /// Parámetro adaptativo p: tamaño objetivo de T1
    /// El algoritmo ajusta p dinámicamente entre 0 y c (tamaño del cache)
    p: usize,
    
    /// Estadísticas
    hits: u64,
    misses: u64,
    t1_to_t2_promotions: u64,
    adaptations: u64,
}

impl AdaptiveReplacementCache {
    /// Crear nuevo cache ARC
    pub fn new() -> Self {
        Self {
            t1: VecDeque::new(),
            t2: VecDeque::new(),
            b1: VecDeque::new(),
            b2: VecDeque::new(),
            cache_data: HashMap::new(),
            access_count: HashMap::new(),
            p: ARC_CACHE_SIZE / 2, // Comenzar equilibrado
            hits: 0,
            misses: 0,
            t1_to_t2_promotions: 0,
            adaptations: 0,
        }
    }
    
    /// Verificar si un inode está en cache (sin modificar estado)
    pub fn contains(&self, inode: u32) -> bool {
        self.cache_data.contains_key(&inode)
    }
    
    /// Obtener un nodo del cache
    pub fn get(&mut self, inode: u32) -> Option<EclipseFSNode> {
        // Verificar si está en T1
        if let Some(pos) = self.t1.iter().position(|&x| x == inode) {
            self.hits += 1;
            
            // Mover de T1 a T2 (promover a frecuente)
            self.t1.remove(pos);
            self.t2.push_back(inode);
            
            // Incrementar contador de acceso
            *self.access_count.entry(inode).or_insert(0) += 1;
            self.t1_to_t2_promotions += 1;
            
            return self.cache_data.get(&inode).cloned();
        }
        
        // Verificar si está en T2
        if let Some(pos) = self.t2.iter().position(|&x| x == inode) {
            self.hits += 1;
            
            // Mover al final de T2 (MRU)
            self.t2.remove(pos);
            self.t2.push_back(inode);
            
            // Incrementar contador de acceso
            *self.access_count.entry(inode).or_insert(0) += 1;
            
            return self.cache_data.get(&inode).cloned();
        }
        
        self.misses += 1;
        None
    }
    
    /// Insertar un nodo en el cache
    pub fn put(&mut self, inode: u32, node: EclipseFSNode) {
        // Caso 1: Ya está en cache (actualizar)
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.cache_data.entry(inode) {
            e.insert(node);
            return;
        }
        
        // Caso 2: Está en B1 (hit fantasma - patrón reciente)
        if let Some(pos) = self.b1.iter().position(|&x| x == inode) {
            // Adaptar: aumentar p (favorecer recientes)
            self.adapt_on_b1_hit();
            self.b1.remove(pos);
            
            // Reemplazar y añadir a T2 (es frecuente ahora)
            self.replace(inode, true);
            self.t2.push_back(inode);
            self.cache_data.insert(inode, node);
            *self.access_count.entry(inode).or_insert(0) = 2;
            return;
        }
        
        // Caso 3: Está en B2 (hit fantasma - patrón frecuente)
        if let Some(pos) = self.b2.iter().position(|&x| x == inode) {
            // Adaptar: disminuir p (favorecer frecuentes)
            self.adapt_on_b2_hit();
            self.b2.remove(pos);
            
            // Reemplazar y añadir a T2
            self.replace(inode, true);
            self.t2.push_back(inode);
            self.cache_data.insert(inode, node);
            *self.access_count.entry(inode).or_insert(0) = 2;
            return;
        }
        
        // Caso 4: Cache miss completo - añadir a T1
        let total_cache = self.t1.len() + self.t2.len();
        let total_lists = total_cache + self.b1.len() + self.b2.len();
        
        if total_cache < ARC_CACHE_SIZE {
            // Hay espacio - añadir directamente
            if total_lists >= ARC_CACHE_SIZE {
                // Listas fantasma llenas - eliminar de B1
                if !self.b1.is_empty() {
                    self.b1.pop_front();
                }
            }
            self.t1.push_back(inode);
        } else {
            // Cache lleno - reemplazar
            self.replace(inode, false);
            self.t1.push_back(inode);
        }
        
        self.cache_data.insert(inode, node);
        *self.access_count.entry(inode).or_insert(0) = 1;
    }
    
    /// Algoritmo de reemplazo adaptativo
    fn replace(&mut self, _new_inode: u32, in_b2: bool) {
        loop {
            let t1_len = self.t1.len();
            let t2_len = self.t2.len();
            
            // Decidir de dónde evictar
            let evict_from_t1 = if t1_len > 0 && (t1_len > self.p || (in_b2 && t1_len == self.p)) {
                true
            } else if t2_len > 0 {
                false
            } else if t1_len > 0 {
                true
            } else {
                break;
            };
            
            if evict_from_t1 {
                // Evictar de T1 (LRU)
                if let Some(victim) = self.t1.pop_front() {
                    self.cache_data.remove(&victim);
                    self.b1.push_back(victim);
                    
                    // Limitar tamaño de B1
                    while self.b1.len() > ARC_CACHE_SIZE {
                        self.b1.pop_front();
                    }
                    break;
                }
            } else {
                // Evictar de T2 (LRU)
                if let Some(victim) = self.t2.pop_front() {
                    self.cache_data.remove(&victim);
                    self.b2.push_back(victim);
                    
                    // Limitar tamaño de B2
                    while self.b2.len() > ARC_CACHE_SIZE {
                        self.b2.pop_front();
                    }
                    break;
                }
            }
        }
    }
    
    /// Adaptar parámetro p cuando hay hit en B1
    fn adapt_on_b1_hit(&mut self) {
        let delta = if self.b2.len() >= self.b1.len() {
            1
        } else {
            self.b1.len() / self.b2.len().max(1)
        };
        
        self.p = (self.p + delta).min(ARC_CACHE_SIZE);
        self.adaptations += 1;
    }
    
    /// Adaptar parámetro p cuando hay hit en B2
    fn adapt_on_b2_hit(&mut self) {
        let delta = if self.b1.len() >= self.b2.len() {
            1
        } else {
            self.b2.len() / self.b1.len().max(1)
        };
        
        self.p = self.p.saturating_sub(delta);
        self.adaptations += 1;
    }
    
    /// Obtener estadísticas del cache
    pub fn stats(&self) -> ARCStats {
        ARCStats {
            t1_size: self.t1.len(),
            t2_size: self.t2.len(),
            b1_size: self.b1.len(),
            b2_size: self.b2.len(),
            p: self.p,
            hits: self.hits,
            misses: self.misses,
            hit_rate: if self.hits + self.misses > 0 {
                self.hits as f64 / (self.hits + self.misses) as f64
            } else {
                0.0
            },
            t1_to_t2_promotions: self.t1_to_t2_promotions,
            adaptations: self.adaptations,
            total_capacity: ARC_CACHE_SIZE,
        }
    }
    
    /// Limpiar el cache
    pub fn clear(&mut self) {
        self.t1.clear();
        self.t2.clear();
        self.b1.clear();
        self.b2.clear();
        self.cache_data.clear();
        self.access_count.clear();
        self.p = ARC_CACHE_SIZE / 2;
        self.hits = 0;
        self.misses = 0;
        self.t1_to_t2_promotions = 0;
        self.adaptations = 0;
    }
}

impl Default for AdaptiveReplacementCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Estadísticas del cache ARC
#[derive(Debug, Clone)]
pub struct ARCStats {
    pub t1_size: usize,      // Tamaño actual de T1 (recientes)
    pub t2_size: usize,      // Tamaño actual de T2 (frecuentes)
    pub b1_size: usize,      // Tamaño de lista fantasma B1
    pub b2_size: usize,      // Tamaño de lista fantasma B2
    pub p: usize,            // Parámetro adaptativo (target T1 size)
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub t1_to_t2_promotions: u64,
    pub adaptations: u64,
    pub total_capacity: usize,
}

impl ARCStats {
    pub fn print(&self) {
        println!("=== ARC Cache Statistics (Algoritmo Arquera) ===");
        println!("Cache Lists:");
        println!("  T1 (Recent):    {} entries", self.t1_size);
        println!("  T2 (Frequent):  {} entries", self.t2_size);
        println!("  B1 (Ghost):     {} entries", self.b1_size);
        println!("  B2 (Ghost):     {} entries", self.b2_size);
        println!("  Total Cached:   {}/{}", self.t1_size + self.t2_size, self.total_capacity);
        println!("\nAdaptive Parameter:");
        println!("  p (T1 target):  {} ({}% recent preference)", 
                 self.p, (self.p as f64 / self.total_capacity as f64 * 100.0) as u32);
        println!("\nPerformance:");
        println!("  Hits:           {}", self.hits);
        println!("  Misses:         {}", self.misses);
        println!("  Hit Rate:       {:.2}%", self.hit_rate * 100.0);
        println!("  T1→T2 Promo:    {}", self.t1_to_t2_promotions);
        println!("  Adaptations:    {}", self.adaptations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EclipseFSNode;
    
    #[test]
    fn test_arc_basic_operations() {
        let mut arc = AdaptiveReplacementCache::new();
        let node = EclipseFSNode::new_file();
        
        // Primera inserción - debe ir a T1
        arc.put(1, node.clone());
        let stats = arc.stats();
        assert_eq!(stats.t1_size, 1);
        assert_eq!(stats.t2_size, 0);
        
        // Primera lectura - debe promover a T2
        let result = arc.get(1);
        assert!(result.is_some());
        let stats = arc.stats();
        assert_eq!(stats.t1_size, 0);
        assert_eq!(stats.t2_size, 1);
        assert_eq!(stats.t1_to_t2_promotions, 1);
    }
    
    #[test]
    fn test_arc_adaptation() {
        let mut arc = AdaptiveReplacementCache::new();
        let node = EclipseFSNode::new_file();
        
        // Llenar cache
        for i in 0..ARC_CACHE_SIZE {
            arc.put(i as u32, node.clone());
        }
        
        // Acceder algunas varias veces (promover a T2)
        for i in 0..10 {
            arc.get(i);
        }
        
        let stats = arc.stats();
        assert_eq!(stats.t1_to_t2_promotions, 10);
        assert!(stats.t2_size > 0);
    }
    
    #[test]
    fn test_arc_ghost_lists() {
        let mut arc = AdaptiveReplacementCache::new();
        let node = EclipseFSNode::new_file();
        
        // Llenar y sobrellenar cache
        for i in 0..(ARC_CACHE_SIZE + 100) {
            arc.put(i as u32, node.clone());
        }
        
        let stats = arc.stats();
        // Debe haber entradas en listas fantasma
        assert!(stats.b1_size > 0 || stats.b2_size > 0);
    }
}
