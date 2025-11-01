//! Sistema de Logging Estructurado para Eclipse Kernel
//!
//! Este módulo proporciona un sistema de logging avanzado con:
//! - Niveles de severidad (DEBUG, INFO, WARN, ERROR)
//! - Categorización por módulos
//! - Timestamps
//! - Salida dual (serial + framebuffer)
//! - Filtrado por nivel y módulo

#![no_std]
#![allow(unused_imports)]

extern crate alloc;
use alloc::string::{String, ToString};
use alloc::format;
use alloc::vec::Vec;
use core::fmt::Write;

/// Niveles de severidad del log
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    /// Convierte el nivel a string para display
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    /// Color del framebuffer para cada nivel
    pub fn color(&self) -> crate::drivers::framebuffer::Color {
        match self {
            LogLevel::Debug => crate::drivers::framebuffer::Color::LIGHT_GRAY,
            LogLevel::Info => crate::drivers::framebuffer::Color::WHITE,
            LogLevel::Warn => crate::drivers::framebuffer::Color::YELLOW,
            LogLevel::Error => crate::drivers::framebuffer::Color::RED,
        }
    }
}

/// Estructura de configuración del logger
pub struct LoggerConfig {
    /// Nivel mínimo de logging
    pub min_level: LogLevel,
    /// Módulos permitidos (vacío = todos)
    pub allowed_modules: Vec<&'static str>,
    /// Habilitar timestamps
    pub enable_timestamps: bool,
    /// Habilitar salida al framebuffer
    pub enable_framebuffer: bool,
    /// Línea actual del framebuffer para logging
    pub fb_line: u32,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            min_level: LogLevel::Info,
            allowed_modules: Vec::new(), // Todos permitidos
            enable_timestamps: true,
            enable_framebuffer: true,
            fb_line: 1, // Empezar desde línea 1 (después del título)
        }
    }
}

/// Logger global del kernel
pub struct KernelLogger {
    config: LoggerConfig,
}

impl KernelLogger {
    /// Crea un nuevo logger con configuración por defecto
    pub const fn new() -> Self {
        Self {
            config: LoggerConfig {
                min_level: LogLevel::Info,
                allowed_modules: Vec::new(),
                enable_timestamps: true,
                enable_framebuffer: true,
                fb_line: 1,
            },
        }
    }

    /// Configura el logger
    pub fn configure(&mut self, config: LoggerConfig) {
        self.config = config;
    }

    /// Verifica si un módulo está permitido
    fn is_module_allowed(&self, module: &str) -> bool {
        self.config.allowed_modules.is_empty() ||
        self.config.allowed_modules.iter().any(|m| module.contains(m))
    }

    /// Obtiene timestamp simple (milisegundos desde boot)
    fn get_timestamp() -> u64 {
        // Por ahora usamos un contador simple
        // En un sistema real usaríamos el timer del sistema
        static mut TIMESTAMP: u64 = 0;
        unsafe {
            TIMESTAMP += 1;
            TIMESTAMP
        }
    }

    /// Log interno con todos los parámetros
    fn log_internal(&mut self, level: LogLevel, module: &str, message: &str) {
        // Verificar nivel mínimo
        if level < self.config.min_level {
            return;
        }

        // Verificar módulo permitido
        if !self.is_module_allowed(module) {
            return;
        }

        // Construir mensaje
        let mut full_message = String::new();

        if self.config.enable_timestamps {
            let timestamp = Self::get_timestamp();
            write!(full_message, "[{}] ", timestamp).ok();
        }

        write!(full_message, "[{}] [{}] {}", level.as_str(), module, message).ok();

        // Log a serial
        crate::debug::serial_write_str(&full_message);
        crate::debug::serial_write_str("\n");

        // Log a framebuffer si está habilitado
        if self.config.enable_framebuffer {
            if let Some(mut fb) = crate::drivers::framebuffer::get_framebuffer() {
                // Limitar línea del framebuffer
                if self.config.fb_line < fb.info.height / 16 {
                    fb.write_text_kernel(&full_message, level.color());
                    self.config.fb_line += 1;
                } else {
                    // Scroll si nos quedamos sin espacio
                    // Por simplicidad, solo incrementamos línea (no scroll real)
                    self.config.fb_line = fb.info.height / 16 - 1;
                }
            }
        }
    }

    /// Log de debug
    pub fn debug(&mut self, module: &str, message: &str) {
        self.log_internal(LogLevel::Debug, module, message);
    }

    /// Log de info
    pub fn info(&mut self, module: &str, message: &str) {
        self.log_internal(LogLevel::Info, module, message);
    }

    /// Log de warning
    pub fn warn(&mut self, module: &str, message: &str) {
        self.log_internal(LogLevel::Warn, module, message);
    }

    /// Log de error
    pub fn error(&mut self, module: &str, message: &str) {
        self.log_internal(LogLevel::Error, module, message);
    }
}

/// Logger global estático
static mut KERNEL_LOGGER: Option<KernelLogger> = None;

/// Inicializa el sistema de logging
pub fn init_logger() -> Result<(), &'static str> {
    unsafe {
        KERNEL_LOGGER = Some(KernelLogger::new());
    }
    Ok(())
}

/// Configura el logger global
pub fn configure_logger(config: LoggerConfig) {
    unsafe {
        if let Some(logger) = KERNEL_LOGGER.as_mut() {
            logger.configure(config);
        }
    }
}

/// Obtiene referencia al logger global
pub fn get_logger() -> &'static mut KernelLogger {
    unsafe {
        KERNEL_LOGGER.as_mut().expect("Logger no inicializado")
    }
}

// Las macros se definen en macros.rs para evitar duplicación

/// Función de conveniencia para logging rápido
pub fn log(level: LogLevel, module: &str, message: &str) {
    let logger = get_logger();
    match level {
        LogLevel::Debug => logger.debug(module, message),
        LogLevel::Info => logger.info(module, message),
        LogLevel::Warn => logger.warn(module, message),
        LogLevel::Error => logger.error(module, message),
    }
}

/// Configuración de debug del logger
pub fn set_debug_mode(enable: bool) {
    let mut config = LoggerConfig::default();
    config.min_level = if enable { LogLevel::Debug } else { LogLevel::Info };
    configure_logger(config);
}

/// Configuración de módulos permitidos
pub fn set_allowed_modules(modules: Vec<&'static str>) {
    let mut config = LoggerConfig::default();
    config.allowed_modules = modules;
    configure_logger(config);
}
