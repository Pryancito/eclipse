//! Sistema de manejo de errores robusto para Eclipse OS
//! 
//! Implementa manejo de errores, recuperación y notificaciones

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Debug;

/// Tipo de error
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorType {
    Memory,
    Process,
    FileSystem,
    Network,
    Audio,
    Graphics,
    Security,
    Hardware,
    Software,
    Unknown,
}

/// Severidad del error
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorSeverity {
    Info = 0,
    Warning = 1,
    Error = 2,
    Critical = 3,
    Fatal = 4,
}

/// Información de error
#[derive(Debug, Clone)]
pub struct ErrorInfo {
    pub error_type: ErrorType,
    pub severity: ErrorSeverity,
    pub message: String,
    pub module: String,
    pub function: String,
    pub line: u32,
    pub timestamp: u64,
    pub context: Vec<String>,
    pub is_recoverable: bool,
    pub recovery_action: Option<String>,
}

/// Configuración de manejo de errores
#[derive(Debug, Clone)]
pub struct ErrorConfig {
    pub enable_logging: bool,
    pub enable_recovery: bool,
    pub enable_notifications: bool,
    pub max_errors: usize,
    pub auto_recovery: bool,
    pub panic_on_fatal: bool,
    pub enable_stack_trace: bool,
}

impl Default for ErrorConfig {
    fn default() -> Self {
        Self {
            enable_logging: true,
            enable_recovery: true,
            enable_notifications: true,
            max_errors: 1000,
            auto_recovery: true,
            panic_on_fatal: true,
            enable_stack_trace: true,
        }
    }
}

/// Gestor de errores
pub struct ErrorManager {
    config: ErrorConfig,
    errors: Vec<ErrorInfo>,
    initialized: bool,
}

impl ErrorManager {
    pub fn new(config: ErrorConfig) -> Self {
        Self {
            config,
            errors: Vec::new(),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Error manager already initialized");
        }

        self.initialized = true;
        Ok(())
    }

