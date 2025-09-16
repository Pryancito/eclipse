//! Driver de Framebuffer para Eclipse OS
//! 
//! Implementa un sistema de framebuffer robusto basado en las mejores prácticas
//! de Rust y optimizado para sistemas bare metal con UEFI.
//! 
//! Características modernas:
//! - Compositing de múltiples capas
//! - Aceleración por hardware cuando está disponible
//! - API moderna similar a wgpu pero compatible con no_std
//! - Soporte para efectos de transparencia y blending
//! - Pipeline de renderizado optimizado

use core::ptr;
use core::cmp::min;
use core::mem;
use core::ops::{Index, IndexMut};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::boxed::Box;
use core::ptr::read_volatile;

use crate::drivers::pci::{GpuType, GpuInfo};
use crate::drivers::intel_graphics::{IntelGraphicsDriver, IntelDriverState};
use crate::drivers::nvidia_graphics::{NvidiaGraphicsDriver, NvidiaDriverState};
use crate::drivers::amd_graphics::{AmdGraphicsDriver, AmdDriverState};

static FONT_DATA: [(char, [u8; 8]); 88] = [
    // Your character-bitmap tuples
    ('A', [0b00011000, 0b00100100, 0b01000010, 0b01000010, 0b01111110, 0b01000010, 0b01000010, 0b00000000]),
    ('B', [0b01111100, 0b01000010, 0b01000010, 0b01111100, 0b01000010, 0b01000010, 0b01111100, 0b00000000]),
    ('C', [0b00111100, 0b01000010, 0b01000000, 0b01000000, 0b01000000, 0b01000010, 0b00111100, 0b00000000]),
    ('D', [0b01111000, 0b01000100, 0b01000010, 0b01000010, 0b01000010, 0b01000100, 0b01111000, 0b00000000]),
    ('E', [0b01111110, 0b01000000, 0b01000000, 0b01111100, 0b01000000, 0b01000000, 0b01111110, 0b00000000]),
    ('F', [0b01111110, 0b01000000, 0b01000000, 0b01111100, 0b01000000, 0b01000000, 0b01000000, 0b00000000]),
    ('G', [0b00111100, 0b01000010, 0b01000000, 0b01001110, 0b01000010, 0b01000010, 0b00111100, 0b00000000]),
    ('H', [0b01000010, 0b01000010, 0b01000010, 0b01111110, 0b01000010, 0b01000010, 0b01000010, 0b00000000]),
    ('I', [0b00111100, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00111100, 0b00000000]),
    ('J', [0b00011110, 0b00000100, 0b00000100, 0b00000100, 0b01000100, 0b01000100, 0b00111000, 0b00000000]),
    ('K', [0b01000010, 0b01000100, 0b01001000, 0b01110000, 0b01001000, 0b01000100, 0b01000010, 0b00000000]),
    ('L', [0b01000000, 0b01000000, 0b01000000, 0b01000000, 0b01000000, 0b01000000, 0b01111110, 0b00000000]),
    ('M', [0b01000010, 0b01100110, 0b01011010, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b00000000]),
    ('N', [0b01000010, 0b01100010, 0b01010010, 0b01001010, 0b01000110, 0b01000010, 0b01000010, 0b00000000]),
    ('O', [0b00111100, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b00111100, 0b00000000]),
    ('P', [0b01111100, 0b01000010, 0b01000010, 0b01111100, 0b01000000, 0b01000000, 0b01000000, 0b00000000]),
    ('Q', [0b00111100, 0b01000010, 0b01000010, 0b01000010, 0b01001010, 0b01000100, 0b00111010, 0b00000000]),
    ('R', [0b01111100, 0b01000010, 0b01000010, 0b01111100, 0b01001000, 0b01000100, 0b01000010, 0b00000000]),
    ('S', [0b00111100, 0b01000010, 0b01000000, 0b00111100, 0b00000010, 0b01000010, 0b00111100, 0b00000000]),
    ('T', [0b01111110, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00000000]),
    ('U', [0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b00111100, 0b00000000]),
    ('V', [0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b00100100, 0b00011000, 0b00000000]),
    ('W', [0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b01011010, 0b01100110, 0b01000010, 0b00000000]),
    ('X', [0b01000010, 0b00100100, 0b00011000, 0b00011000, 0b00011000, 0b00100100, 0b01000010, 0b00000000]),
    ('Y', [0b01000010, 0b00100100, 0b00011000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00000000]),
    ('Z', [0b01111110, 0b00000010, 0b00000100, 0b00001000, 0b00010000, 0b00100000, 0b01111110, 0b00000000]),
    ('a', [0b00000000, 0b00111000, 0b00000100, 0b00111100, 0b01000100, 0b01000100, 0b00111100, 0b00000000]),
    ('b', [0b01000000, 0b01000000, 0b01111000, 0b01000100, 0b01000100, 0b01000100, 0b01111000, 0b00000000]),
    ('c', [0b00000000, 0b00111100, 0b01000000, 0b01000000, 0b01000000, 0b01000000, 0b00111100, 0b00000000]),
    ('d', [0b00000100, 0b00000100, 0b00111100, 0b01000100, 0b01000100, 0b01000100, 0b00111100, 0b00000000]),
    ('e', [0b00000000, 0b00111000, 0b01000100, 0b01111100, 0b01000000, 0b01000000, 0b00111100, 0b00000000]),
    ('f', [0b00011100, 0b00100000, 0b00100000, 0b01110000, 0b00100000, 0b00100000, 0b00100000, 0b00000000]),
    ('g', [0b00000000, 0b00111100, 0b01000100, 0b01000100, 0b00111100, 0b00000100, 0b00111000, 0b00000000]),
    ('h', [0b01000000, 0b01000000, 0b01111000, 0b01000100, 0b01000100, 0b01000100, 0b01000100, 0b00000000]),
    ('i', [0b00010000, 0b00000000, 0b00110000, 0b00010000, 0b00010000, 0b00010000, 0b00111000, 0b00000000]),
    ('j', [0b00001000, 0b00000000, 0b00011000, 0b00001000, 0b00001000, 0b01001000, 0b00110000, 0b00000000]),
    ('k', [0b01000000, 0b01001000, 0b01010000, 0b01100000, 0b01010000, 0b01001000, 0b01000100, 0b00000000]),
    ('l', [0b00110000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00111000, 0b00000000]),
    ('m', [0b00000000, 0b01101100, 0b01010100, 0b01010100, 0b01010100, 0b01010100, 0b01010100, 0b00000000]),
    ('n', [0b00000000, 0b01111000, 0b01000100, 0b01000100, 0b01000100, 0b01000100, 0b01000100, 0b00000000]),
    ('o', [0b00000000, 0b00111000, 0b01000100, 0b01000100, 0b01000100, 0b01000100, 0b00111000, 0b00000000]),
    ('p', [0b00000000, 0b01111000, 0b01000100, 0b01000100, 0b01111000, 0b01000000, 0b01000000, 0b00000000]),
    ('q', [0b00000000, 0b00111100, 0b01000100, 0b01000100, 0b00111100, 0b00000100, 0b00000100, 0b00000000]),
    ('r', [0b00000000, 0b01011000, 0b01100100, 0b01000000, 0b01000000, 0b01000000, 0b01000000, 0b00000000]),
    ('s', [0b00000000, 0b00111100, 0b01000000, 0b00111000, 0b00000100, 0b01000100, 0b00111000, 0b00000000]),
    ('t', [0b00100000, 0b01110000, 0b00100000, 0b00100000, 0b00100000, 0b00100000, 0b00011000, 0b00000000]),
    ('u', [0b00000000, 0b01000100, 0b01000100, 0b01000100, 0b01000100, 0b01001100, 0b00110100, 0b00000000]),
    ('v', [0b00000000, 0b01000100, 0b01000100, 0b01000100, 0b00101000, 0b00101000, 0b00010000, 0b00000000]),
    ('w', [0b00000000, 0b01000100, 0b01000100, 0b01010100, 0b01010100, 0b01010100, 0b00101000, 0b00000000]),
    ('x', [0b00000000, 0b01000100, 0b00101000, 0b00010000, 0b00101000, 0b01000100, 0b01000100, 0b00000000]),
    ('y', [0b00000000, 0b01000100, 0b01000100, 0b00111100, 0b00000100, 0b01000100, 0b00111000, 0b00000000]),
    ('z', [0b00000000, 0b01111100, 0b00001000, 0b00010000, 0b00100000, 0b01000000, 0b01111100, 0b00000000]),
    ('0', [0b00111100, 0b01000110, 0b01001010, 0b01010010, 0b01100010, 0b01000010, 0b00111100, 0b00000000]),
    ('1', [0b00010000, 0b00110000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00111000, 0b00000000]),
    ('2', [0b00111100, 0b01000010, 0b00000010, 0b00001100, 0b00110000, 0b01000000, 0b01111110, 0b00000000]),
    ('3', [0b00111100, 0b01000010, 0b00000010, 0b00011100, 0b00000010, 0b01000010, 0b00111100, 0b00000000]),
    ('4', [0b00001100, 0b00010100, 0b00100100, 0b01000100, 0b01111110, 0b00000100, 0b00000100, 0b00000000]),
    ('5', [0b01111110, 0b01000000, 0b01111100, 0b00000010, 0b00000010, 0b01000010, 0b00111100, 0b00000000]),
    ('6', [0b00111100, 0b01000000, 0b01111100, 0b01000010, 0b01000010, 0b01000010, 0b00111100, 0b00000000]),
    ('7', [0b01111110, 0b00000010, 0b00000100, 0b00001000, 0b00010000, 0b00010000, 0b00010000, 0b00000000]),
    ('8', [0b00111100, 0b01000010, 0b01000010, 0b00111100, 0b01000010, 0b01000010, 0b00111100, 0b00000000]),
    ('9', [0b00111100, 0b01000010, 0b01000010, 0b00111110, 0b00000010, 0b01000010, 0b00111100, 0b00000000]),
    ('.', [0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00011000, 0b00011000, 0b00000000]),
    (',', [0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00011000, 0b00010000, 0b00100000]),
    (':', [0b00000000, 0b00000000, 0b00011000, 0b00011000, 0b00000000, 0b00011000, 0b00011000, 0b00000000]),
    (';', [0b00000000, 0b00000000, 0b00011000, 0b00011000, 0b00000000, 0b00011000, 0b00010000, 0b00100000]),
    ('!', [0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00000000, 0b00010000, 0b00000000]),
    ('?', [0b00111100, 0b01000010, 0b00000010, 0b00001100, 0b00010000, 0b00000000, 0b00010000, 0b00000000]),
    ('@', [0b00111100, 0b01000010, 0b01011010, 0b01010110, 0b01011110, 0b01000000, 0b00111100, 0b00000000]),
    ('#', [0b00000000, 0b00101000, 0b01111100, 0b00101000, 0b00101000, 0b01111100, 0b00101000, 0b00000000]),
    (' ', [0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000]),
    ('-', [0b00000000, 0b00000000, 0b00000000, 0b00111100, 0b00000000, 0b00000000, 0b00000000, 0b00000000]),
    ('=', [0b00000000, 0b00000000, 0b00111100, 0b00000000, 0b00111100, 0b00000000, 0b00000000, 0b00000000]),
    ('+', [0b00000000, 0b00010000, 0b00010000, 0b01111100, 0b00010000, 0b00010000, 0b00000000, 0b00000000]),
    ('/', [0b00000010, 0b00000100, 0b00001000, 0b00010000, 0b00100000, 0b01000000, 0b00000000, 0b00000000]),
    ('*', [0b00000000, 0b01010100, 0b00111000, 0b01111100, 0b00111000, 0b01010100, 0b00000000, 0b00000000]),
    ('(', [0b00001000, 0b00010000, 0b00100000, 0b00100000, 0b00100000, 0b00010000, 0b00001000, 0b00000000]),
    (')', [0b00100000, 0b00010000, 0b00001000, 0b00001000, 0b00001000, 0b00010000, 0b00100000, 0b00000000]),
    ('[', [0b00011000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00011000, 0b00000000]),
    (']', [0b00110000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00110000, 0b00000000]),
    ('{', [0b00001000, 0b00010000, 0b00010000, 0b00100000, 0b00010000, 0b00010000, 0b00001000, 0b00000000]),
    ('}', [0b00100000, 0b00010000, 0b00010000, 0b00001000, 0b00010000, 0b00010000, 0b00100000, 0b00000000]),
    ('|', [0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00010000, 0b00000000]),
    ('\\', [0b01000000, 0b00100000, 0b00010000, 0b00001000, 0b00000100, 0b00000010, 0b00000000, 0b00000000]),
    ('\"', [0b00101000, 0b00101000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000]),
    ('`', [0b00010000, 0b00001000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000]),
    ('~', [0b00000000, 0b00000000, 0b00110010, 0b01001100, 0b00000000, 0b00000000, 0b00000000, 0b00000000]),
    ('\'', [0b00010000, 0b00010000, 0b00010000, 0b00000000, 0b00000000, 0b00000000, 0b00000000, 0b00000000]),
];

/// Trait para operaciones básicas de dibujo
pub trait Drawable {
    /// Dibujar un pixel en las coordenadas especificadas
    fn put_pixel(&mut self, x: u32, y: u32, color: Color);
    
    /// Leer un pixel de las coordenadas especificadas
    fn get_pixel(&self, x: u32, y: u32) -> Color;
    
    /// Llenar un rectángulo con color
    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color);
    
    /// Limpiar toda la superficie con un color
    fn clear(&mut self, color: Color);
}

/// Trait para operaciones de texto
pub trait TextRenderer {
    /// Escribir texto en las coordenadas especificadas
    fn write_text(&mut self, x: u32, y: u32, text: &str, color: Color);
    
    /// Obtener dimensiones de un carácter
    fn char_dimensions(&self) -> (u32, u32);
}

/// Trait para operaciones de geometría
pub trait GeometryRenderer {
    /// Dibujar una línea
    fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color);
    
    /// Dibujar un rectángulo (solo bordes)
    fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color);
    
    /// Dibujar un círculo
    fn draw_circle(&mut self, center_x: i32, center_y: i32, radius: u32, color: Color);
}

/// Trait para operaciones de blit (copia de superficies)
pub trait Blittable {
    /// Copiar región de otra superficie
    fn blit_from<T: Drawable>(&mut self, src: &T, src_x: u32, src_y: u32, 
                              dst_x: u32, dst_y: u32, width: u32, height: u32);
}

// ============================================================================
// API MODERNA SIMILAR A WGPU (COMPATIBLE CON NO_STD)
// ============================================================================

// ============================================================================
// SISTEMA DE RENDERIZADO DE TEXTO MODERNO
// ============================================================================

/// Estilos de fuente
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontStyle {
    Normal,
    Bold,
    Italic,
    BoldItalic,
}

/// Alineación de texto
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
    Justify,
}

/// Alineación vertical de texto
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VerticalAlign {
    Top,
    Middle,
    Bottom,
    Baseline,
}

/// Efectos de texto
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextEffect {
    None,
    Shadow { offset_x: i32, offset_y: i32, blur: u32, color: Color },
    Outline { width: u32, color: Color },
    Gradient { start_color: Color, end_color: Color },
    Glow { intensity: f32, color: Color },
}

/// Información de un carácter en la fuente
#[derive(Debug, Clone)]
pub struct GlyphInfo {
    pub width: u32,
    pub height: u32,
    pub advance: u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
    pub bitmap: Vec<u8>, // Datos del bitmap del carácter
}

/// Fuente bitmap simple
#[derive(Debug, Clone)]
pub struct Font {
    pub name: String,
    pub size: u32,
    pub style: FontStyle,
    pub line_height: u32,
    pub glyphs: Vec<Option<GlyphInfo>>, // Índice = código de carácter
    pub default_char: char,
}

impl Font {
    /// Crear una nueva fuente
    pub fn new(name: String, size: u32, style: FontStyle) -> Self {
        Self {
            name,
            size,
            style,
            line_height: size + 4, // Altura de línea con espaciado
            glyphs: {
                let mut glyphs = Vec::with_capacity(256);
                for _ in 0..256 {
                    glyphs.push(None);
                }
                glyphs
            }, // ASCII básico
            default_char: '?',
        }
    }
    
    /// Crear fuente por defecto del sistema
    pub fn default_font() -> Self {
        let mut font = Self::new("System".to_string(), 16, FontStyle::Normal);
        font.generate_basic_glyphs();
        font
    }
    
    /// Generar glifos básicos para caracteres ASCII
    fn generate_basic_glyphs(&mut self) {
        // Generar glifos para caracteres básicos (0-127)
        for i in 0..128 {
            if let Some(ch) = char::from_u32(i) {
                if ch.is_ascii_graphic() || ch.is_whitespace() {
                    self.glyphs[i as usize] = Some(self.create_glyph(ch));
                }
            }
        }
    }
    
    /// Crear un glifo para un carácter específico
    fn create_glyph(&self, ch: char) -> GlyphInfo {
        let (width, height) = self.get_char_size(ch);
        let mut bitmap = Vec::with_capacity((width * height) as usize);
        
        // Generar bitmap simple del carácter
        for y in 0..height {
            for x in 0..width {
                let pixel = self.draw_char_pixel(ch, x, y, width, height);
                bitmap.push(pixel);
            }
        }
        
        GlyphInfo {
            width,
            height,
            advance: width + 1, // Espaciado entre caracteres
            bearing_x: 0,
            bearing_y: height as i32,
            bitmap,
        }
    }
    
    /// Obtener el tamaño de un carácter
    fn get_char_size(&self, ch: char) -> (u32, u32) {
        match ch {
            ' ' => (self.size / 4, self.size), // Espacio
            '\t' => (self.size * 2, self.size), // Tab
            '\n' => (0, self.size), // Nueva línea
            _ => (self.size, self.size), // Caracteres normales
        }
    }
    
