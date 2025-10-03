//! Sistema de gráficos modular para Eclipse OS
//!
//! Arquitectura de 3 fases:
//! 1. UEFI/GOP para bootloader
//! 2. UEFI/GOP para kernel en detección de gráficos
//! 3. DRM/FB/GOP para kernel posterior

pub mod drm_graphics;
pub mod phases;
pub mod transition;
pub mod uefi_graphics;

use phases::{init_graphics_phase_manager, GraphicsPhase, GraphicsPhaseManager};

/// Inicializar el sistema de gráficos
pub fn init_graphics_system() -> Result<(), &'static str> {
    // Inicializar manager de fases
    init_graphics_phase_manager();

    // Inicializar gráficos UEFI para detección
    uefi_graphics::init_uefi_graphics()?;

    Ok(())
}

/// Obtener la fase actual de gráficos
pub fn get_current_graphics_phase() -> Option<GraphicsPhase> {
    phases::get_graphics_phase_manager().map(|manager| manager.get_state().current_phase)
}

/// Verificar si podemos usar DRM
pub fn can_use_drm() -> bool {
    phases::get_graphics_phase_manager()
        .map(|manager| manager.can_use_drm())
        .unwrap_or(false)
}

/// Verificar si debemos usar UEFI
pub fn should_use_uefi() -> bool {
    phases::get_graphics_phase_manager()
        .map(|manager| manager.should_use_uefi())
        .unwrap_or(true)
}

/// Obtener el manager de fases
pub fn get_graphics_phase_manager() -> Option<&'static mut GraphicsPhaseManager> {
    phases::get_graphics_phase_manager()
}

/// Transicionar a fase DRM
pub fn transition_to_drm(
    framebuffer_info: crate::drivers::framebuffer::FramebufferInfo,
) -> Result<(), &'static str> {
    if let Some(manager) = get_graphics_phase_manager() {
        manager.init_drm_runtime(framebuffer_info)?;

        // Inicializar DRM después de la transición
        drm_graphics::init_drm_graphics()?;

        Ok(())
    } else {
        Err("Manager de fases no inicializado")
    }
}
