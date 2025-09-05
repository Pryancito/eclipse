//! Sistema de renderizado de fuentes
//! 
//! Maneja el renderizado de texto en el framebuffer

use crate::gui::framebuffer::{Framebuffer, Color, Point};

/// Información de una fuente
#[derive(Debug, Clone, Copy)]
pub struct FontInfo {
    pub width: u8,
    pub height: u8,
    pub baseline: u8,
    pub line_spacing: u8,
}

impl FontInfo {
    pub fn default_8x16() -> Self {
        Self {
            width: 8,
            height: 16,
            baseline: 12,
            line_spacing: 16,
        }
    }
    
    pub fn small_6x12() -> Self {
        Self {
            width: 6,
            height: 12,
            baseline: 9,
            line_spacing: 12,
        }
    }
}

/// Fuente bitmap básica 8x16
/// Cada caracter se representa como un array de 16 bytes (una fila por byte)
pub struct BitmapFont {
    pub info: FontInfo,
    pub glyphs: &'static [u8],
}

impl BitmapFont {
    /// Obtener la fuente por defecto
    pub fn default() -> Self {
        Self {
            info: FontInfo::default_8x16(),
            glyphs: &DEFAULT_FONT_8X16,
        }
    }
    
    /// Obtener bitmap de un carácter
    pub fn get_glyph(&self, character: char) -> &[u8] {
        let char_code = character as usize;
        
        // Solo soportamos caracteres ASCII básicos
        if char_code >= 32 && char_code < 127 {
            let start_index = (char_code - 32) * self.info.height as usize;
            let end_index = start_index + self.info.height as usize;
            &self.glyphs[start_index..end_index]
        } else {
            // Carácter por defecto (espacio)
            let start_index = 0;
            let end_index = self.info.height as usize;
            &self.glyphs[start_index..end_index]
        }
    }
    
    /// Calcular el ancho de un texto
    pub fn calculate_text_width(&self, text: &str) -> u32 {
        text.len() as u32 * self.info.width as u32
    }
    
    /// Calcular la altura de un texto (con saltos de línea)
    pub fn calculate_text_height(&self, text: &str) -> u32 {
        let line_count = text.lines().count().max(1);
        line_count as u32 * self.info.line_spacing as u32
    }
}

/// Renderizador de fuentes
pub struct FontRenderer {
    pub current_font: BitmapFont,
    pub default_color: Color,
    pub background_color: Option<Color>,
    pub tab_width: u8,
}

impl FontRenderer {
    pub fn new() -> Self {
        Self {
            current_font: BitmapFont::default(),
            default_color: Color::BLACK,
            background_color: None,
            tab_width: 4,
        }
    }
    
    /// Renderizar un carácter en una posición específica
    pub fn render_char(&self, framebuffer: &mut Framebuffer, character: char, position: Point, color: Color) {
        let glyph = self.current_font.get_glyph(character);
        
        for (row, &byte) in glyph.iter().enumerate() {
            for col in 0..8 {
                if (byte >> (7 - col)) & 1 != 0 {
                    let x = position.x + col as i32;
                    let y = position.y + row as i32;
                    
                    if x >= 0 && x < framebuffer.info.width as i32 &&
                       y >= 0 && y < framebuffer.info.height as i32 {
                        framebuffer.put_pixel(x as u32, y as u32, color);
                    }
                } else if let Some(bg_color) = self.background_color {
                    let x = position.x + col as i32;
                    let y = position.y + row as i32;
                    
                    if x >= 0 && x < framebuffer.info.width as i32 &&
                       y >= 0 && y < framebuffer.info.height as i32 {
                        framebuffer.put_pixel(x as u32, y as u32, bg_color);
                    }
                }
            }
        }
    }
    
    /// Renderizar texto en una posición específica
    pub fn render_text(&self, framebuffer: &mut Framebuffer, text: &str, position: Point, color: Color) {
        let mut current_pos = position;
        
        for character in text.chars() {
            match character {
                '\n' => {
                    // Nueva línea
                    current_pos.x = position.x;
                    current_pos.y += self.current_font.info.line_spacing as i32;
                }
                '\r' => {
                    // Retorno de carro
                    current_pos.x = position.x;
                }
                '\t' => {
                    // Tabulación
                    let tab_pixels = self.tab_width as i32 * self.current_font.info.width as i32;
                    current_pos.x = ((current_pos.x - position.x + tab_pixels) / tab_pixels) * tab_pixels + position.x;
                }
                _ => {
                    // Carácter normal
                    self.render_char(framebuffer, character, current_pos, color);
                    current_pos.x += self.current_font.info.width as i32;
                }
            }
            
            // Verificar si necesitamos hacer wrap
            if current_pos.x >= framebuffer.info.width as i32 {
                current_pos.x = position.x;
                current_pos.y += self.current_font.info.line_spacing as i32;
            }
        }
    }
    
    /// Renderizar texto centrado en un rectángulo
    pub fn render_text_centered(&self, framebuffer: &mut Framebuffer, text: &str, rect: crate::gui::framebuffer::Rect, color: Color) {
        let text_width = self.current_font.calculate_text_width(text);
        let text_height = self.current_font.calculate_text_height(text);
        
        let center_x = rect.x + (rect.width as i32 - text_width as i32) / 2;
        let center_y = rect.y + (rect.height as i32 - text_height as i32) / 2;
        
        let center_pos = Point::new(center_x, center_y);
        self.render_text(framebuffer, text, center_pos, color);
    }
    