    /// Dibujar un pixel de un carácter
    fn draw_char_pixel(&self, ch: char, x: u32, y: u32, width: u32, height: u32) -> u8 {
        if ch == ' ' || ch == '\t' || ch == '\n' {
            return 0; // Transparente
        }
        
        // Patrones simples para caracteres básicos
        let pattern = self.get_char_pattern(ch);
        let x_norm = (x * 8) / width; // Normalizar a 8x8
        let y_norm = (y * 8) / height;
        
        if x_norm < 8 && y_norm < 8 {
            let bit = (pattern[y_norm as usize] >> (7 - x_norm)) & 1;
            if bit != 0 {
                255 // Opaco
            } else {
                0 // Transparente
            }
        } else {
            0
        }
    }
    
    /// Obtener patrón de bits para un carácter (8x8)
    fn get_char_pattern(&self, ch: char) -> [u8; 8] {
        match ch {
            'A' => [0x18, 0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x00],
            'B' => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x7C, 0x00],
            'C' => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00],
            'D' => [0x78, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00],
            'E' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x7E, 0x00],
            'F' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x00],
            'G' => [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x3C, 0x00],
            'H' => [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
            'I' => [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
            'J' => [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x6C, 0x38, 0x00],
            'K' => [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x00],
            'L' => [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00],
            'M' => [0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x00],
            'N' => [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x00],
            'O' => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
            'P' => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x00],
            'Q' => [0x3C, 0x66, 0x66, 0x66, 0x6A, 0x6C, 0x36, 0x00],
            'R' => [0x7C, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0x00],
            'S' => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x66, 0x3C, 0x00],
            'T' => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00],
            'U' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
            'V' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00],
            'W' => [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00],
            'X' => [0x66, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0x66, 0x00],
            'Y' => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x00],
            'Z' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00],
            '0' => [0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x3C, 0x00],
            '1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
            '2' => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x30, 0x7E, 0x00],
            '3' => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x66, 0x3C, 0x00],
            '4' => [0x0C, 0x1C, 0x3C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00],
            '5' => [0x7E, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00],
            '6' => [0x3C, 0x66, 0x60, 0x7C, 0x66, 0x66, 0x3C, 0x00],
            '7' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00],
            '8' => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x3C, 0x00],
            '9' => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x66, 0x3C, 0x00],
            '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00],
            ',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30],
            ':' => [0x00, 0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00],
            ';' => [0x00, 0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x30],
            '!' => [0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x18, 0x00],
            '?' => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x00, 0x18, 0x00],
            _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Carácter no soportado
        }
    }
    
    /// Obtener información de un glifo
    pub fn get_glyph(&self, ch: char) -> Option<&GlyphInfo> {
        let code = ch as usize;
        if code < self.glyphs.len() {
            self.glyphs[code].as_ref()
        } else {
            None
        }
    }
    
    /// Obtener el ancho de una cadena de texto
    pub fn measure_text(&self, text: &str) -> u32 {
        let mut width = 0;
        for ch in text.chars() {
            if let Some(glyph) = self.get_glyph(ch) {
                width += glyph.advance;
            } else {
                width += self.size; // Ancho por defecto
            }
        }
        width
    }
    
    /// Obtener la altura de una línea de texto
    pub fn line_height(&self) -> u32 {
        self.line_height
    }
}

/// Configuración de texto
#[derive(Debug, Clone)]
pub struct TextConfig {
    pub font: Font,
    pub color: Color,
    pub background_color: Option<Color>,
    pub effect: TextEffect,
    pub align: TextAlign,
    pub vertical_align: VerticalAlign,
    pub line_spacing: f32,
    pub word_wrap: bool,
    pub max_width: Option<u32>,
}

impl TextConfig {
    pub fn new(font: Font, color: Color) -> Self {
        Self {
            font,
            color,
            background_color: None,
            effect: TextEffect::None,
            align: TextAlign::Left,
            vertical_align: VerticalAlign::Top,
            line_spacing: 1.0,
            word_wrap: false,
            max_width: None,
        }
    }
    
    pub fn with_effect(mut self, effect: TextEffect) -> Self {
        self.effect = effect;
        self
    }
    
    pub fn with_alignment(mut self, align: TextAlign, vertical_align: VerticalAlign) -> Self {
        self.align = align;
        self.vertical_align = vertical_align;
        self
    }
    
    pub fn with_background(mut self, background_color: Color) -> Self {
        self.background_color = Some(background_color);
        self
    }
    
    pub fn with_wrapping(mut self, word_wrap: bool, max_width: Option<u32>) -> Self {
        self.word_wrap = word_wrap;
        self.max_width = max_width;
        self
    }
}

/// Información de layout de texto
#[derive(Debug, Clone)]
pub struct TextLayout {
    pub lines: Vec<TextLine>,
    pub total_width: u32,
    pub total_height: u32,
    pub line_count: usize,
}

#[derive(Debug, Clone)]
pub struct TextLine {
    pub text: String,
    pub width: u32,
    pub height: u32,
    pub start_x: u32,
    pub start_y: u32,
}

/// Motor de renderizado de texto moderno
pub struct ModernTextRenderer {
    fonts: Vec<Font>,
    current_font_index: usize,
}

impl ModernTextRenderer {
    pub fn new() -> Self {
        let mut renderer = Self {
            fonts: Vec::new(),
            current_font_index: 0,
        };
        renderer.add_font(Font::default_font());
        renderer
    }
    
    pub fn add_font(&mut self, font: Font) -> usize {
        self.fonts.push(font);
        self.fonts.len() - 1
    }
    
    pub fn set_font(&mut self, index: usize) -> Result<(), &'static str> {
        if index < self.fonts.len() {
            self.current_font_index = index;
            Ok(())
        } else {
            Err("Font index out of range")
        }
    }
    
    pub fn get_font(&self) -> &Font {
        &self.fonts[self.current_font_index]
    }
    
    /// Calcular layout de texto
    pub fn layout_text(&self, text: &str, config: &TextConfig) -> TextLayout {
        let font = &config.font;
        let mut lines = Vec::new();
        let mut total_width = 0;
        let mut total_height = 0;
        
        // Dividir texto en líneas
        let text_lines: Vec<&str> = text.split('\n').collect();
        
        for (line_index, line_text) in text_lines.iter().enumerate() {
            let line_width = font.measure_text(line_text);
            let line_height = font.line_height();
            
            let start_x = match config.align {
                TextAlign::Left => 0,
                TextAlign::Center => if let Some(max_w) = config.max_width {
                    (max_w.saturating_sub(line_width)) / 2
                } else {
                    0
                },
                TextAlign::Right => if let Some(max_w) = config.max_width {
                    max_w.saturating_sub(line_width)
                } else {
                    0
                },
                TextAlign::Justify => 0, // Se manejará después
            };
            
            let start_y = (line_index as u32) * line_height;
            
            lines.push(TextLine {
                text: (*line_text).to_string(),
                width: line_width,
                height: line_height,
                start_x,
                start_y,
            });
            
            total_width = total_width.max(line_width);
            total_height += line_height;
        }
        
        let line_count = lines.len();
        TextLayout {
            lines,
            total_width,
            total_height,
            line_count,
        }
    }
}

/// Modos de blending para transparencias
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlendMode {
    None,           // Sin blending
    Alpha,          // Blending alpha estándar
    Additive,       // Aditivo
    Multiply,       // Multiplicativo
    Screen,         // Pantalla
    Overlay,        // Superposición
}

/// Modos de filtrado para texturas
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterMode {
    Nearest,        // Vecino más cercano (pixelado)
    Linear,         // Lineal (suavizado)
}

/// Formato de textura
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextureFormat {
    R8,             // 8 bits rojo
    RG8,            // 8 bits rojo-verde
    RGB8,           // 8 bits RGB
    RGBA8,          // 8 bits RGBA
    R16,            // 16 bits rojo
    RG16,           // 16 bits rojo-verde
    RGB16,          // 16 bits RGB
    RGBA16,         // 16 bits RGBA
}

/// Textura en memoria
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub data: Vec<u8>,
    pub filter_mode: FilterMode,
}

impl Texture {
    pub fn new(width: u32, height: u32, format: TextureFormat) -> Self {
        let bytes_per_pixel = match format {
            TextureFormat::R8 => 1,
            TextureFormat::RG8 => 2,
            TextureFormat::RGB8 => 3,
            TextureFormat::RGBA8 => 4,
            TextureFormat::R16 => 2,
            TextureFormat::RG16 => 4,
            TextureFormat::RGB16 => 6,
            TextureFormat::RGBA16 => 8,
        };
        
        let size = (width * height * bytes_per_pixel) as usize;
        
        let mut data = Vec::with_capacity(size);
        for _ in 0..size {
            data.push(0u8);
        }
        
        Self {
            width,
            height,
            format,
            data,
            filter_mode: FilterMode::Linear,
        }
    }
    
    pub fn from_data(width: u32, height: u32, format: TextureFormat, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            format,
            data,
            filter_mode: FilterMode::Linear,
        }
    }
    
    pub fn set_pixel(&mut self, x: u32, y: u32, color: Color) {
        if x >= self.width || y >= self.height {
            return;
        }
        
        let bytes_per_pixel = match self.format {
            TextureFormat::R8 => 1,
            TextureFormat::RG8 => 2,
            TextureFormat::RGB8 => 3,
            TextureFormat::RGBA8 => 4,
            TextureFormat::R16 => 2,
            TextureFormat::RG16 => 4,
            TextureFormat::RGB16 => 6,
            TextureFormat::RGBA16 => 8,
        };
        
        let index = ((y * self.width + x) * bytes_per_pixel) as usize;
        
        match self.format {
            TextureFormat::R8 => {
                if index < self.data.len() {
                    self.data[index] = color.r;
                }
            },
            TextureFormat::RG8 => {
                if index + 1 < self.data.len() {
                    self.data[index] = color.r;
                    self.data[index + 1] = color.g;
                }
            },
            TextureFormat::RGB8 => {
                if index + 2 < self.data.len() {
                    self.data[index] = color.r;
                    self.data[index + 1] = color.g;
                    self.data[index + 2] = color.b;
                }
            },
            TextureFormat::RGBA8 => {
                if index + 3 < self.data.len() {
                    self.data[index] = color.r;
                    self.data[index + 1] = color.g;
                    self.data[index + 2] = color.b;
                    self.data[index + 3] = color.a;
                }
            },
            _ => {
                // Para formatos de 16 bits, convertir a 8 bits por simplicidad
                if index < self.data.len() {
                    self.data[index] = color.r;
                }
            },
        }
    }
    
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        if x >= self.width || y >= self.height {
            return Color::BLACK;
        }
        
        let bytes_per_pixel = match self.format {
            TextureFormat::R8 => 1,
            TextureFormat::RG8 => 2,
            TextureFormat::RGB8 => 3,
            TextureFormat::RGBA8 => 4,
            _ => 1,
        };
        
        let index = ((y * self.width + x) * bytes_per_pixel) as usize;
        
        match self.format {
            TextureFormat::R8 => {
                if index < self.data.len() {
                    Color::rgb(self.data[index], 0, 0)
                } else {
                    Color::BLACK
                }
            },
            TextureFormat::RG8 => {
                if index + 1 < self.data.len() {
                    Color::rgb(self.data[index], self.data[index + 1], 0)
                } else {
                    Color::BLACK
                }
            },
            TextureFormat::RGB8 => {
                if index + 2 < self.data.len() {
                    Color::rgb(self.data[index], self.data[index + 1], self.data[index + 2])
                } else {
                    Color::BLACK
                }
            },
            TextureFormat::RGBA8 => {
                if index + 3 < self.data.len() {
                    Color::rgba(self.data[index], self.data[index + 1], 
                               self.data[index + 2], self.data[index + 3])
                } else {
                    Color::BLACK
                }
            },
            _ => Color::BLACK,
        }
    }
}

/// Capa de compositing
pub struct CompositingLayer {
    pub texture: Texture,
    pub position: (i32, i32),
    pub blend_mode: BlendMode,
    pub alpha: f32,
    pub visible: bool,
}

impl CompositingLayer {
    pub fn new(texture: Texture, x: i32, y: i32) -> Self {
        Self {
            texture,
            position: (x, y),
            blend_mode: BlendMode::Alpha,
            alpha: 1.0,
            visible: true,
        }
    }
}

/// Pipeline de renderizado moderno
pub struct ModernRenderPipeline {
    layers: Vec<CompositingLayer>,
    clear_color: Color,
    enable_hardware_acceleration: bool,
}

impl ModernRenderPipeline {
    pub fn new() -> Self {
        Self {
            layers: Vec::new(),
            clear_color: Color::BLACK,
            enable_hardware_acceleration: false,
        }
    }
    
    pub fn add_layer(&mut self, layer: CompositingLayer) {
        self.layers.push(layer);
    }
    
    pub fn remove_layer(&mut self, index: usize) {
        if index < self.layers.len() {
            self.layers.remove(index);
        }
    }
    
    pub fn set_clear_color(&mut self, color: Color) {
        self.clear_color = color;
    }
    
    pub fn enable_hardware_acceleration(&mut self, enable: bool) {
        self.enable_hardware_acceleration = enable;
    }
    
    /// Renderizar todas las capas al framebuffer
    pub fn render_to_framebuffer(&self, framebuffer: &mut FramebufferDriver) {
        // Limpiar framebuffer
        framebuffer.clear(self.clear_color);
        
        // Renderizar cada capa en orden
        for layer in &self.layers {
            if !layer.visible {
                continue;
            }
            
            self.render_layer_to_framebuffer(framebuffer, layer);
        }
    }
    
    fn render_layer_to_framebuffer(&self, framebuffer: &mut FramebufferDriver, layer: &CompositingLayer) {
        let (layer_x, layer_y) = layer.position;
        
        for y in 0..layer.texture.height {
            for x in 0..layer.texture.width {
                let src_color = layer.texture.get_pixel(x, y);
                let dst_x = layer_x + x as i32;
                let dst_y = layer_y + y as i32;
                
                if dst_x >= 0 && dst_y >= 0 && 
                   dst_x < framebuffer.info.width as i32 && 
                   dst_y < framebuffer.info.height as i32 {
                    
                    let final_color = self.blend_colors(
                        src_color, 
                        framebuffer.get_pixel(dst_x as u32, dst_y as u32),
                        layer.blend_mode,
                        layer.alpha
                    );
                    
                    framebuffer.put_pixel(dst_x as u32, dst_y as u32, final_color);
                }
            }
        }
    }
    
    fn blend_colors(&self, src: Color, dst: Color, blend_mode: BlendMode, alpha: f32) -> Color {
        match blend_mode {
            BlendMode::None => src,
            BlendMode::Alpha => {
                let src_alpha = (src.a as f32 / 255.0) * alpha;
                let dst_alpha = dst.a as f32 / 255.0;
                let final_alpha = src_alpha + dst_alpha * (1.0 - src_alpha);
                
                if final_alpha == 0.0 {
                    return dst;
                }
                
                let r = ((src.r as f32 * src_alpha + dst.r as f32 * dst_alpha * (1.0 - src_alpha)) / final_alpha) as u8;
                let g = ((src.g as f32 * src_alpha + dst.g as f32 * dst_alpha * (1.0 - src_alpha)) / final_alpha) as u8;
                let b = ((src.b as f32 * src_alpha + dst.b as f32 * dst_alpha * (1.0 - src_alpha)) / final_alpha) as u8;
                
                Color::rgba(r, g, b, (final_alpha * 255.0) as u8)
            },
            BlendMode::Additive => {
                let r = min(255, src.r + dst.r);
                let g = min(255, src.g + dst.g);
                let b = min(255, src.b + dst.b);
                Color::rgba(r, g, b, dst.a)
            },
            BlendMode::Multiply => {
                let r = ((src.r as u16 * dst.r as u16) / 255) as u8;
                let g = ((src.g as u16 * dst.g as u16) / 255) as u8;
                let b = ((src.b as u16 * dst.b as u16) / 255) as u8;
                Color::rgba(r, g, b, dst.a)
            },
            BlendMode::Screen => {
                let r = 255 - (((255 - src.r as u16) * (255 - dst.r as u16)) / 255) as u8;
                let g = 255 - (((255 - src.g as u16) * (255 - dst.g as u16)) / 255) as u8;
                let b = 255 - (((255 - src.b as u16) * (255 - dst.b as u16)) / 255) as u8;
                Color::rgba(r, g, b, dst.a)
            },
            BlendMode::Overlay => {
                // Implementación simplificada de overlay
                let r = if dst.r < 128 { 
                    (2 * src.r as u16 * dst.r as u16 / 255) as u8 
                } else { 
                    255 - (2 * (255 - src.r as u16) * (255 - dst.r as u16) / 255) as u8 
                };
                let g = if dst.g < 128 { 
                    (2 * src.g as u16 * dst.g as u16 / 255) as u8 
                } else { 
                    255 - (2 * (255 - src.g as u16) * (255 - dst.g as u16) / 255) as u8 
                };
                let b = if dst.b < 128 { 
                    (2 * src.b as u16 * dst.b as u16 / 255) as u8 
                } else { 
                    255 - (2 * (255 - src.b as u16) * (255 - dst.b as u16) / 255) as u8 
                };
                Color::rgba(r, g, b, dst.a)
            },
        }
    }
}

/// Información del framebuffer obtenida del hardware
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
}

/// Formatos de pixel soportados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelFormat {
    RGB888,     // 24-bit RGB
    RGBA8888,   // 32-bit RGBA
    BGR888,     // 24-bit BGR
    BGRA8888,   // 32-bit BGRA
    RGB565,     // 16-bit RGB
    BGR565,     // 16-bit BGR
    Unknown,
}

impl PixelFormat {
    pub fn from_uefi_format(format: u32) -> Self {
        match format {
            0 => PixelFormat::RGB888,      // PixelRedGreenBlueReserved8BitPerColor
            1 => PixelFormat::BGR888,      // PixelBlueGreenRedReserved8BitPerColor
            2 => PixelFormat::RGB565,      // PixelBitMask
            3 => PixelFormat::BGR565,      // PixelBltOnly
            _ => PixelFormat::Unknown,
        }
    }
    
    pub fn bytes_per_pixel(&self) -> u8 {
        match self {
            PixelFormat::RGB888 => 3,
            PixelFormat::RGBA8888 => 4,
            PixelFormat::BGR888 => 3,
            PixelFormat::BGRA8888 => 4,
            PixelFormat::RGB565 => 2,
            PixelFormat::BGR565 => 2,
            PixelFormat::Unknown => 4, // Default to 4 bytes
        }
    }
}

