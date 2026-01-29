//! Sistema de Recuperación de Errores para Eclipse Kernel
//!
//! Este módulo implementa un sistema robusto de manejo de errores durante
//! el arranque del kernel, permitiendo recuperación automática y modos
//! de boot alternativos cuando ocurren fallos.

#![no_std]
#![allow(unused_imports)]

extern crate alloc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Niveles de criticidad de los errores durante el boot
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    /// Error crítico que impide continuar el boot
    Critical,
    /// Error recuperable que permite continuar con funcionalidades reducidas
    Recoverable,
    /// Advertencia que no impide el boot pero reduce funcionalidades
    Warning,
}

/// Modos de boot disponibles
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    /// Boot completo con todas las funcionalidades
    Full,
    /// Boot seguro con validaciones adicionales
    Safe,
    /// Boot mínimo con funcionalidades esenciales
    Minimal,
    /// Boot de recuperación para diagnóstico
    Recovery,
}

/// Estado del sistema de recuperación
#[derive(Debug)]
pub struct RecoveryState {
    pub current_mode: BootMode,
    pub errors_encountered: Vec<InitError>,
    pub recovery_attempts: u32,
    pub degraded_features: Vec<String>,
}

impl RecoveryState {
    pub fn new() -> Self {
        Self {
            current_mode: BootMode::Full,
            errors_encountered: Vec::new(),
            recovery_attempts: 0,
            degraded_features: Vec::new(),
        }
    }

    /// Registra un error y determina si se puede recuperar
    pub fn register_error(&mut self, error: InitError) -> RecoveryAction {
        self.errors_encountered.push(error.clone());

        match error.severity {
            ErrorSeverity::Critical => {
                // logging removido
                RecoveryAction::Panic(error.message)
            }
            ErrorSeverity::Recoverable => {
                // logging removido

                // Intentar cambiar a modo de recuperación
                if self.recovery_attempts < 3 {
                    self.recovery_attempts += 1;
                    self.degraded_features.push(error.component.clone());
                    RecoveryAction::SwitchMode(BootMode::Safe)
                } else {
                    RecoveryAction::SwitchMode(BootMode::Minimal)
                }
            }
            ErrorSeverity::Warning => {
                // logging removido
                self.degraded_features.push(error.component.clone());
                RecoveryAction::Continue
            }
        }
    }

    /// Verifica si una funcionalidad está disponible en el modo actual
    pub fn is_feature_available(&self, feature: &str) -> bool {
        match self.current_mode {
            BootMode::Full => !self.degraded_features.contains(&feature.to_string()),
            BootMode::Safe => {
                // En modo seguro, algunas funcionalidades avanzadas están deshabilitadas
                !self.degraded_features.contains(&feature.to_string()) &&
                !matches!(feature, "ai_services" | "networking" | "graphics_acceleration")
            }
            BootMode::Minimal => {
                // En modo mínimo, solo funcionalidades esenciales
                matches!(feature, "logging" | "memory" | "basic_filesystem")
            }
            BootMode::Recovery => false, // Modo de recuperación es solo para diagnóstico
        }
    }

    /// Obtiene el estado actual del sistema
    pub fn get_status(&self) -> BootStatus {
        BootStatus {
            mode: self.current_mode,
            error_count: self.errors_encountered.len(),
            degraded_features: self.degraded_features.len(),
            recovery_attempts: self.recovery_attempts,
        }
    }
}

/// Acciones de recuperación posibles
#[derive(Debug)]
pub enum RecoveryAction {
    /// Continuar normalmente
    Continue,
    /// Cambiar a un modo de boot diferente
    SwitchMode(BootMode),
    /// Terminar el boot con pánico
    Panic(String),
}

/// Estado actual del boot
#[derive(Debug)]
pub struct BootStatus {
    pub mode: BootMode,
    pub error_count: usize,
    pub degraded_features: usize,
    pub recovery_attempts: u32,
}

/// Error de inicialización con contexto
#[derive(Debug, Clone)]
pub struct InitError {
    pub component: String,
    pub message: String,
    pub severity: ErrorSeverity,
    pub recoverable: bool,
}

impl InitError {
    pub fn new(component: &str, message: &str, severity: ErrorSeverity) -> Self {
        Self {
            component: component.to_string(),
            message: message.to_string(),
            severity,
            recoverable: matches!(severity, ErrorSeverity::Recoverable | ErrorSeverity::Warning),
        }
    }

    pub fn critical(component: &str, message: &str) -> Self {
        Self::new(component, message, ErrorSeverity::Critical)
    }

