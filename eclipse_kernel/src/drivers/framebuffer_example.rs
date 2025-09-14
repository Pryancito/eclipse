//! Ejemplo de uso del framebuffer moderno con API similar a wgpu
//! 
//! Este archivo demuestra cómo usar las nuevas características del framebuffer
//! que proporcionan una API moderna similar a wgpu pero compatible con no_std.

use crate::drivers::framebuffer::{
    FramebufferDriver, ModernRenderPipeline, CompositingLayer, Texture, 
    TextureFormat, BlendMode, Color, ModernGraphicsUtils
};

/// Ejemplo de uso del pipeline moderno
pub fn demonstrate_modern_framebuffer() {
    // Crear framebuffer (esto normalmente se hace en la inicialización del kernel)
    let mut framebuffer = FramebufferDriver::new();
    
    // Simular inicialización con UEFI (en un caso real, estos valores vienen del bootloader)
    let _ = framebuffer.init_from_uefi(
        0x80000000,  // Dirección base del framebuffer
        1024,        // Ancho
        768,         // Alto
        1024,        // Píxeles por línea de escaneo
        1,           // Formato BGR
        0            // Bitmask
    );
    
    // Crear pipeline moderno
    let mut pipeline = framebuffer.create_modern_pipeline();
    
    // Ejemplo 1: Crear gradientes
    let gradient_texture = ModernGraphicsUtils::create_linear_gradient(
        200, 100, 
        Color::BLUE, 
        Color::CYAN
    );
    
    // Ejemplo 2: Crear gradiente radial
    let radial_texture = ModernGraphicsUtils::create_radial_gradient(
        150, 150,
        75, 75,  // Centro
        75,      // Radio
        Color::RED,
        Color::YELLOW
    );
    
    // Ejemplo 3: Crear textura con ruido
    let noise_texture = ModernGraphicsUtils::create_noise_texture(
        100, 100, 0.3  // Intensidad del ruido
    );
    
    // Crear capas de compositing
    let background_layer = CompositingLayer::new(gradient_texture, 0, 0);
    let foreground_layer = CompositingLayer {
        texture: radial_texture,
        position: (100, 100),
        blend_mode: BlendMode::Alpha,
        alpha: 0.7,
        visible: true,
    };
    
    let noise_layer = CompositingLayer {
        texture: noise_texture,
        position: (300, 200),
        blend_mode: BlendMode::Multiply,
        alpha: 0.5,
        visible: true,
    };
    
    // Agregar capas al pipeline
    pipeline.add_layer(background_layer);
    pipeline.add_layer(foreground_layer);
    pipeline.add_layer(noise_layer);
    
    // Configurar color de fondo
    pipeline.set_clear_color(Color::DARK_BLUE);
    
    // Habilitar aceleración por hardware si está disponible
    pipeline.enable_hardware_acceleration(true);
    
    // Renderizar todo al framebuffer
    framebuffer.render_with_pipeline(&pipeline);
    
    // Ejemplo adicional: Dibujar texturas individuales con diferentes modos de blending
    let simple_texture = create_simple_texture();
    
    // Dibujar con blending aditivo
    framebuffer.draw_texture(&simple_texture, 400, 50, BlendMode::Additive, 1.0);
    
    // Dibujar con blending multiplicativo
    framebuffer.draw_texture(&simple_texture, 450, 50, BlendMode::Multiply, 0.8);
    
    // Dibujar con blending de pantalla
    framebuffer.draw_texture(&simple_texture, 500, 50, BlendMode::Screen, 0.6);
    
    // Ejemplo de captura de región del framebuffer
    let captured_texture = framebuffer.create_texture_from_region(0, 0, 100, 100);
    
    // Dibujar la textura capturada en otra posición
    framebuffer.draw_texture(&captured_texture, 600, 100, BlendMode::None, 1.0);
}

/// Crear una textura simple con un patrón
fn create_simple_texture() -> Texture {
    let mut texture = Texture::new(50, 50, TextureFormat::RGBA8);
    
    for y in 0..50 {
        for x in 0..50 {
            let color = if (x + y) % 10 == 0 {
                Color::WHITE
            } else if (x + y) % 5 == 0 {
                Color::GRAY
            } else {
                Color::TRANSPARENT
            };
            
            texture.set_pixel(x, y, color);
        }
    }
    
    texture
}