/// Color RGBA con operaciones avanzadas
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 255)
    }
    
    pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::new(r, g, b, a)
    }
    
    /// Crear color desde valor hexadecimal (0x00RRGGBB, donde 0x00FF0000 es rojo)
    pub fn from_hex(hex: u32) -> Self {
        // 0x00RRGGBB: Rojo en bits 16-23, Verde en 8-15, Azul en 0-7
        Self::rgb(
            ((hex >> 16) & 0xFF) as u8, // Rojo
            ((hex >> 8) & 0xFF) as u8,  // Verde
            (hex & 0xFF) as u8          // Azul
        )
    }
    
    /// Crear color desde valor hexadecimal con alpha (0xAARRGGBB, donde 0xFFFF0000 es rojo opaco)
    pub fn from_hex_alpha(hex: u32) -> Self {
        // 0xAARRGGBB: Alpha en bits 24-31, Rojo en 16-23, Verde en 8-15, Azul en 0-7
        Self::new(
            ((hex >> 16) & 0xFF) as u8, // Rojo
            ((hex >> 8) & 0xFF) as u8,  // Verde
            (hex & 0xFF) as u8,         // Azul
            ((hex >> 24) & 0xFF) as u8  // Alpha
        )
    }

    /// Convertir a valor u32 RGBA
    pub fn to_u32(&self) -> u32 {
        ((self.a as u32) << 24) | 
        ((self.r as u32) << 16) | 
        ((self.g as u32) << 8) | 
        (self.b as u32)
    }

    /// Mezclar dos colores con alpha blending
    pub fn blend(&self, other: Color) -> Color {
        let alpha = other.a as f32 / 255.0;
        let inv_alpha = 1.0 - alpha;
        
        Color::new(
            (self.r as f32 * inv_alpha + other.r as f32 * alpha) as u8,
            (self.g as f32 * inv_alpha + other.g as f32 * alpha) as u8,
            (self.b as f32 * inv_alpha + other.b as f32 * alpha) as u8,
            self.a.max(other.a)
        )
    }
    
    /// Aplicar factor de brillo (0.0 = negro, 1.0 = original, >1.0 = más brillante)
    pub fn brighten(&self, factor: f32) -> Color {
        Color::new(
            (self.r as f32 * factor).min(255.0) as u8,
            (self.g as f32 * factor).min(255.0) as u8,
            (self.b as f32 * factor).min(255.0) as u8,
            self.a
        )
    }
    
    /// Obtener luminancia del color
    pub fn luminance(&self) -> f32 {
        0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32
    }
    
    /// Verificar si el color es oscuro (luminancia < 128)
    pub fn is_dark(&self) -> bool {
        self.luminance() < 128.0
    }
    
    /// Convertir color a pixel según el formato
    pub fn to_pixel(&self, format: PixelFormat) -> u32 {
        match format {
            PixelFormat::RGBA8888 => {
                ((self.a as u32) << 24) | 
                ((self.r as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.b as u32)
            },
            PixelFormat::BGRA8888 => {
                ((self.a as u32) << 24) | 
                ((self.b as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.r as u32)
            },
            PixelFormat::RGB888 => {
                ((self.r as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.b as u32)
            },
            PixelFormat::BGR888 => {
                ((self.b as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.r as u32)
            },
            PixelFormat::RGB565 => {
                (((self.r as u32) >> 3) << 11) |
                (((self.g as u32) >> 2) << 5) |
                ((self.b as u32) >> 3)
            },
            PixelFormat::BGR565 => {
                (((self.b as u32) >> 3) << 11) |
                (((self.g as u32) >> 2) << 5) |
                ((self.r as u32) >> 3)
            },
            PixelFormat::Unknown => 0,
        }
    }
    
    /// Crear color desde un pixel según el formato
    pub fn from_pixel(pixel: u32, format: PixelFormat) -> Self {
        match format {
            PixelFormat::RGBA8888 => {
                Self {
                    r: ((pixel >> 16) & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: (pixel & 0xFF) as u8,
                    a: ((pixel >> 24) & 0xFF) as u8,
                }
            },
            PixelFormat::BGRA8888 => {
                Self {
                    r: (pixel & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: ((pixel >> 16) & 0xFF) as u8,
                    a: ((pixel >> 24) & 0xFF) as u8,
                }
            },
            PixelFormat::RGB888 => {
                Self {
                    r: ((pixel >> 16) & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: (pixel & 0xFF) as u8,
                    a: 255,
                }
            },
            PixelFormat::BGR888 => {
                Self {
                    r: (pixel & 0xFF) as u8,
                    g: ((pixel >> 8) & 0xFF) as u8,
                    b: ((pixel >> 16) & 0xFF) as u8,
                    a: 255,
                }
            },
            PixelFormat::RGB565 => {
                Self {
                    r: (((pixel >> 11) & 0x1F) << 3) as u8,
                    g: (((pixel >> 5) & 0x3F) << 2) as u8,
                    b: ((pixel & 0x1F) << 3) as u8,
                    a: 255,
                }
            },
            PixelFormat::BGR565 => {
                Self {
                    r: ((pixel & 0x1F) << 3) as u8,
                    g: (((pixel >> 5) & 0x3F) << 2) as u8,
                    b: (((pixel >> 11) & 0x1F) << 3) as u8,
                    a: 255,
                }
            },
            PixelFormat::Unknown => Self::new(0, 0, 0, 0),
        }
    }
}

/// Tipo de aceleración de hardware disponible
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HardwareAcceleration {
    None,
    Intel2D,
    Nvidia2D,
    Amd2D,
    Generic2D,
}

/// Información de capacidades de aceleración
#[derive(Debug, Clone)]
pub struct AccelerationCapabilities {
    pub supports_hardware_blit: bool,
    pub supports_hardware_fill: bool,
    pub supports_hardware_alpha: bool,
    pub supports_hardware_gradients: bool,
    pub supports_hardware_scaling: bool,
    pub supports_hardware_rotation: bool,
    pub max_blit_size: (u32, u32),
    pub memory_bandwidth: u64, // MB/s
}

/// Trait para aceleración de hardware 2D
pub trait HardwareAccelerated {
    /// Obtener tipo de aceleración disponible
    fn acceleration_type(&self) -> HardwareAcceleration;
    
    /// Obtener capacidades de aceleración
    fn acceleration_capabilities(&self) -> AccelerationCapabilities;
    
    /// Blit acelerado por hardware
    fn hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str>;
    
    /// Fill acelerado por hardware
    fn hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str>;
    
    /// Alpha blending acelerado por hardware
    fn hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                            color: Color, alpha: u8) -> Result<(), &'static str>;
    
    /// Escalado acelerado por hardware
    fn hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                      dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str>;
}

/// Gestor de aceleración de hardware
#[derive(Debug, Clone)]
pub struct HardwareAccelerationManager {
    gpu_type: Option<GpuType>,
    capabilities: AccelerationCapabilities,
    is_initialized: bool,
}

impl HardwareAccelerationManager {
    /// Crear nuevo gestor de aceleración
    pub fn new() -> Self {
        Self {
            gpu_type: None,
            capabilities: AccelerationCapabilities {
                supports_hardware_blit: false,
                supports_hardware_fill: false,
                supports_hardware_alpha: false,
                supports_hardware_gradients: false,
                supports_hardware_scaling: false,
                supports_hardware_rotation: false,
                max_blit_size: (0, 0),
                memory_bandwidth: 0,
            },
            is_initialized: false,
        }
    }
    
    /// Inicializar con información de GPU
    pub fn initialize_with_gpu(&mut self, gpu_info: &GpuInfo) -> Result<(), &'static str> {
        self.gpu_type = Some(gpu_info.gpu_type);
        
        match gpu_info.gpu_type {
            GpuType::Intel => {
                // Crear driver Intel (simplificado para el ejemplo)
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: true,
                    supports_hardware_fill: true,
                    supports_hardware_alpha: true,
                    supports_hardware_gradients: false,
                    supports_hardware_scaling: true,
                    supports_hardware_rotation: false,
                    max_blit_size: (4096, 4096),
                    memory_bandwidth: 10000, // 10 GB/s estimado
                };
            },
            GpuType::Nvidia => {
                // Crear driver NVIDIA (simplificado para el ejemplo)
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: true,
                    supports_hardware_fill: true,
                    supports_hardware_alpha: true,
                    supports_hardware_gradients: true,
                    supports_hardware_scaling: true,
                    supports_hardware_rotation: true,
                    max_blit_size: (8192, 8192),
                    memory_bandwidth: 20000, // 20 GB/s estimado
                };
            },
            GpuType::Amd => {
                // Crear driver AMD (simplificado para el ejemplo)
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: true,
                    supports_hardware_fill: true,
                    supports_hardware_alpha: true,
                    supports_hardware_gradients: true,
                    supports_hardware_scaling: true,
                    supports_hardware_rotation: false,
                    max_blit_size: (6144, 6144),
                    memory_bandwidth: 15000, // 15 GB/s estimado
                };
            },
            _ => {
                // Sin aceleración de hardware
                self.capabilities = AccelerationCapabilities {
                    supports_hardware_blit: false,
                    supports_hardware_fill: false,
                    supports_hardware_alpha: false,
                    supports_hardware_gradients: false,
                    supports_hardware_scaling: false,
                    supports_hardware_rotation: false,
                    max_blit_size: (0, 0),
                    memory_bandwidth: 0,
                };
            }
        }
        
        self.is_initialized = true;
        Ok(())
    }
    
    /// Obtener capacidades de aceleración
    pub fn get_capabilities(&self) -> &AccelerationCapabilities {
        &self.capabilities
    }
    
    /// Verificar si hay aceleración disponible
    pub fn has_acceleration(&self) -> bool {
        self.is_initialized && self.capabilities.supports_hardware_blit
    }
    
    /// Obtener tipo de aceleración
    pub fn get_acceleration_type(&self) -> HardwareAcceleration {
        match self.gpu_type {
            Some(GpuType::Intel) => HardwareAcceleration::Intel2D,
            Some(GpuType::Nvidia) => HardwareAcceleration::Nvidia2D,
            Some(GpuType::Amd) => HardwareAcceleration::Amd2D,
            _ => HardwareAcceleration::None,
        }
    }
}

impl Color {
    // Colores básicos
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const RED: Color = Color { r: 255, g: 0, b: 0, a: 255 };
    pub const GREEN: Color = Color { r: 0, g: 255, b: 0, a: 255 };
    pub const BLUE: Color = Color { r: 0, g: 0, b: 255, a: 255 };
    pub const YELLOW: Color = Color { r: 255, g: 255, b: 0, a: 255 };
    pub const CYAN: Color = Color { r: 0, g: 255, b: 255, a: 255 };
    pub const MAGENTA: Color = Color { r: 255, g: 0, b: 255, a: 255 };
    
    // Colores del sistema
    pub const DARK_BLUE: Color = Color { r: 0, g: 0, b: 128, a: 255 };
    pub const DARKER_BLUE: Color = Color { r: 0, g: 0, b: 64, a: 255 };
    pub const GRAY: Color = Color { r: 128, g: 128, b: 128, a: 255 };
    pub const DARK_GRAY: Color = Color { r: 64, g: 64, b: 64, a: 255 };
    pub const LIGHT_GRAY: Color = Color { r: 192, g: 192, b: 192, a: 255 };
    
    // Colores adicionales para UI
    pub const ORANGE: Color = Color { r: 255, g: 165, b: 0, a: 255 };
    pub const PURPLE: Color = Color { r: 128, g: 0, b: 128, a: 255 };
    pub const PINK: Color = Color { r: 255, g: 192, b: 203, a: 255 };
    pub const BROWN: Color = Color { r: 165, g: 42, b: 42, a: 255 };
    pub const LIME: Color = Color { r: 0, g: 255, b: 0, a: 255 };
    pub const TEAL: Color = Color { r: 0, g: 128, b: 128, a: 255 };
    pub const NAVY: Color = Color { r: 0, g: 0, b: 128, a: 255 };
    pub const MAROON: Color = Color { r: 128, g: 0, b: 0, a: 255 };
    pub const OLIVE: Color = Color { r: 128, g: 128, b: 0, a: 255 };
    
    // Colores semitransparentes
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const SEMI_TRANSPARENT_BLACK: Color = Color { r: 0, g: 0, b: 0, a: 128 };
    pub const SEMI_TRANSPARENT_WHITE: Color = Color { r: 255, g: 255, b: 255, a: 128 };
}

/// Driver de Framebuffer
#[derive(Debug, Clone)]
pub struct FramebufferDriver {
    pub info: FramebufferInfo,
    buffer: *mut u8,
    is_initialized: bool,
    hardware_acceleration: HardwareAccelerationManager,
    current_x: u32,
}

impl FramebufferDriver {
    /// Crear nuevo driver de framebuffer
    pub fn new() -> Self {
        Self {
            info: FramebufferInfo {
                base_address: 0,
                width: 0,
                height: 0,
                pixels_per_scan_line: 0,
                pixel_format: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                reserved_mask: 0,
            },
            buffer: ptr::null_mut(),
            is_initialized: false,
            hardware_acceleration: HardwareAccelerationManager::new(),
            current_x: 10,
        }
    }
    
    /// Inicializar framebuffer con información de UEFI
    pub fn init_from_uefi(&mut self, 
                          base_address: u64,
                          width: u32,
                          height: u32,
                          pixels_per_scan_line: u32,
                          pixel_format: u32,
                          pixel_bitmask: u32) -> Result<(), &'static str> {
        
        // Validar parámetros básicos con más detalle
        if base_address == 0 {
            return Err("Invalid base address");
        }
        if width == 0 {
            return Err("Invalid width (cannot be zero)");
        }
        if height == 0 {
            return Err("Invalid height (cannot be zero)");
        }
        if pixels_per_scan_line == 0 && width == 0 {
            return Err("Both pixels_per_scan_line and width cannot be zero");
        }
        
        // Determinar formato de pixel
        let format = PixelFormat::from_uefi_format(pixel_format);
        if format == PixelFormat::Unknown {
            return Err("Unsupported pixel format");
        }
        
        // Calcular bytes por pixel usando el método del enum
        let bytes_per_pixel = format.bytes_per_pixel();

        // Log para debugging (comentado porque serial_write_hex32 no está disponible aquí)
        // serial_write_str("FB: format=");
        // serial_write_hex32(format);
        // serial_write_str(" bytes_per_pixel=");
        // serial_write_hex32(bytes_per_pixel as u32);
        // serial_write_str("\r\n");

        // Calcular pitch (bytes per scanline)
        let pitch = if pixels_per_scan_line > 0 {
            pixels_per_scan_line * bytes_per_pixel as u32
        } else {
            width * bytes_per_pixel as u32
        };

        // ✅ CORRECCIÓN: El pitch ya está en bytes, no multiplicar por bytes_per_pixel nuevamente
        // pixels_per_scan_line ya es el número de píxeles, multiplicarlo por bytes_per_pixel
        // nos da los bytes por línea correctamente
        
        // Calcular tamaño total del buffer
        let size = (height * pitch) as u64;
        
        // Configurar información del framebuffer
        // Evitar división por cero con validación adicional
        let pixels_per_line = if bytes_per_pixel > 0 && pitch > 0 {
            pitch / (bytes_per_pixel as u32)
        } else if pixels_per_scan_line > 0 {
            pixels_per_scan_line // Usar el valor original si hay problemas
        } else if width > 0 {
            width // Usar width como fallback
        } else {
            1920 // Valor por defecto seguro
        };

        self.info = FramebufferInfo {
            base_address,
            width,
            height,
            pixels_per_scan_line: pixels_per_line,
            pixel_format,
            red_mask: 0,      // Se configurarán según el formato
            green_mask: 0,
            blue_mask: 0,
            reserved_mask: 0,
        };
        
        // Configurar offsets según el formato
        self.configure_pixel_offsets();
        
        // ✅ MAPEO SEGURO: Verificar que la dirección sea válida
        if base_address < 0x1000 {
            return Err("Invalid framebuffer base address");
        }
        
        // Mapear memoria del framebuffer de forma segura
        self.buffer = base_address as *mut u8;
        
        // Validar que el mapeo sea válido
        if self.buffer.is_null() {
            return Err("Failed to map framebuffer memory");
        }
        
        // ✅ VALIDACIÓN ADICIONAL: Verificar que podemos leer el primer byte (de forma segura)
        unsafe {
            // Solo intentar leer si la dirección parece razonable
            if base_address >= 0x1000 && base_address < 0x100000000 { // Hasta 4GB
                // Intentar leer el primer byte para verificar que la memoria es accesible
                let test_byte = core::ptr::read_volatile(self.buffer);
                // Si llegamos aquí, la memoria es accesible
                core::ptr::write_volatile(self.buffer, test_byte); // Restaurar el valor original
            }
        }

        self.is_initialized = true;

        // ❌ REMOVER: No llamar clear_screen aquí para evitar page faults
        // La limpieza se hará después de la inicialización exitosa
        
        Ok(())
    }
    
    /// Inicializar aceleración de hardware con información de GPU
    pub fn init_hardware_acceleration(&mut self, gpu_info: &GpuInfo) -> Result<(), &'static str> {
        self.hardware_acceleration.initialize_with_gpu(gpu_info)
    }
    
    /// Obtener capacidades de aceleración de hardware
    pub fn get_acceleration_capabilities(&self) -> &AccelerationCapabilities {
        self.hardware_acceleration.get_capabilities()
    }
    
    /// Verificar si hay aceleración de hardware disponible
    pub fn has_hardware_acceleration(&self) -> bool {
        self.hardware_acceleration.has_acceleration()
    }
    
    /// Obtener tipo de aceleración de hardware
    pub fn get_acceleration_type(&self) -> HardwareAcceleration {
        self.hardware_acceleration.get_acceleration_type()
    }
    
    /// Configurar offsets de pixel según el formato
    fn configure_pixel_offsets(&mut self) {
        // Configurar máscaras según el formato de pixel
        match self.info.pixel_format {
            0 => { // RGB888
                self.info.red_mask = 0x00FF0000;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x000000FF;
                self.info.reserved_mask = 0xFF000000;
            },
            1 => { // BGR888
                self.info.red_mask = 0x000000FF;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x00FF0000;
                self.info.reserved_mask = 0xFF000000;
            },
            2 => { // RGBA8888
                self.info.red_mask = 0x00FF0000;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x000000FF;
                self.info.reserved_mask = 0xFF000000;
            },
            3 => { // BGRA8888
                self.info.red_mask = 0x0000FF00;
                self.info.green_mask = 0x00FF0000;
                self.info.blue_mask = 0xFF000000;
                self.info.reserved_mask = 0x000000FF;
            },
            _ => {
                // Formato desconocido, usar valores por defecto (RGBA8888)
                self.info.red_mask = 0x00FF0000;
                self.info.green_mask = 0x0000FF00;
                self.info.blue_mask = 0x000000FF;
                self.info.reserved_mask = 0xFF000000;
            }
        }
    }
    
    /// Verificar si el framebuffer está inicializado
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }
    
    /// Obtener información del framebuffer
    pub fn get_info(&self) -> &FramebufferInfo {
        &self.info
    }
    
    /// Obtener puntero a pixel en coordenadas (x, y)
    fn get_pixel_ptr(&self, x: u32, y: u32) -> *mut u8 {
        if !self.is_initialized || x >= self.info.width || y >= self.info.height {
            return ptr::null_mut();
        }

        // Verificar que el buffer no sea nulo
        if self.buffer.is_null() {
            return ptr::null_mut();
        }

        // Usar la función bytes_per_pixel() para consistencia
        let bytes_per_pixel = self.bytes_per_pixel() as u32;

        // Verificar que no haya overflow en el cálculo del offset
        let scan_line_bytes = match self.info.pixels_per_scan_line.checked_mul(bytes_per_pixel) {
            Some(val) => val,
            None => return ptr::null_mut(),
        };
        
        let y_offset = match y.checked_mul(scan_line_bytes) {
            Some(val) => val,
            None => return ptr::null_mut(),
        };
        
        let x_offset = match x.checked_mul(bytes_per_pixel) {
            Some(val) => val,
            None => return ptr::null_mut(),
        };
        
        let total_offset = match y_offset.checked_add(x_offset) {
            Some(val) => val,
            None => return ptr::null_mut(),
        };

        // Verificar que el offset esté dentro de límites razonables
        if total_offset > 0x7FFFFFFF { // Límite de 2GB
            return ptr::null_mut();
        }

        unsafe { self.buffer.offset(total_offset as isize) }
    }
    
    /// Llenar rectángulo con color
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        let end_x = core::cmp::min(x + width, self.info.width);
        let end_y = core::cmp::min(y + height, self.info.height);
        
        for py in y..end_y {
            for px in x..end_x {
                self.put_pixel(px, py, color);
            }
        }
    }
    
    /// Limpiar pantalla con color
    /// Limpia toda la pantalla con el color especificado.
    /// Limpiar la pantalla con un color específico (estilo wgpu moderno)
    /// 
    /// # Argumentos
    /// * `color` - Color con el que limpiar la pantalla
    /// 
    /// # Ejemplo
    /// ```rust
    /// framebuffer.clear_screen(Color::BLACK);
    /// framebuffer.clear_screen(Color::rgb(64, 128, 255));
    /// ```
    pub fn clear_screen(&mut self, color: Color) {
        let width = self.info.width;
        let height = self.info.height;
        
        // Limpiar pantalla línea por línea para mejor rendimiento
        for y in 0..height {
            for x in 0..width {
                let stride = self.info.pixels_per_scan_line;
                let fb_ptr = self.info.base_address as *mut u32;
                let offset = (y * stride + x) as isize;
                unsafe { core::ptr::write_volatile(fb_ptr.add(offset as usize), color.to_u32()); } // VERDE
            }
        }
    }

    pub fn draw_character(&mut self, x: u32, y: u32, ch: char, color: Color) {
        let fb_width = self.info.width;
        let fb_height = self.info.height;

        // Buscar el bitmap correspondiente al carácter en FONT_DATA, que es un array de tuplas (char, [u8; 8])
        let char_bitmap: [u8; 8] = *FONT_DATA
            .iter()
            .find(|(c, _)| *c == ch)
            .map(|(_, bitmap)| bitmap)
            .unwrap_or(&[0; 8]);


        for i in 0..64 {
            let px = i / 8;
            let py = i % 8;
            if (char_bitmap[px] & (1 << (7 - py))) != 0 {
                let pixel_x = x + px as u32;
                let pixel_y = y + py as u32;
                if pixel_x < fb_width && pixel_y < fb_height {
                    self.put_pixel(pixel_x, pixel_y, color);
                }
            }
        }
    }

    pub fn draw_text(&mut self, x: u32, y: u32, text: &String, color: Color) {
        let mut current_x = x;
        let char_width = 8;
        let char_height = 8;
    
        // Enfoque más seguro: iterar sobre los bytes de la cadena sin usar punteros directos
        for ch in text.chars() {
            if current_x + char_width > self.info.width {
                break;
            }

            // Llamar a la función de dibujo con el carácter Unicode
            self.draw_character(current_x, y, ch, color);
            current_x += char_width;
        }
    }
    
    /// Función optimizada para kernel que usa punteros directos sin asignaciones de memoria
    /// Esta es la versión recomendada para kernels como Eclipse OS
    pub fn write_text_kernel(&mut self, text: &str, color: Color) {
        let mut buffer_x: u32 = self.current_x;
        let mut current_y = 5;
        let char_width = 8;
        let char_height = 8;

        let mut current_ptr = text.as_ptr();
        let end_ptr = unsafe { current_ptr.add(text.len()) };

        while current_ptr < end_ptr {
            let char_code = unsafe { core::ptr::read_volatile(current_ptr) };

            // Verificar límites de pantalla
            if current_y + char_height > self.info.height {
                break;
            }
            if buffer_x + char_width > self.info.width {
                buffer_x = 10;
                self.current_x = 10;
                current_y = 5; // Reiniciar al principio de la pantalla
                self.clear_screen(Color::BLACK);
            }
            if current_y + char_height > self.info.height {
                buffer_x = 10;
                self.current_x = 10;
                current_y = 5; // Reiniciar al principio de la pantalla
                self.clear_screen(Color::BLACK);
            }
            // Llamar a la función de dibujo con el byte del carácter
            self.draw_character(buffer_x, current_y + 2, char_code as char, color);
            current_y += char_width;

            // Avanzar el puntero al siguiente byte
            current_ptr = unsafe { current_ptr.add(1) };
        }
        self.current_x += 16;
    }
    
    /// Versión optimizada con efecto de escritura para kernel
    pub fn write_text_kernel_typing(&mut self, x: u32, y: u32, text: &str, color: Color) {
        let mut current_x = x;
        let char_width = 8;

        // Obtener punteros directos
        let mut current_ptr = text.as_ptr();
        let end_ptr = unsafe { current_ptr.add(text.len()) };

        // Bucle optimizado con efecto de escritura
        while current_ptr < end_ptr {
            let char_code = unsafe { core::ptr::read_volatile(current_ptr) };

            // Verificar límites
            if current_x + char_width > self.info.width {
                break;
            }

            // Dibujar caracter
            self.draw_character(current_x, y, char_code as char, color);
            current_x += char_width;

            // Pausa para efecto de escritura (optimizada para kernel)
            for _ in 0..10000 {
                core::hint::spin_loop();
            }

            // Avanzar puntero
            current_ptr = unsafe { current_ptr.add(1) };
        }
    }

    pub fn ia_text(&mut self, x: u32, y: u32, text: &String, color: Color) {
        let char_width = 8;

        self.write_text_kernel(text.as_str(), color);
    }
    
    pub fn ia_text_with_delay(&mut self, x: u32, y: u32, text: &String, color: Color, delay_ms: u32) {
        let char_width = 8;

        self.write_text_kernel(text.as_str(), color);
            
        // Pausa personalizable para efecto de escritura
        let delay_cycles = delay_ms * 10000;
        for _ in 0..delay_cycles {
            core::hint::spin_loop();
        }
    }
    
    pub fn ia_typing_effect(&mut self, x: u32, y: u32, text: &String, color: Color) {
        let mut current_x = x;
        let char_width = 8;
        let mut cursor_x = current_x;
        let mut cursor_y = y + 10; // Cursor debajo del texto
        
        // Limpiar área donde se va a escribir
        self.fill_rect_fast(x, y, text.len() as u32 * char_width, 16, Color::BLACK);
        
        for (i, &byte) in text.as_bytes().iter().enumerate() {
            // Dibujar cursor parpadeante
            if i % 2 == 0 {
                self.draw_rect(cursor_x, cursor_y, 2, 8, Color::WHITE);
            } else {
                self.fill_rect_fast(cursor_x, cursor_y, 2, 8, Color::BLACK);
            }
            
            // Dibujar caracter
            self.draw_character(current_x, y, byte as char, color);
            current_x += char_width;
            cursor_x = current_x;
            
            // Pausa para efecto de escritura
            for _ in 0..50000 {
                core::hint::spin_loop();
            }
        }
        
        // Dibujar cursor final
        self.draw_rect(cursor_x, cursor_y, 2, 8, Color::WHITE);
    }
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        // ✅ VALIDACIÓN: Verificar que el framebuffer esté inicializado
        if !self.is_initialized {
            return;
        }
        
        unsafe {
            let stride = self.info.pixels_per_scan_line;
            let fb_ptr = self.info.base_address as *mut u32;
            core::ptr::write_volatile(fb_ptr.add((x * stride + y) as usize), color.to_u32());
        }
    }
    
    fn color_to_pixel(&self, color: Color) -> u32 {
        match self.info.pixel_format {
            0 => { // RGB888
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32)
            },
            1 => { // BGR888
                ((color.b as u32) << 16) | ((color.g as u32) << 8) | (color.r as u32)
            },
            2 => { // RGBA8888
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32) | ((color.a as u32) << 24)
            },
            3 => { // BGRA8888
                ((color.b as u32) << 16) | ((color.g as u32) << 8) | (color.r as u32) | ((color.a as u32) << 24)
            },
            _ => { // Por defecto o formato desconocido
                // Usar una representación segura por defecto
                ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32) | ((color.a as u32) << 24)
            }
        }
    }

    /// Leer un pixel
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        let pixel_ptr = self.get_pixel_ptr(x, y);
        if !pixel_ptr.is_null() {
            // Determinar bytes por pixel basado en el formato
            let bytes_per_pixel = match self.info.pixel_format {
                0 | 1 => 3, // RGB888, BGR888
                2 | 3 => 4, // RGBA8888, BGRA8888
                _ => 4,     // Por defecto 4 bytes
            };
            
            unsafe {
                let pixel_value = match bytes_per_pixel {
                    1 => *pixel_ptr as u32,
                    2 => {
                        let pixel_ptr_16 = pixel_ptr as *mut u16;
                        *pixel_ptr_16 as u32
                    },
                    3 => {
                        (*pixel_ptr as u32) |
                        ((*pixel_ptr.offset(1) as u32) << 8) |
                        ((*pixel_ptr.offset(2) as u32) << 16)
                    },
                    4 => {
                        let pixel_ptr_32 = pixel_ptr as *mut u32;
                        *pixel_ptr_32
                    },
                    _ => 0,
                };
                
                self.pixel_to_color(pixel_value)
            }
        } else {
            Color::BLACK
        }
    }
    
    /// Convertir valor de pixel a color
    fn pixel_to_color(&self, pixel_value: u32) -> Color {
        match self.info.pixel_format {
            2 => { // RGBA8888
                let r = ((pixel_value & self.info.red_mask) >> 16) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 8) as u8;
                let b = (pixel_value & self.info.blue_mask) as u8;
                let a = ((pixel_value & self.info.reserved_mask) >> 24) as u8;
                Color::new(r, g, b, a)
            },
            3 => { // BGRA8888
                let r = ((pixel_value & self.info.red_mask) >> 8) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 16) as u8;
                let b = ((pixel_value & self.info.blue_mask) >> 24) as u8;
                let a = (pixel_value & self.info.reserved_mask) as u8;
                Color::new(r, g, b, a)
            },
            0 => { // RGB888
                let r = ((pixel_value & self.info.red_mask) >> 16) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 8) as u8;
                let b = (pixel_value & self.info.blue_mask) as u8;
                Color::new(r, g, b, 255)
            },
            1 => { // BGR888
                let r = (pixel_value & self.info.red_mask) as u8;
                let g = ((pixel_value & self.info.green_mask) >> 8) as u8;
                let b = ((pixel_value & self.info.blue_mask) >> 16) as u8;
                Color::new(r, g, b, 255)
            },
            _ => Color::BLACK, // Formato desconocido
        }
    }

    /// Limpiar una región específica de la pantalla (estilo wgpu)
    /// 
    /// # Argumentos
    /// * `x` - Coordenada X de la esquina superior izquierda
    /// * `y` - Coordenada Y de la esquina superior izquierda  
    /// * `width` - Ancho de la región a limpiar
    /// * `height` - Alto de la región a limpiar
    /// * `color` - Color con el que limpiar la región
    /// 
    /// # Ejemplo
    /// ```rust
    /// // Limpiar solo la mitad superior de la pantalla
    /// framebuffer.clear_region(0, 0, 1024, 384, Color::BLUE);
    /// ```
    pub fn clear_region(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        // Verificar que el framebuffer esté inicializado
        if !self.is_initialized() {
            return;
        }

        // Verificar límites y ajustar si es necesario
        let x = x.min(self.info.width);
        let y = y.min(self.info.height);
        let width = width.min(self.info.width - x);
        let height = height.min(self.info.height - y);

        if width == 0 || height == 0 {
            return;
        }

        // Usar el método más eficiente según el tamaño
        if width * height > 10000 {
            self.clear_region_fast(x, y, width, height, color);
        } else {
            self.clear_region_safe(x, y, width, height, color);
        }
    }

    /// Limpiar región de forma segura (para regiones pequeñas)
    fn clear_region_safe(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        for py in y..y + height {
            for px in x..x + width {
                self.put_pixel(px, py, color);
            }
        }
    }

    /// Limpiar región de forma rápida (para regiones grandes)
    fn clear_region_fast(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        // Verificar que el buffer no sea nulo
        if self.buffer.is_null() {
            return;
        }

        let bytes_per_pixel = self.bytes_per_pixel();
        let pixel_value = self.color_to_pixel(color);
        
        // Calcular el offset inicial
        let start_offset = match self.calculate_pixel_offset(x, y) {
            Some(offset) => offset,
            None => return,
        };

        // Limpiar línea por línea para mejor rendimiento
        for row in 0..height {
            let row_offset = start_offset + (row * self.info.pixels_per_scan_line * bytes_per_pixel as u32);
            
            if row_offset + (width * bytes_per_pixel as u32) > 0x7FFFFFFF {
                return; // Evitar overflow
            }

                unsafe {
                let row_ptr = self.buffer.offset(row_offset as isize);
                
                match bytes_per_pixel {
                    1 => {
                        // 8 bits por píxel
                        core::ptr::write_bytes(row_ptr, pixel_value as u8, width as usize);
                    }
                    2 => {
                        // 16 bits por píxel
                        let pixel_16 = pixel_value as u16;
                        let mut ptr = row_ptr as *mut u16;
                        for _ in 0..width {
                            core::ptr::write_volatile(ptr, pixel_16);
                            ptr = ptr.add(1);
                        }
                    }
                    3 => {
                        // 24 bits por píxel (RGB)
                        let mut ptr = row_ptr;
                        for _ in 0..width {
                            core::ptr::write_volatile(ptr, (pixel_value >> 16) as u8); // R
                            core::ptr::write_volatile(ptr.add(1), (pixel_value >> 8) as u8); // G
                            core::ptr::write_volatile(ptr.add(2), pixel_value as u8); // B
                            ptr = ptr.add(3);
                        }
                    }
                    4 => {
                        // 32 bits por píxel
                        let mut ptr = row_ptr as *mut u32;
                        for _ in 0..width {
                            core::ptr::write_volatile(ptr, pixel_value);
                            ptr = ptr.add(1);
                        }
                    }
                    _ => {
                        // Fallback para otros formatos
                        self.clear_region_safe(x, y + row, width, 1, color);
                    }
                }
            }
        }
    }

    /// Calcular el offset de un píxel de forma segura
    fn calculate_pixel_offset(&self, x: u32, y: u32) -> Option<u32> {
        if x >= self.info.width || y >= self.info.height {
            return None;
        }

        let bytes_per_pixel = self.bytes_per_pixel();
        
        let scan_line_bytes = self.info.pixels_per_scan_line.checked_mul(bytes_per_pixel as u32)?;
        let y_offset = y.checked_mul(scan_line_bytes)?;
        let x_offset = x.checked_mul(bytes_per_pixel as u32)?;
        let total_offset = y_offset.checked_add(x_offset)?;

        if total_offset > 0x7FFFFFFF {
            return None;
        }

        Some(total_offset)
    }

    /// Limpiar pantalla con gradiente (estilo wgpu)
    /// 
    /// # Argumentos
    /// * `start_color` - Color inicial (esquina superior izquierda)
    /// * `end_color` - Color final (esquina inferior derecha)
    /// 
    /// # Ejemplo
    /// ```rust
    /// framebuffer.clear_screen_gradient(Color::BLUE, Color::BLACK);
    /// ```
    pub fn clear_screen_gradient(&mut self, start_color: Color, end_color: Color) {
        self.clear_region_gradient(0, 0, self.info.width, self.info.height, start_color, end_color);
    }

    /// Limpiar región con gradiente
    /// 
    /// # Argumentos
    /// * `x` - Coordenada X de la esquina superior izquierda
    /// * `y` - Coordenada Y de la esquina superior izquierda
    /// * `width` - Ancho de la región
    /// * `height` - Alto de la región
    /// * `start_color` - Color inicial
    /// * `end_color` - Color final
    pub fn clear_region_gradient(&mut self, x: u32, y: u32, width: u32, height: u32, 
                                start_color: Color, end_color: Color) {
        if !self.is_initialized() {
            return;
        }

        let x = x.min(self.info.width);
        let y = y.min(self.info.height);
        let width = width.min(self.info.width - x);
        let height = height.min(self.info.height - y);

        if width == 0 || height == 0 {
            return;
        }

        for py in y..y + height {
            let t = (py - y) as f32 / (height - 1) as f32;
            let color = self.lerp_color(start_color, end_color, t);
            
            for px in x..x + width {
                self.put_pixel(px, py, color);
            }
        }
    }

    /// Limpiar pantalla con patrón (estilo wgpu)
    /// 
    /// # Argumentos
    /// * `pattern` - Patrón de colores a usar
    /// * `tile_size` - Tamaño de cada tile del patrón
    /// 
    /// # Ejemplo
    /// ```rust
    /// let pattern = [Color::WHITE, Color::BLACK, Color::BLACK, Color::WHITE];
    /// framebuffer.clear_screen_pattern(&pattern, 16);
    /// ```
    pub fn clear_screen_pattern(&mut self, pattern: &[Color], tile_size: u32) {
        if !self.is_initialized() || pattern.is_empty() || tile_size == 0 {
            return;
        }

        let pattern_width = ModernGraphicsUtils::sqrt_approx(pattern.len() as u32) as u32;
        let pattern_height = pattern.len() as u32 / pattern_width;

        for y in 0..self.info.height {
            for x in 0..self.info.width {
                let pattern_x = (x / tile_size) % pattern_width;
                let pattern_y = (y / tile_size) % pattern_height;
                let pattern_index = (pattern_y * pattern_width + pattern_x) as usize;
                
                if pattern_index < pattern.len() {
                    self.put_pixel(x, y, pattern[pattern_index]);
                }
            }
        }
    }

    /// Limpiar pantalla con múltiples colores (estilo wgpu)
    /// 
    /// # Argumentos
    /// * `colors` - Array de colores para diferentes regiones
    /// * `regions` - Array de regiones (x, y, width, height) para cada color
    /// 
    /// # Ejemplo
    /// ```rust
    /// let colors = [Color::RED, Color::GREEN, Color::BLUE];
    /// let regions = [(0, 0, 512, 384), (512, 0, 512, 384), (0, 384, 1024, 384)];
    /// framebuffer.clear_screen_multi(&colors, &regions);
    /// ```
    pub fn clear_screen_multi(&mut self, colors: &[Color], regions: &[(u32, u32, u32, u32)]) {
        if !self.is_initialized() || colors.len() != regions.len() {
            return;
        }

        for (i, &(x, y, width, height)) in regions.iter().enumerate() {
            if i < colors.len() {
                self.clear_region(x, y, width, height, colors[i]);
            }
        }
    }

    /// Limpiar pantalla con efecto de desvanecimiento (estilo wgpu)
    /// 
    /// # Argumentos
    /// * `color` - Color base
    /// * `alpha` - Nivel de transparencia (0.0 = transparente, 1.0 = opaco)
    /// 
    /// # Ejemplo
    /// ```rust
    /// framebuffer.clear_screen_fade(Color::BLUE, 0.5);
    /// ```
    pub fn clear_screen_fade(&mut self, color: Color, alpha: f32) {
        if !self.is_initialized() {
            return;
        }

        let alpha = alpha.clamp(0.0, 1.0);
        let fade_color = Color::rgba(
            color.r,
            color.g,
            color.b,
            (color.a as f32 * alpha) as u8
        );

        self.clear_screen(fade_color);
    }
    
    /// Dibujar un círculo usando el algoritmo del punto medio
    pub fn draw_circle(&mut self, center_x: i32, center_y: i32, radius: u32, color: Color) {
        let mut x = 0i32;
        let mut y = radius as i32;
        let mut d = 1 - radius as i32;

        while x <= y {
            // Dibujar 8 puntos simétricos del círculo usando put_pixel
            self.put_pixel_safe(center_x + x, center_y + y, color);
            self.put_pixel_safe(center_x - x, center_y + y, color);
            self.put_pixel_safe(center_x + x, center_y - y, color);
            self.put_pixel_safe(center_x - x, center_y - y, color);
            self.put_pixel_safe(center_x + y, center_y + x, color);
            self.put_pixel_safe(center_x - y, center_y + x, color);
            self.put_pixel_safe(center_x + y, center_y - x, color);
            self.put_pixel_safe(center_x - y, center_y - x, color);

            if d < 0 {
                d += 2 * x + 3;
            } else {
                d += 2 * (x - y) + 5;
                y -= 1;
            }
            x += 1;
        }
    }
    
    /// Dibujar un pixel usando acceso directo a memoria (como clear_screen)
    fn draw_pixel_direct(fb_ptr: *mut u8, width: u32, height: u32, pixels_per_scan_line: u32, x: i32, y: i32, color_value: u32, bytes_per_pixel: u32) {
        if x >= 0 && x < width as i32 && y >= 0 && y < height as i32 {
            let offset = (y as u32 * pixels_per_scan_line + x as u32) * bytes_per_pixel;
            unsafe {
                let pixel_ptr = fb_ptr.add(offset as usize);
                match bytes_per_pixel {
                    4 => {
                        // Formato de 32 bpp (ej. RGBA8888 o ARGB8888)
                        core::ptr::write_volatile(pixel_ptr as *mut u32, color_value);
                    }
                    3 => {
                        // Formato de 24 bpp (ej. RGB888 o BGR888)
                        // Suponemos que color_value es 0x00RRGGBB
                        let r = ((color_value >> 16) & 0xFF) as u8;
                        let g = ((color_value >> 8) & 0xFF) as u8;
                        let b = (color_value & 0xFF) as u8;
                        
                        // El orden de bytes puede depender de la configuración del framebuffer (RGB vs BGR)
                        // Aquí asumimos un orden BGR, que es común.
                        core::ptr::write_volatile(pixel_ptr.add(0), b);
                        core::ptr::write_volatile(pixel_ptr.add(1), g);
                        core::ptr::write_volatile(pixel_ptr.add(2), r);
                    }
                    _ => {
                        // Otros formatos como 16 bpp (RGB565) o 8 bpp (indexado) requerirían
                        // una conversión de color más compleja. Por ahora no se soportan.
                    }
                }
            }
        }
    }

    /// Versión segura de put_pixel que no falla en coordenadas inválidas
    fn put_pixel_safe(&mut self, x: i32, y: i32, color: Color) {
        if x >= 0 && x < self.info.width as i32 && y >= 0 && y < self.info.height as i32 {
            self.put_pixel(x as u32, y as u32, color);
        }
    }
    
    /// Obtener dimensiones del framebuffer
    pub fn dimensions(&self) -> (u32, u32) {
        (self.info.width, self.info.height)
    }
    
    /// Verificar si las coordenadas están dentro de los límites
    pub fn is_valid_coordinate(&self, x: u32, y: u32) -> bool {
        x < self.info.width && y < self.info.height
    }
    
    /// Obtener el formato de pixel actual
    pub fn pixel_format(&self) -> PixelFormat {
        PixelFormat::from_uefi_format(self.info.pixel_format)
    }
    
    /// Obtener bytes por pixel
    pub fn bytes_per_pixel(&self) -> u8 {
        self.pixel_format().bytes_per_pixel()
    }
    
    /// Obtener pitch (bytes por línea)
    pub fn pitch(&self) -> u32 {
        self.info.pixels_per_scan_line * self.bytes_per_pixel() as u32
    }
    
    /// Llenar rectángulo optimizado (versión rápida para colores sólidos)
    pub fn fill_rect_fast(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        if !self.is_initialized || x >= self.info.width || y >= self.info.height {
            return;
        }
        
        let end_x = core::cmp::min(x + width, self.info.width);
        let end_y = core::cmp::min(y + height, self.info.height);
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        // Usar la función color_to_pixel para consistencia
        let pixel_value = self.color_to_pixel(color);
        
        unsafe {
            for py in y..end_y {
                let row_start = self.buffer.offset((py * pitch + x * bytes_per_pixel as u32) as isize);
                
                match bytes_per_pixel {
                    1 => {
                        core::ptr::write_bytes(row_start, pixel_value as u8, (end_x - x) as usize);
                    },
                    2 => {
                        let row_start_16 = row_start as *mut u16;
                        let pixel_16 = pixel_value as u16;
                        for px in x..end_x {
                            core::ptr::write_volatile(row_start_16.offset((px - x) as isize), pixel_16);
                        }
                    },
                    3 => {
                        for px in x..end_x {
                            let pixel_ptr = row_start.offset((px - x) as isize * 3);
                            core::ptr::write_volatile(pixel_ptr, (pixel_value & 0xFF) as u8);
                            core::ptr::write_volatile(pixel_ptr.offset(1), ((pixel_value >> 8) & 0xFF) as u8);
                            core::ptr::write_volatile(pixel_ptr.offset(2), ((pixel_value >> 16) & 0xFF) as u8);
                        }
                    },
                    4 => {
                        let row_start_32 = row_start as *mut u32;
                        for px in x..end_x {
                            core::ptr::write_volatile(row_start_32.offset((px - x) as isize), pixel_value);
                        }
                    },
                    _ => {},
                }
            }
        }
    }

    /// Llenar pantalla completa optimizado
    pub fn clear_screen_fast(&mut self, color: Color) {
        self.fill_rect_fast(0, 0, self.info.width, self.info.height, color);
    }
    
    /// Dibujar línea horizontal optimizada
    pub fn draw_hline(&mut self, x: u32, y: u32, width: u32, color: Color) {
        if y < self.info.height {
            self.fill_rect_fast(x, y, width, 1, color);
        }
    }
    
    /// Dibujar línea vertical optimizada
    pub fn draw_vline(&mut self, x: u32, y: u32, height: u32, color: Color) {
        if x < self.info.width {
            self.fill_rect_fast(x, y, 1, height, color);
        }
    }
    
    /// Copiar región de memoria optimizada (para blit rápido)
    pub fn blit_fast(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_fb: &FramebufferDriver) {
        if !self.is_initialized || !src_fb.is_initialized {
            return;
        }
        
        let end_x = core::cmp::min(dst_x + width, self.info.width);
        let end_y = core::cmp::min(dst_y + height, self.info.height);
        let actual_width = end_x - dst_x;
        let actual_height = end_y - dst_y;
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let src_pitch = src_fb.pitch();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..actual_height {
                let src_row = src_fb.buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                // Copiar línea completa de una vez
                core::ptr::copy_nonoverlapping(
                    src_row as *const u8,
                    dst_row as *mut u8,
                    (actual_width * bytes_per_pixel as u32) as usize
                );
            }
        }
    }
    
    /// Dibujar línea usando algoritmo de Bresenham
    pub fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color) {
        let fb_width = self.info.width;
        let fb_height = self.info.height;

        // Algoritmo de Bresenham para dibujar una línea
        let mut x = x1 as i32;
        let mut y = y1 as i32;
        let x2 = x2 as i32;
        let y2 = y2 as i32;

        let dx = (x2 - x).abs();
        let dy = (y2 - y).abs();
        let sx = if x < x2 { 1 } else { -1 };
        let sy = if y < y2 { 1 } else { -1 };
        let mut err = dx - dy;

        loop {
            if x >= 0 && (x as u32) < fb_width && y >= 0 && (y as u32) < fb_height {
                self.put_pixel(x as u32, y as u32, color);
            }
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
    
    /// Dibujar rectángulo
    pub fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        // Dibujar rectángulo
        for py in y..y + height {
            for px in x..x + width {
                if px < self.info.width && py < self.info.height {
                    self.put_pixel(px, py, color);
                }
            }
        }
    }
    
    /// Copiar región de framebuffer
    pub fn blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, width: u32, height: u32, src_fb: &FramebufferDriver) {
        let end_x = core::cmp::min(dst_x + width, self.info.width);
        let end_y = core::cmp::min(dst_y + height, self.info.height);
        
        for y in 0..height {
            for x in 0..width {
                let src_px = src_x + x;
                let src_py = src_y + y;
                let dst_px = dst_x + x;
                let dst_py = dst_y + y;
                
                if src_px < src_fb.info.width && src_py < src_fb.info.height &&
                   dst_px < self.info.width && dst_py < self.info.height {
                    let color = src_fb.get_pixel(src_px, src_py);
                    self.put_pixel(dst_px, dst_py, color);
                }
            }
        }
    }

    /// Escribir texto usando fuente simple de 8x16 pixels
    pub fn write_text(&mut self, x: u32, y: u32, text: &str, color: Color) {
        let mut current_x = x;
        let char_width = 8;
        let char_height = 8;

        for ch in text.chars().collect::<Vec<char>>() {
            let char_code = ch as u8;
            if current_x + char_width > self.info.width {
                break; // Salir si no cabe más texto
            }

            //self.draw_character(current_x, y, char_code, color);
            self.draw_rect(current_x, y, char_width, char_height, color);
            current_x += char_width;
        }
    }

    pub fn write_line(&mut self, text: &str, color: Color) {
        self.write_text(10, 10, text, color);
    }

    /// Obtener bitmap de un carácter (fuente simple)
    fn get_char_bitmap(&self, ch: char) -> [u8; 16] {
        let character_bitmap: [u8; 16] = [0; 16];
        // Aquí se define una fuente simple para caracteres ASCII.
        // Cada elemento del array es una fila de 8 píxeles.
        // Corrige el mapeo de índices para mayúsculas y minúsculas, y maneja mejor los caracteres desconocidos
        /*match ch {
            '0'..='9' => FONT_DATA[(ch as u8 - b'0') as usize],
            'A'..='Z' => FONT_DATA[(ch as u8 - b'A' + 10) as usize],
            'a'..='z' => FONT_DATA[(ch as u8 - b'a' + 36) as usize], // minúsculas después de los dígitos y mayúsculas
            ' ' => FONT_DATA[62], // Asegúrate de que el índice 62 sea el espacio en FONT_DATA
            _ => FONT_DATA[62],   // Usa el mismo índice para caracteres desconocidos
        }*/
        character_bitmap.clone()
    }
}