    /// Cambiar fuente actual
    pub fn set_font(&mut self, font: BitmapFont) {
        self.current_font = font;
    }
    
    /// Establecer color de fondo para texto
    pub fn set_background_color(&mut self, color: Option<Color>) {
        self.background_color = color;
    }
    
    /// Establecer ancho de tabulación
    pub fn set_tab_width(&mut self, width: u8) {
        self.tab_width = width;
    }
}

/// Fuente bitmap por defecto 8x16 (ASCII básico)
/// Esta es una fuente muy simplificada solo para demostración
/// En un sistema real, esto vendría de un archivo de fuente o ROM
static DEFAULT_FONT_8X16: [u8; 96] = [
    // Carácter ' ' (espacio)
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    
    // Carácter '!' 
    0x00, 0x00, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18,
    0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00,
    
    // Carácter '"'
    0x00, 0x00, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    
    // Carácter '#'
    0x00, 0x00, 0x36, 0x36, 0x7F, 0x36, 0x36, 0x36,
    0x7F, 0x36, 0x36, 0x36, 0x00, 0x00, 0x00, 0x00,
    
    // Carácter '$'
    0x00, 0x18, 0x18, 0x3E, 0x63, 0x60, 0x30, 0x18,
    0x0C, 0x06, 0x63, 0x3E, 0x18, 0x18, 0x00, 0x00,
    
    // Los siguientes caracteres son simplificados para el ejemplo
    // En un sistema real, cada carácter tendría su bitmap completo
    
    // Para simplificar, repetir el patrón del '!' para los demás caracteres
    // (esto es solo para demostración)
    0x00, 0x00, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18,
    0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00,
    
    // ... (repetir para los 90 caracteres restantes)
    // En una implementación real, aquí estarían todos los bitmaps de caracteres ASCII
];

// Rellenar el resto del array con el mismo patrón
// (Esto es solo para compilación, en un sistema real cada carácter tendría su bitmap único)
const fn generate_default_font() -> [u8; 95 * 16] {
    let mut font = [0u8; 95 * 16];
    let pattern = [
        0x00, 0x00, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18,
        0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00,
    ];
    
    let mut i = 0;
    while i < 95 {
        let mut j = 0;
        while j < 16 {
            font[i * 16 + j] = pattern[j];
            j += 1;
        }
        i += 1;
    }
    
    font
}

/// Renderizador de fuentes global
static mut FONT_RENDERER: Option<FontRenderer> = None;

/// Inicializar el renderizador de fuentes
pub fn init_font_renderer() {
    let renderer = FontRenderer::new();
    unsafe {
        FONT_RENDERER = Some(renderer);
    }
}

/// Obtener referencia al renderizador de fuentes
pub fn get_font_renderer() -> Option<&'static mut FontRenderer> {
    unsafe {
        FONT_RENDERER.as_mut()
    }
}

/// Renderizar texto (función de conveniencia)
pub fn render_text(framebuffer: &mut Framebuffer, text: &str, position: Point, color: Color) {
    if let Some(renderer) = get_font_renderer() {
        renderer.render_text(framebuffer, text, position, color);
    } else {
        // Fallback simple si no hay renderizador
        // Usar VGA text mode para mostrar texto básico
        render_text_vga_fallback(text, position, color);
    }
}

/// Renderizar carácter (función de conveniencia)
pub fn render_char(framebuffer: &mut Framebuffer, character: char, position: Point, color: Color) {
    if let Some(renderer) = get_font_renderer() {
        renderer.render_char(framebuffer, character, position, color);
    }
}

/// Fallback para renderizado de texto usando VGA text mode
fn render_text_vga_fallback(text: &str, position: Point, _color: Color) {
    // Mapear a buffer de texto VGA (0xB8000)
    let vga_buffer = 0xB8000 as *mut u8;
    
    // Calcular posición en el buffer de texto VGA (80x25 caracteres)
    let x = (position.x / 8) as usize; // 8 pixels por carácter
    let y = (position.y / 16) as usize; // 16 pixels por línea
    
    if x < 80 && y < 25 {
        for (i, byte) in text.bytes().enumerate() {
            if x + i < 80 {
                unsafe {
                    let offset = (y * 80 + x + i) * 2;
                    *vga_buffer.add(offset) = byte;
                    *vga_buffer.add(offset + 1) = 0x07; // Blanco sobre negro
                }
            }
        }
    }
}

/// Calcular ancho de texto
pub fn calculate_text_width(text: &str) -> u32 {
    if let Some(renderer) = get_font_renderer() {
        renderer.current_font.calculate_text_width(text)
    } else {
        text.len() as u32 * 8 // Fallback: 8 pixels por carácter
    }
}

/// Calcular altura de texto
pub fn calculate_text_height(text: &str) -> u32 {
    if let Some(renderer) = get_font_renderer() {
        renderer.current_font.calculate_text_height(text)
    } else {
        text.lines().count().max(1) as u32 * 16 // Fallback: 16 pixels por línea
    }
}
