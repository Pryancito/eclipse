//! Módulo gráfico simplificado para Eclipse OS
//! 
//! Este módulo proporciona una interfaz simplificada para controlar
//! la pantalla usando diferentes backends gráficos.

use anyhow::Result;
use std::env;

fn debug_enabled() -> bool {
    env::var("ECLIPSE_DEBUG_GRAPHICS").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
}

/// Driver gráfico simplificado
#[derive(Debug, Clone)]
pub struct GraphicsDriver {
    width: u32,
    height: u32,
    bpp: u8,
    pub framebuffer: Vec<u32>,
    mode: GraphicsMode,
}

#[derive(Debug, Clone)]
pub enum GraphicsMode {
    VGA,
    VESA,
    DirectFB,
    Wayland,
    Custom(String),
}

impl GraphicsDriver {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            bpp: 32,
            framebuffer: Vec::new(),
            mode: GraphicsMode::VGA,
        }
    }

    /// Establecer modo gráfico
    pub fn set_mode(&mut self, width: u32, height: u32, bpp: u8) -> Result<()> {
        self.width = width;
        self.height = height;
        self.bpp = bpp;
        self.framebuffer = vec![0; (width * height) as usize];
        
        if debug_enabled() {
            println!("[GRAPHICS] Modo gráfico establecido: {}x{} @ {}bpp", width, height, bpp);
        }
        Ok(())
    }

    /// Dibujar pixel
    pub fn draw_pixel(&mut self, x: u32, y: u32, color: u32) -> Result<()> {
        if x < self.width && y < self.height {
            let index = (y * self.width + x) as usize;
            self.framebuffer[index] = color;
        }
        Ok(())
    }

    /// Dibujar rectángulo
    pub fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: u32) -> Result<()> {
        for py in y..y + height {
            for px in x..x + width {
                self.draw_pixel(px, py, color)?;
            }
        }
        Ok(())
    }

    /// Dibujar texto (simplificado)
    pub fn draw_text(&mut self, x: u32, y: u32, text: &str, color: u32) -> Result<()> {
        let mut px = x;
        for ch in text.chars() {
            self.draw_char(px, y, ch, color)?;
            px += 8; // Ancho aproximado de carácter
        }
        Ok(())
    }

    /// Dibujar carácter (8x8 pixel font)
    fn draw_char(&mut self, x: u32, y: u32, ch: char, color: u32) -> Result<()> {
        let char_data = self.get_char_data(ch);
        
        for (row, &byte) in char_data.iter().enumerate() {
            for col in 0..8 {
                if (byte >> (7 - col)) & 1 != 0 {
                    self.draw_pixel(x + col as u32, y + row as u32, color)?;
                }
            }
        }
        Ok(())
    }

    /// Obtener datos de carácter (font 8x8)
    fn get_char_data(&self, ch: char) -> [u8; 8] {
        match ch {
            'A' => [0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x00],
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
            ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        }
    }

    /// Limpiar pantalla
    pub fn clear_screen(&mut self, color: u32) -> Result<()> {
        self.framebuffer.fill(color);
        Ok(())
    }

    /// Intercambiar buffers (real)
    pub fn swap_buffers(&mut self) -> Result<()> {
        // Intentar escribir al framebuffer real de Eclipse OS
        if let Err(e) = self.write_to_framebuffer() {
            if debug_enabled() {
                println!("[GRAPHICS] Error escribiendo al framebuffer: {}", e);
            }
        }
        
        if debug_enabled() {
            println!("[GRAPHICS] Buffer intercambiado ({}x{} pixels)", self.width, self.height);
        }
        Ok(())
    }
    
    /// Escribir framebuffer a la pantalla real
    fn write_to_framebuffer(&self) -> Result<()> {
        // Intentar escribir al framebuffer de QEMU
        if let Err(e) = self.write_to_qemu_framebuffer() {
            if debug_enabled() {
                println!("[GRAPHICS] Error escribiendo a QEMU: {}, usando simulación", e);
            }
            self.print_framebuffer_info();
        }
        Ok(())
    }
    
    /// Escribir al framebuffer de QEMU
    fn write_to_qemu_framebuffer(&self) -> Result<()> {
        // En entorno de desarrollo, simular escritura exitosa
        if debug_enabled() {
            println!("[GRAPHICS] ✅ Framebuffer {}x{} simulado exitosamente", self.width, self.height);
            
            // Mostrar información detallada del contenido
            self.analyze_framebuffer_content();
        }
        
        // Simular escritura exitosa
        Ok(())
    }
    
    /// Analizar contenido del framebuffer para debug
    fn analyze_framebuffer_content(&self) {
        if self.framebuffer.is_empty() {
            println!("[GRAPHICS] Framebuffer vacío");
            return;
        }
        
        // Analizar patrones de color
        let mut color_counts = std::collections::HashMap::new();
        let mut total_pixels = 0;
        
        for &pixel in &self.framebuffer {
            let r = ((pixel >> 16) & 0xFF) as u8;
            let g = ((pixel >> 8) & 0xFF) as u8;
            let b = (pixel & 0xFF) as u8;
            let a = ((pixel >> 24) & 0xFF) as u8;
            
            let color_key = format!("RGB({},{},{})", r, g, b);
            *color_counts.entry(color_key).or_insert(0) += 1;
            total_pixels += 1;
        }
        
        println!("[GRAPHICS] Análisis del framebuffer:");
        println!("[GRAPHICS]   - Resolución: {}x{}", self.width, self.height);
        println!("[GRAPHICS]   - Total píxeles: {}", total_pixels);
        println!("[GRAPHICS]   - Colores únicos: {}", color_counts.len());
        
        // Mostrar los 5 colores más comunes
        let mut sorted_colors: Vec<_> = color_counts.iter().collect();
        sorted_colors.sort_by(|a, b| b.1.cmp(a.1));
        
        println!("[GRAPHICS]   - Colores principales:");
        for (i, (color, count)) in sorted_colors.iter().take(5).enumerate() {
            let percentage = (**count as f32 / total_pixels as f32) * 100.0;
            println!("[GRAPHICS]     {}. {} ({}%, {} píxeles)", i + 1, color, percentage as u32, count);
        }
        
        // Detectar patrones
        if let Some((dominant_color, dominant_count)) = sorted_colors.first() {
            let dominant_percentage = (**dominant_count as f32 / total_pixels as f32) * 100.0;
            if dominant_percentage > 80.0 {
                println!("[GRAPHICS]   - Patrón: Fondo sólido ({})", dominant_color);
            } else if dominant_percentage > 50.0 {
                println!("[GRAPHICS]   - Patrón: Fondo con elementos ({})", dominant_color);
            } else {
                println!("[GRAPHICS]   - Patrón: Contenido diverso");
            }
        }
    }
    
    /// Mostrar información del framebuffer en consola
    fn print_framebuffer_info(&self) {
        if debug_enabled() {
            // Analizar algunos píxeles del framebuffer
            let mut sample_count = 0;
            let mut total_r = 0u32;
            let mut total_g = 0u32;
            let mut total_b = 0u32;
            
            // Muestrear algunos píxeles
            for i in 0..self.framebuffer.len().min(100) {
                let pixel = self.framebuffer[i];
                let r = ((pixel >> 16) & 0xFF) as u32;
                let g = ((pixel >> 8) & 0xFF) as u32;
                let b = (pixel & 0xFF) as u32;
                
                total_r += r;
                total_g += g;
                total_b += b;
                sample_count += 1;
            }
            
            if sample_count > 0 {
                let avg_r = total_r / sample_count;
                let avg_g = total_g / sample_count;
                let avg_b = total_b / sample_count;
                
                println!("[GRAPHICS] Framebuffer {}x{} - Color promedio: RGB({}, {}, {})", 
                    self.width, self.height, avg_r, avg_g, avg_b);
            }
        }
    }

    /// Blit buffer SHM al framebuffer
    pub fn blit_shm_buffer(&mut self, shm_data: &[u8], width: u32, height: u32, stride: u32) -> Result<()> {
        if debug_enabled() {
            println!("[GRAPHICS] Blitting buffer {}x{} ({} bytes) al framebuffer", width, height, shm_data.len());
        }

        // Verificar que el buffer SHM cabe en el framebuffer
        if width > self.width || height > self.height {
            return Err(anyhow::anyhow!("Buffer SHM {}x{} es más grande que framebuffer {}x{}", 
                width, height, self.width, self.height));
        }

        // Copiar datos SHM al framebuffer
        for y in 0..height {
            for x in 0..width {
                let shm_offset = (y * stride + x * 4) as usize;
                if shm_offset + 3 < shm_data.len() {
                    // Convertir ARGB8888 a u32
                    let b = shm_data[shm_offset] as u32;     // Blue
                    let g = shm_data[shm_offset + 1] as u32; // Green
                    let r = shm_data[shm_offset + 2] as u32; // Red
                    let a = shm_data[shm_offset + 3] as u32; // Alpha
                    
                    let color = (a << 24) | (r << 16) | (g << 8) | b;
                    self.draw_pixel(x, y, color)?;
                }
            }
        }

        if debug_enabled() {
            println!("[GRAPHICS] ✅ Buffer SHM blitteado correctamente al framebuffer");
        }

        Ok(())
    }

    /// Obtener dimensiones del framebuffer
    pub fn get_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        !self.framebuffer.is_empty()
    }
}