// Implementar traits para FramebufferDriver
impl Drawable for FramebufferDriver {
    fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        FramebufferDriver::put_pixel(self, x, y, color);
    }
    
    fn get_pixel(&self, x: u32, y: u32) -> Color {
        FramebufferDriver::get_pixel(self, x, y)
    }
    
    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        FramebufferDriver::fill_rect(self, x, y, width, height, color);
    }
    
    fn clear(&mut self, color: Color) {
        self.clear_screen(color);
    }
}

impl TextRenderer for FramebufferDriver {
    fn write_text(&mut self, x: u32, y: u32, text: &str, color: Color) {
        FramebufferDriver::write_text(self, x, y, text, color);
    }
    
    fn char_dimensions(&self) -> (u32, u32) {
        (8, 16) // Tamaño de carácter de la fuente actual
    }
}

impl GeometryRenderer for FramebufferDriver {
    fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color) {
        FramebufferDriver::draw_line(self, x1, y1, x2, y2, color);
    }
    
    fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        FramebufferDriver::draw_rect(self, x, y, width, height, color);
    }
    
    fn draw_circle(&mut self, center_x: i32, center_y: i32, radius: u32, color: Color) {
        FramebufferDriver::draw_circle(self, center_x, center_y, radius, color);
    }
}

impl Blittable for FramebufferDriver {
    fn blit_from<T: Drawable>(&mut self, _src: &T, src_x: u32, src_y: u32, 
                              dst_x: u32, dst_y: u32, width: u32, height: u32) {
        // Esta implementación es un placeholder
        // En una implementación real, necesitarías acceso a los datos de src
        // Por ahora, simplemente llenamos con negro
        self.fill_rect(dst_x, dst_y, width, height, Color::BLACK);
    }
}

