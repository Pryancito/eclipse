//! Compositor de ventanas con aceleración
//! 
//! Maneja la composición y renderizado de ventanas, similar a un
//! compositor de Wayland con aceleración por hardware.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::geometry::{Point, Size, Rectangle};
use super::client_api::{ClientAPI, WindowInfo};
use super::{WindowId, ClientId};
use crate::drivers::framebuffer::{FramebufferDriver, Color};

/// Buffer de composición
#[derive(Debug, Clone)]
pub struct CompositionBuffer {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u32>, // ARGB pixels
}

impl CompositionBuffer {
    pub fn new(width: u32, height: u32) -> Self {
        // Crear buffer pequeño inicial para evitar colgadas
        let buffer_size = 1024; // Solo 1KB inicialmente
        
        Self {
            width,
            height,
            data: {
                let mut vec = Vec::new();
                vec.resize(buffer_size, 0u32);
                vec
            },
        }
    }

    /// Limpiar el buffer con un color
    pub fn clear(&mut self, color: Color) {
        let argb = color_to_argb(color);
        for pixel in self.data.iter_mut() {
            *pixel = argb;
        }
    }

    /// Dibujar un píxel
    pub fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height {
            let argb = color_to_argb(color);
            let index = (y as u32 * self.width + x as u32) as usize;
            self.data[index] = argb;
        }
    }

    /// Obtener un píxel
    pub fn get_pixel(&self, x: i32, y: i32) -> Option<Color> {
        if x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height {
            let index = (y as u32 * self.width + x as u32) as usize;
            Some(argb_to_color(self.data[index]))
        } else {
            None
        }
    }

    /// Dibujar rectángulo
    pub fn draw_rect(&mut self, rect: Rectangle, color: Color) {
        let argb = color_to_argb(color);
        for y in rect.y..(rect.y + rect.height as i32) {
            for x in rect.x..(rect.x + rect.width as i32) {
                if x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height {
                    let index = (y as u32 * self.width + x as u32) as usize;
                    self.data[index] = argb;
                }
            }
        }
    }

    /// Dibujar línea
    pub fn draw_line(&mut self, start: Point, end: Point, color: Color) {
        let dx = (end.x - start.x).abs();
        let dy = (end.y - start.y).abs();
        let sx = if start.x < end.x { 1 } else { -1 };
        let sy = if start.y < end.y { 1 } else { -1 };
        let mut err = dx - dy;

        let mut x = start.x;
        let mut y = start.y;

        loop {
            self.set_pixel(x, y, color);

            if x == end.x && y == end.y {
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

    /// Blit desde otro buffer
    pub fn blit_from(&mut self, source: &CompositionBuffer, src_rect: Rectangle, dst_pos: Point) {
        for y in 0..src_rect.height {
            for x in 0..src_rect.width {
                if let Some(color) = source.get_pixel(src_rect.x + x as i32, src_rect.y + y as i32) {
                    self.set_pixel(dst_pos.x + x as i32, dst_pos.y + y as i32, color);
                }
            }
        }
    }
}

/// Información de renderizado de una ventana
#[derive(Debug, Clone)]
pub struct WindowRenderInfo {
    pub window_id: WindowId,
    pub geometry: Rectangle,
    pub buffer: CompositionBuffer,
    pub needs_redraw: bool,
    pub z_order: i32,
}

/// Compositor de ventanas
pub struct WindowCompositor {
    /// Buffer principal de composición
    main_buffer: CompositionBuffer,
    /// Información de renderizado de ventanas
    window_buffers: BTreeMap<WindowId, WindowRenderInfo>,
    /// Orden Z de las ventanas (mayor = más arriba)
    z_order: Vec<WindowId>,
    /// Compositor inicializado
    initialized: AtomicBool,
    /// FPS actual
    frame_rate: AtomicU32,
    /// Último tiempo de frame
    last_frame_time: u64,
    /// Contador de frames
    frame_count: AtomicU32,
}

impl WindowCompositor {
    pub fn new() -> Result<Self, &'static str> {
        Ok(Self {
            main_buffer: CompositionBuffer::new(1024, 768), // Tamaño por defecto
            window_buffers: BTreeMap::new(),
            z_order: Vec::new(),
            initialized: AtomicBool::new(false),
            frame_rate: AtomicU32::new(0),
            last_frame_time: 0,
            frame_count: AtomicU32::new(0),
        })
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Establecer resolución del compositor
    pub fn set_resolution(&mut self, width: u32, height: u32) -> Result<(), &'static str> {
        self.main_buffer = CompositionBuffer::new(width, height);
        Ok(())
    }

    /// Registrar una ventana para composición
    pub fn register_window(&mut self, window_id: WindowId, geometry: Rectangle) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Compositor no inicializado");
        }

        let buffer = CompositionBuffer::new(geometry.width, geometry.height);
        let render_info = WindowRenderInfo {
            window_id,
            geometry,
            buffer,
            needs_redraw: true,
            z_order: 0,
        };

        self.window_buffers.insert(window_id, render_info);
        self.z_order.push(window_id);
        
        Ok(())
    }

    /// Desregistrar una ventana
    pub fn unregister_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        self.window_buffers.remove(&window_id);
        self.z_order.retain(|&id| id != window_id);
        Ok(())
    }

    /// Actualizar geometría de una ventana
    pub fn update_window_geometry(&mut self, window_id: WindowId, geometry: Rectangle) -> Result<(), &'static str> {
        if let Some(render_info) = self.window_buffers.get_mut(&window_id) {
            render_info.geometry = geometry;
            render_info.buffer = CompositionBuffer::new(geometry.width, geometry.height);
            render_info.needs_redraw = true;
        }
        Ok(())
    }

    /// Marcar ventana para redibujado
    pub fn mark_window_dirty(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(render_info) = self.window_buffers.get_mut(&window_id) {
            render_info.needs_redraw = true;
        }
        Ok(())
    }

    /// Establecer orden Z de una ventana
    pub fn set_window_z_order(&mut self, window_id: WindowId, z_order: i32) -> Result<(), &'static str> {
        if let Some(render_info) = self.window_buffers.get_mut(&window_id) {
            render_info.z_order = z_order;
        }
        
        // Reordenar ventanas por Z-order
        self.z_order.sort_by(|a, b| {
            let z_a = self.window_buffers.get(a).map(|w| w.z_order).unwrap_or(0);
            let z_b = self.window_buffers.get(b).map(|w| w.z_order).unwrap_or(0);
            z_a.cmp(&z_b)
        });
        
        Ok(())
    }

    /// Componer un frame
    pub fn compose_frame(&mut self) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Compositor no inicializado");
        }

        // Limpiar buffer principal
        self.main_buffer.clear(Color::BLACK);

        // Componer ventanas en orden Z
        let z_order_clone = self.z_order.clone();
        for &window_id in &z_order_clone {
            // Primero renderizar si es necesario
            if let Some(render_info) = self.window_buffers.get(&window_id) {
                if render_info.needs_redraw {
                    self.render_window(window_id)?;
                }
            }
            
            // Luego hacer blit
            if let Some(render_info) = self.window_buffers.get(&window_id) {
                let src_rect = Rectangle::new(0, 0, render_info.geometry.width, render_info.geometry.height);
                let dst_pos = Point::new(render_info.geometry.x, render_info.geometry.y);
                
                self.main_buffer.blit_from(&render_info.buffer, src_rect, dst_pos);
            }
        }

        // Actualizar estadísticas de frame
        self.update_frame_stats();

        Ok(())
    }

    /// Renderizar una ventana individual
    fn render_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(render_info) = self.window_buffers.get_mut(&window_id) {
            // Limpiar buffer de la ventana con color de fondo
            render_info.buffer.clear(Color::LIGHT_GRAY);
            
            // Dibujar barra de título
            let title_bar_height = 30;
            let title_bar_rect = Rectangle::new(0, 0, render_info.geometry.width, title_bar_height);
            render_info.buffer.draw_rect(title_bar_rect, Color::DARK_GRAY);
            
            // Dibujar bordes de la ventana
            let border_color = Color::BLACK;
            let border_width = 2;
            
            // Borde superior
            let border_rect = Rectangle::new(0, 0, render_info.geometry.width, border_width);
            render_info.buffer.draw_rect(border_rect, border_color);
            
            // Borde izquierdo
            let border_rect = Rectangle::new(0, 0, border_width, render_info.geometry.height);
            render_info.buffer.draw_rect(border_rect, border_color);
            
            // Borde derecho
            let border_rect = Rectangle::new((render_info.geometry.width - border_width) as i32, 0, border_width, render_info.geometry.height);
            render_info.buffer.draw_rect(border_rect, border_color);
            
            // Borde inferior
            let border_rect = Rectangle::new(0, (render_info.geometry.height - border_width) as i32, render_info.geometry.width, border_width);
            render_info.buffer.draw_rect(border_rect, border_color);
            
            // Dibujar botones de la barra de título (simplificados)
            let button_size = 20;
            let button_y = 5;
            let button_spacing = 25;
            
            // Botón cerrar (rojo)
            let close_rect = Rectangle::new((render_info.geometry.width - button_spacing) as i32, button_y, button_size, button_size);
            render_info.buffer.draw_rect(close_rect, Color::RED);
            
            // Botón minimizar (amarillo)
            let minimize_rect = Rectangle::new((render_info.geometry.width - button_spacing * 2) as i32, button_y, button_size, button_size);
            render_info.buffer.draw_rect(minimize_rect, Color::YELLOW);
            
            // Botón maximizar (verde)
            let maximize_rect = Rectangle::new((render_info.geometry.width - button_spacing * 3) as i32, button_y, button_size, button_size);
            render_info.buffer.draw_rect(maximize_rect, Color::GREEN);
            
            // Dibujar contenido de la ventana (área de trabajo)
            let content_rect = Rectangle::new(
                border_width as i32, 
                (title_bar_height + border_width) as i32, 
                render_info.geometry.width - (border_width * 2), 
                render_info.geometry.height - (title_bar_height + border_width * 2)
            );
            render_info.buffer.draw_rect(content_rect, Color::WHITE);
            
            // Dibujar patrón de cuadros en el área de contenido
            Self::draw_window_content_pattern_static(&mut render_info.buffer, content_rect);
            
            // Marcar como no necesita redibujado
            render_info.needs_redraw = false;
        }
        
        Ok(())
    }
    
    /// Dibujar patrón de contenido en la ventana
    fn draw_window_content_pattern(&self, buffer: &mut CompositionBuffer, rect: Rectangle) {
        Self::draw_window_content_pattern_static(buffer, rect);
    }
    
    fn draw_window_content_pattern_static(buffer: &mut CompositionBuffer, rect: Rectangle) {
        let pattern_size = 20;
        let mut color_toggle = false;
        
        for y in 0..(rect.height / pattern_size) {
            for x in 0..(rect.width / pattern_size) {
                let pattern_rect = Rectangle::new(
                    rect.x + (x * pattern_size) as i32,
                    rect.y + (y * pattern_size) as i32,
                    pattern_size,
                    pattern_size
                );
                
                let color = if color_toggle { Color::LIGHT_GRAY } else { Color::WHITE };
                buffer.draw_rect(pattern_rect, color);
                color_toggle = !color_toggle;
            }
            color_toggle = !color_toggle; // Alternar filas
        }
    }

    /// Actualizar estadísticas de frame
    fn update_frame_stats(&mut self) {
        let current_time = self.get_timestamp();
        self.frame_count.fetch_add(1, Ordering::SeqCst);
        
        if current_time - self.last_frame_time >= 1000000000 { // 1 segundo en nanosegundos
            let frame_count = self.frame_count.swap(0, Ordering::SeqCst);
            self.frame_rate.store(frame_count, Ordering::SeqCst);
            self.last_frame_time = current_time;
        }
    }

    /// Obtener timestamp actual (simplificado)
    fn get_timestamp(&self) -> u64 {
        // En una implementación real, esto usaría un timer del sistema
        0 // Placeholder
    }

    /// Renderizar a framebuffer del sistema
    pub fn render_to_framebuffer(&mut self, framebuffer: &mut FramebufferDriver) -> Result<(), &'static str> {
        // Componer frame si es necesario
        self.compose_frame()?;
        
        // Obtener dimensiones del framebuffer del sistema
        let fb_width = framebuffer.info.width;
        let fb_height = framebuffer.info.height;
        
        // Ajustar resolución del compositor si es necesario
        if self.main_buffer.width != fb_width || self.main_buffer.height != fb_height {
            self.set_resolution(fb_width, fb_height)?;
            self.compose_frame()?; // Re-componer con nueva resolución
        }
        
        // Copiar buffer principal al framebuffer del sistema de manera eficiente
        let min_width = core::cmp::min(self.main_buffer.width, fb_width);
        let min_height = core::cmp::min(self.main_buffer.height, fb_height);
        
        for y in 0..min_height {
            for x in 0..min_width {
                if let Some(color) = self.main_buffer.get_pixel(x as i32, y as i32) {
                    framebuffer.put_pixel(x, y, color);
                }
            }
        }
        
        // Rellenar el resto del framebuffer con negro si es necesario
        if fb_width > self.main_buffer.width {
            for y in 0..min_height {
                for x in self.main_buffer.width..fb_width {
                    framebuffer.put_pixel(x, y, Color::BLACK);
                }
            }
        }
        
        if fb_height > self.main_buffer.height {
            for y in self.main_buffer.height..fb_height {
                for x in 0..fb_width {
                    framebuffer.put_pixel(x, y, Color::BLACK);
                }
            }
        }
        
        Ok(())
    }

    /// Obtener buffer de composición principal
    pub fn get_main_buffer(&self) -> &CompositionBuffer {
        &self.main_buffer
    }

    /// Obtener buffer de una ventana específica
    pub fn get_window_buffer(&self, window_id: WindowId) -> Option<&CompositionBuffer> {
        self.window_buffers.get(&window_id).map(|info| &info.buffer)
    }

    /// Obtener buffer mutable de una ventana específica
    pub fn get_window_buffer_mut(&mut self, window_id: WindowId) -> Option<&mut CompositionBuffer> {
        self.window_buffers.get_mut(&window_id).map(|info| &mut info.buffer)
    }

    /// Obtener FPS actual
    pub fn get_frame_rate(&self) -> f32 {
        self.frame_rate.load(Ordering::Acquire) as f32
    }

    /// Obtener número de ventanas registradas
    pub fn get_window_count(&self) -> usize {
        self.window_buffers.len()
    }

    /// Obtener orden de las ventanas
    pub fn get_window_order(&self) -> &Vec<WindowId> {
        &self.z_order
    }

    /// Obtener estadísticas del compositor
    pub fn get_stats(&self) -> CompositorStats {
        CompositorStats {
            window_count: self.window_buffers.len(),
            frame_rate: self.get_frame_rate(),
            buffer_width: self.main_buffer.width,
            buffer_height: self.main_buffer.height,
        }
    }
}

