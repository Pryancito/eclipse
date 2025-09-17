//! Sistema de logging avanzado para Eclipse OS
//! 
//! Implementa logging estructurado, niveles de log y rotación de archivos

use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU32, Ordering};

/// Nivel de logging
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
    Fatal = 5,
}

/// Entrada de log
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: u64,
    pub level: LogLevel,
    pub module: String,
    pub message: String,
    pub thread_id: u32,
    pub process_id: u32,
}

/// Configuración de logging
#[derive(Debug, Clone)]
pub struct LogConfig {
    pub min_level: LogLevel,
    pub max_entries: usize,
    pub enable_console: bool,
    pub enable_file: bool,
    pub log_file_path: String,
    pub enable_rotation: bool,
    pub max_file_size: usize,
    pub max_files: usize,
    pub enable_colors: bool,
    pub enable_timestamps: bool,
    pub enable_thread_info: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            min_level: LogLevel::Info,
            max_entries: 10000,
            enable_console: true,
            enable_file: false,
            log_file_path: "/var/log/eclipse.log"String::from(.to_string(),
            enable_rotation: true,
            max_file_size: 10 * 1024 * 1024, // 10MB
            max_files: 5,
            enable_colors: true,
            enable_timestamps: true,
            enable_thread_info: true,
        }
    }
}

/// Gestor de logging
pub struct LogManager {
    config: LogConfig,
    entries: VecDeque<LogEntry>,
    next_entry_id: AtomicU32,
    initialized: bool,
}

impl LogManager {
    pub fn new(config: LogConfig) -> Self {
        Self {
            config,
            entries: VecDeque::new(),
            next_entry_id: AtomicU32::new(1),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Log manager already initialized");
        }

        self.initialized = true;
        Ok(())
    }

    pub fn log(&mut self, level: LogLevel, module: &str, message: &str) {
        if !self.initialized || level < self.config.min_level {
            return;
        }

        let entry = LogEntry {
            timestamp: self.get_current_time(),
            level,
            module: moduleString::from(.to_string(),
            message: messageString::from(.to_string(),
            thread_id: 0, // Se establecería en una implementación real
            process_id: 0, // Se establecería en una implementación real
        };

        // Agregar entrada
        self.entries.push_back(entry);

        // Mantener límite de entradas
        while self.entries.len() > self.config.max_entries {
            self.entries.pop_front();
        }

        // Mostrar en consola si está habilitado
        if self.config.enable_console {
            self.print_to_console(&entry);
        }

        // Escribir a archivo si está habilitado
        if self.config.enable_file {
            let _ = self.write_to_file(&entry);
        }
    }

    fn print_to_console(&self, entry: &LogEntry) {
        let color = if self.config.enable_colors {
            self.get_color_for_level(entry.level)
        } else {
            ""
        };

        let reset = if self.config.enable_colors {
            "\x1b[0m"
        } else {
            ""
        };

        let timestamp = if self.config.enable_timestamps {
            alloc::format!("[{}] ", entry.timestamp)
        } else {
            String::new()
        };

        let thread_info = if self.config.enable_thread_info {
            alloc::format!("[T{}:P{}] ", entry.thread_id, entry.process_id)
        } else {
            String::new()
        };

        let level_str = self.get_level_string(entry.level);
        let module_str = if !entry.module.is_empty() {
            alloc::format!("[{}] ", entry.module)
        } else {
            String::new()
        };

        let log_line = alloc::format!(
            "{}{}{}{}{}{}{}",
            color,
            timestamp,
            thread_info,
            level_str,
            module_str,
            entry.message,
            reset
        );

        // En una implementación real, aquí se imprimiría a la consola
        // Por ahora, solo simulamos
    }

    fn write_to_file(&self, entry: &LogEntry) -> Result<(), &'static str> {
        // En una implementación real, aquí se escribiría al archivo
        // Por ahora, solo simulamos
        Ok(())
    }

    fn get_color_for_level(&self, level: LogLevel) -> &'static str {
        match level {
            LogLevel::Trace => "\x1b[37m", // Blanco
            LogLevel::Debug => "\x1b[36m", // Cian
            LogLevel::Info => "\x1b[32m",  // Verde
            LogLevel::Warn => "\x1b[33m",  // Amarillo
            LogLevel::Error => "\x1b[31m", // Rojo
            LogLevel::Fatal => "\x1b[35m", // Magenta
        }
    }

