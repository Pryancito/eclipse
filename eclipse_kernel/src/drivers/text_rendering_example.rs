//! Ejemplo de uso del sistema de renderizado de texto moderno
//! 
//! Este archivo demuestra todas las capacidades del nuevo sistema de texto
//! inspirado en wgpu pero compatible con no_std.

use crate::drivers::framebuffer::{
    FramebufferDriver, Font, FontStyle, TextConfig, TextEffect, 
    TextAlign, VerticalAlign, Color
};

/// Demostrar todas las capacidades del sistema de texto
pub fn demonstrate_text_rendering() {
    // Crear framebuffer (esto normalmente se hace en la inicialización del kernel)
    let mut framebuffer = FramebufferDriver::new();
    
    // Simular inicialización con UEFI
    let _ = framebuffer.init_from_uefi(
        0x80000000,  // Dirección base del framebuffer
        1024,        // Ancho
        768,         // Alto
        1024,        // Píxeles por línea de escaneo
        1,           // Formato BGR
        0            // Bitmask
    );
    
    // Limpiar pantalla
    framebuffer.clear(Color::DARK_BLUE);
    
    // Ejemplo 1: Texto básico
    demonstrate_basic_text(&mut framebuffer);
    
    // Ejemplo 2: Diferentes fuentes y tamaños
    demonstrate_font_variations(&mut framebuffer);
    
    // Ejemplo 3: Efectos de texto
    demonstrate_text_effects(&mut framebuffer);
    
    // Ejemplo 4: Alineación de texto
    demonstrate_text_alignment(&mut framebuffer);
    
    // Ejemplo 5: Texto con gradientes
    demonstrate_gradient_text(&mut framebuffer);
    
    // Ejemplo 6: Texto como textura
    demonstrate_text_textures(&mut framebuffer);
    
    // Ejemplo 7: Texto multilínea
    demonstrate_multiline_text(&mut framebuffer);
}

/// Ejemplo 1: Texto básico
fn demonstrate_basic_text(framebuffer: &mut FramebufferDriver) {
    // Texto simple
    framebuffer.draw_text_simple(50, 50, "Hola Eclipse OS!", Color::WHITE);
    
    // Texto con configuración personalizada
    let font = Font::new("Custom".to_string(), 24, FontStyle::Bold);
    let config = TextConfig::new(font, Color::YELLOW);
    framebuffer.draw_text_advanced(50, 80, "Texto personalizado", &config);
}

/// Ejemplo 2: Diferentes fuentes y tamaños
fn demonstrate_font_variations(framebuffer: &mut FramebufferDriver) {
    let mut y = 120;
    
    // Diferentes tamaños
    for size in [12, 16, 20, 24, 32].iter() {
        let font = Font::new("System".to_string(), *size, FontStyle::Normal);
        let config = TextConfig::new(font, Color::LIGHT_GRAY);
        framebuffer.draw_text_advanced(50, y, &format!("Tamaño {}px", size), &config);
        y += *size + 10;
    }
    
    // Diferentes estilos
    y += 20;
    let styles = [
        (FontStyle::Normal, "Normal"),
        (FontStyle::Bold, "Bold"),
        (FontStyle::Italic, "Italic"),
        (FontStyle::BoldItalic, "Bold Italic"),
    ];
    
    for (style, name) in styles.iter() {
        let font = Font::new("System".to_string(), 18, *style);
        let config = TextConfig::new(font, Color::CYAN);
        framebuffer.draw_text_advanced(50, y, name, &config);
        y += 25;
    }
}