impl HardwareAccelerated for FramebufferDriver {
    fn acceleration_type(&self) -> HardwareAcceleration {
        self.get_acceleration_type()
    }
    
    fn acceleration_capabilities(&self) -> AccelerationCapabilities {
        self.get_acceleration_capabilities().clone()
    }
    
    fn hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_blit {
            return Err("Hardware blit not supported");
        }
        
        if width > capabilities.max_blit_size.0 || height > capabilities.max_blit_size.1 {
            return Err("Blit size exceeds hardware limits");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
    
    fn hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_fill {
            return Err("Hardware fill not supported");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_fill(x, y, width, height, color)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_fill(x, y, width, height, color)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_fill(x, y, width, height, color)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
    
    fn hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                            color: Color, alpha: u8) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_alpha {
            return Err("Hardware alpha blending not supported");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_alpha_blend(x, y, width, height, color, alpha)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_alpha_blend(x, y, width, height, color, alpha)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_alpha_blend(x, y, width, height, color, alpha)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
    
    fn hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                      dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        if !self.has_hardware_acceleration() {
            return Err("Hardware acceleration not available");
        }
        
        let capabilities = self.get_acceleration_capabilities();
        if !capabilities.supports_hardware_scaling {
            return Err("Hardware scaling not supported");
        }
        
        match self.get_acceleration_type() {
            HardwareAcceleration::Intel2D => {
                self.intel_hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
            },
            HardwareAcceleration::Nvidia2D => {
                self.nvidia_hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
            },
            HardwareAcceleration::Amd2D => {
                self.amd_hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
            },
            _ => Err("Unsupported acceleration type")
        }
    }
}

// Implementaciones específicas de aceleración de hardware para cada fabricante

impl FramebufferDriver {
    /// Blit acelerado por hardware para Intel Graphics
    fn intel_hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                           width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, aquí se configurarían los registros de Intel
        // y se usaría la aceleración 2D del hardware
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                let src_row = src_buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                core::ptr::copy_nonoverlapping(
                    src_row,
                    dst_row as *mut u8,
                    (width * bytes_per_pixel as u32) as usize
                );
            }
        }
        
        Ok(())
    }
    
    /// Fill acelerado por hardware para Intel Graphics
    fn intel_hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, se usaría la aceleración 2D de Intel
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        let pixel = color.to_pixel(self.pixel_format());
        
        unsafe {
            for y in 0..height {
                let row = self.buffer.offset(((y) * pitch + x * bytes_per_pixel as u32) as isize);
                
                for x_offset in 0..width {
                    let pixel_offset = row.offset((x_offset * bytes_per_pixel as u32) as isize);
                    match bytes_per_pixel {
                        1 => *pixel_offset = pixel as u8,
                        2 => *(pixel_offset as *mut u16) = pixel as u16,
                        3 => {
                            *pixel_offset = (pixel >> 16) as u8;
                            *pixel_offset.offset(1) = (pixel >> 8) as u8;
                            *pixel_offset.offset(2) = pixel as u8;
                        },
                        4 => *(pixel_offset as *mut u32) = pixel,
                        _ => return Err("Unsupported pixel format for Intel acceleration")
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Alpha blending acelerado por hardware para Intel Graphics
    fn intel_hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                                  color: Color, alpha: u8) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, se usaría la aceleración 2D de Intel
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                for x_offset in 0..width {
                    let pixel_offset = self.buffer.offset(((y) * pitch + (x + x_offset) * bytes_per_pixel as u32) as isize);
                    
                    // Leer pixel actual
                    let current_color = match bytes_per_pixel {
                        4 => {
                            let pixel = *(pixel_offset as *const u32);
                            Color::from_pixel(pixel, self.pixel_format())
                        },
                        _ => return Err("Alpha blending requires 32-bit pixel format")
                    };
                    
                    // Aplicar alpha blending
                    let color_with_alpha = Color::new(color.r, color.g, color.b, alpha);
                    let blended_color = current_color.blend(color_with_alpha);
                    let blended_pixel = blended_color.to_pixel(self.pixel_format());
                    
                    *(pixel_offset as *mut u32) = blended_pixel;
                }
            }
        }
        
        Ok(())
    }
    
    /// Escalado acelerado por hardware para Intel Graphics
    fn intel_hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                            dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        // Implementación simplificada para Intel Graphics
        // En una implementación real, se usaría la aceleración 2D de Intel
        
        let scale_x = dst_width as f32 / src_width as f32;
        let scale_y = dst_height as f32 / src_height as f32;
        
        for y in 0..dst_height {
            for x in 0..dst_width {
                let src_x_f = (x as f32 / scale_x) as u32;
                let src_y_f = (y as f32 / scale_y) as u32;
                
                if src_x_f < src_width && src_y_f < src_height {
                    let src_color = self.get_pixel(src_x + src_x_f, src_y + src_y_f);
                    self.put_pixel(dst_x + x, dst_y + y, src_color);
                }
            }
        }
        
        Ok(())
    }
    
    /// Blit acelerado por hardware para NVIDIA
    fn nvidia_hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                            width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, aquí se configurarían los registros de NVIDIA
        // y se usaría la aceleración 2D del hardware
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                let src_row = src_buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                // NVIDIA puede manejar blits más grandes de una vez
                if width >= 64 {
                    // Blit optimizado para NVIDIA
                    core::ptr::copy_nonoverlapping(
                        src_row,
                        dst_row as *mut u8,
                        (width * bytes_per_pixel as u32) as usize
                    );
                } else {
                    // Blit pixel por pixel para áreas pequeñas
                    for x in 0..width {
                        let src_pixel = src_row.offset((x * bytes_per_pixel as u32) as isize);
                        let dst_pixel = dst_row.offset((x * bytes_per_pixel as u32) as isize);
                        core::ptr::copy_nonoverlapping(src_pixel, dst_pixel as *mut u8, bytes_per_pixel as usize);
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Fill acelerado por hardware para NVIDIA
    fn nvidia_hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, se usaría la aceleración 2D de NVIDIA
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        let pixel = color.to_pixel(self.pixel_format());
        
        unsafe {
            for y in 0..height {
                let row = self.buffer.offset(((y) * pitch + x * bytes_per_pixel as u32) as isize);
                
                // NVIDIA puede llenar líneas completas de una vez
                if width >= 32 {
                    core::ptr::write_bytes(row as *mut u8, pixel as u8, (width * bytes_per_pixel as u32) as usize);
                } else {
                    for x_offset in 0..width {
                        let pixel_offset = row.offset((x_offset * bytes_per_pixel as u32) as isize);
                        match bytes_per_pixel {
                            1 => *pixel_offset = pixel as u8,
                            2 => *(pixel_offset as *mut u16) = pixel as u16,
                            3 => {
                                *pixel_offset = (pixel >> 16) as u8;
                                *pixel_offset.offset(1) = (pixel >> 8) as u8;
                                *pixel_offset.offset(2) = pixel as u8;
                            },
                            4 => *(pixel_offset as *mut u32) = pixel,
                            _ => return Err("Unsupported pixel format for NVIDIA acceleration")
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Alpha blending acelerado por hardware para NVIDIA
    fn nvidia_hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                                   color: Color, alpha: u8) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, se usaría la aceleración 2D de NVIDIA
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                for x_offset in 0..width {
                    let pixel_offset = self.buffer.offset(((y) * pitch + (x + x_offset) * bytes_per_pixel as u32) as isize);
                    
                    // Leer pixel actual
                    let current_color = match bytes_per_pixel {
                        4 => {
                            let pixel = *(pixel_offset as *const u32);
                            Color::from_pixel(pixel, self.pixel_format())
                        },
                        _ => return Err("Alpha blending requires 32-bit pixel format")
                    };
                    
                    // Aplicar alpha blending
                    let color_with_alpha = Color::new(color.r, color.g, color.b, alpha);
                    let blended_color = current_color.blend(color_with_alpha);
                    let blended_pixel = blended_color.to_pixel(self.pixel_format());
                    
                    *(pixel_offset as *mut u32) = blended_pixel;
                }
            }
        }
        
        Ok(())
    }
    
    /// Escalado acelerado por hardware para NVIDIA
    fn nvidia_hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                             dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        // Implementación simplificada para NVIDIA
        // En una implementación real, se usaría la aceleración 2D de NVIDIA
        
        let scale_x = dst_width as f32 / src_width as f32;
        let scale_y = dst_height as f32 / src_height as f32;
        
        for y in 0..dst_height {
            for x in 0..dst_width {
                let src_x_f = (x as f32 / scale_x) as u32;
                let src_y_f = (y as f32 / scale_y) as u32;
                
                if src_x_f < src_width && src_y_f < src_height {
                    let src_color = self.get_pixel(src_x + src_x_f, src_y + src_y_f);
                    self.put_pixel(dst_x + x, dst_y + y, src_color);
                }
            }
        }
        
        Ok(())
    }
    
    /// Blit acelerado por hardware para AMD
    fn amd_hardware_blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                         width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, aquí se configurarían los registros de AMD
        // y se usaría la aceleración 2D del hardware
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let dst_pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                let src_row = src_buffer.offset(((src_y + y) * src_pitch + src_x * bytes_per_pixel as u32) as isize);
                let dst_row = self.buffer.offset(((dst_y + y) * dst_pitch + dst_x * bytes_per_pixel as u32) as isize);
                
                // AMD puede manejar blits medianos de una vez
                if width >= 32 {
                    core::ptr::copy_nonoverlapping(
                        src_row,
                        dst_row as *mut u8,
                        (width * bytes_per_pixel as u32) as usize
                    );
                } else {
                    for x in 0..width {
                        let src_pixel = src_row.offset((x * bytes_per_pixel as u32) as isize);
                        let dst_pixel = dst_row.offset((x * bytes_per_pixel as u32) as isize);
                        core::ptr::copy_nonoverlapping(src_pixel, dst_pixel as *mut u8, bytes_per_pixel as usize);
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Fill acelerado por hardware para AMD
    fn amd_hardware_fill(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, se usaría la aceleración 2D de AMD
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        let pixel = color.to_pixel(self.pixel_format());
        
        unsafe {
            for y in 0..height {
                let row = self.buffer.offset(((y) * pitch + x * bytes_per_pixel as u32) as isize);
                
                // AMD puede llenar líneas medianas de una vez
                if width >= 16 {
                    core::ptr::write_bytes(row as *mut u8, pixel as u8, (width * bytes_per_pixel as u32) as usize);
                } else {
                    for x_offset in 0..width {
                        let pixel_offset = row.offset((x_offset * bytes_per_pixel as u32) as isize);
                        match bytes_per_pixel {
                            1 => *pixel_offset = pixel as u8,
                            2 => *(pixel_offset as *mut u16) = pixel as u16,
                            3 => {
                                *pixel_offset = (pixel >> 16) as u8;
                                *pixel_offset.offset(1) = (pixel >> 8) as u8;
                                *pixel_offset.offset(2) = pixel as u8;
                            },
                            4 => *(pixel_offset as *mut u32) = pixel,
                            _ => return Err("Unsupported pixel format for AMD acceleration")
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Alpha blending acelerado por hardware para AMD
    fn amd_hardware_alpha_blend(&mut self, x: u32, y: u32, width: u32, height: u32, 
                                color: Color, alpha: u8) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, se usaría la aceleración 2D de AMD
        
        let bytes_per_pixel = self.bytes_per_pixel();
        let pitch = self.pitch();
        
        unsafe {
            for y in 0..height {
                for x_offset in 0..width {
                    let pixel_offset = self.buffer.offset(((y) * pitch + (x + x_offset) * bytes_per_pixel as u32) as isize);
                    
                    // Leer pixel actual
                    let current_color = match bytes_per_pixel {
                        4 => {
                            let pixel = *(pixel_offset as *const u32);
                            Color::from_pixel(pixel, self.pixel_format())
                        },
                        _ => return Err("Alpha blending requires 32-bit pixel format")
                    };
                    
                    // Aplicar alpha blending
                    let color_with_alpha = Color::new(color.r, color.g, color.b, alpha);
                    let blended_color = current_color.blend(color_with_alpha);
                    let blended_pixel = blended_color.to_pixel(self.pixel_format());
                    
                    *(pixel_offset as *mut u32) = blended_pixel;
                }
            }
        }
        
        Ok(())
    }
    
    /// Escalado acelerado por hardware para AMD
    fn amd_hardware_scale(&mut self, src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                          dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
        // Implementación simplificada para AMD
        // En una implementación real, se usaría la aceleración 2D de AMD
        
        let scale_x = dst_width as f32 / src_width as f32;
        let scale_y = dst_height as f32 / src_height as f32;
        
        for y in 0..dst_height {
            for x in 0..dst_width {
                let src_x_f = (x as f32 / scale_x) as u32;
                let src_y_f = (y as f32 / scale_y) as u32;
                
                if src_x_f < src_width && src_y_f < src_height {
                    let src_color = self.get_pixel(src_x + src_x_f, src_y + src_y_f);
                    self.put_pixel(dst_x + x, dst_y + y, src_color);
                }
            }
        }
        
        Ok(())
    }
}

/// Framebuffer global del sistema
static mut SYSTEM_FRAMEBUFFER: Option<FramebufferDriver> = None;

/// Inicializar framebuffer del sistema
pub fn init_framebuffer(base_address: u64,
                       width: u32,
                       height: u32,
                       pixels_per_scan_line: u32,
                       pixel_format: u32,
                       pixel_bitmask: u32) -> Result<(), &'static str> {
    let mut fb = FramebufferDriver::new();
    
    // Inicializar el framebuffer y verificar que fue exitoso
    match fb.init_from_uefi(base_address, width, height, pixels_per_scan_line, pixel_format, pixel_bitmask) {
        Ok(()) => {
            // Verificar que el framebuffer esté realmente inicializado
            if fb.is_initialized() {
                unsafe {
                    SYSTEM_FRAMEBUFFER = Some(fb);
                }
                Ok(())
            } else {
                Err("Framebuffer initialization failed - not properly initialized")
            }
        },
        Err(e) => {
            Err(e)
        }
    }
}

/// Obtener referencia al framebuffer del sistema
pub fn get_framebuffer() -> Option<&'static mut FramebufferDriver> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_mut()
    }
}

/// Verificar si el framebuffer está disponible
pub fn is_framebuffer_available() -> bool {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_ref().map_or(false, |fb| fb.is_initialized())
    }
}

/// Obtener información del framebuffer
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_ref().map(|fb| fb.info)
    }
}

/// Inicializar framebuffer con información UEFI
/// Esta función es llamada desde el punto de entrada UEFI
pub fn init_framebuffer_from_uefi(uefi_fb_info: &FramebufferInfo) -> Result<(), &'static str> {
    // ✅ CORREGIR: Mapeo correcto de formatos UEFI
    let pixel_format = match uefi_fb_info.pixel_format {
        0 => 0, // PixelRedGreenBlueReserved8BitPerColor
        1 => 1, // PixelBlueGreenRedReserved8BitPerColor  
        2 => 2, // PixelBitMask
        3 => 3, // PixelBltOnly
        _ => 0, // Default to RGB
    };

    // ✅ CORREGIR: Crear bitmask correcto sin truncar
    let pixel_bitmask = uefi_fb_info.red_mask |
                       (uefi_fb_info.green_mask << 8) |
                       (uefi_fb_info.blue_mask << 16) |
                       (uefi_fb_info.reserved_mask << 24);

    init_framebuffer(
        uefi_fb_info.base_address,
        uefi_fb_info.width,
        uefi_fb_info.height,
        uefi_fb_info.pixels_per_scan_line,
        pixel_format,
        pixel_bitmask
    )
}

/// Escribir texto en el framebuffer usando fuente simple
pub fn write_text(x: u32, y: u32, text: &str, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.write_text(x, y, text, color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Limpiar pantalla del framebuffer
pub fn clear_screen(color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.clear_screen(color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar un rectángulo con bordes redondeados
pub fn draw_rounded_rect(x: u32, y: u32, width: u32, height: u32, radius: u32, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        // Implementación simple de rectángulo redondeado
        // Dibujar las esquinas como círculos
        if radius > 0 {
            fb.draw_circle((x + radius) as i32, (y + radius) as i32, radius, color);
            fb.draw_circle((x + width - radius - 1) as i32, (y + radius) as i32, radius, color);
            fb.draw_circle((x + radius) as i32, (y + height - radius - 1) as i32, radius, color);
            fb.draw_circle((x + width - radius - 1) as i32, (y + height - radius - 1) as i32, radius, color);
        }
        
        // Dibujar los lados rectos
        if width > radius * 2 {
            fb.fill_rect(x + radius, y, width - radius * 2, radius, color);
            fb.fill_rect(x + radius, y + height - radius, width - radius * 2, radius, color);
        }
        if height > radius * 2 {
            fb.fill_rect(x, y + radius, radius, height - radius * 2, color);
            fb.fill_rect(x + width - radius, y + radius, radius, height - radius * 2, color);
        }
        
        // Llenar el centro
        if width > radius * 2 && height > radius * 2 {
            fb.fill_rect(x + radius, y + radius, width - radius * 2, height - radius * 2, color);
        }
        
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar un gradiente horizontal
pub fn draw_horizontal_gradient(x: u32, y: u32, width: u32, height: u32, 
                               start_color: Color, end_color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        for i in 0..width {
            let factor = i as f32 / (width - 1) as f32;
            let r = (start_color.r as f32 * (1.0 - factor) + end_color.r as f32 * factor) as u8;
            let g = (start_color.g as f32 * (1.0 - factor) + end_color.g as f32 * factor) as u8;
            let b = (start_color.b as f32 * (1.0 - factor) + end_color.b as f32 * factor) as u8;
            let a = (start_color.a as f32 * (1.0 - factor) + end_color.a as f32 * factor) as u8;
            
            let color = Color::new(r, g, b, a);
            fb.fill_rect(x + i, y, 1, height, color);
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar un gradiente vertical
pub fn draw_vertical_gradient(x: u32, y: u32, width: u32, height: u32, 
                             start_color: Color, end_color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        for i in 0..height {
            let factor = i as f32 / (height - 1) as f32;
            let r = (start_color.r as f32 * (1.0 - factor) + end_color.r as f32 * factor) as u8;
            let g = (start_color.g as f32 * (1.0 - factor) + end_color.g as f32 * factor) as u8;
            let b = (start_color.b as f32 * (1.0 - factor) + end_color.b as f32 * factor) as u8;
            let a = (start_color.a as f32 * (1.0 - factor) + end_color.a as f32 * factor) as u8;
            
            let color = Color::new(r, g, b, a);
            fb.fill_rect(x, y + i, width, 1, color);
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar texto con sombra
pub fn write_text_with_shadow(x: u32, y: u32, text: &str, text_color: Color, 
                             shadow_color: Color, shadow_offset: (i32, i32)) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        // Dibujar sombra
        let shadow_x = (x as i32 + shadow_offset.0).max(0) as u32;
        let shadow_y = (y as i32 + shadow_offset.1).max(0) as u32;
        fb.write_text(shadow_x, shadow_y, text, shadow_color);
        
        // Dibujar texto principal
        fb.write_text(x, y, text, text_color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Obtener información detallada del framebuffer
pub fn get_framebuffer_details() -> Option<FramebufferDetails> {
    if let Some(fb) = get_framebuffer() {
        Some(FramebufferDetails {
            width: fb.info.width,
            height: fb.info.height,
            pixel_format: fb.pixel_format(),
            bytes_per_pixel: fb.bytes_per_pixel(),
            pitch: fb.pitch(),
            total_size: (fb.info.height * fb.pitch()) as u64,
            is_initialized: fb.is_initialized(),
        })
    } else {
        None
    }
}

/// Información detallada del framebuffer
#[derive(Debug, Clone, Copy)]
pub struct FramebufferDetails {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub bytes_per_pixel: u8,
    pub pitch: u32,
    pub total_size: u64,
    pub is_initialized: bool,
}

/// Sistema de capas para composición
pub struct LayerManager {
    layers: [Option<Layer>; 8], // Máximo 8 capas
    active_layers: u8,
}

/// Una capa individual
#[derive(Clone, Copy)]
pub struct Layer {
    pub id: u8,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub alpha: u8, // 0-255
    pub buffer: *mut u8,
    pub pitch: u32,
    pub bytes_per_pixel: u8,
}

impl LayerManager {
    pub fn new() -> Self {
        Self {
            layers: [None; 8],
            active_layers: 0,
        }
    }
    
    /// Crear una nueva capa
    pub fn create_layer(&mut self, id: u8, width: u32, height: u32, bytes_per_pixel: u8) -> Result<(), &'static str> {
        if id >= 8 {
            return Err("Layer ID must be less than 8");
        }
        
        if self.layers[id as usize].is_some() {
            return Err("Layer already exists");
        }
        
        let pitch = width * bytes_per_pixel as u32;
        let size = (height * pitch) as usize;
        
        // En un sistema real, aquí asignarías memoria
        // Por ahora, usamos un puntero nulo (esto necesitaría un allocator)
        let buffer = core::ptr::null_mut();
        
        self.layers[id as usize] = Some(Layer {
            id,
            x: 0,
            y: 0,
            width,
            height,
            visible: true,
            alpha: 255,
            buffer,
            pitch,
            bytes_per_pixel,
        });
        
        self.active_layers |= 1 << id;
        Ok(())
    }
    
    /// Eliminar una capa
    pub fn remove_layer(&mut self, id: u8) -> Result<(), &'static str> {
        if id >= 8 {
            return Err("Layer ID must be less than 8");
        }
        
        if self.layers[id as usize].is_none() {
            return Err("Layer does not exist");
        }
        
        self.layers[id as usize] = None;
        self.active_layers &= !(1 << id);
        Ok(())
    }
    
    /// Mostrar/ocultar una capa
    pub fn set_layer_visibility(&mut self, id: u8, visible: bool) -> Result<(), &'static str> {
        if let Some(layer) = self.layers[id as usize].as_mut() {
            layer.visible = visible;
            Ok(())
        } else {
            Err("Layer does not exist")
        }
    }
    
    /// Establecer posición de una capa
    pub fn set_layer_position(&mut self, id: u8, x: u32, y: u32) -> Result<(), &'static str> {
        if let Some(layer) = self.layers[id as usize].as_mut() {
            layer.x = x;
            layer.y = y;
            Ok(())
        } else {
            Err("Layer does not exist")
        }
    }
    
    /// Establecer transparencia de una capa
    pub fn set_layer_alpha(&mut self, id: u8, alpha: u8) -> Result<(), &'static str> {
        if let Some(layer) = self.layers[id as usize].as_mut() {
            layer.alpha = alpha;
            Ok(())
        } else {
            Err("Layer does not exist")
        }
    }
    
    /// Componer todas las capas en el framebuffer principal
    pub fn compose_layers(&self, target_fb: &mut FramebufferDriver) {
        if !target_fb.is_initialized() {
            return;
        }
        
        // Limpiar el framebuffer principal
        target_fb.clear_screen_fast(Color::TRANSPARENT);
        
        // Dibujar capas en orden (capa 0 es la más baja)
        for i in 0..8 {
            if let Some(layer) = &self.layers[i] {
                if layer.visible && !layer.buffer.is_null() {
                    self.blit_layer_to_framebuffer(layer, target_fb);
                }
            }
        }
    }
    
    /// Copiar una capa al framebuffer principal
    fn blit_layer_to_framebuffer(&self, layer: &Layer, target_fb: &mut FramebufferDriver) {
        if layer.alpha == 0 {
            return; // Capa completamente transparente
        }
        
        let end_x = core::cmp::min(layer.x + layer.width, target_fb.info.width);
        let end_y = core::cmp::min(layer.y + layer.height, target_fb.info.height);
        let actual_width = end_x - layer.x;
        let actual_height = end_y - layer.y;
        
        if actual_width == 0 || actual_height == 0 {
            return;
        }
        
        // Si la capa es completamente opaca, usar blit rápido
        if layer.alpha == 255 {
            target_fb.blit_fast(0, 0, layer.x, layer.y, actual_width, actual_height, 
                               &FramebufferDriver {
                                   info: FramebufferInfo {
                                       base_address: layer.buffer as u64,
                                       width: layer.width,
                                       height: layer.height,
                                       pixels_per_scan_line: layer.width,
                                       pixel_format: target_fb.info.pixel_format,
                                       red_mask: 0,
                                       green_mask: 0,
                                       blue_mask: 0,
                                       reserved_mask: 0,
                                   },
                                   buffer: layer.buffer,
                                   is_initialized: true,
                                   hardware_acceleration: HardwareAccelerationManager::new(),
                                   current_x: 0,
                                });
        } else {
            // Alpha blending pixel por pixel
            for y in 0..actual_height {
                for x in 0..actual_width {
                    // Leer pixel de la capa
                    let layer_pixel = self.get_layer_pixel(layer, x, y);
                    
                    // Leer pixel del framebuffer principal
                    let fb_pixel = target_fb.get_pixel(layer.x + x, layer.y + y);
                    
                    // Aplicar alpha blending
                    let blended = layer_pixel.blend(fb_pixel);
                    
                    // Escribir pixel resultante
                    target_fb.put_pixel(layer.x + x, layer.y + y, blended);
                }
            }
        }
    }
    
    /// Obtener pixel de una capa
    fn get_layer_pixel(&self, layer: &Layer, x: u32, y: u32) -> Color {
        if x >= layer.width || y >= layer.height || layer.buffer.is_null() {
            return Color::TRANSPARENT;
        }
        
        let offset = (y * layer.pitch + x * layer.bytes_per_pixel as u32) as isize;
        let pixel_ptr = unsafe { layer.buffer.offset(offset) };
        
        // Leer pixel según el formato (simplificado)
        unsafe {
            let pixel_value = match layer.bytes_per_pixel {
                1 => *pixel_ptr as u32,
                2 => {
                    let pixel_ptr_16 = pixel_ptr as *mut u16;
                    *pixel_ptr_16 as u32
                },
                3 => {
                    (*pixel_ptr as u32) |
                    ((*pixel_ptr.offset(1) as u32) << 8) |
                    ((*pixel_ptr.offset(2) as u32) << 16)
                },
                4 => {
                    let pixel_ptr_32 = pixel_ptr as *mut u32;
                    *pixel_ptr_32
                },
                _ => 0,
            };
            
            // Convertir a Color (simplificado)
            Color::from_hex_alpha(pixel_value)
        }
    }
}

/// Manager global de capas
static mut LAYER_MANAGER: Option<LayerManager> = None;

/// Inicializar el sistema de capas
pub fn init_layer_system() {
    unsafe {
        LAYER_MANAGER = Some(LayerManager::new());
    }
}

/// Obtener el manager de capas
pub fn get_layer_manager() -> Option<&'static mut LayerManager> {
    unsafe {
        LAYER_MANAGER.as_mut()
    }
}

/// Componer todas las capas
pub fn compose_all_layers() -> Result<(), &'static str> {
    if let Some(layer_mgr) = get_layer_manager() {
        if let Some(fb) = get_framebuffer() {
            layer_mgr.compose_layers(fb);
            Ok(())
        } else {
            Err("Framebuffer not initialized")
        }
    } else {
        Err("Layer system not initialized")
    }
}

/// Sprite o bitmap para dibujar
pub struct Sprite {
    pub width: u32,
    pub height: u32,
    pub data: &'static [u8], // Datos de pixel en formato RGBA8888
    pub has_alpha: bool,
}

impl Sprite {
    /// Crear sprite desde datos de pixel
    pub fn new(width: u32, height: u32, data: &'static [u8], has_alpha: bool) -> Self {
        Self {
            width,
            height,
            data,
            has_alpha,
        }
    }
    
    /// Obtener pixel en coordenadas (x, y)
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        if x >= self.width || y >= self.height {
            return Color::TRANSPARENT;
        }
        
        let index = ((y * self.width + x) * 4) as usize;
        if index + 3 >= self.data.len() {
            return Color::TRANSPARENT;
        }
        
        Color::new(
            self.data[index],
            self.data[index + 1],
            self.data[index + 2],
            self.data[index + 3],
        )
    }
    
    /// Verificar si el pixel es transparente
    pub fn is_pixel_transparent(&self, x: u32, y: u32) -> bool {
        if !self.has_alpha {
            return false;
        }
        
        let pixel = self.get_pixel(x, y);
        pixel.a == 0
    }
}

/// Dibujar sprite en el framebuffer
pub fn draw_sprite(x: u32, y: u32, sprite: &Sprite) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        draw_sprite_to_framebuffer(fb, x, y, sprite);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar sprite en un framebuffer específico
fn draw_sprite_to_framebuffer(fb: &mut FramebufferDriver, x: u32, y: u32, sprite: &Sprite) {
    let end_x = core::cmp::min(x + sprite.width, fb.info.width);
    let end_y = core::cmp::min(y + sprite.height, fb.info.height);
    
    for sy in 0..(end_y - y) {
        for sx in 0..(end_x - x) {
            let pixel = sprite.get_pixel(sx, y + sy);
            
            if !sprite.is_pixel_transparent(sx, y + sy) {
                fb.put_pixel(x + sx, y + sy, pixel);
            }
        }
    }
}

/// Dibujar sprite con escalado
pub fn draw_sprite_scaled(x: u32, y: u32, sprite: &Sprite, scale: f32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let scaled_width = (sprite.width as f32 * scale) as u32;
        let scaled_height = (sprite.height as f32 * scale) as u32;
        
        let end_x = core::cmp::min(x + scaled_width, fb.info.width);
        let end_y = core::cmp::min(y + scaled_height, fb.info.height);
        
        for dy in 0..(end_y - y) {
            for dx in 0..(end_x - x) {
                let sx = (dx as f32 / scale) as u32;
                let sy = (dy as f32 / scale) as u32;
                
                if sx < sprite.width && sy < sprite.height {
                    let pixel = sprite.get_pixel(sx, sy);
                    
                    if !sprite.is_pixel_transparent(sx, sy) {
                        fb.put_pixel(x + dx, y + dy, pixel);
                    }
                }
            }
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Dibujar sprite con rotación (simplificado - solo 90 grados)
pub fn draw_sprite_rotated(x: u32, y: u32, sprite: &Sprite, rotation: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let (width, height) = match rotation % 4 {
            1 | 3 => (sprite.height, sprite.width), // 90 o 270 grados
            _ => (sprite.width, sprite.height),     // 0 o 180 grados
        };
        
        let end_x = core::cmp::min(x + width, fb.info.width);
        let end_y = core::cmp::min(y + height, fb.info.height);
        
        for dy in 0..(end_y - y) {
            for dx in 0..(end_x - x) {
                let (sx, sy) = match rotation % 4 {
                    0 => (dx, dy),                                    // 0 grados
                    1 => (dy, sprite.height - 1 - dx),               // 90 grados
                    2 => (sprite.width - 1 - dx, sprite.height - 1 - dy), // 180 grados
                    3 => (sprite.width - 1 - dy, dx),                // 270 grados
                    _ => (dx, dy),
                };
                
                if sx < sprite.width && sy < sprite.height {
                    let pixel = sprite.get_pixel(sx, sy);
                    
                    if !sprite.is_pixel_transparent(sx, sy) {
                        fb.put_pixel(x + dx, y + dy, pixel);
                    }
                }
            }
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Crear sprite desde patrón de colores
/// Nota: Esta función requiere un allocator para funcionar correctamente
pub fn create_sprite_from_pattern(width: u32, height: u32, pattern: &[Color]) -> Option<Sprite> {
    if pattern.len() != (width * height) as usize {
        return None;
    }
    
    // En un entorno no_std sin allocator, no podemos crear Vec dinámicamente
    // Esta función está aquí para completar la API, pero necesita un allocator
    // para funcionar correctamente en un sistema real
    None
}

/// Dibujar patrón de colores directamente
pub fn draw_pattern(x: u32, y: u32, width: u32, height: u32, pattern: &[Color]) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        if pattern.len() != (width * height) as usize {
            return Err("Pattern size mismatch");
        }
        
        let end_x = core::cmp::min(x + width, fb.info.width);
        let end_y = core::cmp::min(y + height, fb.info.height);
        
        for dy in 0..(end_y - y) {
            for dx in 0..(end_x - x) {
                let pattern_index = ((dy * width + dx) as usize);
                if pattern_index < pattern.len() {
                    fb.put_pixel(x + dx, y + dy, pattern[pattern_index]);
                }
            }
        }
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Escribir texto escalado
pub fn write_text_scaled(x: u32, y: u32, text: &str, color: Color, scale: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let char_width = 8 * scale;
        let mut current_x = x;

        for ch in text.chars() {
            if current_x + char_width > fb.dimensions().0 {
                break;
            }
            draw_char_scaled(fb, current_x, y, ch, color, scale);
            current_x += char_width;
        }
        Ok(())
    } else {
        Err("Framebuffer not available")
    }
}

/// Dibujar un carácter escalado
fn draw_char_scaled(fb: &mut FramebufferDriver, x: u32, y: u32, ch: char, color: Color, scale: u32) {
    if scale == 0 {
        return;
    }

    let bitmap = fb.get_char_bitmap(ch);

    for row in 0..16 {
        let bits = bitmap[row];
        for col in 0..8 {
            if (bits & (1 << (7 - col))) != 0 {
                // Dibujar pixel escalado
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = x + col * scale + sx;
                        let py = y + row as u32 * scale + sy;
                        if px < fb.info.width && py < fb.info.height {
                            fb.put_pixel(px, py, color);
                        }
                    }
                }
            }
        }
    }
}

/// Escribir texto con fondo
pub fn write_text_with_background(x: u32, y: u32, text: &str, 
                                 text_color: Color, bg_color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let char_width = 8;
        let char_height = 16;
        let text_width = text.len() as u32 * char_width;
        
        // Dibujar fondo
        fb.fill_rect(x, y, text_width, char_height, bg_color);
        
        // Dibujar texto
        fb.write_text(x, y, text, text_color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Escribir texto centrado
pub fn write_text_centered(y: u32, text: &str, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        let char_width = 8;
        let text_width = text.len() as u32 * char_width;
        let x = if text_width < fb.info.width {
            (fb.info.width - text_width) / 2
        } else {
            0
        };
        
        fb.write_text(x, y, text, color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

// Funciones globales para aceleración de hardware

/// Inicializar aceleración de hardware del framebuffer
pub fn init_hardware_acceleration(gpu_info: &GpuInfo) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.init_hardware_acceleration(gpu_info)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Verificar si hay aceleración de hardware disponible
pub fn has_hardware_acceleration() -> bool {
    if let Some(fb) = get_framebuffer() {
        fb.has_hardware_acceleration()
    } else {
        false
    }
}

/// Obtener tipo de aceleración de hardware
pub fn get_acceleration_type() -> HardwareAcceleration {
    if let Some(fb) = get_framebuffer() {
        fb.get_acceleration_type()
    } else {
        HardwareAcceleration::None
    }
}

/// Obtener capacidades de aceleración de hardware
pub fn get_acceleration_capabilities() -> Option<AccelerationCapabilities> {
    if let Some(fb) = get_framebuffer() {
        Some(fb.get_acceleration_capabilities().clone())
    } else {
        None
    }
}

/// Blit acelerado por hardware
pub fn hardware_blit(src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, 
                     width: u32, height: u32, src_buffer: *const u8, src_pitch: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_blit(src_x, src_y, dst_x, dst_y, width, height, src_buffer, src_pitch)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Fill acelerado por hardware
pub fn hardware_fill(x: u32, y: u32, width: u32, height: u32, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_fill(x, y, width, height, color)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Alpha blending acelerado por hardware
pub fn hardware_alpha_blend(x: u32, y: u32, width: u32, height: u32, 
                            color: Color, alpha: u8) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_alpha_blend(x, y, width, height, color, alpha)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Escalado acelerado por hardware
pub fn hardware_scale(src_x: u32, src_y: u32, src_width: u32, src_height: u32,
                      dst_x: u32, dst_y: u32, dst_width: u32, dst_height: u32) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.hardware_scale(src_x, src_y, src_width, src_height, dst_x, dst_y, dst_width, dst_height)
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Obtener información detallada de aceleración de hardware
pub fn get_hardware_acceleration_info() -> Option<String> {
    if let Some(fb) = get_framebuffer() {
        let capabilities = fb.get_acceleration_capabilities();
        let accel_type = fb.get_acceleration_type();
        
        let info = format!(
            "Hardware Acceleration: {:?}\n\
             Blit Support: {}\n\
             Fill Support: {}\n\
             Alpha Support: {}\n\
             Gradients Support: {}\n\
             Scaling Support: {}\n\
             Rotation Support: {}\n\
             Max Blit Size: {}x{}\n\
             Memory Bandwidth: {} MB/s",
            accel_type,
            capabilities.supports_hardware_blit,
            capabilities.supports_hardware_fill,
            capabilities.supports_hardware_alpha,
            capabilities.supports_hardware_gradients,
            capabilities.supports_hardware_scaling,
            capabilities.supports_hardware_rotation,
            capabilities.max_blit_size.0,
            capabilities.max_blit_size.1,
            capabilities.memory_bandwidth
        );
        
        Some(info)
    } else {
        None
    }
}

// ============================================================================
// INTEGRACIÓN DEL PIPELINE MODERNO CON FRAMEBUFFER EXISTENTE
// ============================================================================

/// Extensión del FramebufferDriver para usar el pipeline moderno
impl FramebufferDriver {
    /// Crear un pipeline de renderizado moderno asociado a este framebuffer
    pub fn create_modern_pipeline(&self) -> ModernRenderPipeline {
        ModernRenderPipeline::new()
    }
    
    /// Crear un motor de renderizado de texto
    pub fn create_text_renderer(&self) -> ModernTextRenderer {
        ModernTextRenderer::new()
    }
    
    /// Dibujar texto con configuración avanzada
    pub fn draw_text_advanced(&mut self, x: i32, y: i32, text: &str, config: &TextConfig) {
        let layout = self.create_text_renderer().layout_text(text, config);
        self.render_text_layout(x, y, &layout, config);
    }
    
    /// Obtener patrón de bits para un carácter (8x8)
    fn get_char_pattern(&self, ch: char) -> [u8; 8] {
        match ch {
            'A' => [0x18, 0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x00],
            'B' => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x7C, 0x00],
            'C' => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00],
            'D' => [0x78, 0x6C, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00],
            'E' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x7E, 0x00],
            'F' => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x00],
            'G' => [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x3C, 0x00],
            'H' => [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
            'I' => [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00],
            'J' => [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x6C, 0x38, 0x00],
            'K' => [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x00],
            'L' => [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00],
            'M' => [0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x00],
            'N' => [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x00],
            'O' => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
            'P' => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x00],
            'Q' => [0x3C, 0x66, 0x66, 0x66, 0x6A, 0x6C, 0x36, 0x00],
            'R' => [0x7C, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0x00],
            'S' => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x66, 0x3C, 0x00],
            'T' => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00],
            'U' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00],
            'V' => [0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00],
            'W' => [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00],
            'X' => [0x66, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0x66, 0x00],
            'Y' => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x00],
            'Z' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00],
            '0' => [0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x3C, 0x00],
            '1' => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00],
            '2' => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x30, 0x7E, 0x00],
            '3' => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x66, 0x3C, 0x00],
            '4' => [0x0C, 0x1C, 0x3C, 0x6C, 0x7E, 0x0C, 0x0C, 0x00],
            '5' => [0x7E, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00],
            '6' => [0x3C, 0x66, 0x60, 0x7C, 0x66, 0x66, 0x3C, 0x00],
            '7' => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x00],
            '8' => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x3C, 0x00],
            '9' => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x66, 0x3C, 0x00],
            '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00],
            ',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30],
            ':' => [0x00, 0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00],
            ';' => [0x00, 0x00, 0x18, 0x18, 0x00, 0x18, 0x18, 0x30],
            '!' => [0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x18, 0x00],
            '?' => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x00, 0x18, 0x00],
            _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Carácter no soportado
        }
    }
    
    /*pub fn draw_character(&mut self, x: u32, y: u32, character: char, color: Color) {
        self.draw_char(x, y, character, color);
    }*/
    
    /// Dibujar texto simple (compatible con la API existente)
    pub fn draw_text_simple(&mut self, x: u32, y: u32, text: &str, color: Color) {
        // Verificar que el framebuffer esté inicializado
        if !self.is_initialized() {
            return; // Salir silenciosamente si no está inicializado
        }
        
        // Verificar que las coordenadas estén dentro de los límites
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        
        let font = Font::default_font();
        let config = TextConfig::new(font, color);
        self.draw_text_advanced(x as i32, y as i32, text, &config);
    }
    
    /// Dibujar texto con efectos
    pub fn draw_text_with_effect(&mut self, x: i32, y: i32, text: &str, config: &TextConfig) {
        match config.effect {
            TextEffect::None => {
                self.draw_text_advanced(x, y, text, config);
            },
            TextEffect::Shadow { offset_x, offset_y, blur, color } => {
                // Dibujar sombra primero
                let mut shadow_config = config.clone();
                shadow_config.color = color;
                shadow_config.effect = TextEffect::None;
                self.draw_text_advanced(x + offset_x, y + offset_y, text, &shadow_config);
                
                // Dibujar texto principal
                let mut main_config = config.clone();
                main_config.effect = TextEffect::None;
                self.draw_text_advanced(x, y, text, &main_config);
            },
            TextEffect::Outline { width, color } => {
                // Dibujar contorno en todas las direcciones
                for dx in -(width as i32)..=(width as i32) {
                    for dy in -(width as i32)..=(width as i32) {
                        if dx != 0 || dy != 0 {
                            let mut outline_config = config.clone();
                            outline_config.color = color;
                            outline_config.effect = TextEffect::None;
                            self.draw_text_advanced(x + dx, y + dy, text, &outline_config);
                        }
                    }
                }
                
                // Dibujar texto principal
                let mut main_config = config.clone();
                main_config.effect = TextEffect::None;
                self.draw_text_advanced(x, y, text, &main_config);
            },
            TextEffect::Gradient { start_color, end_color } => {
                self.draw_text_gradient(x, y, text, config, start_color, end_color);
            },
            TextEffect::Glow { intensity, color } => {
                // Dibujar efecto de resplandor
                for radius in 1..=3 {
                    let alpha = (intensity * (4 - radius) as f32 / 3.0 * 255.0) as u8;
                    let glow_color = Color::rgba(color.r, color.g, color.b, alpha);
                    let mut glow_config = config.clone();
                    glow_config.color = glow_color;
                    glow_config.effect = TextEffect::None;
                    
                    for dx in -(radius as i32)..=(radius as i32) {
                        for dy in -(radius as i32)..=(radius as i32) {
                            if dx * dx + dy * dy <= radius * radius {
                                self.draw_text_advanced(x + dx, y + dy, text, &glow_config);
                            }
                        }
                    }
                }
                
                // Dibujar texto principal
                let mut main_config = config.clone();
                main_config.effect = TextEffect::None;
                self.draw_text_advanced(x, y, text, &main_config);
            },
        }
    }
    
    /// Renderizar layout de texto
    fn render_text_layout(&mut self, x: i32, y: i32, layout: &TextLayout, config: &TextConfig) {
        for line in &layout.lines {
            let line_x = x + line.start_x as i32;
            let line_y = y + line.start_y as i32;
            
            // Dibujar fondo si está configurado
            if let Some(bg_color) = config.background_color {
                self.fill_rect(
                    line_x as u32,
                    line_y as u32,
                    line.width,
                    line.height,
                    bg_color
                );
            }
            
            // Dibujar cada carácter de la línea
            self.render_text_line(line_x, line_y, &line.text, config);
        }
    }
    
    /// Renderizar una línea de texto
    fn render_text_line(&mut self, x: i32, y: i32, text: &str, config: &TextConfig) {
        let mut current_x = x;
        
        for ch in text.chars() {
            if let Some(glyph) = config.font.get_glyph(ch) {
                self.render_glyph(current_x, y, glyph, &config.color);
                current_x += glyph.advance as i32;
            } else {
                // Carácter no soportado, usar espacio
                current_x += config.font.size as i32;
            }
        }
    }
    
    /// Renderizar un glifo individual
    fn render_glyph(&mut self, x: i32, y: i32, glyph: &GlyphInfo, color: &Color) {
        // Verificar que el framebuffer esté inicializado
        if !self.is_initialized() {
            return;
        }
        
        // Verificar límites de seguridad
        if x < -1000 || x > 10000 || y < -1000 || y > 10000 {
            return; // Evitar coordenadas extremas que podrían causar overflow
        }
        
        for gy in 0..glyph.height {
            for gx in 0..glyph.width {
                let pixel_index = (gy * glyph.width + gx) as usize;
                if pixel_index < glyph.bitmap.len() {
                    let alpha = glyph.bitmap[pixel_index];
                    if alpha > 0 {
                        let final_color = Color::rgba(
                            color.r,
                            color.g,
                            color.b,
                            alpha
                        );
                        
                        let pixel_x = x + gx as i32 + glyph.bearing_x;
                        let pixel_y = y + gy as i32 - glyph.bearing_y;
                        
                        // Verificaciones adicionales de seguridad
                        if pixel_x >= 0 && pixel_y >= 0 &&
                           pixel_x < self.info.width as i32 &&
                           pixel_y < self.info.height as i32 &&
                           pixel_x < 10000 && pixel_y < 10000 { // Límites adicionales
                            self.put_pixel(pixel_x as u32, pixel_y as u32, final_color);
                        }
                    }
                }
            }
        }
    }
    
    /// Dibujar texto con gradiente
    fn draw_text_gradient(&mut self, x: i32, y: i32, text: &str, config: &TextConfig, 
                         start_color: Color, end_color: Color) {
        let layout = self.create_text_renderer().layout_text(text, config);
        let text_width = layout.total_width as f32;
        
        for line in &layout.lines {
            let line_x = x + line.start_x as i32;
            let line_y = y + line.start_y as i32;
            let mut current_x = line_x;
            
            for ch in line.text.chars() {
                if let Some(glyph) = config.font.get_glyph(ch) {
                    // Calcular posición relativa para el gradiente
                    let relative_x = (current_x - x) as f32 / text_width;
                    let gradient_color = self.lerp_color(start_color, end_color, relative_x);
                    
                    let mut char_config = config.clone();
                    char_config.color = gradient_color;
                    char_config.effect = TextEffect::None;
                    
                    self.render_glyph(current_x, line_y, glyph, &gradient_color);
                    current_x += glyph.advance as i32;
                } else {
                    current_x += config.font.size as i32;
                }
            }
        }
    }
    
    /// Interpolar entre dos colores
    fn lerp_color(&self, start: Color, end: Color, t: f32) -> Color {
        let r = (start.r as f32 + (end.r as f32 - start.r as f32) * t) as u8;
        let g = (start.g as f32 + (end.g as f32 - start.g as f32) * t) as u8;
        let b = (start.b as f32 + (end.b as f32 - start.b as f32) * t) as u8;
        let a = (start.a as f32 + (end.a as f32 - start.a as f32) * t) as u8;
        
        Color::rgba(r, g, b, a)
    }
    
    /// Medir texto sin dibujarlo
    pub fn measure_text(&self, text: &str, font: &Font) -> (u32, u32) {
        let width = font.measure_text(text);
        let height = font.line_height();
        (width, height)
    }
    
    /// Dibujar texto centrado
    pub fn draw_text_centered(&mut self, x: i32, y: i32, text: &str, config: &TextConfig) {
        let (text_width, text_height) = self.measure_text(text, &config.font);
        let centered_x = x - (text_width as i32) / 2;
        let centered_y = y - (text_height as i32) / 2;
        
        self.draw_text_with_effect(centered_x, centered_y, text, config);
    }
    
    /// Dibujar texto con múltiples líneas
    pub fn draw_multiline_text(&mut self, x: i32, y: i32, text: &str, config: &TextConfig) {
        let layout = self.create_text_renderer().layout_text(text, config);
        self.render_text_layout(x, y, &layout, config);
    }
    
    /// Crear texto como textura
    pub fn create_text_texture(&self, text: &str, config: &TextConfig) -> Texture {
        let layout = self.create_text_renderer().layout_text(text, config);
        let mut texture = Texture::new(layout.total_width, layout.total_height, TextureFormat::RGBA8);
        
        // Renderizar texto en la textura
        for line in &layout.lines {
            let mut current_x = line.start_x as i32;
            
            for ch in line.text.chars() {
                if let Some(glyph) = config.font.get_glyph(ch) {
                    for gy in 0..glyph.height {
                        for gx in 0..glyph.width {
                            let pixel_index = (gy * glyph.width + gx) as usize;
                            if pixel_index < glyph.bitmap.len() {
                                let alpha = glyph.bitmap[pixel_index];
                                if alpha > 0 {
                                    let final_color = Color::rgba(
                                        config.color.r,
                                        config.color.g,
                                        config.color.b,
                                        alpha
                                    );
                                    
                                    let pixel_x = current_x + gx as i32 + glyph.bearing_x;
                                    let pixel_y = line.start_y as i32 + gy as i32 - glyph.bearing_y;
                                    
                                    if pixel_x >= 0 && pixel_y >= 0 &&
                                       pixel_x < layout.total_width as i32 &&
                                       pixel_y < layout.total_height as i32 {
                                        texture.set_pixel(pixel_x as u32, pixel_y as u32, final_color);
                                    }
                                }
                            }
                        }
                    }
                    current_x += glyph.advance as i32;
                } else {
                    current_x += config.font.size as i32;
                }
            }
        }
        
        texture
    }
    
    /// Renderizar usando el pipeline moderno
    pub fn render_with_pipeline(&mut self, pipeline: &ModernRenderPipeline) {
        pipeline.render_to_framebuffer(self);
    }
    
    /// Crear una textura desde una región del framebuffer
    pub fn create_texture_from_region(&self, x: u32, y: u32, width: u32, height: u32) -> Texture {
        let mut texture = Texture::new(width, height, TextureFormat::RGBA8);
        
        for ty in 0..height {
            for tx in 0..width {
                let pixel = self.get_pixel(x + tx, y + ty);
                texture.set_pixel(tx, ty, pixel);
            }
        }
        
        texture
    }
    
    /// Dibujar una textura en el framebuffer con blending
    pub fn draw_texture(&mut self, texture: &Texture, x: i32, y: i32, blend_mode: BlendMode, alpha: f32) {
        for ty in 0..texture.height {
            for tx in 0..texture.width {
                let src_color = texture.get_pixel(tx, ty);
                let dst_x = x + tx as i32;
                let dst_y = y + ty as i32;
                
                if dst_x >= 0 && dst_y >= 0 && 
                   dst_x < self.info.width as i32 && 
                   dst_y < self.info.height as i32 {
                    
                    let dst_color = self.get_pixel(dst_x as u32, dst_y as u32);
                    let final_color = self.blend_colors(src_color, dst_color, blend_mode, alpha);
                    self.put_pixel(dst_x as u32, dst_y as u32, final_color);
                }
            }
        }
    }
    
    /// Método helper para blending de colores
    fn blend_colors(&self, src: Color, dst: Color, blend_mode: BlendMode, alpha: f32) -> Color {
        match blend_mode {
            BlendMode::None => src,
            BlendMode::Alpha => {
                let src_alpha = (src.a as f32 / 255.0) * alpha;
                let dst_alpha = dst.a as f32 / 255.0;
                let final_alpha = src_alpha + dst_alpha * (1.0 - src_alpha);
                
                if final_alpha == 0.0 {
                    return dst;
                }
                
                let r = ((src.r as f32 * src_alpha + dst.r as f32 * dst_alpha * (1.0 - src_alpha)) / final_alpha) as u8;
                let g = ((src.g as f32 * src_alpha + dst.g as f32 * dst_alpha * (1.0 - src_alpha)) / final_alpha) as u8;
                let b = ((src.b as f32 * src_alpha + dst.b as f32 * dst_alpha * (1.0 - src_alpha)) / final_alpha) as u8;
                
                Color::rgba(r, g, b, (final_alpha * 255.0) as u8)
            },
            BlendMode::Additive => {
                let r = min(255, src.r + dst.r);
                let g = min(255, src.g + dst.g);
                let b = min(255, src.b + dst.b);
                Color::rgba(r, g, b, dst.a)
            },
            BlendMode::Multiply => {
                let r = ((src.r as u16 * dst.r as u16) / 255) as u8;
                let g = ((src.g as u16 * dst.g as u16) / 255) as u8;
                let b = ((src.b as u16 * dst.b as u16) / 255) as u8;
                Color::rgba(r, g, b, dst.a)
            },
            BlendMode::Screen => {
                let r = 255 - (((255 - src.r as u16) * (255 - dst.r as u16)) / 255) as u8;
                let g = 255 - (((255 - src.g as u16) * (255 - dst.g as u16)) / 255) as u8;
                let b = 255 - (((255 - src.b as u16) * (255 - dst.b as u16)) / 255) as u8;
                Color::rgba(r, g, b, dst.a)
            },
            BlendMode::Overlay => {
                let r = if dst.r < 128 { 
                    (2 * src.r as u16 * dst.r as u16 / 255) as u8 
                } else { 
                    255 - (2 * (255 - src.r as u16) * (255 - dst.r as u16) / 255) as u8 
                };
                let g = if dst.g < 128 { 
                    (2 * src.g as u16 * dst.g as u16 / 255) as u8 
                } else { 
                    255 - (2 * (255 - src.g as u16) * (255 - dst.g as u16) / 255) as u8 
                };
                let b = if dst.b < 128 { 
                    (2 * src.b as u16 * dst.b as u16 / 255) as u8 
                } else { 
                    255 - (2 * (255 - src.b as u16) * (255 - dst.b as u16) / 255) as u8 
                };
                Color::rgba(r, g, b, dst.a)
            },
        }
    }
}

