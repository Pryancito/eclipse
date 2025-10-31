#![allow(dead_code)]
//! Sistema de compositor para Eclipse OS
//! 
//! Maneja la composición de capas y el renderizado final

use core::fmt;
use alloc::vec::Vec;
use alloc::string::{String, ToString};

/// Compositor principal
pub struct Compositor {
    pub width: u32,
    pub height: u32,
    pub layers: Vec<Layer>,
    pub background_color: Color,
    pub final_buffer: Vec<u32>,
}

/// Capa del compositor
#[derive(Clone)]
pub struct Layer {
    pub id: u32,
    pub name: String,
    pub layer_type: LayerType,
    pub visible: bool,
    pub opacity: f32,
    pub z_order: u32,
    pub position: Point,
    pub size: Size,
    pub buffer: Vec<u32>,
}

/// Tipos de capa
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayerType {
    Background,
    Desktop,
    Window,
    Cursor,
    Overlay,
    Tooltip,
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

/// Color RGBA
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Compositor {
    /// Crear nuevo compositor
    pub fn new(width: u32, height: u32) -> Self {
        let buffer_size = (width * height) as usize;
        let mut final_buffer = Vec::with_capacity(buffer_size);
        for _ in 0..buffer_size {
            final_buffer.push(0xFF000000); // Negro por defecto
        }
        
        Self {
            width,
            height,
            layers: Vec::new(),
            background_color: Color { r: 45, g: 45, b: 45, a: 255 },
            final_buffer,
        }
    }
    
    /// Agregar capa
    pub fn add_layer(&mut self, layer: Layer) -> u32 {
        let id = layer.id;
        self.layers.push(layer);
        self.sort_layers();
        id
    }
    
    /// Remover capa
    pub fn remove_layer(&mut self, id: u32) -> bool {
        if let Some(pos) = self.layers.iter().position(|l| l.id == id) {
            self.layers.remove(pos);
            true
        } else {
            false
        }
    }
    
    /// Obtener capa
    pub fn get_layer(&self, id: u32) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }
    
    /// Obtener capa mutable
    pub fn get_layer_mut(&mut self, id: u32) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|l| l.id == id)
    }
    
    /// Ordenar capas por z-order
    fn sort_layers(&mut self) {
        self.layers.sort_by_key(|l| l.z_order);
    }
    
    /// Componer todas las capas
    pub fn compose(&mut self) {
        // Limpiar buffer final
        let bg_color = self.background_color.to_u32();
        for pixel in &mut self.final_buffer {
            *pixel = bg_color;
        }
        
        // Componer cada capa visible
        let visible_layers: Vec<Layer> = self.layers.iter().filter(|l| l.visible).cloned().collect();
        for layer in &visible_layers {
            self.compose_layer(layer);
        }
    }
    
    /// Componer una capa específica
    fn compose_layer(&mut self, layer: &Layer) {
        if layer.opacity <= 0.0 {
            return;
        }
        
        let opacity = layer.opacity.clamp(0.0, 1.0);
        
        for y in 0..layer.size.height {
            for x in 0..layer.size.width {
                let src_index = (y * layer.size.width + x) as usize;
                if src_index >= layer.buffer.len() {
                    continue;
                }
                
                let src_pixel = layer.buffer[src_index];
                let src_color = Color::from_u32(src_pixel);
                
                // Calcular posición en el buffer final
                let final_x = layer.position.x + x as i32;
                let final_y = layer.position.y + y as i32;
                
                if final_x >= 0 && final_y >= 0 && 
                   final_x < self.width as i32 && final_y < self.height as i32 {
                    
                    let final_index = (final_y as u32 * self.width + final_x as u32) as usize;
                    if final_index < self.final_buffer.len() {
                        let dst_color = Color::from_u32(self.final_buffer[final_index]);
                        let blended_color = self.blend_colors(dst_color, src_color, opacity);
                        self.final_buffer[final_index] = blended_color.to_u32();
                    }
                }
            }
        }
    }
    
    /// Mezclar dos colores
    fn blend_colors(&self, dst: Color, src: Color, opacity: f32) -> Color {
        let alpha = (src.a as f32 / 255.0) * opacity;
        let inv_alpha = 1.0 - alpha;
        
        Color {
            r: ((dst.r as f32 * inv_alpha + src.r as f32 * alpha) as u8),
            g: ((dst.g as f32 * inv_alpha + src.g as f32 * alpha) as u8),
            b: ((dst.b as f32 * inv_alpha + src.b as f32 * alpha) as u8),
            a: ((dst.a as f32 * inv_alpha + src.a as f32 * alpha) as u8),
        }
    }
    
    /// Establecer color de fondo
    pub fn set_background_color(&mut self, color: Color) {
        self.background_color = color;
    }
    
    /// Obtener buffer final
    pub fn get_final_buffer(&self) -> &[u32] {
        &self.final_buffer
    }
    
    /// Obtener estadísticas del compositor
    pub fn get_stats(&self) -> CompositorStats {
        CompositorStats {
            width: self.width,
            height: self.height,
            total_layers: self.layers.len(),
            visible_layers: self.layers.iter().filter(|l| l.visible).count(),
            background_color: self.background_color,
        }
    }
}

