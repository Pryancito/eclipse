#![allow(dead_code)]
//! Sistema de gráficos para Eclipse OS
//! 
//! Proporciona funciones de renderizado 2D y manejo de fuentes

use alloc::vec::Vec;
use alloc::string::String;

/// Contexto de gráficos
pub struct GraphicsContext {
    pub width: u32,
    pub height: u32,
    pub buffer: Vec<u32>, // Buffer de píxeles RGBA
    pub current_color: Color,
    pub background_color: Color,
    pub font: Font,
}

/// Color RGBA
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// Punto en 2D
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Tamaño en 2D
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Rectángulo
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Fuente del sistema
pub struct Font {
    pub name: String,
    pub size: u32,
    pub width: u32,
    pub height: u32,
    pub glyphs: Vec<Glyph>,
}

/// Glifo de fuente
pub struct Glyph {
    pub character: char,
    pub width: u32,
    pub height: u32,
    pub bitmap: Vec<u8>, // Datos del bitmap
    pub advance: u32,    // Avance horizontal
}

impl Color {
    /// Crear un color RGBA
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    
    /// Crear un color RGB (alpha = 255)
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    
    /// Convertir a valor u32 RGBA
    pub fn to_u32(&self) -> u32 {
        ((self.a as u32) << 24) | ((self.r as u32) << 16) | 
        ((self.g as u32) << 8) | (self.b as u32)
    }
    
    /// Crear desde valor u32 RGBA
    pub fn from_u32(value: u32) -> Self {
        Self {
            a: ((value >> 24) & 0xFF) as u8,
            r: ((value >> 16) & 0xFF) as u8,
            g: ((value >> 8) & 0xFF) as u8,
            b: (value & 0xFF) as u8,
        }
    }
    
    /// Mezclar con otro color
    pub fn blend(&self, other: &Color, alpha: f32) -> Color {
        let alpha = alpha.clamp(0.0, 1.0);
        let inv_alpha = 1.0 - alpha;
        
        Color {
            r: ((self.r as f32 * inv_alpha + other.r as f32 * alpha) as u8),
            g: ((self.g as f32 * inv_alpha + other.g as f32 * alpha) as u8),
            b: ((self.b as f32 * inv_alpha + other.b as f32 * alpha) as u8),
            a: ((self.a as f32 * inv_alpha + other.a as f32 * alpha) as u8),
        }
    }
}

impl GraphicsContext {
    /// Crear nuevo contexto de gráficos
    pub fn new(width: u32, height: u32) -> Self {
        let buffer_size = (width * height) as usize;
        let mut buffer = Vec::with_capacity(buffer_size);
        for _ in 0..buffer_size {
            buffer.push(0xFF000000); // Negro por defecto
        }
        
        Self {
            width,
            height,
            buffer,
            current_color: Color::rgb(255, 255, 255), // Blanco
            background_color: Color::rgb(0, 0, 0),    // Negro
            font: Font::default(),
        }
    }
    
    /// Establecer color actual
    pub fn set_color(&mut self, color: Color) {
        self.current_color = color;
    }
    
    /// Establecer color de fondo
    pub fn set_background(&mut self, color: Color) {
        self.background_color = color;
    }
    
    /// Limpiar el buffer con el color de fondo
    pub fn clear(&mut self) {
        let bg_color = self.background_color.to_u32();
        for pixel in &mut self.buffer {
            *pixel = bg_color;
        }
    }
    
    /// Dibujar un píxel
    pub fn draw_pixel(&mut self, x: i32, y: i32) {
        if x >= 0 && y >= 0 && x < self.width as i32 && y < self.height as i32 {
            let index = (y as u32 * self.width + x as u32) as usize;
            if index < self.buffer.len() {
                self.buffer[index] = self.current_color.to_u32();
            }
        }
    }
    
    /// Dibujar un píxel con color específico
    pub fn draw_pixel_color(&mut self, x: i32, y: i32, color: Color) {
        if x >= 0 && y >= 0 && x < self.width as i32 && y < self.height as i32 {
            let index = (y as u32 * self.width + x as u32) as usize;
            if index < self.buffer.len() {
                self.buffer[index] = color.to_u32();
            }
        }
    }
    