/// Ejemplo de animación simple usando el pipeline moderno
pub fn demonstrate_animation(framebuffer: &mut FramebufferDriver) {
    let mut pipeline = framebuffer.create_modern_pipeline();
    
    // Crear textura animada
    let mut animated_texture = Texture::new(100, 100, TextureFormat::RGBA8);
    
    // Simular animación de frame
    for frame in 0..10 {
        // Limpiar textura
        for y in 0..100 {
            for x in 0..100 {
                animated_texture.set_pixel(x, y, Color::TRANSPARENT);
            }
        }
        
        // Dibujar círculo animado
        let center_x = 50 + (frame * 3) as u32;
        let center_y = 50;
        let radius = 20 + frame as u32;
        
        for y in 0..100 {
            for x in 0..100 {
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance = ((dx * dx + dy * dy) as f32).sqrt() as u32;
                
                if distance <= radius {
                    let alpha = (255 - (distance * 255 / radius)) as u8;
                    let color = Color::rgba(255, 0, 255, alpha); // Magenta con alpha
                    animated_texture.set_pixel(x, y, color);
                }
            }
        }
        
        // Crear capa animada
        let animated_layer = CompositingLayer {
            texture: animated_texture.clone(),
            position: (200 + frame as i32 * 10, 300),
            blend_mode: BlendMode::Alpha,
            alpha: 0.8,
            visible: true,
        };
        
        pipeline.add_layer(animated_layer);
        
        // Renderizar frame
        framebuffer.render_with_pipeline(&pipeline);
        
        // Limpiar pipeline para el siguiente frame
        pipeline.remove_layer(0);
        
        // En un sistema real, aquí habría una pausa o sincronización con el refresh rate
    }
}

/// Ejemplo de efectos visuales avanzados
pub fn demonstrate_visual_effects(framebuffer: &mut FramebufferDriver) {
    let mut pipeline = framebuffer.create_modern_pipeline();
    
    // Efecto 1: Gradiente de fondo con múltiples colores
    let bg_gradient = create_multicolor_gradient();
    let bg_layer = CompositingLayer::new(bg_gradient, 0, 0);
    pipeline.add_layer(bg_layer);
    
    // Efecto 2: Partículas con blending aditivo
    for i in 0..20 {
        let particle_texture = create_particle_texture();
        let particle_layer = CompositingLayer {
            texture: particle_texture,
            position: (i as i32 * 50, i as i32 * 30),
            blend_mode: BlendMode::Additive,
            alpha: 0.6,
            visible: true,
        };
        pipeline.add_layer(particle_layer);
    }
    
    // Efecto 3: Overlay con blending multiplicativo
    let overlay_texture = create_overlay_pattern();
    let overlay_layer = CompositingLayer {
        texture: overlay_texture,
        position: (0, 0),
        blend_mode: BlendMode::Multiply,
        alpha: 0.3,
        visible: true,
    };
    pipeline.add_layer(overlay_layer);
    
    // Renderizar efectos
    framebuffer.render_with_pipeline(&pipeline);
}

fn create_multicolor_gradient() -> Texture {
    let mut texture = Texture::new(800, 600, TextureFormat::RGBA8);
    
    for y in 0..600 {
        for x in 0..800 {
            let r = (x * 255 / 800) as u8;
            let g = (y * 255 / 600) as u8;
            let b = ((x + y) * 255 / (800 + 600)) as u8;
            let color = Color::rgb(r, g, b);
            texture.set_pixel(x, y, color);
        }
    }
    
    texture
}

fn create_particle_texture() -> Texture {
    let mut texture = Texture::new(20, 20, TextureFormat::RGBA8);
    
    for y in 0..20 {
        for x in 0..20 {
            let dx = x as i32 - 10;
            let dy = y as i32 - 10;
            let distance = ((dx * dx + dy * dy) as f32).sqrt();
            
            if distance <= 10.0 {
                let alpha = ((10.0 - distance) * 25.5) as u8;
                let color = Color::rgba(255, 255, 0, alpha); // Amarillo brillante
                texture.set_pixel(x, y, color);
            }
        }
    }
    
    texture
}

fn create_overlay_pattern() -> Texture {
    let mut texture = Texture::new(800, 600, TextureFormat::RGBA8);
    
    for y in 0..600 {
        for x in 0..800 {
            let pattern = ((x / 10) + (y / 10)) % 2;
            let color = if pattern == 0 {
                Color::rgba(255, 255, 255, 128) // Blanco semitransparente
            } else {
                Color::rgba(200, 200, 200, 128) // Gris semitransparente
            };
            texture.set_pixel(x, y, color);
        }
    }
    
    texture
}