impl Layer {
    /// Crear nueva capa
    pub fn new(id: u32, name: &str, layer_type: LayerType, width: u32, height: u32) -> Self {
        let buffer_size = (width * height) as usize;
        let mut buffer = Vec::with_capacity(buffer_size);
        for _ in 0..buffer_size {
            buffer.push(0x00000000); // Transparente por defecto
        }
        
        Self {
            id,
            name: name.to_string(),
            layer_type,
            visible: true,
            opacity: 1.0,
            z_order: 0,
            position: Point { x: 0, y: 0 },
            size: Size { width, height },
            buffer,
        }
    }
    
    /// Establecer posición
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.position = Point { x, y };
    }
    
    /// Establecer tamaño
    pub fn set_size(&mut self, width: u32, height: u32) {
        self.size = Size { width, height };
        
        // Recrear buffer con nuevo tamaño
        let buffer_size = (width * height) as usize;
        self.buffer.clear();
        self.buffer.reserve(buffer_size);
        for _ in 0..buffer_size {
            self.buffer.push(0x00000000);
        }
    }
    
    /// Establecer opacidad
    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
    }
    
    /// Establecer z-order
    pub fn set_z_order(&mut self, z_order: u32) {
        self.z_order = z_order;
    }
    
    /// Mostrar/ocultar capa
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }
    
    /// Dibujar píxel en la capa
    pub fn draw_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.size.width && y < self.size.height {
            let index = (y * self.size.width + x) as usize;
            if index < self.buffer.len() {
                self.buffer[index] = color;
            }
        }
    }
    
    /// Obtener píxel de la capa
    pub fn get_pixel(&self, x: u32, y: u32) -> u32 {
        if x < self.size.width && y < self.size.height {
            let index = (y * self.size.width + x) as usize;
            if index < self.buffer.len() {
                self.buffer[index]
            } else {
                0x00000000
            }
        } else {
            0x00000000
        }
    }
    
    /// Limpiar capa
    pub fn clear(&mut self) {
        for pixel in &mut self.buffer {
            *pixel = 0x00000000; // Transparente
        }
    }
}

impl Color {
    /// Crear color RGBA
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    
    /// Crear color RGB (alpha = 255)
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    
    /// Convertir a u32 RGBA
    pub fn to_u32(&self) -> u32 {
        ((self.a as u32) << 24) | ((self.r as u32) << 16) | 
        ((self.g as u32) << 8) | (self.b as u32)
    }
    
    /// Crear desde u32 RGBA
    pub fn from_u32(value: u32) -> Self {
        Self {
            a: ((value >> 24) & 0xFF) as u8,
            r: ((value >> 16) & 0xFF) as u8,
            g: ((value >> 8) & 0xFF) as u8,
            b: (value & 0xFF) as u8,
        }
    }
}

/// Estadísticas del compositor
#[derive(Debug, Clone, Copy)]
pub struct CompositorStats {
    pub width: u32,
    pub height: u32,
    pub total_layers: usize,
    pub visible_layers: usize,
    pub background_color: Color,
}

impl fmt::Display for CompositorStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Compositor: {}x{}, layers={}/{}, bg=({},{},{},{})",
               self.width, self.height, self.visible_layers, self.total_layers,
               self.background_color.r, self.background_color.g, 
               self.background_color.b, self.background_color.a)
    }
}

/// Instancia global del compositor
static mut COMPOSITOR: Option<Compositor> = None;

/// Inicializar el compositor
pub fn init_compositor() -> Result<(), &'static str> {
    unsafe {
        if COMPOSITOR.is_some() {
            return Ok(());
        }
        
        let compositor = Compositor::new(1024, 768);
        COMPOSITOR = Some(compositor);
    }
    
    Ok(())
}

/// Obtener el compositor
pub fn get_compositor() -> Option<&'static mut Compositor> {
    unsafe { COMPOSITOR.as_mut() }
}

/// Componer todas las capas
pub fn compose_layers() {
    if let Some(compositor) = get_compositor() {
        compositor.compose();
    }
}

/// Obtener información del compositor
pub fn get_compositor_info() -> Option<CompositorStats> {
    get_compositor().map(|compositor| compositor.get_stats())
}