/// Ejemplo 3: Efectos de texto
fn demonstrate_text_effects(framebuffer: &mut FramebufferDriver) {
    let font = Font::new("System".to_string(), 20, FontStyle::Bold);
    let mut y = 300;
    
    // Texto normal
    let config = TextConfig::new(font.clone(), Color::WHITE);
    framebuffer.draw_text_advanced(50, y, "Texto normal", &config);
    y += 30;
    
    // Texto con sombra
    let shadow_config = TextConfig::new(font.clone(), Color::WHITE)
        .with_effect(TextEffect::Shadow {
            offset_x: 2,
            offset_y: 2,
            blur: 1,
            color: Color::BLACK,
        });
    framebuffer.draw_text_with_effect(50, y, "Texto con sombra", &shadow_config);
    y += 30;
    
    // Texto con contorno
    let outline_config = TextConfig::new(font.clone(), Color::YELLOW)
        .with_effect(TextEffect::Outline {
            width: 2,
            color: Color::BLACK,
        });
    framebuffer.draw_text_with_effect(50, y, "Texto con contorno", &outline_config);
    y += 30;
    
    // Texto con resplandor
    let glow_config = TextConfig::new(font.clone(), Color::MAGENTA)
        .with_effect(TextEffect::Glow {
            intensity: 0.8,
            color: Color::MAGENTA,
        });
    framebuffer.draw_text_with_effect(50, y, "Texto con resplandor", &glow_config);
    y += 30;
    
    // Texto con fondo
    let background_config = TextConfig::new(font.clone(), Color::WHITE)
        .with_background(Color::DARK_GRAY);
    framebuffer.draw_text_advanced(50, y, "Texto con fondo", &background_config);
}

/// Ejemplo 4: Alineación de texto
fn demonstrate_text_alignment(framebuffer: &mut FramebufferDriver) {
    let font = Font::new("System".to_string(), 18, FontStyle::Normal);
    let center_x = 512; // Centro de la pantalla
    let mut y = 500;
    
    // Texto centrado
    let center_config = TextConfig::new(font.clone(), Color::GREEN)
        .with_alignment(TextAlign::Center, VerticalAlign::Top);
    framebuffer.draw_text_centered(center_x, y, "Texto centrado", &center_config);
    y += 30;
    
    // Texto alineado a la derecha
    let right_config = TextConfig::new(font.clone(), Color::ORANGE)
        .with_alignment(TextAlign::Right, VerticalAlign::Top);
    framebuffer.draw_text_advanced(center_x, y, "Texto a la derecha", &right_config);
    y += 30;
    
    // Texto alineado a la izquierda
    let left_config = TextConfig::new(font.clone(), Color::PINK)
        .with_alignment(TextAlign::Left, VerticalAlign::Top);
    framebuffer.draw_text_advanced(center_x - 200, y, "Texto a la izquierda", &left_config);
}

/// Ejemplo 5: Texto con gradientes
fn demonstrate_gradient_text(framebuffer: &mut FramebufferDriver) {
    let font = Font::new("System".to_string(), 24, FontStyle::Bold);
    let gradient_config = TextConfig::new(font, Color::WHITE)
        .with_effect(TextEffect::Gradient {
            start_color: Color::RED,
            end_color: Color::BLUE,
        });
    
    framebuffer.draw_text_with_effect(50, 600, "TEXTO CON GRADIENTE", &gradient_config);
}

/// Ejemplo 6: Texto como textura
fn demonstrate_text_textures(framebuffer: &mut FramebufferDriver) {
    let font = Font::new("System".to_string(), 20, FontStyle::Bold);
    let config = TextConfig::new(font, Color::YELLOW);
    
    // Crear textura de texto
    let text_texture = framebuffer.create_text_texture("TEXTO COMO TEXTURA", &config);
    
    // Dibujar la textura en diferentes posiciones con diferentes efectos
    framebuffer.draw_texture(&text_texture, 300, 100, BlendMode::None, 1.0);
    framebuffer.draw_texture(&text_texture, 300, 130, BlendMode::Alpha, 0.7);
    framebuffer.draw_texture(&text_texture, 300, 160, BlendMode::Additive, 0.5);
}

/// Ejemplo 7: Texto multilínea
fn demonstrate_multiline_text(framebuffer: &mut FramebufferDriver) {
    let font = Font::new("System".to_string(), 16, FontStyle::Normal);
    let config = TextConfig::new(font, Color::WHITE)
        .with_alignment(TextAlign::Left, VerticalAlign::Top);
    
    let multiline_text = "Este es un ejemplo de texto\nque se extiende por múltiples\nlíneas para demostrar\nel sistema de layout.";
    
    framebuffer.draw_multiline_text(50, 400, multiline_text, &config);
}

