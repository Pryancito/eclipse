//! Gestor principal de memoria para Eclipse OS
//! 
//! Este módulo implementa:
//! - Coordinación de todos los subsistemas de memoria
//! - Gestión de memoria a nivel del sistema
//! - Optimización de memoria
//! - Monitoreo y estadísticas
//! - Limpieza automática

use crate::debug::serial_write_str;
use alloc::format;
use alloc::vec::Vec;
use alloc::boxed::Box;
use crate::memory::{
    paging, heap, dma, shared_memory, virtual_memory,
    MemoryConfig, MemoryState, DmaStats
};

/// Configuración del gestor de memoria
pub struct MemoryManagerConfig {
    /// Habilitar limpieza automática
    pub enable_auto_cleanup: bool,
    /// Intervalo de limpieza en milisegundos
    pub cleanup_interval: u64,
    /// Habilitar monitoreo de memoria
    pub enable_monitoring: bool,
    /// Umbral de fragmentación para limpieza
    pub fragmentation_threshold: f32,
    /// Límite de memoria para alertas
    pub memory_limit_threshold: f32,
}

impl Default for MemoryManagerConfig {
    fn default() -> Self {
        Self {
            enable_auto_cleanup: true,
            cleanup_interval: 5000, // 5 segundos
            enable_monitoring: true,
            fragmentation_threshold: 0.7, // 70%
            memory_limit_threshold: 0.9,  // 90%
        }
    }
}

/// Gestor principal de memoria
pub struct MemoryManager {
    /// Configuración del gestor
    config: MemoryManagerConfig,
    /// Estado actual del sistema de memoria
    current_state: MemoryState,
    /// Timestamp de última limpieza
    last_cleanup: u64,
    /// Estadísticas del gestor
    manager_stats: MemoryManagerStats,
    /// Si el gestor está inicializado
    is_initialized: bool,
}

/// Estadísticas del gestor de memoria
#[derive(Debug, Clone, Copy)]
pub struct MemoryManagerStats {
    /// Número de limpiezas automáticas realizadas
    pub auto_cleanups_performed: u64,
    /// Número de alertas de memoria generadas
    pub memory_alerts_generated: u64,
    /// Número de optimizaciones realizadas
    pub optimizations_performed: u64,
    /// Tiempo total gastado en limpieza
    pub total_cleanup_time: u64,
    /// Tiempo total gastado en optimización
    pub total_optimization_time: u64,
}

impl Default for MemoryManagerStats {
    fn default() -> Self {
        Self {
            auto_cleanups_performed: 0,
            memory_alerts_generated: 0,
            optimizations_performed: 0,
            total_cleanup_time: 0,
            total_optimization_time: 0,
        }
    }
}

impl MemoryManager {
    /// Crear un nuevo gestor de memoria
    pub fn new(config: MemoryManagerConfig) -> Self {
        Self {
            config,
            current_state: MemoryState {
                total_physical: 0,
                used_physical: 0,
                total_virtual: 0,
                used_virtual: 0,
                allocated_pages: 0,
                free_pages: 0,
                heap_fragmentation: 0.0,
                dma_stats: DmaStats::default(),
            },
            last_cleanup: 0,
            manager_stats: MemoryManagerStats::default(),
            is_initialized: false,
        }
    }
    
    /// Inicializar el gestor de memoria
    pub fn initialize(&mut self, memory_config: MemoryConfig) -> Result<(), &'static str> {
        serial_write_str("MEMORY_MANAGER: Inicializando gestor principal de memoria...\n");
        
        // Inicializar todos los subsistemas de memoria
        crate::memory::init_memory_system(memory_config)?;
        
        // Actualizar el estado inicial
        self.update_state();
        
        self.is_initialized = true;
        self.last_cleanup = get_timestamp();
        