/// Utilidades para crear efectos visuales modernos
pub struct ModernGraphicsUtils;

impl ModernGraphicsUtils {
    /// Crear un gradiente radial
    pub fn create_radial_gradient(width: u32, height: u32, center_x: u32, center_y: u32, 
                                 radius: u32, start_color: Color, end_color: Color) -> Texture {
        let mut texture = Texture::new(width, height, TextureFormat::RGBA8);
        
        for y in 0..height {
            for x in 0..width {
                let dx = x as i32 - center_x as i32;
                let dy = y as i32 - center_y as i32;
                let distance_squared = (dx * dx + dy * dy) as u32;
                let distance = Self::sqrt_approx(distance_squared);
                
                if distance <= radius {
                    let t = distance as f32 / radius as f32;
                    let color = Self::lerp_color(start_color, end_color, t);
                    texture.set_pixel(x, y, color);
                }
            }
        }
        
        texture
    }
    
    /// Crear un gradiente lineal
    pub fn create_linear_gradient(width: u32, height: u32, start_color: Color, end_color: Color) -> Texture {
        let mut texture = Texture::new(width, height, TextureFormat::RGBA8);
        
        for y in 0..height {
            let t = y as f32 / height as f32;
            let color = Self::lerp_color(start_color, end_color, t);
            
            for x in 0..width {
                texture.set_pixel(x, y, color);
            }
        }
        
        texture
    }
    
