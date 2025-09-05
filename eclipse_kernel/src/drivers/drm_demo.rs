//! Demostración del driver DRM para Eclipse OS
//! 
//! Muestra cómo usar el driver DRM para crear gráficos avanzados

use crate::drivers::drm::*;

/// Demostrar capacidades del driver DRM
pub fn demo_drm_capabilities() {
    // Inicializar driver DRM
    if let Err(_) = init_drm_driver() {
        return; // DRM no disponible
    }

    // Obtener modos de video disponibles
    if let Ok(modes) = get_available_video_modes() {
        // Usar el primer modo disponible
        if let Some(mode) = modes.first() {
            if let Err(_) = set_video_mode(mode) {
                return;
            }
        }
    }

    // Crear framebuffer
    if let Ok(fb_info) = create_framebuffer(1024, 768, 32) {
        // Limpiar pantalla con color azul
        let _ = clear_framebuffer(0x0000FF); // Azul

        // Dibujar algunos rectángulos de colores
        let _ = draw_rectangle(100, 100, 200, 150, 0xFF0000); // Rojo
        let _ = draw_rectangle(350, 100, 200, 150, 0x00FF00); // Verde
        let _ = draw_rectangle(600, 100, 200, 150, 0xFFFF00); // Amarillo

        // Dibujar algunos pixels individuales
        for i in 0..50 {
            let _ = draw_pixel(500 + i, 300 + i, 0xFF00FF); // Magenta
        }
    }

    // Obtener información del GPU
    if let Ok(gpu_info) = get_gpu_info() {
        // La información del GPU estaría disponible para mostrar
        // En un sistema real, esto se mostraría en pantalla
    }
}

/// Demostrar animación simple con DRM
pub fn demo_drm_animation() {
    if !is_drm_available() {
        return;
    }

    // Crear framebuffer para animación
    if let Ok(_) = create_framebuffer(800, 600, 32) {
        // Limpiar pantalla
        let _ = clear_framebuffer(0x000000); // Negro

        // Animación simple: rectángulo que se mueve
        for frame in 0..100 {
            // Limpiar frame anterior
            let _ = clear_framebuffer(0x000000);

            // Calcular posición del rectángulo
            let x = (frame * 4) % 700;
            let y = 250 + ((frame as f32 * 0.1).sin() * 50.0) as u32;

            // Dibujar rectángulo animado
            let _ = draw_rectangle(x, y, 100, 100, 0x00FFFF); // Cian

            // En un sistema real, aquí habría un delay para controlar FPS
            // y se actualizaría la pantalla
        }
    }
}

/// Demostrar efectos gráficos con DRM
pub fn demo_drm_effects() {
    if !is_drm_available() {
        return;
    }

    if let Ok(_) = create_framebuffer(1024, 768, 32) {
        // Efecto de gradiente
        for y in 0..768 {
            let color = ((y as f32 / 768.0) * 255.0) as u32;
            let gradient_color = (color << 16) | (color << 8) | color; // Escala de grises
            let _ = draw_rectangle(0, y, 1024, 1, gradient_color);
        }

        // Efecto de ondas
        for x in 0..1024 {
            for y in 0..768 {
                let wave = ((x as f32 * 0.02).sin() + (y as f32 * 0.02).sin()) * 0.5 + 0.5;
                let intensity = (wave * 255.0) as u32;
                let color = (intensity << 16) | (intensity << 8) | intensity;
                let _ = draw_pixel(x, y, color);
            }
        }
    }
}