    pub fn recoverable(component: &str, message: &str) -> Self {
        Self::new(component, message, ErrorSeverity::Recoverable)
    }

    pub fn warning(component: &str, message: &str) -> Self {
        Self::new(component, message, ErrorSeverity::Warning)
    }
}

/// Resultado de inicialización que puede incluir recuperación
pub type InitResult<T> = Result<T, InitError>;

/// Estado global del sistema de recuperación
static mut RECOVERY_STATE: Option<RecoveryState> = None;

/// Inicializa el sistema de recuperación de errores
pub fn init_error_recovery() -> Result<(), &'static str> {
    unsafe {
        RECOVERY_STATE = Some(RecoveryState::new());
    }
    // log removido
    Ok(())
}

/// Obtiene referencia al estado de recuperación
pub fn get_recovery_state() -> &'static mut RecoveryState {
    unsafe {
        RECOVERY_STATE.as_mut().expect("Sistema de recuperación no inicializado")
    }
}

/// Función de inicialización con recuperación automática
pub fn init_with_recovery<F, T>(
    component: &str,
    operation: F,
    fallback: Option<T>
) -> Result<T, RecoveryAction>
where
    F: FnOnce() -> InitResult<T>,
{
    let state = get_recovery_state();

    match operation() {
        Ok(result) => Ok(result),
        Err(init_error) => {
            let action = state.register_error(init_error);
            match action {
                RecoveryAction::Continue => {
                    // Usar fallback si está disponible
                    if let Some(fallback_value) = fallback {
                        // log removido
                        Ok(fallback_value)
                    } else {
                        Err(RecoveryAction::Continue)
                    }
                }
                other_action => Err(other_action),
            }
        }
    }
}

/// Inicialización de componentes críticos con manejo de errores
pub mod init_components {
    use super::*;

    /// Inicializa el allocator de memoria
    pub fn init_memory_allocator() -> InitResult<()> {
        #[cfg(feature = "alloc")]
        {

            // log removido
            Ok(())
        }

        #[cfg(not(feature = "alloc"))]
        {
            Err(InitError::critical("memory", "Allocator no disponible - feature 'alloc' deshabilitada"))
        }
    }

    /// Inicializa el sistema de paginación
    pub fn init_paging_system() -> InitResult<crate::paging::PagingManager> {
        let paging_manager = crate::paging::PagingManager::new();
        // log removido
        Ok(paging_manager)
    }

    /// Inicializa la detección de hardware
    pub fn init_hardware_detection() -> InitResult<crate::hardware_detection::HardwareDetectionResult> {
        let hw_result = crate::hardware_detection::detect_graphics_hardware();
        // log removido
        Ok(hw_result.clone())
    }

    /// Inicializa el sistema de archivos
    pub fn init_filesystem() -> InitResult<()> {
        crate::filesystem::vfs::init_vfs();
        // log removido
        Ok(())
    }

    /// Inicializa servicios de IA (puede fallar en modo seguro)
    pub fn init_ai_services() -> InitResult<()> {
        let state = get_recovery_state();

        if !state.is_feature_available("ai_services") {
            return Err(InitError::warning("ai", "Servicios de IA deshabilitados por configuración de recuperación"));
        }

        // Aquí iría la inicialización real de servicios de IA
        // Por ahora, solo simulamos que puede fallar
        // log removido
        Ok(())
    }
}

/// Macros para facilitar el uso del sistema de recuperación
#[macro_export]
macro_rules! try_init {
    ($component:expr, $operation:expr) => {
        $crate::error_recovery::init_with_recovery($component, || $operation, None)
    };
}

#[macro_export]
macro_rules! try_init_with_fallback {
    ($component:expr, $operation:expr, $fallback:expr) => {
        $crate::error_recovery::init_with_recovery($component, || $operation, Some($fallback))
    };
}

#[macro_export]
macro_rules! recovery_status {
    () => {
        $crate::error_recovery::get_recovery_state().get_status()
    };
}

/// Función para mostrar el estado de recuperación en pantalla
pub fn display_recovery_status(fb: &mut crate::drivers::framebuffer::FramebufferDriver) {
    let status = recovery_status!();

    let mode_str = match status.mode {
        BootMode::Full => "COMPLETO",
        BootMode::Safe => "SEGURO",
        BootMode::Minimal => "MÍNIMO",
        BootMode::Recovery => "RECUPERACIÓN",
    };

    fb.write_text_kernel(
        &alloc::format!("Modo de Boot: {} | Errores: {} | Funcionalidades degradadas: {}",
                       mode_str, status.error_count, status.degraded_features),
        crate::drivers::framebuffer::Color::CYAN
    );
}
