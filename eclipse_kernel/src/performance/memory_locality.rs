//! Optimizador de Localidad de Memoria para Eclipse OS
//!
//! Este módulo optimiza la localidad de memoria para mejorar
//! el rendimiento del sistema multihilo

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Configuración de optimización de localidad de memoria
#[derive(Debug, Clone)]
pub struct MemoryLocalityConfig {
    pub enable_numa_awareness: bool,
    pub enable_memory_compaction: bool,
    pub enable_page_migration: bool,
    pub enable_memory_interleaving: bool,
    pub numa_nodes: usize,
    pub page_size: u64,
    pub migration_threshold: f64,
}

impl Default for MemoryLocalityConfig {
    fn default() -> Self {
        Self {
            enable_numa_awareness: false,
            enable_memory_compaction: true,
            enable_page_migration: true,
            enable_memory_interleaving: false,
            numa_nodes: 1,
            page_size: 4096,          // 4KB
            migration_threshold: 0.8, // 80% threshold
        }
    }
}

/// Información de una página de memoria
#[derive(Debug, Clone)]
pub struct MemoryPage {
    pub address: u64,
    pub size: u64,
    pub numa_node: usize,
    pub access_count: u64,
    pub last_access: u64,
    pub is_hot: bool,
    pub is_migrated: bool,
    pub thread_affinity: Option<u32>,
}

/// Métricas de localidad de memoria
#[derive(Debug, Clone)]
pub struct MemoryLocalityMetrics {
    pub total_pages: u64,
    pub hot_pages: u64,
    pub cold_pages: u64,
    pub migrated_pages: u64,
    pub locality_score: f64,
    pub numa_balance_score: f64,
    pub memory_fragmentation: f64,
    pub average_access_distance: f64,
}

/// Optimizador de localidad de memoria
pub struct MemoryLocalityOptimizer {
    config: MemoryLocalityConfig,
    metrics: MemoryLocalityMetrics,
    pages: Vec<MemoryPage>,
    access_patterns: Vec<(u64, u64)>, // (address, timestamp)
    numa_usage: Vec<u64>,             // Uso por nodo NUMA
    migration_count: AtomicUsize,
    optimization_cycles: AtomicU64,
}

impl MemoryLocalityOptimizer {
    /// Crear un nuevo optimizador
    pub fn new(numa_aware: bool) -> Self {
        let config = MemoryLocalityConfig {
            enable_numa_awareness: numa_aware,
            numa_nodes: if numa_aware { 4 } else { 1 },
            ..Default::default()
        };

        Self {
            config,
            metrics: MemoryLocalityMetrics {
                total_pages: 0,
                hot_pages: 0,
                cold_pages: 0,
                migrated_pages: 0,
                locality_score: 0.0,
                numa_balance_score: 0.0,
                memory_fragmentation: 0.0,
                average_access_distance: 0.0,
            },
            pages: Vec::new(),
            access_patterns: Vec::new(),
            numa_usage: vec![0; if numa_aware { 4 } else { 1 }],
            migration_count: AtomicUsize::new(0),
            optimization_cycles: AtomicU64::new(0),
        }
    }

    /// Inicializar el optimizador
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Limpiar datos
        self.pages.clear();
        self.access_patterns.clear();
        self.numa_usage.fill(0);

        // Inicializar métricas
        self.metrics = MemoryLocalityMetrics {
            total_pages: 0,
            hot_pages: 0,
            cold_pages: 0,
            migrated_pages: 0,
            locality_score: 0.0,
            numa_balance_score: 0.0,
            memory_fragmentation: 0.0,
            average_access_distance: 0.0,
        };