    pub fn report_error(
        &mut self,
        error_type: ErrorType,
        severity: ErrorSeverity,
        message: &str,
        module: &str,
        function: &str,
        line: u32,
        context: Vec<String>,
    ) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Error manager not initialized");
        }

        let error_info = ErrorInfo {
            error_type,
            severity,
            message: messageString::from(.to_string(),
            module: moduleString::from(.to_string(),
            function: functionString::from(.to_string(),
            line,
            timestamp: self.get_current_time(),
            context,
            is_recoverable: self.is_recoverable(error_type, severity),
            recovery_action: self.get_recovery_action(error_type, severity),
        };

        // Agregar error a la lista
        self.errors.push(error_info.clone());

        // Mantener límite de errores
        while self.errors.len() > self.config.max_errors {
            self.errors.remove(0);
        }

        // Logging si está habilitado
        if self.config.enable_logging {
            self.log_error(&error_info);
        }

        // Notificaciones si está habilitado
        if self.config.enable_notifications {
            self.notify_error(&error_info);
        }

        // Recuperación automática si está habilitado
        if self.config.auto_recovery && error_info.is_recoverable {
            self.attempt_recovery(&error_info);
        }

        // Panic si es fatal y está habilitado
        if self.config.panic_on_fatal && severity == ErrorSeverity::Fatal {
            self.panic_on_fatal_error(&error_info);
        }

        Ok(())
    }

    fn is_recoverable(&self, error_type: ErrorType, severity: ErrorSeverity) -> bool {
        match (error_type, severity) {
            (ErrorType::Memory, ErrorSeverity::Error) => true,
            (ErrorType::Process, ErrorSeverity::Warning) => true,
            (ErrorType::FileSystem, ErrorSeverity::Error) => true,
            (ErrorType::Network, ErrorSeverity::Warning) => true,
            (ErrorType::Audio, ErrorSeverity::Warning) => true,
            (ErrorType::Graphics, ErrorSeverity::Warning) => true,
            (ErrorType::Security, ErrorSeverity::Error) => false,
            (ErrorType::Hardware, ErrorSeverity::Critical) => false,
            (ErrorType::Software, ErrorSeverity::Error) => true,
            (ErrorType::Unknown, _) => false,
            (_, ErrorSeverity::Fatal) => false,
            (_, ErrorSeverity::Critical) => false,
            _ => true,
        }
    }

    fn get_recovery_action(&self, error_type: ErrorType, severity: ErrorSeverity) -> Option<String> {
        match (error_type, severity) {
            (ErrorType::Memory, _) => Some("Free memory and retry"String::from(.to_string()),
            (ErrorType::Process, _) => Some("Restart process"String::from(.to_string()),
            (ErrorType::FileSystem, _) => Some("Check filesystem and retry"String::from(.to_string()),
            (ErrorType::Network, _) => Some("Reconnect network"String::from(.to_string()),
            (ErrorType::Audio, _) => Some("Reinitialize audio driver"String::from(.to_string()),
            (ErrorType::Graphics, _) => Some("Reset graphics driver"String::from(.to_string()),
            (ErrorType::Software, _) => Some("Restart software component"String::from(.to_string()),
            _ => None,
        }
    }

    fn log_error(&self, error_info: &ErrorInfo) {
        // En una implementación real, aquí se registraría el error
        // Por ahora, solo simulamos
    }

    fn notify_error(&self, error_info: &ErrorInfo) {
        // En una implementación real, aquí se notificaría el error
        // Por ahora, solo simulamos
    }

    fn attempt_recovery(&self, error_info: &ErrorInfo) {
        // En una implementación real, aquí se intentaría la recuperación
        // Por ahora, solo simulamos
    }

    fn panic_on_fatal_error(&self, error_info: &ErrorInfo) {
        // En una implementación real, aquí se haría panic
        // Por ahora, solo simulamos
    }

    pub fn get_errors(&self) -> &[ErrorInfo] {
        &self.errors
    }

    pub fn get_errors_by_type(&self, error_type: ErrorType) -> Vec<&ErrorInfo> {
        self.errors.iter()
            .filter(|error| error.error_type == error_type)
            .collect()
    }

    pub fn get_errors_by_severity(&self, severity: ErrorSeverity) -> Vec<&ErrorInfo> {
        self.errors.iter()
            .filter(|error| error.severity == severity)
            .collect()
    }

    pub fn get_error_count(&self) -> usize {
        self.errors.len()
    }

    pub fn get_error_count_by_type(&self, error_type: ErrorType) -> usize {
        self.errors.iter()
            .filter(|error| error.error_type == error_type)
            .count()
    }

    pub fn get_error_count_by_severity(&self, severity: ErrorSeverity) -> usize {
        self.errors.iter()
            .filter(|error| error.severity == severity)
            .count()
    }

    pub fn clear_errors(&mut self) {
        self.errors.clear();
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    fn get_current_time(&self) -> u64 {
        // En un sistema real, esto obtendría el tiempo actual del sistema
        0 // Simulado
    }
}

/// Macros para facilitar el reporte de errores
#[macro_export]
macro_rules! report_error {
    ($error_type:expr, $severity:expr, $message:expr, $module:expr, $function:expr, $line:expr) => {
        if let Some(error_manager) = &mut crate::error_handling::GLOBAL_ERROR_MANAGER {
            let _ = error_manager.report_error(
                $error_type,
                $severity,
                $message,
                $module,
                $function,
                $line,
                Vec::new(),
            );
        }
    };
}

#[macro_export]
macro_rules! report_error_with_context {
    ($error_type:expr, $severity:expr, $message:expr, $module:expr, $function:expr, $line:expr, $context:expr) => {
        if let Some(error_manager) = &mut crate::error_handling::GLOBAL_ERROR_MANAGER {
            let _ = error_manager.report_error(
                $error_type,
                $severity,
                $message,
                $module,
                $function,
                $line,
                $context,
            );
        }
    };
}

/// Gestor de errores global
pub static mut GLOBAL_ERROR_MANAGER: Option<ErrorManager> = None;

pub fn initialize_global_error_handling(config: ErrorConfig) -> Result<(), &'static str> {
    unsafe {
        GLOBAL_ERROR_MANAGER = Some(ErrorManager::new(config));
        if let Some(error_manager) = &mut GLOBAL_ERROR_MANAGER {
            error_manager.initialize()
        } else {
            Err("Failed to create global error manager")
        }
    }
}

pub fn get_global_error_manager() -> Option<&'static mut ErrorManager> {
    unsafe {
        GLOBAL_ERROR_MANAGER.as_mut()
    }
}