    /// Crear una textura con efecto de ruido
    pub fn create_noise_texture(width: u32, height: u32, intensity: f32) -> Texture {
        let mut texture = Texture::new(width, height, TextureFormat::RGBA8);
        
        for y in 0..height {
            for x in 0..width {
                // Generador de ruido simple (puede mejorarse con algoritmos más sofisticados)
                let noise = ((x * 7919 + y * 65537) % 256) as f32 / 255.0;
                let gray = (noise * intensity * 255.0) as u8;
                let color = Color::rgba(gray, gray, gray, 255);
                texture.set_pixel(x, y, color);
            }
        }
        
        texture
    }
    
    /// Interpolar entre dos colores
    fn lerp_color(start: Color, end: Color, t: f32) -> Color {
        let r = (start.r as f32 + (end.r as f32 - start.r as f32) * t) as u8;
        let g = (start.g as f32 + (end.g as f32 - start.g as f32) * t) as u8;
        let b = (start.b as f32 + (end.b as f32 - start.b as f32) * t) as u8;
        let a = (start.a as f32 + (end.a as f32 - start.a as f32) * t) as u8;
        
        Color::rgba(r, g, b, a)
    }
    
    /// Aproximación simple de raíz cuadrada para no_std
    fn sqrt_approx(n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if n == 1 {
            return 1;
        }
        
        // Método de Newton-Raphson simplificado
        let mut x = n;
        let mut y = if x > 0 { (x + n / x) / 2 } else { n };
        
        while y < x && y > 0 {
            x = y;
            y = if x > 0 { (x + n / x) / 2 } else { x };
        }
        
        x
    }
}