    fn get_level_string(&self, level: LogLevel) -> String {
        match level {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO ",
            LogLevel::Warn => "WARN ",
            LogLevel::Error => "ERROR",
            LogLevel::Fatal => "FATAL",
        }String::from(.to_string()
    }

    pub fn get_entries(&self) -> &VecDeque<LogEntry> {
        &self.entries
    }

    pub fn get_entries_by_level(&self, level: LogLevel) -> Vec<&LogEntry> {
        self.entries.iter()
            .filter(|entry| entry.level == level)
            .collect()
    }

    pub fn get_entries_by_module(&self, module: &str) -> Vec<&LogEntry> {
        self.entries.iter()
            .filter(|entry| entry.module == module)
            .collect()
    }

    pub fn clear_entries(&mut self) {
        self.entries.clear();
    }

    pub fn set_min_level(&mut self, level: LogLevel) {
        self.config.min_level = level;
    }

    pub fn get_config(&self) -> &LogConfig {
        &self.config
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    fn get_current_time(&self) -> u64 {
        // En un sistema real, esto obtendría el tiempo actual del sistema
        0 // Simulado
    }
}

/// Macros de logging para facilitar el uso
#[macro_export]
macro_rules! log_trace {
    ($module:expr, $($arg:tt)*) => {
        if let Some(log_manager) = &mut crate::logging::GLOBAL_LOG_MANAGER {
            log_manager.log(crate::logging::LogLevel::Trace, $module, &alloc::format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_debug {
    ($module:expr, $($arg:tt)*) => {
        if let Some(log_manager) = &mut crate::logging::GLOBAL_LOG_MANAGER {
            log_manager.log(crate::logging::LogLevel::Debug, $module, &alloc::format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_info {
    ($module:expr, $($arg:tt)*) => {
        if let Some(log_manager) = &mut crate::logging::GLOBAL_LOG_MANAGER {
            log_manager.log(crate::logging::LogLevel::Info, $module, &alloc::format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_warn {
    ($module:expr, $($arg:tt)*) => {
        if let Some(log_manager) = &mut crate::logging::GLOBAL_LOG_MANAGER {
            log_manager.log(crate::logging::LogLevel::Warn, $module, &alloc::format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_error {
    ($module:expr, $($arg:tt)*) => {
        if let Some(log_manager) = &mut crate::logging::GLOBAL_LOG_MANAGER {
            log_manager.log(crate::logging::LogLevel::Error, $module, &alloc::format!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! log_fatal {
    ($module:expr, $($arg:tt)*) => {
        if let Some(log_manager) = &mut crate::logging::GLOBAL_LOG_MANAGER {
            log_manager.log(crate::logging::LogLevel::Fatal, $module, &alloc::format!($($arg)*));
        }
    };
}

/// Gestor de logging global
pub static mut GLOBAL_LOG_MANAGER: Option<LogManager> = None;

pub fn initialize_global_logging(config: LogConfig) -> Result<(), &'static str> {
    unsafe {
        GLOBAL_LOG_MANAGER = Some(LogManager::new(config));
        if let Some(log_manager) = &mut GLOBAL_LOG_MANAGER {
            log_manager.initialize()
        } else {
            Err("Failed to create global log manager")
        }
    }
}

pub fn get_global_log_manager() -> Option<&'static mut LogManager> {
    unsafe {
        GLOBAL_LOG_MANAGER.as_mut()
    }
}
