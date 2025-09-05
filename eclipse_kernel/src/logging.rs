//! Sistema de logging avanzado para ReactOS Rust Kernel
//! 
//! Este módulo implementa:
//! - Sistema de logging estructurado
//! - Niveles de log (Debug, Info, Warn, Error, Critical)
//! - Timestamps y contexto
//! - Rotación de logs
//! - Filtrado por módulo y nivel

use core::sync::atomic::{AtomicUsize, AtomicU64, Ordering};

/// Niveles de logging
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
    Critical = 4,
}

/// Entrada de log
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: u64,
    pub level: LogLevel,
    pub module: [u8; 32],      // Módulo como array fijo
    pub message: [u8; 256],    // Mensaje como array fijo
    pub context: [u8; 128],    // Contexto como array fijo
    pub thread_id: u32,
    pub process_id: u32,
}

impl LogEntry {
    /// Crear nueva entrada de log
    pub fn new(level: LogLevel, module: &str, message: &str, context: Option<&str>) -> Self {
        let mut module_array = [0u8; 32];
        let module_bytes = module.as_bytes();
        let copy_len = core::cmp::min(module_bytes.len(), 31);
        module_array[..copy_len].copy_from_slice(&module_bytes[..copy_len]);
        
        let mut message_array = [0u8; 256];
        let message_bytes = message.as_bytes();
        let copy_len = core::cmp::min(message_bytes.len(), 255);
        message_array[..copy_len].copy_from_slice(&message_bytes[..copy_len]);
        
        let mut context_array = [0u8; 128];
        if let Some(ctx) = context {
            let context_bytes = ctx.as_bytes();
            let copy_len = core::cmp::min(context_bytes.len(), 127);
            context_array[..copy_len].copy_from_slice(&context_bytes[..copy_len]);
        }
        
        Self {
            timestamp: 0, // Se establecerá al agregar
            level,
            module: module_array,
            message: message_array,
            context: context_array,
            thread_id: 0,
            process_id: 0,
        }
    }
    
    /// Obtener módulo como string
    pub fn get_module(&self) -> &str {
        let null_pos = self.module.iter().position(|&b| b == 0).unwrap_or(self.module.len());
        core::str::from_utf8(&self.module[..null_pos]).unwrap_or("")
    }
    
    /// Obtener mensaje como string
    pub fn get_message(&self) -> &str {
        let null_pos = self.message.iter().position(|&b| b == 0).unwrap_or(self.message.len());
        core::str::from_utf8(&self.message[..null_pos]).unwrap_or("")
    }
    
    /// Obtener contexto como string
    pub fn get_context(&self) -> &str {
        let null_pos = self.context.iter().position(|&b| b == 0).unwrap_or(self.context.len());
        core::str::from_utf8(&self.context[..null_pos]).unwrap_or("")
    }
}

/// Configuración del logger
#[derive(Debug, Clone, Copy)]
pub struct LoggerConfig {
    pub max_entries: usize,
    pub current_level: LogLevel,
    pub enable_timestamps: bool,
    pub enable_context: bool,
    pub enable_rotation: bool,
    pub rotation_size: usize,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            max_entries: 10000,
            current_level: LogLevel::Info,
            enable_timestamps: true,
            enable_context: true,
            enable_rotation: true,
            rotation_size: 1000,
        }
    }
}

/// Estadísticas del logger
#[derive(Debug, Clone, Copy)]
pub struct LoggerStats {
    pub total_entries: usize,
    pub entries_by_level: [usize; 5], // Debug, Info, Warn, Error, Critical
    pub entries_by_module: [usize; 10], // Top 10 módulos
    pub rotation_count: u64,
    pub dropped_entries: u64,
}

/// Sistema de logging
pub struct Logger {
    pub entries: [Option<LogEntry>; 10000], // Array fijo de entradas
    pub config: LoggerConfig,
    pub stats: LoggerStats,
    pub next_entry_index: AtomicUsize,
    pub total_entries: AtomicUsize,
    pub entries_by_level: [AtomicUsize; 5],
    pub rotation_count: AtomicU64,
    pub dropped_entries: AtomicU64,
    pub is_initialized: bool,
}

impl Logger {
    /// Crear un nuevo logger
    pub fn new(config: LoggerConfig) -> Self {
        Self {
            entries: [(); 10000].map(|_| None),
            config,
            stats: LoggerStats {
                total_entries: 0,
                entries_by_level: [0; 5],
                entries_by_module: [0; 10],
                rotation_count: 0,
                dropped_entries: 0,
            },
            next_entry_index: AtomicUsize::new(0),
            total_entries: AtomicUsize::new(0),
            entries_by_level: [
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ],
            rotation_count: AtomicU64::new(0),
            dropped_entries: AtomicU64::new(0),
            is_initialized: false,
        }
    }
    
    /// Inicializar logger
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.is_initialized {
            return Ok(());
        }
        
        // Limpiar todas las entradas
        for entry in &mut self.entries {
            *entry = None;
        }
        
