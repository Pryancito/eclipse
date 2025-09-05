//! Compositor para el sistema gráfico
//! 
//! Renderiza ventanas y maneja la composición de la pantalla

use crate::gui::framebuffer::{get_framebuffer, Color, Rect, Point};
use crate::gui::window::{get_window_manager, Window, WindowState};
use crate::gui::font::render_text;

/// Compositor del sistema
pub struct Compositor {
    pub background_color: Color,
    pub cursor_position: Point,
    pub cursor_visible: bool,
    pub cursor_color: Color,
    pub dirty_regions: [Option<Rect>; 64],
    pub dirty_count: usize,
    pub vsync_enabled: bool,
    pub fps_counter: u32,
    pub frame_time: u64,
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            background_color: Color::BLUE,
            cursor_position: Point::new(400, 300),
            cursor_visible: true,
            cursor_color: Color::WHITE,
            dirty_regions: [None; 64],
            dirty_count: 0,
            vsync_enabled: true,
            fps_counter: 0,
            frame_time: 0,
        }
    }
    
    /// Marcar región como sucia (necesita re-render)
    pub fn mark_dirty(&mut self, rect: Rect) {
        if self.dirty_count < self.dirty_regions.len() {
            self.dirty_regions[self.dirty_count] = Some(rect);
            self.dirty_count += 1;
        }
    }
    
    /// Limpiar regiones sucias
    pub fn clear_dirty_regions(&mut self) {
        self.dirty_regions = [None; 64];
        self.dirty_count = 0;
    }
    
    /// Renderizar un frame completo
    pub fn render_frame(&mut self) {
        if let Some(framebuffer) = get_framebuffer() {
            // Limpiar el fondo
            framebuffer.clear(self.background_color);
            
            // Renderizar ventanas desde atrás hacia adelante
            if let Some(window_manager) = get_window_manager() {
                self.render_windows(framebuffer, window_manager);
            }
            
            // Renderizar cursor
            if self.cursor_visible {
                self.render_cursor(framebuffer);
            }
            
            // Renderizar información de debug si está habilitada
            self.render_debug_info(framebuffer);
            
            // Limpiar regiones sucias
            self.clear_dirty_regions();
            
            // Incrementar contador de FPS
            self.fps_counter += 1;
        }
    }
    
    /// Renderizar todas las ventanas
    fn render_windows(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer, window_manager: &crate::gui::window::WindowManager) {
        // Iterar desde la cola hacia la cabeza (fondo hacia frente)
        let mut current = window_manager.windows.tail;
        
        while let Some(window_ptr) = current {
            unsafe {
                let window = &*window_ptr.as_ptr();
                if window.is_visible && window.state != WindowState::Minimized {
                    self.render_window(framebuffer, window);
                }
                current = window.prev;
            }
        }
    }
    
    /// Renderizar una ventana individual
    fn render_window(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer, window: &Window) {
        // Renderizar borde si no está deshabilitado
        if !window.flags.no_border {
            self.render_window_border(framebuffer, window);
        }
        
        // Renderizar barra de título si no está deshabilitada
        if !window.flags.no_title_bar {
            self.render_title_bar(framebuffer, window);
        }
        
        // Renderizar área del cliente
        self.render_client_area(framebuffer, window);
        
        // Renderizar decoraciones adicionales si la ventana está enfocada
        if window.is_focused {
            self.render_focus_indicator(framebuffer, window);
        }
    }
    
    /// Renderizar el borde de una ventana
    fn render_window_border(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer, window: &Window) {
        let border_color = if window.is_focused {
            Color::BLUE
        } else {
            window.border_color
        };
        
        // Borde exterior
        framebuffer.draw_rect(window.rect, border_color);
        
        // Borde interior (para efecto 3D)
        let inner_rect = Rect::new(
            window.rect.x + 1,
            window.rect.y + 1,
            window.rect.width.saturating_sub(2),
            window.rect.height.saturating_sub(2),
        );
        framebuffer.draw_rect(inner_rect, Color::LIGHT_GRAY);
    }
    
    /// Renderizar la barra de título
    fn render_title_bar(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer, window: &Window) {
        let title_bar_color = if window.is_focused {
            window.title_bar_color
        } else {
            Color::GRAY
        };
        
        // Fondo de la barra de título
        let title_rect = Rect::new(
            window.rect.x + 2,
            window.rect.y + 2,
            window.rect.width.saturating_sub(4),
            20,
        );
        framebuffer.fill_rect(title_rect, title_bar_color);
        
        // Texto del título
        let title_text_pos = Point::new(window.rect.x + 6, window.rect.y + 6);
        render_text(framebuffer, window.get_title(), title_text_pos, window.text_color);
        
        // Botones de la ventana
        self.render_window_buttons(framebuffer, window);
    }
    
    /// Renderizar botones de la ventana (cerrar, minimizar, maximizar)
    fn render_window_buttons(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer, window: &Window) {
        let button_size = 16;
        let button_y = window.rect.y + 3;
        let mut button_x = window.rect.x + window.rect.width as i32 - button_size - 3;
        
        // Botón cerrar
        if window.flags.closable {
            let close_rect = Rect::new(button_x, button_y, button_size as u32, button_size as u32);
            framebuffer.fill_rect(close_rect, Color::RED);
            
            // X para cerrar
            let x_start = Point::new(button_x + 4, button_y + 4);
            let x_end = Point::new(button_x + 12, button_y + 12);
            framebuffer.draw_line(x_start, x_end, Color::WHITE);
            
            let x_start2 = Point::new(button_x + 12, button_y + 4);
            let x_end2 = Point::new(button_x + 4, button_y + 12);
            framebuffer.draw_line(x_start2, x_end2, Color::WHITE);
            
            button_x -= button_size + 2;
        }
        
        // Botón maximizar
        if window.flags.maximizable {
            let max_rect = Rect::new(button_x, button_y, button_size as u32, button_size as u32);
            framebuffer.fill_rect(max_rect, Color::GREEN);
            
            // Cuadrado para maximizar
            let square_rect = Rect::new(button_x + 4, button_y + 4, 8, 8);
            framebuffer.draw_rect(square_rect, Color::WHITE);
            
            button_x -= button_size + 2;
        }
        
        // Botón minimizar
        if window.flags.minimizable {
            let min_rect = Rect::new(button_x, button_y, button_size as u32, button_size as u32);
            framebuffer.fill_rect(min_rect, Color::YELLOW);
            
            // Línea para minimizar
            let line_start = Point::new(button_x + 4, button_y + 12);
            let line_end = Point::new(button_x + 12, button_y + 12);
            framebuffer.draw_line(line_start, line_end, Color::BLACK);
        }
    }
    
    /// Renderizar el área del cliente
    fn render_client_area(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer, window: &Window) {
        // Llenar con color de fondo
        framebuffer.fill_rect(window.client_rect, window.background_color);
        
        // Aquí es donde las aplicaciones renderizarían su contenido
        // Por ahora solo mostramos información básica
        let text_pos = Point::new(window.client_rect.x + 10, window.client_rect.y + 10);
        let info_text = "Contenido de la ventana";
        render_text(framebuffer, info_text, text_pos, window.text_color);
    }
    
    /// Renderizar indicador de enfoque
    fn render_focus_indicator(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer, window: &Window) {
        // Borde brillante para indicar enfoque
        let focus_rect = Rect::new(
            window.rect.x - 1,
            window.rect.y - 1,
            window.rect.width + 2,
            window.rect.height + 2,
        );
        framebuffer.draw_rect(focus_rect, Color::CYAN);
    }
    
    /// Renderizar cursor del ratón
    fn render_cursor(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        let cursor_size = 12;
        
        // Renderizar flecha del cursor
        for i in 0..cursor_size {
            for j in 0..(cursor_size - i) {
                let x = self.cursor_position.x + j;
                let y = self.cursor_position.y + i;
                
                if x >= 0 && x < framebuffer.info.width as i32 &&
                   y >= 0 && y < framebuffer.info.height as i32 {
                    framebuffer.put_pixel(x as u32, y as u32, self.cursor_color);
                }
            }
        }
        
        // Línea vertical del cursor
        for i in 0..cursor_size {
            let x = self.cursor_position.x;
            let y = self.cursor_position.y + i;
            
            if x >= 0 && x < framebuffer.info.width as i32 &&
               y >= 0 && y < framebuffer.info.height as i32 {
                framebuffer.put_pixel(x as u32, y as u32, self.cursor_color);
            }
        }
    }
    
    /// Renderizar información de debug
    fn render_debug_info(&self, framebuffer: &mut crate::gui::framebuffer::Framebuffer) {
        // Mostrar FPS en la esquina superior derecha
        let fps_text = "FPS: 60"; // En un sistema real, esto sería dinámico
        let fps_pos = Point::new(framebuffer.info.width as i32 - 80, 10);
        render_text(framebuffer, fps_text, fps_pos, Color::WHITE);
        
        // Mostrar información de memoria
        let mem_text = "Mem: 64MB";
        let mem_pos = Point::new(framebuffer.info.width as i32 - 80, 25);
        render_text(framebuffer, mem_text, mem_pos, Color::WHITE);
    }
    
    /// Actualizar posición del cursor
    pub fn update_cursor_position(&mut self, position: Point) {
        self.cursor_position = position;
        
        // Marcar área del cursor como sucia
        let cursor_rect = Rect::new(
            position.x - 2,
            position.y - 2,
            16,
            16,
        );
        self.mark_dirty(cursor_rect);
    }
    
    /// Mostrar/ocultar cursor
    pub fn set_cursor_visible(&mut self, visible: bool) {
        if self.cursor_visible != visible {
            self.cursor_visible = visible;
            
            // Marcar área del cursor como sucia
            let cursor_rect = Rect::new(
                self.cursor_position.x - 2,
                self.cursor_position.y - 2,
                16,
                16,
            );
            self.mark_dirty(cursor_rect);
        }
    }
    
    /// Cambiar color de fondo del escritorio
    pub fn set_background_color(&mut self, color: Color) {
        self.background_color = color;
        
        // Marcar toda la pantalla como sucia
        if let Some(info) = crate::gui::framebuffer::get_framebuffer_info() {
            let full_screen = Rect::new(0, 0, info.width, info.height);
            self.mark_dirty(full_screen);
        }
    }
    
    /// Obtener estadísticas del compositor
    pub fn get_stats(&self) -> (u32, u64, usize) {
        (self.fps_counter, self.frame_time, self.dirty_count)
    }
}

/// Compositor global
static mut COMPOSITOR: Option<Compositor> = None;

/// Inicializar el compositor
pub fn init_compositor() {
    let compositor = Compositor::new();
    unsafe {
        COMPOSITOR = Some(compositor);
    }
}

/// Obtener referencia al compositor
pub fn get_compositor() -> Option<&'static mut Compositor> {
    unsafe {
        COMPOSITOR.as_mut()
    }
}

/// Renderizar un frame
pub fn render_frame() {
    if let Some(compositor) = get_compositor() {
        compositor.render_frame();
    }
}

/// Actualizar posición del cursor
pub fn update_cursor_position(position: Point) {
    if let Some(compositor) = get_compositor() {
        compositor.update_cursor_position(position);
    }
}

/// Marcar región como sucia
pub fn mark_dirty_region(rect: Rect) {
    if let Some(compositor) = get_compositor() {
        compositor.mark_dirty(rect);
    }
}

/// Cambiar color de fondo
pub fn set_background_color(color: Color) {
    if let Some(compositor) = get_compositor() {
        compositor.set_background_color(color);
    }
}