/// Estadísticas del compositor
#[derive(Debug, Clone)]
pub struct CompositorStats {
    pub window_count: usize,
    pub frame_rate: f32,
    pub buffer_width: u32,
    pub buffer_height: u32,
}

/// Instancia global del compositor
static mut WINDOW_COMPOSITOR: Option<WindowCompositor> = None;

/// Inicializar el compositor global
pub fn init_window_compositor() -> Result<(), &'static str> {
    unsafe {
        if WINDOW_COMPOSITOR.is_some() {
            return Err("Compositor ya inicializado");
        }
        
        let mut compositor = WindowCompositor::new()?;
        compositor.initialize()?;
        WINDOW_COMPOSITOR = Some(compositor);
    }
    Ok(())
}

/// Obtener referencia al compositor
pub fn get_window_compositor() -> Result<&'static mut WindowCompositor, &'static str> {
    unsafe {
        WINDOW_COMPOSITOR.as_mut().ok_or("Compositor no inicializado")
    }
}

/// Verificar si el compositor está inicializado
pub fn is_window_compositor_initialized() -> bool {
    unsafe { WINDOW_COMPOSITOR.is_some() }
}

/// Componer frame globalmente
pub fn compose_global_frame() -> Result<(), &'static str> {
    let compositor = get_window_compositor()?;
    compositor.compose_frame()
}