        self.is_initialized = true;
        Ok(())
    }
    
    /// Logear mensaje
    pub fn log(&mut self, level: LogLevel, module: &str, message: &str, context: Option<&str>) {
        if level < self.config.current_level {
            return;
        }
        
        let mut entry = LogEntry::new(level, module, message, context);
        entry.timestamp = self.get_system_time();
        
        let index = self.next_entry_index.load(Ordering::SeqCst);
        
        // Verificar si necesitamos rotación
        if self.config.enable_rotation && index >= self.config.rotation_size {
            self.rotate_logs();
        }
        
        // Agregar entrada
        if index < self.entries.len() {
            self.entries[index] = Some(entry);
            self.next_entry_index.store((index + 1) % self.entries.len(), Ordering::SeqCst);
            self.total_entries.fetch_add(1, Ordering::SeqCst);
            self.entries_by_level[level as usize].fetch_add(1, Ordering::SeqCst);
        } else {
            self.dropped_entries.fetch_add(1, Ordering::SeqCst);
        }
    }
    
    /// Rotar logs
    fn rotate_logs(&mut self) {
        // Limpiar entradas antiguas
        let clear_count = self.config.rotation_size / 2;
        for i in 0..clear_count {
            let index = (self.next_entry_index.load(Ordering::SeqCst) + i) % self.entries.len();
            self.entries[index] = None;
        }
        
        self.rotation_count.fetch_add(1, Ordering::SeqCst);
    }
    
    /// Obtener entradas por nivel
    pub fn get_entries_by_level(&self, level: LogLevel) -> [Option<&LogEntry>; 100] {
        let mut result = [(); 100].map(|_| None);
        let mut count = 0;
        
        for entry in &self.entries {
            if let Some(ref log_entry) = entry {
                if log_entry.level == level && count < 100 {
                    result[count] = Some(log_entry);
                    count += 1;
                }
            }
        }
        
        result
    }
    
    /// Obtener entradas por módulo
    pub fn get_entries_by_module(&self, module: &str) -> [Option<&LogEntry>; 100] {
        let mut result = [(); 100].map(|_| None);
        let mut count = 0;
        
        for entry in &self.entries {
            if let Some(ref log_entry) = entry {
                if log_entry.get_module() == module && count < 100 {
                    result[count] = Some(log_entry);
                    count += 1;
                }
            }
        }
        
        result
    }
    
    /// Obtener entradas recientes
    pub fn get_recent_entries(&self, count: usize) -> [Option<&LogEntry>; 100] {
        let mut result = [(); 100].map(|_| None);
        let mut collected = 0;
        let max_count = core::cmp::min(count, 100);
        
        // Recorrer desde la entrada más reciente
        let start_index = self.next_entry_index.load(Ordering::SeqCst);
        
        for i in 0..self.entries.len() {
            let index = (start_index + self.entries.len() - 1 - i) % self.entries.len();
            
            if let Some(ref log_entry) = self.entries[index] {
                if collected < max_count {
                    result[collected] = Some(log_entry);
                    collected += 1;
                }
                
                if collected >= max_count {
                    break;
                }
            }
        }
        
        result
    }
    
    /// Limpiar logs
    pub fn clear_logs(&mut self) {
        for entry in &mut self.entries {
            *entry = None;
        }
        self.next_entry_index.store(0, Ordering::SeqCst);
    }
    
    /// Obtener estadísticas
    pub fn get_stats(&self) -> LoggerStats {
        let mut entries_by_level = [0; 5];
        for i in 0..5 {
            entries_by_level[i] = self.entries_by_level[i].load(Ordering::SeqCst);
        }
        
        LoggerStats {
            total_entries: self.total_entries.load(Ordering::SeqCst),
            entries_by_level,
            entries_by_module: [0; 10], // Simplificado para no_std
            rotation_count: self.rotation_count.load(Ordering::SeqCst),
            dropped_entries: self.dropped_entries.load(Ordering::SeqCst),
        }
    }
    
    /// Obtener tiempo del sistema
    fn get_system_time(&self) -> u64 {
        // En un sistema real, esto obtendría el tiempo del sistema
        0
    }
}

/// Macros de logging
#[macro_export]
macro_rules! log_debug {
    ($module:expr, $msg:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Debug, $module, $msg, None);
        }
    };
    ($module:expr, $msg:expr, $ctx:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Debug, $module, $msg, Some($ctx));
        }
    };
}

#[macro_export]
macro_rules! log_info {
    ($module:expr, $msg:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Info, $module, $msg, None);
        }
    };
    ($module:expr, $msg:expr, $ctx:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Info, $module, $msg, Some($ctx));
        }
    };
}

#[macro_export]
macro_rules! log_warn {
    ($module:expr, $msg:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Warn, $module, $msg, None);
        }
    };
    ($module:expr, $msg:expr, $ctx:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Warn, $module, $msg, Some($ctx));
        }
    };
}

#[macro_export]
macro_rules! log_error {
    ($module:expr, $msg:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Error, $module, $msg, None);
        }
    };
    ($module:expr, $msg:expr, $ctx:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Error, $module, $msg, Some($ctx));
        }
    };
}

#[macro_export]
macro_rules! log_critical {
    ($module:expr, $msg:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Critical, $module, $msg, None);
        }
    };
    ($module:expr, $msg:expr, $ctx:expr) => {
        if let Some(logger) = $crate::logging::get_logger() {
            logger.log($crate::logging::LogLevel::Critical, $module, $msg, Some($ctx));
        }
    };
}

/// Gestor de logging global
static mut LOGGER: Option<Logger> = None;

/// Inicializar logger
pub fn init_logger(config: LoggerConfig) -> Result<(), &'static str> {
    let mut logger = Logger::new(config);
    logger.initialize()?;
    
    unsafe {
        LOGGER = Some(logger);
    }
    
    Ok(())
}

/// Obtener logger
pub fn get_logger() -> Option<&'static mut Logger> {
    unsafe {
        LOGGER.as_mut()
    }
}

/// Logear mensaje
pub fn log_message(level: LogLevel, module: &str, message: &str, context: Option<&str>) {
    if let Some(logger) = get_logger() {
        logger.log(level, module, message, context);
    }
}

/// Obtener estadísticas del logger
pub fn get_logger_stats() -> Option<LoggerStats> {
    get_logger().map(|logger| logger.get_stats())
}

/// Limpiar logs
pub fn clear_logs() {
    if let Some(logger) = get_logger() {
        logger.clear_logs();
    }
}