        serial_write_str("MEMORY_MANAGER: Gestor principal de memoria inicializado\n");
        Ok(())
    }
    
    /// Actualizar el estado actual del sistema de memoria
    pub fn update_state(&mut self) {
        self.current_state = crate::memory::get_memory_state();
    }
    
    /// Realizar limpieza automática del sistema de memoria
    pub fn perform_cleanup(&mut self) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Gestor de memoria no inicializado");
        }
        
        let cleanup_start = get_timestamp();
        
        serial_write_str("MEMORY_MANAGER: Realizando limpieza automática...\n");
        
        // Limpiar buffers DMA inactivos
        dma::dma_cleanup();
        
        // Limpiar regiones de memoria compartida inactivas
        shared_memory::shared_memory_cleanup();
        
        // Verificar integridad del heap
        if !heap::verify_heap_integrity() {
            serial_write_str("MEMORY_MANAGER: Advertencia: Integridad del heap comprometida\n");
        }
        
        // Verificar integridad del sistema DMA
        if !dma::dma_verify_integrity() {
            serial_write_str("MEMORY_MANAGER: Advertencia: Integridad del sistema DMA comprometida\n");
        }
        
        // Verificar integridad de la memoria compartida
        if !shared_memory::shared_memory_verify_integrity() {
            serial_write_str("MEMORY_MANAGER: Advertencia: Integridad de la memoria compartida comprometida\n");
        }
        
        // Verificar integridad de la memoria virtual
        if !virtual_memory::virtual_memory_verify_integrity() {
            serial_write_str("MEMORY_MANAGER: Advertencia: Integridad de la memoria virtual comprometida\n");
        }
        
        let cleanup_end = get_timestamp();
        let cleanup_time = cleanup_end - cleanup_start;
        
        self.manager_stats.auto_cleanups_performed += 1;
        self.manager_stats.total_cleanup_time += cleanup_time;
        self.last_cleanup = cleanup_end;
        
        serial_write_str(&format!("MEMORY_MANAGER: Limpieza completada en {} ciclos\n", cleanup_time));
        Ok(())
    }
    
    /// Realizar optimización del sistema de memoria
    pub fn perform_optimization(&mut self) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Gestor de memoria no inicializado");
        }
        
        let optimization_start = get_timestamp();
        
        serial_write_str("MEMORY_MANAGER: Realizando optimización de memoria...\n");
        
        // Actualizar el estado antes de optimizar
        self.update_state();
        
        // Optimizar si la fragmentación es alta
        if self.current_state.heap_fragmentation > self.config.fragmentation_threshold {
            serial_write_str("MEMORY_MANAGER: Fragmentación alta detectada, optimizando...\n");
            
            // Aquí se implementarían algoritmos de desfragmentación
            // Por ahora, solo registramos la optimización
            self.manager_stats.optimizations_performed += 1;
        }
        
        // Verificar límites de memoria
        let memory_usage_ratio = self.current_state.used_physical as f32 / self.current_state.total_physical as f32;
        if memory_usage_ratio > self.config.memory_limit_threshold {
            serial_write_str("MEMORY_MANAGER: Alerta: Uso de memoria alto\n");
            self.manager_stats.memory_alerts_generated += 1;
        }
        
        let optimization_end = get_timestamp();
        let optimization_time = optimization_end - optimization_start;
        
        self.manager_stats.total_optimization_time += optimization_time;
        
        serial_write_str(&format!("MEMORY_MANAGER: Optimización completada en {} ciclos\n", optimization_time));
        Ok(())
    }
    
    /// Verificar si es necesario realizar limpieza
    pub fn should_perform_cleanup(&self) -> bool {
        if !self.config.enable_auto_cleanup {
            return false;
        }
        
        let current_time = get_timestamp();
        current_time - self.last_cleanup > self.config.cleanup_interval
    }
    
    /// Obtener el estado actual del sistema de memoria
    pub fn get_current_state(&self) -> MemoryState {
        self.current_state
    }
    
    /// Obtener estadísticas del gestor
    pub fn get_manager_stats(&self) -> MemoryManagerStats {
        self.manager_stats
    }
    
    /// Obtener la configuración del gestor
    pub fn get_config(&self) -> &MemoryManagerConfig {
        &self.config
    }
    
    /// Cambiar la configuración del gestor
    pub fn update_config(&mut self, new_config: MemoryManagerConfig) {
        self.config = new_config;
    }
    
    /// Verificar la salud del sistema de memoria
    pub fn check_memory_health(&mut self) -> MemoryHealthStatus {
        self.update_state();
        
        let mut health = MemoryHealthStatus::Healthy;
        let mut issues = Vec::new();
        
        // Verificar uso de memoria física
        let physical_usage_ratio = self.current_state.used_physical as f32 / self.current_state.total_physical as f32;
        if physical_usage_ratio > 0.9 {
            health = MemoryHealthStatus::Critical;
            issues.push("Uso de memoria física crítico (>90%)");
        } else if physical_usage_ratio > 0.8 {
            health = MemoryHealthStatus::Warning;
            issues.push("Uso de memoria física alto (>80%)");
        }
        
        // Verificar fragmentación del heap
        if self.current_state.heap_fragmentation > 0.8 {
            health = MemoryHealthStatus::Warning;
            issues.push("Fragmentación del heap alta (>80%)");
        }
        
        // Verificar páginas libres
        let free_pages_ratio = self.current_state.free_pages as f32 / (self.current_state.allocated_pages + self.current_state.free_pages) as f32;
        if free_pages_ratio < 0.1 {
            health = MemoryHealthStatus::Critical;
            issues.push("Páginas libres críticamente bajas (<10%)");
        } else if free_pages_ratio < 0.2 {
            health = MemoryHealthStatus::Warning;
            issues.push("Páginas libres bajas (<20%)");
        }
        
        // Verificar integridad de los subsistemas
        if !heap::verify_heap_integrity() {
            health = MemoryHealthStatus::Critical;
            issues.push("Integridad del heap comprometida");
        }
        
        if !dma::dma_verify_integrity() {
            health = MemoryHealthStatus::Warning;
            issues.push("Integridad del sistema DMA comprometida");
        }
        
        if !shared_memory::shared_memory_verify_integrity() {
            health = MemoryHealthStatus::Warning;
            issues.push("Integridad de la memoria compartida comprometida");
        }
        
        if !virtual_memory::virtual_memory_verify_integrity() {
            health = MemoryHealthStatus::Warning;
            issues.push("Integridad de la memoria virtual comprometida");
        }
        
        MemoryHealthStatus::Detailed { health: Box::new(health), issues }
    }
    
    /// Imprimir reporte completo del sistema de memoria
    pub fn print_memory_report(&mut self) {
        self.update_state();
        
        serial_write_str("=== REPORTE COMPLETO DEL SISTEMA DE MEMORIA ===\n");
        
        // Estado general
        serial_write_str(&format!("Memoria física total: {} MB\n", self.current_state.total_physical / (1024 * 1024)));
        serial_write_str(&format!("Memoria física usada: {} MB\n", self.current_state.used_physical / (1024 * 1024)));
        serial_write_str(&format!("Memoria física libre: {} MB\n", (self.current_state.total_physical - self.current_state.used_physical) / (1024 * 1024)));
        serial_write_str(&format!("Memoria virtual total: {} MB\n", self.current_state.total_virtual / (1024 * 1024)));
        serial_write_str(&format!("Memoria virtual usada: {} MB\n", self.current_state.used_virtual / (1024 * 1024)));
        
        // Páginas
        serial_write_str(&format!("Páginas asignadas: {}\n", self.current_state.allocated_pages));
        serial_write_str(&format!("Páginas libres: {}\n", self.current_state.free_pages));
        
        // Heap
        serial_write_str(&format!("Fragmentación del heap: {:.2}%\n", self.current_state.heap_fragmentation * 100.0));
        
        // DMA
        serial_write_str(&format!("Buffers DMA activos: {}\n", self.current_state.dma_stats.active_buffers));
        serial_write_str(&format!("Memoria DMA total: {} KB\n", self.current_state.dma_stats.total_dma_memory / 1024));
        serial_write_str(&format!("Transferencias DMA completadas: {}\n", self.current_state.dma_stats.completed_transfers));
        serial_write_str(&format!("Transferencias DMA fallidas: {}\n", self.current_state.dma_stats.failed_transfers));
        
        // Estadísticas del gestor
        serial_write_str(&format!("Limpiezas automáticas realizadas: {}\n", self.manager_stats.auto_cleanups_performed));
        serial_write_str(&format!("Alertas de memoria generadas: {}\n", self.manager_stats.memory_alerts_generated));
        serial_write_str(&format!("Optimizaciones realizadas: {}\n", self.manager_stats.optimizations_performed));
        serial_write_str(&format!("Tiempo total de limpieza: {} ciclos\n", self.manager_stats.total_cleanup_time));
        serial_write_str(&format!("Tiempo total de optimización: {} ciclos\n", self.manager_stats.total_optimization_time));
        
        // Salud del sistema
        let health = self.check_memory_health();
        match health {
            MemoryHealthStatus::Healthy => {
                serial_write_str("Estado de salud: SALUDABLE\n");
            }
            MemoryHealthStatus::Warning => {
                serial_write_str("Estado de salud: ADVERTENCIA\n");
            }
            MemoryHealthStatus::Critical => {
                serial_write_str("Estado de salud: CRÍTICO\n");
            }
            MemoryHealthStatus::Detailed { health: _, issues } => {
                serial_write_str("Estado de salud: DETALLADO\n");
                for issue in issues {
                    serial_write_str(&format!("  - {}\n", issue));
                }
            }
        }
        
        serial_write_str("===============================================\n");
    }
}