/// Ejemplo avanzado: Interfaz de usuario con texto
pub fn demonstrate_text_ui(framebuffer: &mut FramebufferDriver) {
    // Limpiar pantalla
    framebuffer.clear(Color::DARK_BLUE);
    
    // Título principal
    let title_font = Font::new("System".to_string(), 32, FontStyle::Bold);
    let title_config = TextConfig::new(title_font, Color::WHITE)
        .with_effect(TextEffect::Shadow {
            offset_x: 3,
            offset_y: 3,
            blur: 2,
            color: Color::BLACK,
        });
    framebuffer.draw_text_centered(512, 50, "ECLIPSE OS", &title_config);
    
    // Subtítulo
    let subtitle_font = Font::new("System".to_string(), 18, FontStyle::Italic);
    let subtitle_config = TextConfig::new(subtitle_font, Color::LIGHT_GRAY);
    framebuffer.draw_text_centered(512, 90, "Sistema Operativo Moderno", &subtitle_config);
    
    // Menú de opciones
    let menu_font = Font::new("System".to_string(), 16, FontStyle::Normal);
    let menu_items = [
        "1. Iniciar sistema",
        "2. Configuración",
        "3. Diagnósticos",
        "4. Apagar",
    ];
    
    let mut y = 150;
    for (i, item) in menu_items.iter().enumerate() {
        let color = if i == 0 { Color::YELLOW } else { Color::WHITE };
        let config = TextConfig::new(menu_font.clone(), color);
        framebuffer.draw_text_advanced(100, y, item, &config);
        y += 30;
    }
    
    // Información del sistema
    let info_font = Font::new("System".to_string(), 14, FontStyle::Normal);
    let info_config = TextConfig::new(info_font, Color::GREEN);
    
    let system_info = "Sistema: Eclipse OS v0.5.0\n\
                      Kernel: Rust bare metal\n\
                      Framebuffer: Moderno con wgpu-like API\n\
                      Estado: Funcionando correctamente";
    
    framebuffer.draw_multiline_text(100, 300, system_info, &info_config);
    
    // Barra de estado
    let status_font = Font::new("System".to_string(), 12, FontStyle::Bold);
    let status_config = TextConfig::new(status_font, Color::CYAN)
        .with_background(Color::DARK_GRAY);
    
    framebuffer.draw_text_advanced(50, 700, "Estado: Listo | Memoria: 8GB | CPU: 100%", &status_config);
}

/// Ejemplo de animación de texto
pub fn demonstrate_text_animation(framebuffer: &mut FramebufferDriver) {
    let font = Font::new("System".to_string(), 24, FontStyle::Bold);
    
    for frame in 0..60 {
        // Limpiar pantalla
        framebuffer.clear(Color::BLACK);
        
        // Calcular posición animada
        let x = 100 + (frame * 5) as i32;
        let y = 200 + ((frame as f32 * 0.1).sin() * 50.0) as i32;
        
        // Crear efecto de color animado
        let hue = (frame * 6) % 360;
        let color = Color::from_hsv(hue, 255, 255);
        
        let config = TextConfig::new(font.clone(), color)
            .with_effect(TextEffect::Glow {
                intensity: 0.5 + (frame as f32 * 0.01),
                color: color,
            });
        
        framebuffer.draw_text_with_effect(x, y, "TEXTO ANIMADO", &config);
        
        // En un sistema real, aquí habría una pausa o sincronización
    }
}

/// Utilidades para crear efectos de texto avanzados
pub struct TextEffects;

impl TextEffects {
    /// Crear efecto de texto tipo neón
    pub fn neon_effect(font: Font, text_color: Color, glow_color: Color) -> TextConfig {
        TextConfig::new(font, text_color)
            .with_effect(TextEffect::Glow {
                intensity: 0.8,
                color: glow_color,
            })
    }
    
    /// Crear efecto de texto tipo 3D
    pub fn three_d_effect(font: Font, text_color: Color, shadow_color: Color) -> TextConfig {
        TextConfig::new(font, text_color)
            .with_effect(TextEffect::Shadow {
                offset_x: 3,
                offset_y: 3,
                blur: 2,
                color: shadow_color,
            })
    }
    
    /// Crear efecto de texto tipo arcoíris
    pub fn rainbow_effect(font: Font) -> TextConfig {
        TextConfig::new(font, Color::WHITE)
            .with_effect(TextEffect::Gradient {
                start_color: Color::RED,
                end_color: Color::PURPLE,
            })
    }
}
