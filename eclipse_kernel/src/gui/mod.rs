//! Sistema Gráfico ReactOS Rust
//! 
//! Implementa un sistema de ventanas básico con soporte para 
//! framebuffer, eventos y gestión de ventanas.

pub mod framebuffer;
pub mod window;
pub mod event;
pub mod compositor;
pub mod font;
pub mod nvidia;
pub mod nvidia_control;
pub mod nvidia_benchmark;

// use crate::drivers; // Comentado temporalmente

/// Inicializar el sistema gráfico
pub fn init() {
    // Inicializar el framebuffer
    framebuffer::init_framebuffer();
    
    // Inicializar el gestor de ventanas
    window::init_window_manager();
    
    // Inicializar el compositor
    compositor::init_compositor();
    
    // Inicializar el sistema de eventos
    event::init_event_system();
    
    // Inicializar el renderizador de fuentes
    font::init_font_renderer();
    
    // Inicializar driver NVIDIA si está disponible
    if let Err(_) = nvidia::init_nvidia_driver() {
        // Si no hay GPU NVIDIA, continuar sin aceleración
    }
    
    // Inicializar panel de control NVIDIA
    nvidia_control::init_nvidia_control_panel();
    
    // Inicializar benchmark NVIDIA
    nvidia_benchmark::init_nvidia_benchmark();
}

/// Procesar eventos del sistema gráfico
pub fn process_events() {
    event::process_pending_events();
}

/// Actualizar la pantalla
pub fn update_display() {
    // Intentar usar aceleración NVIDIA si está disponible
    if nvidia::is_nvidia_gpu_available() {
        // Obtener framebuffer y renderizar con NVIDIA
        if let Some(framebuffer) = framebuffer::get_framebuffer() {
            let fb_bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    framebuffer.buffer,
                    framebuffer.size
                )
            };
            
            if let Err(_) = nvidia::render_frame_nvidia(fb_bytes) {
                // Fallback al compositor normal si falla NVIDIA
                compositor::render_frame();
            }
        } else {
            compositor::render_frame();
        }
        
        // Actualizar estadísticas NVIDIA
        nvidia::update_nvidia_stats();
    } else {
        // Usar compositor normal
        compositor::render_frame();
    }
}
