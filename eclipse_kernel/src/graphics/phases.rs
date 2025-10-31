//! Sistema de fases de inicialización de gráficos
//!
//! Arquitectura de 3 fases:
//! 1. UEFI/GOP para bootloader
//! 2. UEFI/GOP para kernel en detección de gráficos  
//! 3. DRM/FB/GOP para kernel posterior

use crate::drivers::framebuffer::FramebufferInfo;
use core::fmt;

/// Fases de inicialización de gráficos
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsPhase {
    /// Fase 1: UEFI/GOP para bootloader
    UefiBootloader,
    /// Fase 2: UEFI/GOP para kernel en detección de gráficos
    UefiKernelDetection,
    /// Fase 3: DRM/FB/GOP para kernel posterior
    DrmKernelRuntime,
    /// Fase de fallback: gráficos básicos
    FallbackBasic,
}

impl fmt::Display for GraphicsPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphicsPhase::UefiBootloader => write!(f, "UEFI Bootloader"),
            GraphicsPhase::UefiKernelDetection => write!(f, "UEFI Kernel Detection"),
            GraphicsPhase::DrmKernelRuntime => write!(f, "DRM Kernel Runtime"),
            GraphicsPhase::FallbackBasic => write!(f, "Fallback Basic"),
        }
    }
}

/// Estado de la fase de gráficos actual
#[derive(Debug)]
pub struct GraphicsPhaseState {
    /// Fase actual
    pub current_phase: GraphicsPhase,
    /// Información del framebuffer actual
    pub framebuffer_info: Option<FramebufferInfo>,
    /// Si la fase está completamente inicializada
    pub is_initialized: bool,
    /// Timestamp de la última transición
    pub transition_time: u64,
}

impl GraphicsPhaseState {
    /// Crear nuevo estado en fase de bootloader
    pub fn new() -> Self {
        Self {
            current_phase: GraphicsPhase::UefiBootloader,
            framebuffer_info: None,
            is_initialized: false,
            transition_time: 0,
        }
    }

    /// Transicionar a la siguiente fase
    pub fn transition_to(&mut self, new_phase: GraphicsPhase) -> Result<(), &'static str> {
        // Validar transición válida
        if !self.is_valid_transition(new_phase) {
            return Err("Transición de fase inválida");
        }

        self.current_phase = new_phase;
        self.transition_time = Self::get_timestamp();
        self.is_initialized = false;

        Ok(())
    }

    /// Verificar si la transición es válida
    fn is_valid_transition(&self, new_phase: GraphicsPhase) -> bool {
        match (self.current_phase, new_phase) {
            (GraphicsPhase::UefiBootloader, GraphicsPhase::UefiKernelDetection) => true,
            (GraphicsPhase::UefiKernelDetection, GraphicsPhase::DrmKernelRuntime) => true,
            (GraphicsPhase::DrmKernelRuntime, GraphicsPhase::DrmKernelRuntime) => true, // Re-inicialización
            _ => false,
        }
    }

    /// Obtener timestamp simple
    fn get_timestamp() -> u64 {
        // En una implementación real, esto usaría un timer del sistema
        // Por ahora, simulamos con un contador
        unsafe {
            static mut COUNTER: u64 = 0;
            COUNTER += 1;
            COUNTER
        }
    }

    /// Marcar la fase como inicializada
    pub fn mark_initialized(&mut self, framebuffer_info: FramebufferInfo) {
        self.framebuffer_info = Some(framebuffer_info);
        self.is_initialized = true;
    }

    /// Verificar si estamos en fase de detección
    pub fn is_detection_phase(&self) -> bool {
        matches!(self.current_phase, GraphicsPhase::UefiKernelDetection)
    }

    /// Verificar si estamos en fase de runtime
    pub fn is_runtime_phase(&self) -> bool {
        matches!(self.current_phase, GraphicsPhase::DrmKernelRuntime)
    }
}

/// Manager de fases de gráficos
pub struct GraphicsPhaseManager {
    state: GraphicsPhaseState,
}

impl GraphicsPhaseManager {
    /// Crear nuevo manager
    pub fn new() -> Self {
        Self {
            state: GraphicsPhaseState::new(),
        }
    }

    /// Obtener estado actual
    pub fn get_state(&self) -> &GraphicsPhaseState {
        &self.state
    }

    /// Obtener estado mutable
    pub fn get_state_mut(&mut self) -> &mut GraphicsPhaseState {
        &mut self.state
    }

    /// Inicializar fase de detección UEFI
    pub fn init_uefi_detection(&mut self) -> Result<(), &'static str> {
        self.state
            .transition_to(GraphicsPhase::UefiKernelDetection)?;
        Ok(())
    }

    /// Inicializar fase de runtime DRM
    pub fn init_drm_runtime(
        &mut self,
        framebuffer_info: FramebufferInfo,
    ) -> Result<(), &'static str> {
        self.state.transition_to(GraphicsPhase::DrmKernelRuntime)?;
        self.state.mark_initialized(framebuffer_info);
        Ok(())
    }

    /// Verificar si podemos usar DRM
    pub fn can_use_drm(&self) -> bool {
        self.state.is_runtime_phase() && self.state.is_initialized
    }

    /// Verificar si debemos usar UEFI
    pub fn should_use_uefi(&self) -> bool {
        matches!(
            self.state.current_phase,
            GraphicsPhase::UefiBootloader | GraphicsPhase::UefiKernelDetection
        )
    }
}

/// Instancia global del manager de fases
static mut GRAPHICS_PHASE_MANAGER: Option<GraphicsPhaseManager> = None;

/// Inicializar el manager de fases
pub fn init_graphics_phase_manager() {
    unsafe {
        GRAPHICS_PHASE_MANAGER = Some(GraphicsPhaseManager::new());
    }
}

/// Obtener el manager de fases
pub fn get_graphics_phase_manager() -> Option<&'static mut GraphicsPhaseManager> {
    unsafe { GRAPHICS_PHASE_MANAGER.as_mut() }
}