/// Estado de salud del sistema de memoria
#[derive(Debug, Clone)]
pub enum MemoryHealthStatus {
    Healthy,
    Warning,
    Critical,
    Detailed {
        health: Box<MemoryHealthStatus>,
        issues: Vec<&'static str>,
    },
}

/// Instancia global del gestor de memoria
static mut MEMORY_MANAGER: Option<MemoryManager> = None;

/// Obtener timestamp actual (simulado)
fn get_timestamp() -> u64 {
    // En un sistema real, esto usaría un timer del sistema
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
}

/// Inicializar el gestor principal de memoria
pub fn init_memory_manager(config: MemoryManagerConfig, memory_config: MemoryConfig) -> Result<(), &'static str> {
    serial_write_str("MEMORY_MANAGER: Inicializando gestor principal de memoria...\n");
    
    let mut manager = MemoryManager::new(config);
    manager.initialize(memory_config)?;
    
    unsafe {
        MEMORY_MANAGER = Some(manager);
    }
    
    serial_write_str("MEMORY_MANAGER: Gestor principal de memoria inicializado\n");
    Ok(())
}

/// Obtener el gestor de memoria
fn get_memory_manager() -> &'static mut MemoryManager {
    unsafe {
        MEMORY_MANAGER.as_mut().expect("Gestor de memoria no inicializado")
    }
}

