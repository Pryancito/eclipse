//! Módulo de Optimización de Rendimiento para Eclipse OS
//!
//! Este módulo proporciona optimizaciones avanzadas para el sistema multihilo:
//! - Load balancing inteligente
//! - Optimización de context switching
//! - Cache-aware scheduling
//! - Memory locality optimization
//! - Performance profiling

pub mod adaptive_scheduler;
pub mod cache_optimizer;
pub mod context_switch_optimizer;
pub mod load_balancer;
pub mod memory_locality;
pub mod performance_profiler;
pub mod thread_pool;

// Re-exportar las estructuras principales
pub use adaptive_scheduler::AdaptiveScheduler;
pub use cache_optimizer::CacheOptimizer;
pub use context_switch_optimizer::ContextSwitchOptimizer;
pub use load_balancer::LoadBalancer;
pub use memory_locality::MemoryLocalityOptimizer;
pub use performance_profiler::PerformanceProfiler;
pub use thread_pool::ThreadPool;

/// Configuración de optimización de rendimiento
#[derive(Debug, Clone)]
pub struct PerformanceConfig {
    pub enable_load_balancing: bool,
    pub enable_cache_optimization: bool,
    pub enable_memory_locality: bool,
    pub enable_adaptive_scheduling: bool,
    pub context_switch_threshold: u64,
    pub load_balance_interval: u64,
    pub cache_line_size: u64,
    pub numa_aware: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            enable_load_balancing: true,
            enable_cache_optimization: true,
            enable_memory_locality: true,
            enable_adaptive_scheduling: true,
            context_switch_threshold: 1000, // 1ms
            load_balance_interval: 10000,   // 10ms
            cache_line_size: 64,            // 64 bytes
            numa_aware: false,              // Por defecto no NUMA
        }
    }
}

/// Gestor principal de optimización de rendimiento
pub struct PerformanceManager {
    config: PerformanceConfig,
    load_balancer: LoadBalancer,
    context_optimizer: ContextSwitchOptimizer,
    cache_optimizer: CacheOptimizer,
    memory_optimizer: MemoryLocalityOptimizer,
    profiler: PerformanceProfiler,
    thread_pool: ThreadPool,
    adaptive_scheduler: AdaptiveScheduler,
}

impl PerformanceManager {
    /// Crear un nuevo gestor de rendimiento
    pub fn new(config: PerformanceConfig) -> Self {
        Self {
            load_balancer: LoadBalancer::new(),
            context_optimizer: ContextSwitchOptimizer::new(),
            cache_optimizer: CacheOptimizer::new(config.cache_line_size),
            memory_optimizer: MemoryLocalityOptimizer::new(config.numa_aware),
            profiler: PerformanceProfiler::new(),
            thread_pool: ThreadPool::new(),
            adaptive_scheduler: AdaptiveScheduler::new(),
            config,
        }
    }

    /// Inicializar el gestor de rendimiento
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.load_balancer.initialize()?;
        self.context_optimizer.initialize()?;
        self.cache_optimizer.initialize()?;
        self.memory_optimizer.initialize()?;
        self.profiler.initialize()?;
        self.thread_pool.initialize()?;
        self.adaptive_scheduler.initialize()?;
        Ok(())
    }

    /// Optimizar el rendimiento del sistema
    pub fn optimize_performance(&mut self) -> Result<(), &'static str> {
        if self.config.enable_load_balancing {
            self.load_balancer.balance_load()?;
        }

        if self.config.enable_cache_optimization {
            self.cache_optimizer.optimize_cache_usage()?;
        }

        if self.config.enable_memory_locality {
            self.memory_optimizer.optimize_memory_locality()?;
        }

        if self.config.enable_adaptive_scheduling {
            self.adaptive_scheduler.adapt_scheduling()?;
        }

        Ok(())
    }

    /// Obtener métricas de rendimiento
    pub fn get_performance_metrics(&self) -> PerformanceMetrics {
        PerformanceMetrics {
            load_balance_score: self.load_balancer.get_balance_score(),
            context_switch_efficiency: self.context_optimizer.get_efficiency(),
            cache_hit_rate: self.cache_optimizer.get_hit_rate(),
            memory_locality_score: self.memory_optimizer.get_locality_score(),
            thread_utilization: self.thread_pool.get_utilization(),
            adaptive_scheduling_score: self.adaptive_scheduler.get_score(),
            total_optimizations: self.profiler.get_total_optimizations(),
        }
    }

    /// Actualizar configuración
    pub fn update_config(&mut self, config: PerformanceConfig) {
        let cache_line_size = config.cache_line_size;
        let numa_aware = config.numa_aware;

        self.config = config;
        self.cache_optimizer.update_cache_line_size(cache_line_size);
        self.memory_optimizer.update_numa_awareness(numa_aware);
    }
}

/// Métricas de rendimiento del sistema
#[derive(Debug, Clone)]
pub struct PerformanceMetrics {
    pub load_balance_score: f64,
    pub context_switch_efficiency: f64,
    pub cache_hit_rate: f64,
    pub memory_locality_score: f64,
    pub thread_utilization: f64,
    pub adaptive_scheduling_score: f64,
    pub total_optimizations: u64,
}

/// Inicializar el sistema de optimización de rendimiento
pub fn init_performance_system() -> Result<PerformanceManager, &'static str> {
    let config = PerformanceConfig::default();
    let mut manager = PerformanceManager::new(config);
    manager.initialize()?;
    Ok(manager)
}