        Ok(())
    }

    /// Registrar acceso a memoria
    pub fn record_memory_access(&mut self, address: u64, thread_id: u32) {
        let current_time = self.get_current_time();

        // Registrar patrón de acceso
        self.access_patterns.push((address, current_time));

        // Mantener solo los últimos 1000 accesos
        if self.access_patterns.len() > 1000 {
            self.access_patterns.remove(0);
        }

        // Buscar o crear página
        let page_address = self.get_page_address(address);
        if let Some(page) = self.pages.iter_mut().find(|p| p.address == page_address) {
            page.access_count += 1;
            page.last_access = current_time;
            page.thread_affinity = Some(thread_id);
        } else {
            // Crear nueva página
            let new_page = MemoryPage {
                address: page_address,
                size: self.config.page_size,
                numa_node: self.get_numa_node(address),
                access_count: 1,
                last_access: current_time,
                is_hot: false,
                is_migrated: false,
                thread_affinity: Some(thread_id),
            };
            self.pages.push(new_page);
            self.metrics.total_pages += 1;
        }

        // Actualizar uso de NUMA
        if self.config.enable_numa_awareness {
            let numa_node = self.get_numa_node(address);
            if numa_node < self.numa_usage.len() {
                self.numa_usage[numa_node] += 1;
            }
        }
    }

    /// Optimizar localidad de memoria
    pub fn optimize_memory_locality(&mut self) -> Result<(), &'static str> {
        self.optimization_cycles.fetch_add(1, Ordering::Relaxed);

        // Clasificar páginas como hot/cold
        self.classify_pages();

        if self.config.enable_page_migration {
            self.migrate_pages();
        }

        if self.config.enable_memory_compaction {
            self.compact_memory();
        }

        if self.config.enable_memory_interleaving {
            self.interleave_memory();
        }

        // Actualizar métricas
        self.update_metrics();

        Ok(())
    }

    /// Clasificar páginas como hot/cold
    fn classify_pages(&mut self) {
        let current_time = self.get_current_time();
        let hot_threshold = 10; // Accesos mínimos para ser hot
        let time_threshold = 1000000; // 1 segundo en nanosegundos

        for page in &mut self.pages {
            let is_recent = current_time - page.last_access < time_threshold;
            let is_frequently_accessed = page.access_count >= hot_threshold;

            page.is_hot = is_recent && is_frequently_accessed;
        }

        // Actualizar contadores
        self.metrics.hot_pages = self.pages.iter().filter(|p| p.is_hot).count() as u64;
        self.metrics.cold_pages = self.pages.iter().filter(|p| !p.is_hot).count() as u64;
    }

    /// Migrar páginas para mejorar localidad
    fn migrate_pages(&mut self) {
        if !self.config.enable_numa_awareness {
            return;
        }

        let current_time = self.get_current_time();
        let migration_threshold = self.config.migration_threshold;

        let mut pages_to_migrate = Vec::new();

        for (index, page) in self.pages.iter().enumerate() {
            if page.is_migrated || !page.is_hot {
                continue;
            }

            // Calcular score de migración
            let migration_score = self.calculate_migration_score(page);

            if migration_score > migration_threshold {
                pages_to_migrate.push(index);
            }
        }

        for index in pages_to_migrate {
            if let Some(page) = self.pages.get_mut(index) {
                // Simular migración de página
                let numa_nodes = self.config.numa_nodes;
                page.numa_node = (page.numa_node + 1) % numa_nodes;
                page.is_migrated = true;
                self.migration_count.fetch_add(1, Ordering::Relaxed);
                self.metrics.migrated_pages += 1;
            }
        }
    }

    /// Calcular score de migración
    fn calculate_migration_score(&self, page: &MemoryPage) -> f64 {
        if !self.config.enable_numa_awareness {
            return 0.0;
        }

        // Score basado en frecuencia de acceso y localidad
        let access_score = (page.access_count as f64 / 100.0).min(1.0);
        let locality_score = self.calculate_page_locality(page);

        (access_score + locality_score) / 2.0
    }

    /// Calcular localidad de una página
    fn calculate_page_locality(&self, page: &MemoryPage) -> f64 {
        // Buscar accesos cercanos en el patrón de acceso
        let page_start = page.address;
        let page_end = page.address + page.size;

        let nearby_accesses = self
            .access_patterns
            .iter()
            .filter(|(addr, _)| *addr >= page_start && *addr < page_end)
            .count();

        // Normalizar score
        (nearby_accesses as f64 / 10.0).min(1.0)
    }

    /// Simular migración de página
    fn simulate_page_migration(&self, page: &mut MemoryPage) {
        // En un sistema real, esto movería la página a un nodo NUMA diferente
        // Por ahora, simulamos cambiando el nodo NUMA
        let numa_nodes = self.config.numa_nodes;
        page.numa_node = (page.numa_node + 1) % numa_nodes;
    }

    /// Compactar memoria
    fn compact_memory(&mut self) {
        // Simulación de compactación de memoria
        // En un sistema real, esto movería páginas para reducir fragmentación
        self.metrics.memory_fragmentation = self.calculate_fragmentation();
    }

    /// Intercalar memoria
    fn interleave_memory(&mut self) {
        if !self.config.enable_numa_awareness {
            return;
        }

        // Simulación de intercalado de memoria
        // En un sistema real, esto distribuiría páginas entre nodos NUMA
        self.balance_numa_usage();
    }

    /// Calcular fragmentación de memoria
    fn calculate_fragmentation(&self) -> f64 {
        if self.pages.is_empty() {
            return 0.0;
        }

        // Simulación de cálculo de fragmentación
        // En un sistema real, esto calcularía la fragmentación real
        let total_pages = self.pages.len() as f64;
        let fragmented_pages = self.pages.iter().filter(|p| p.is_migrated).count() as f64;

        (fragmented_pages / total_pages) * 100.0
    }

    /// Balancear uso de NUMA
    fn balance_numa_usage(&mut self) {
        if !self.config.enable_numa_awareness || self.numa_usage.len() <= 1 {
            return;
        }

        let total_usage: u64 = self.numa_usage.iter().sum();
        let average_usage = total_usage / self.numa_usage.len() as u64;

        // Calcular score de balance
        let variance = self
            .numa_usage
            .iter()
            .map(|&usage| {
                (usage as f64 - average_usage as f64) * (usage as f64 - average_usage as f64)
            })
            .sum::<f64>()
            / self.numa_usage.len() as f64;

        self.metrics.numa_balance_score = (100.0 - (variance / 100.0).min(100.0)).max(0.0);
    }

    /// Actualizar métricas
    fn update_metrics(&mut self) {
        // Actualizar score de localidad
        self.metrics.locality_score = self.calculate_locality_score();

        // Actualizar distancia promedio de acceso
        self.metrics.average_access_distance = self.calculate_average_access_distance();
    }

    /// Calcular score de localidad
    fn calculate_locality_score(&self) -> f64 {
        if self.pages.is_empty() {
            return 0.0;
        }

        let hot_ratio = self.metrics.hot_pages as f64 / self.metrics.total_pages as f64;
        let migration_ratio = self.metrics.migrated_pages as f64 / self.metrics.total_pages as f64;

        // Score combinado de localidad
        (hot_ratio * 0.7 + migration_ratio * 0.3) * 100.0
    }

    /// Calcular distancia promedio de acceso
    fn calculate_average_access_distance(&self) -> f64 {
        if self.access_patterns.len() < 2 {
            return 0.0;
        }

        let mut total_distance = 0.0;
        let mut count = 0;

        for i in 1..self.access_patterns.len() {
            let distance =
                (self.access_patterns[i].0 as f64 - self.access_patterns[i - 1].0 as f64).abs();
            total_distance += distance;
            count += 1;
        }

        if count > 0 {
            total_distance / count as f64
        } else {
            0.0
        }
    }

    /// Obtener dirección de página
    fn get_page_address(&self, address: u64) -> u64 {
        address & !(self.config.page_size - 1)
    }

    /// Obtener nodo NUMA para una dirección
    fn get_numa_node(&self, address: u64) -> usize {
        if !self.config.enable_numa_awareness {
            return 0;
        }

        // Simulación de distribución NUMA
        (address as usize / (1024 * 1024)) % self.config.numa_nodes
    }

    /// Obtener score de localidad
    pub fn get_locality_score(&self) -> f64 {
        self.metrics.locality_score
    }

    /// Obtener métricas de localidad
    pub fn get_metrics(&self) -> &MemoryLocalityMetrics {
        &self.metrics
    }

    /// Actualizar conciencia NUMA
    pub fn update_numa_awareness(&mut self, numa_aware: bool) {
        self.config.enable_numa_awareness = numa_aware;
        self.config.numa_nodes = if numa_aware { 4 } else { 1 };
        self.numa_usage.resize(self.config.numa_nodes, 0);
    }

    /// Obtener tiempo actual (simulado)
    fn get_current_time(&self) -> u64 {
        static mut COUNTER: u64 = 0;
        unsafe {
            COUNTER += 1;
            COUNTER
        }
    }

    /// Obtener estadísticas detalladas
    pub fn get_detailed_stats(&self) -> MemoryLocalityStats {
        MemoryLocalityStats {
            total_pages: self.metrics.total_pages,
            hot_pages: self.metrics.hot_pages,
            cold_pages: self.metrics.cold_pages,
            migrated_pages: self.metrics.migrated_pages,
            locality_score: self.metrics.locality_score,
            numa_balance_score: self.metrics.numa_balance_score,
            memory_fragmentation: self.metrics.memory_fragmentation,
            average_access_distance: self.metrics.average_access_distance,
            migration_count: self.migration_count.load(Ordering::Relaxed),
            optimization_cycles: self.optimization_cycles.load(Ordering::Relaxed),
            numa_usage: self.numa_usage.clone(),
        }
    }
}

/// Estadísticas detalladas de localidad de memoria
#[derive(Debug, Clone)]
pub struct MemoryLocalityStats {
    pub total_pages: u64,
    pub hot_pages: u64,
    pub cold_pages: u64,
    pub migrated_pages: u64,
    pub locality_score: f64,
    pub numa_balance_score: f64,
    pub memory_fragmentation: f64,
    pub average_access_distance: f64,
    pub migration_count: usize,
    pub optimization_cycles: u64,
    pub numa_usage: Vec<u64>,
}
