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

use phases::{init_graphics_phase_manager, with_graphics_phase_manager, GraphicsPhase, GraphicsPhaseManager};
use multi_gpu_manager::MultiGpuManager;
use window_system::WindowCompositor;
use widgets::WidgetManager;
use spin::Mutex;

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
    with_graphics_phase_manager(|manager| manager.get_state().current_phase)
}

/// Verificar si podemos usar DRM
pub fn can_use_drm() -> bool {
    with_graphics_phase_manager(|manager| manager.can_use_drm()).unwrap_or(false)
}

/// Verificar si debemos usar UEFI
pub fn should_use_uefi() -> bool {
    with_graphics_phase_manager(|manager| manager.should_use_uefi()).unwrap_or(true)
}

/// Obtener el manager de fases (obsoleto - usar with_graphics_phase_manager)
#[deprecated(note = "Use with_graphics_phase_manager for thread-safe access")]
pub fn get_graphics_phase_manager() -> Option<&'static spin::Mutex<Option<GraphicsPhaseManager>>> {
    phases::get_graphics_phase_manager()
}

/// Transicionar a fase DRM
pub fn transition_to_drm(
    framebuffer_info: crate::drivers::framebuffer::FramebufferInfo,
) -> Result<(), &'static str> {
    // Transicionar usando el helper thread-safe
    with_graphics_phase_manager(|manager| {
        manager.init_drm_runtime(framebuffer_info)?;
        
        // Inicializar DRM después de la transición
        drm_graphics::init_drm_graphics()?;
        
        Ok(())
    })
    .ok_or("Manager de fases no inicializado")?
}

/// Transicionar a fase avanzada de Multi-GPU
pub fn transition_to_advanced_multi_gpu() -> Result<(), &'static str> {
    with_graphics_phase_manager(|manager| {
        manager.init_advanced_multi_gpu()?;

        // Inicializar el gestor de multi-GPU
        init_multi_gpu_system()?;

        Ok(())
    })
    .ok_or("Manager de fases no inicializado")?
}

/// Transicionar a fase de sistema de ventanas
pub fn transition_to_window_system() -> Result<(), &'static str> {
    with_graphics_phase_manager(|manager| {
        manager.init_window_system()?;

        // Inicializar el sistema de ventanas
        init_window_compositor()?;

        Ok(())
    })
    .ok_or("Manager de fases no inicializado")?
}

/// Transicionar a fase de sistema de widgets
pub fn transition_to_widget_system() -> Result<(), &'static str> {
    with_graphics_phase_manager(|manager| {
        manager.init_widget_system()?;

        // Inicializar el sistema de widgets
        init_widget_manager()?;

        Ok(())
    })
    .ok_or("Manager de fases no inicializado")?
}

/// Inicializar el sistema completo de gráficos con todas las fases
pub fn init_full_graphics_system(
    framebuffer_info: crate::drivers::framebuffer::FramebufferInfo,
) -> Result<(), &'static str> {
    // Fase 1 y 2: UEFI (ya inicializadas por init_graphics_system)
    
    // Fase 3: DRM Runtime
    transition_to_drm(framebuffer_info)?;
    
    // Fase 4: Multi-GPU avanzado (opcional, continuar si falla)
    match transition_to_advanced_multi_gpu() {
        Ok(_) => {
            // Multi-GPU inicializado exitosamente
        }
        Err(e) => {
            // Log: Multi-GPU no disponible, continuando sin él
            // En una implementación real, usar logging aquí
        }
    }
    
    // Fase 5: Sistema de ventanas (opcional, continuar si falla)
    match transition_to_window_system() {
        Ok(_) => {
            // Window System inicializado exitosamente
        }
        Err(e) => {
            // Log: Window System no disponible, continuando sin él
        }
    }
    
    // Fase 6: Sistema de widgets (opcional, continuar si falla)
    match transition_to_widget_system() {
        Ok(_) => {
            // Widget System inicializado exitosamente
        }
        Err(e) => {
            // Log: Widget System no disponible, continuando sin él
        }
    }
    
    Ok(())
}

/// Gestor global de Multi-GPU
static MULTI_GPU_MANAGER: Mutex<Option<MultiGpuManager>> = Mutex::new(None);

/// Gestor global de sistema de ventanas
static WINDOW_COMPOSITOR: Mutex<Option<WindowCompositor>> = Mutex::new(None);

/// Gestor global de widgets
static WIDGET_MANAGER: Mutex<Option<WidgetManager>> = Mutex::new(None);

/// Inicializar el sistema Multi-GPU
fn init_multi_gpu_system() -> Result<(), &'static str> {
    // Crear nuevo gestor de múltiples GPUs
    let mut manager = MultiGpuManager::new();
    
    // Intentar inicializar todos los drivers
    // No es crítico si falla, ya que puede no haber GPUs soportadas
    match manager.initialize_all_drivers() {
        Ok(_) => {
            // Éxito: al menos un driver fue inicializado
        }
        Err(e) => {
            // No hay GPUs soportadas, pero no es un error crítico
            // El sistema puede continuar sin Multi-GPU avanzado
        }
    }
    
    // Guardar el gestor globalmente
    *MULTI_GPU_MANAGER.lock() = Some(manager);
    
    Ok(())
}

/// Inicializar el compositor de ventanas
fn init_window_compositor() -> Result<(), &'static str> {
    // Crear nuevo compositor de ventanas
    let compositor = WindowCompositor::new();
    
    // Guardar el compositor globalmente
    *WINDOW_COMPOSITOR.lock() = Some(compositor);
    
    Ok(())
}

/// Inicializar el gestor de widgets
fn init_widget_manager() -> Result<(), &'static str> {
    // Crear nuevo gestor de widgets
    let manager = WidgetManager::new();
    
    // Guardar el gestor globalmente
    *WIDGET_MANAGER.lock() = Some(manager);
    
    Ok(())
}

/// Verificar si podemos usar el sistema avanzado de multi-GPU
pub fn can_use_advanced_multi_gpu() -> bool {
    with_graphics_phase_manager(|manager| manager.can_use_advanced_multi_gpu()).unwrap_or(false)
}

/// Verificar si podemos usar el sistema de ventanas
pub fn can_use_window_system() -> bool {
    with_graphics_phase_manager(|manager| manager.can_use_window_system()).unwrap_or(false)
}

/// Verificar si podemos usar el sistema de widgets
pub fn can_use_widget_system() -> bool {
    with_graphics_phase_manager(|manager| manager.can_use_widget_system()).unwrap_or(false)
}

/// Obtener acceso al gestor Multi-GPU
/// Ejecuta una función con acceso mutable al gestor
pub fn with_multi_gpu_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut MultiGpuManager) -> R,
{
    let mut manager = MULTI_GPU_MANAGER.lock();
    if let Some(mgr) = manager.as_mut() {
        Some(f(mgr))
    } else {
        None
    }
}

/// Obtener acceso al compositor de ventanas
/// Ejecuta una función con acceso mutable al compositor
pub fn with_window_compositor<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut WindowCompositor) -> R,
{
    let mut compositor = WINDOW_COMPOSITOR.lock();
    if let Some(comp) = compositor.as_mut() {
        Some(f(comp))
    } else {
        None
    }
}

/// Obtener acceso al gestor de widgets
/// Ejecuta una función con acceso mutable al gestor
pub fn with_widget_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut WidgetManager) -> R,
{
    let mut manager = WIDGET_MANAGER.lock();
    if let Some(mgr) = manager.as_mut() {
        Some(f(mgr))
    } else {
        None
    }
}