/// Realizar limpieza automática
pub fn memory_manager_cleanup() -> Result<(), &'static str> {
    let manager = get_memory_manager();
    manager.perform_cleanup()
}

/// Realizar optimización
pub fn memory_manager_optimize() -> Result<(), &'static str> {
    let manager = get_memory_manager();
    manager.perform_optimization()
}

/// Verificar si es necesario realizar limpieza
pub fn memory_manager_should_cleanup() -> bool {
    let manager = get_memory_manager();
    manager.should_perform_cleanup()
}

/// Obtener el estado actual del sistema de memoria
pub fn memory_manager_get_state() -> MemoryState {
    let manager = get_memory_manager();
    manager.get_current_state()
}

/// Obtener estadísticas del gestor
pub fn memory_manager_get_stats() -> MemoryManagerStats {
    let manager = get_memory_manager();
    manager.get_manager_stats()
}

/// Verificar la salud del sistema de memoria
pub fn memory_manager_check_health() -> MemoryHealthStatus {
    let manager = get_memory_manager();
    manager.check_memory_health()
}

/// Imprimir reporte completo del sistema de memoria
pub fn memory_manager_print_report() {
    let manager = get_memory_manager();
    manager.print_memory_report();
}

/// Función de mantenimiento automático (llamada periódicamente)
pub fn memory_manager_maintenance() -> Result<(), &'static str> {
    let manager = get_memory_manager();
    
    // Realizar limpieza si es necesario
    if manager.should_perform_cleanup() {
        manager.perform_cleanup()?;
    }
    
    // Realizar optimización
    manager.perform_optimization()?;
    
    Ok(())
}