    /// Obtener un píxel
    pub fn get_pixel(&self, x: i32, y: i32) -> Color {
        if x >= 0 && y >= 0 && x < self.width as i32 && y < self.height as i32 {
            let index = (y as u32 * self.width + x as u32) as usize;
            if index < self.buffer.len() {
                Color::from_u32(self.buffer[index])
            } else {
                Color::rgb(0, 0, 0)
            }
        } else {
            Color::rgb(0, 0, 0)
        }
    }
    
    /// Dibujar una línea
    pub fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32) {
        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        let sx = if x1 < x2 { 1 } else { -1 };
        let sy = if y1 < y2 { 1 } else { -1 };
        let mut err = dx - dy;
        
        let mut x = x1;
        let mut y = y1;
        
        loop {
            self.draw_pixel(x, y);
            
            if x == x2 && y == y2 {
                break;
            }
            
            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }
    }
    
    /// Dibujar un rectángulo
    pub fn draw_rectangle(&mut self, rect: Rectangle) {
        let x1 = rect.x;
        let y1 = rect.y;
        let x2 = rect.x + rect.width as i32;
        let y2 = rect.y + rect.height as i32;
        
        // Líneas horizontales
        for x in x1..x2 {
            self.draw_pixel(x, y1);
            self.draw_pixel(x, y2 - 1);
        }
        
        // Líneas verticales
        for y in y1..y2 {
            self.draw_pixel(x1, y);
            self.draw_pixel(x2 - 1, y);
        }
    }
    
    /// Rellenar un rectángulo
    pub fn fill_rectangle(&mut self, rect: Rectangle) {
        for y in rect.y..rect.y + rect.height as i32 {
            for x in rect.x..rect.x + rect.width as i32 {
                self.draw_pixel(x, y);
            }
        }
    }
    
    /// Dibujar un círculo
    pub fn draw_circle(&mut self, center_x: i32, center_y: i32, radius: i32) {
        let mut x = 0;
        let mut y = radius;
        let mut d = 1 - radius;
        
        while x <= y {
            // Dibujar 8 puntos simétricos
            self.draw_pixel(center_x + x, center_y + y);
            self.draw_pixel(center_x - x, center_y + y);
            self.draw_pixel(center_x + x, center_y - y);
            self.draw_pixel(center_x - x, center_y - y);
            self.draw_pixel(center_x + y, center_y + x);
            self.draw_pixel(center_x - y, center_y + x);
            self.draw_pixel(center_x + y, center_y - x);
            self.draw_pixel(center_x - y, center_y - x);
            
            if d < 0 {
                d += 2 * x + 3;
            } else {
                d += 2 * (x - y) + 5;
                y -= 1;
            }
            x += 1;
        }
    }
    
    /// Rellenar un círculo
    pub fn fill_circle(&mut self, center_x: i32, center_y: i32, radius: i32) {
        for y in -radius..(radius + 1) {
            for x in -radius..(radius + 1) {
                if x * x + y * y <= radius * radius {
                    self.draw_pixel(center_x + x, center_y + y);
                }
            }
        }
    }
    
    /// Dibujar texto
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str) {
        let mut current_x = x;
        
        for ch in text.chars() {
            if let Some(glyph) = self.font.get_glyph(ch) {
                let advance = glyph.advance;
                let glyph_data = glyph.bitmap.clone();
                let glyph_width = glyph.width;
                let glyph_height = glyph.height;
                let glyph_x_offset = 0; // Offset X fijo
                let glyph_y_offset = 0; // Offset Y fijo
                
                // Dibujar el glifo manualmente
                for gy in 0..glyph_height {
                    for gx in 0..glyph_width {
                        let index = (gy * glyph_width + gx) as usize;
                        if index < glyph_data.len() && glyph_data[index] > 0 {
                            let pixel_x = current_x + glyph_x_offset + gx as i32;
                            let pixel_y = y + glyph_y_offset + gy as i32;
                            if pixel_x >= 0 && pixel_y >= 0 && pixel_x < self.width as i32 && pixel_y < self.height as i32 {
                                let buffer_index = (pixel_y as u32 * self.width + pixel_x as u32) as usize;
                                if buffer_index < self.buffer.len() {
                                    self.buffer[buffer_index] = 0xFFFFFFFF; // Blanco
                                }
                            }
                        }
                    }
                }
                current_x += advance as i32;
            }
        }
    }
    
    /// Dibujar un glifo
    fn draw_glyph(&mut self, x: i32, y: i32, glyph: &Glyph) {
        for gy in 0..glyph.height {
            for gx in 0..glyph.width {
                let index = (gy * glyph.width + gx) as usize;
                if index < glyph.bitmap.len() && glyph.bitmap[index] > 0 {
                    self.draw_pixel(x + gx as i32, y + gy as i32);
                }
            }
        }
    }
    
    /// Obtener el ancho del texto
    pub fn get_text_width(&self, text: &str) -> u32 {
        let mut width = 0;
        
        for ch in text.chars() {
            if let Some(glyph) = self.font.get_glyph(ch) {
                width += glyph.advance;
            }
        }
        
        width
    }
    
    /// Obtener la altura del texto
    pub fn get_text_height(&self) -> u32 {
        self.font.height
    }
    
    /// Copiar buffer a otro contexto
    pub fn blit(&mut self, src: &GraphicsContext, src_rect: Rectangle, dst_x: i32, dst_y: i32) {
        for sy in 0..src_rect.height {
            for sx in 0..src_rect.width {
                let src_x = src_rect.x + sx as i32;
                let src_y = src_rect.y + sy as i32;
                let dst_x = dst_x + sx as i32;
                let dst_y = dst_y + sy as i32;
                
                if src_x >= 0 && src_y >= 0 && 
                   src_x < src.width as i32 && src_y < src.height as i32 &&
                   dst_x >= 0 && dst_y >= 0 &&
                   dst_x < self.width as i32 && dst_y < self.height as i32 {
                    
                    let src_index = (src_y as u32 * src.width + src_x as u32) as usize;
                    if src_index < src.buffer.len() {
                        let color = Color::from_u32(src.buffer[src_index]);
                        self.draw_pixel_color(dst_x, dst_y, color);
                    }
                }
            }
        }
    }
}