/// Renderizar a framebuffer globalmente
pub fn render_global_to_framebuffer(framebuffer: &mut FramebufferDriver) -> Result<(), &'static str> {
    let compositor = get_window_compositor()?;
    compositor.render_to_framebuffer(framebuffer)
}

/// Funciones auxiliares de conversión de color
fn color_to_argb(color: Color) -> u32 {
    match color {
        Color::BLACK => 0xFF000000,
        Color::WHITE => 0xFFFFFFFF,
        Color::RED => 0xFFFF0000,
        Color::GREEN => 0xFF00FF00,
        Color::BLUE => 0xFF0000FF,
        Color::YELLOW => 0xFFFFFF00,
        Color::MAGENTA => 0xFFFF00FF,
        Color::CYAN => 0xFF00FFFF,
        Color::GRAY => 0xFF808080,
        Color::DARK_GRAY => 0xFF404040,
        Color::LIGHT_GRAY => 0xFFC0C0C0,
        _ => 0xFF000000, // Default to black
    }
}

fn argb_to_color(argb: u32) -> Color {
    match argb {
        0xFF000000 => Color::BLACK,
        0xFFFFFFFF => Color::WHITE,
        0xFFFF0000 => Color::RED,
        0xFF00FF00 => Color::GREEN,
        0xFF0000FF => Color::BLUE,
        0xFFFFFF00 => Color::YELLOW,
        0xFFFF00FF => Color::MAGENTA,
        0xFF00FFFF => Color::CYAN,
        0xFF808080 => Color::GRAY,
        0xFF404040 => Color::DARK_GRAY,
        0xFFC0C0C0 => Color::LIGHT_GRAY,
        _ => Color::BLACK, // Default to black
    }
}
