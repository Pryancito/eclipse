//! Sistema de gráficos modular para Eclipse OS
//!
//! Arquitectura extendida de fases:
//! 1. UEFI/GOP para bootloader
//! 2. UEFI/GOP para kernel en detección de gráficos
//! 3. DRM/FB/GOP para kernel posterior
//! 4. Multi-GPU avanzado con drivers específicos
//! 5. Sistema de ventanas y compositor
//! 6. Sistema de widgets y UI completa

pub mod drm_graphics;
pub mod phases;
pub mod transition;
pub mod uefi_graphics;

// Módulos avanzados
pub mod amd_advanced;
pub mod intel_advanced;
pub mod nvidia_advanced;
pub mod multi_gpu_manager;
pub mod graphics_manager;
pub mod window_system;
pub mod widgets;
pub mod real_graphics_manager;

// Ejemplos de integración
pub mod examples;

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

/// Transicionar a fase avanzada de Multi-GPU
pub fn transition_to_advanced_multi_gpu() -> Result<(), &'static str> {
    if let Some(manager) = get_graphics_phase_manager() {
        manager.init_advanced_multi_gpu()?;

        // Inicializar el gestor de multi-GPU
        init_multi_gpu_system()?;

        Ok(())
    } else {
        Err("Manager de fases no inicializado")
    }
}

/// Transicionar a fase de sistema de ventanas
pub fn transition_to_window_system() -> Result<(), &'static str> {
    if let Some(manager) = get_graphics_phase_manager() {
        manager.init_window_system()?;

        // Inicializar el sistema de ventanas
        init_window_compositor()?;

        Ok(())
    } else {
        Err("Manager de fases no inicializado")
    }
}

/// Transicionar a fase de sistema de widgets
pub fn transition_to_widget_system() -> Result<(), &'static str> {
    if let Some(manager) = get_graphics_phase_manager() {
        manager.init_widget_system()?;

        // Inicializar el sistema de widgets
        init_widget_manager()?;

        Ok(())
    } else {
        Err("Manager de fases no inicializado")
    }
}

/// Inicializar el sistema completo de gráficos con todas las fases
pub fn init_full_graphics_system(
    framebuffer_info: crate::drivers::framebuffer::FramebufferInfo,
) -> Result<(), &'static str> {
    // Fase 1 y 2: UEFI (ya inicializadas por init_graphics_system)
    
    // Fase 3: DRM Runtime
    transition_to_drm(framebuffer_info)?;
    
    // Fase 4: Multi-GPU avanzado
    if let Err(_) = transition_to_advanced_multi_gpu() {
        // Si falla, continuar sin multi-GPU avanzado
    }
    
    // Fase 5: Sistema de ventanas
    if let Err(_) = transition_to_window_system() {
        // Si falla, continuar sin sistema de ventanas
    }
    
    // Fase 6: Sistema de widgets
    if let Err(_) = transition_to_widget_system() {
        // Si falla, continuar sin widgets
    }
    
    Ok(())
}

/// Inicializar el sistema Multi-GPU
fn init_multi_gpu_system() -> Result<(), &'static str> {
    // Inicializar el gestor de múltiples GPUs
    // Esto detectará y configurará drivers específicos para NVIDIA, AMD e Intel
    Ok(())
}

/// Inicializar el compositor de ventanas
fn init_window_compositor() -> Result<(), &'static str> {
    // Inicializar el sistema de ventanas y compositor
    Ok(())
}

/// Inicializar el gestor de widgets
fn init_widget_manager() -> Result<(), &'static str> {
    // Inicializar el sistema de widgets para la UI
    Ok(())
}

/// Verificar si podemos usar el sistema avanzado de multi-GPU
pub fn can_use_advanced_multi_gpu() -> bool {
    phases::get_graphics_phase_manager()
        .map(|manager| manager.can_use_advanced_multi_gpu())
        .unwrap_or(false)
}

/// Verificar si podemos usar el sistema de ventanas
pub fn can_use_window_system() -> bool {
    phases::get_graphics_phase_manager()
        .map(|manager| manager.can_use_window_system())
        .unwrap_or(false)
}

/// Verificar si podemos usar el sistema de widgets
pub fn can_use_widget_system() -> bool {
    phases::get_graphics_phase_manager()
        .map(|manager| manager.can_use_widget_system())
        .unwrap_or(false)
}