impl Font {
    /// Crear fuente por defecto
    pub fn default() -> Self {
        Self {
            name: String::from("Default"),
            size: 12,
            width: 8,
            height: 16,
            glyphs: Vec::new(),
        }
    }
    
    /// Obtener glifo para un carácter
    pub fn get_glyph(&self, ch: char) -> Option<&Glyph> {
        self.glyphs.iter().find(|g| g.character == ch)
    }
    
    /// Establecer tamaño de fuente
    pub fn set_size(&mut self, size: u32) {
        self.size = size;
        self.width = size * 2 / 3;
        self.height = size;
    }
}

impl Default for Font {
    fn default() -> Self {
        Self::default()
    }
}

/// Instancia global del contexto de gráficos
static mut GRAPHICS_CONTEXT: Option<GraphicsContext> = None;

/// Inicializar el sistema de gráficos
pub fn init_graphics_system() -> Result<(), &'static str> {
    unsafe {
        if GRAPHICS_CONTEXT.is_some() {
            return Ok(());
        }
        
        // Crear contexto de gráficos por defecto
        let context = GraphicsContext::new(1024, 768);
        GRAPHICS_CONTEXT = Some(context);
    }
    
    Ok(())
}

/// Obtener el contexto de gráficos
pub fn get_graphics_context() -> Option<&'static mut GraphicsContext> {
    unsafe { GRAPHICS_CONTEXT.as_mut() }
}

/// Obtener información del sistema de gráficos
pub fn get_graphics_system_info() -> &'static str {
    "Sistema de Gráficos Eclipse OS v1.0 - 2D Rendering + Fonts"
}
