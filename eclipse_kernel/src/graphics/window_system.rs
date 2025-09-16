//! Sistema de ventanas y compositor para Eclipse OS
//! 
//! Implementa un sistema moderno de ventanas con compositor
//! y aceleración por hardware.

use crate::drivers::framebuffer::FramebufferDriver;
use core::fmt;
use crate::syslog;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;

/// ID único de ventana
pub type WindowId = u32;

/// Posición en pantalla
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

/// Tamaño de ventana
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

/// Estado de ventana
#[derive(Debug, Clone, PartialEq)]
pub enum WindowState {
    Normal,
    Minimized,
    Maximized,
    Hidden,
    Fullscreen,
}

/// Tipo de ventana
#[derive(Debug, Clone, PartialEq)]
pub enum WindowType {
    Normal,
    Dialog,
    Tooltip,
    Popup,
    Desktop,
}

/// Evento de ventana
#[derive(Debug, Clone)]
pub enum WindowEvent {
    Created,
    Destroyed,
    Moved { old_pos: Position, new_pos: Position },
    Resized { old_size: Size, new_size: Size },
    Focused,
    Unfocused,
    Minimized,
    Maximized,
    Restored,
    Hidden,
    Shown,
}

/// Ventana
#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub title: String,
    pub position: Position,
    pub size: Size,
    pub state: WindowState,
    pub window_type: WindowType,
    pub visible: bool,
    pub focused: bool,
    pub z_order: u32,
    pub parent: Option<WindowId>,
    pub children: Vec<WindowId>,
    pub buffer: Vec<u32>, // Buffer de píxeles
    pub dirty: bool,      // Necesita redibujado
    pub created_time: u64,
    pub last_modified: u64,
}

impl Window {
    /// Crear nueva ventana
    pub fn new(id: WindowId, title: String, position: Position, size: Size) -> Self {
        let buffer_size = (size.width * size.height) as usize;
        Self {
            id,
            title,
            position,
            size,
            state: WindowState::Normal,
            window_type: WindowType::Normal,
            visible: true,
            focused: false,
            z_order: 0,
            parent: None,
            children: Vec::new(),
            buffer: {
                let mut buf = Vec::with_capacity(buffer_size);
                buf.resize(buffer_size, 0);
                buf
            }, // Inicializar con píxeles transparentes
            dirty: true,
            created_time: 0, // Se establecerá por el sistema
            last_modified: 0,
        }
    }

    /// Obtener rectángulo de la ventana
    pub fn get_rectangle(&self) -> Rectangle {
        Rectangle {
            x: self.position.x,
            y: self.position.y,
            width: self.size.width,
            height: self.size.height,
        }
    }

    /// Verificar si un punto está dentro de la ventana
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.position.x && 
        y >= self.position.y && 
        x < (self.position.x + self.size.width as i32) && 
        y < (self.position.y + self.size.height as i32)
    }

    /// Verificar si la ventana se superpone con otra
    pub fn overlaps_with(&self, other: &Window) -> bool {
        let rect1 = self.get_rectangle();
        let rect2 = other.get_rectangle();
        
        rect1.x < (rect2.x + rect2.width as i32) &&
        rect2.x < (rect1.x + rect1.width as i32) &&
        rect1.y < (rect2.y + rect2.height as i32) &&
        rect2.y < (rect1.y + rect1.height as i32)
    }

    /// Mover ventana
    pub fn move_to(&mut self, new_position: Position) {
        self.position = new_position;
        self.dirty = true;
        self.last_modified = self.get_current_time();
    }

    /// Redimensionar ventana
    pub fn resize_to(&mut self, new_size: Size) {
        self.size = new_size;
        let new_buffer_size = (new_size.width * new_size.height) as usize;
        self.buffer.resize(new_buffer_size, 0);
        self.dirty = true;
        self.last_modified = self.get_current_time();
    }

    /// Establecer estado de la ventana
    pub fn set_state(&mut self, new_state: WindowState) {
        self.state = new_state;
        self.dirty = true;
        self.last_modified = self.get_current_time();
    }

    /// Establecer visibilidad
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.dirty = true;
        self.last_modified = self.get_current_time();
    }

    /// Establecer foco
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.dirty = true;
        self.last_modified = self.get_current_time();
    }

    /// Dibujar píxel en el buffer
    pub fn draw_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.size.width && y < self.size.height {
            let index = (y * self.size.width + x) as usize;
            if index < self.buffer.len() {
                self.buffer[index] = color;
                self.dirty = true;
            }
        }
    }

    /// Dibujar rectángulo
    pub fn draw_rectangle(&mut self, x: u32, y: u32, width: u32, height: u32, color: u32) {
        for dy in 0..height {
            for dx in 0..width {
                self.draw_pixel(x + dx, y + dy, color);
            }
        }
    }

    /// Limpiar ventana
    pub fn clear(&mut self, color: u32) {
        for pixel in &mut self.buffer {
            *pixel = color;
        }
        self.dirty = true;
    }

    /// Obtener tiempo actual (simulado)
    fn get_current_time(&self) -> u64 {
        // En un sistema real, esto vendría del timer del sistema
        0
    }
}

/// Compositor de ventanas
pub struct WindowCompositor {
    windows: BTreeMap<WindowId, Window>,
    next_window_id: WindowId,
    focused_window: Option<WindowId>,
    desktop_window: Option<WindowId>,
    z_order_counter: u32,
}

impl WindowCompositor {
    /// Crear nuevo compositor
    pub fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
            next_window_id: 1,
            focused_window: None,
            desktop_window: None,
            z_order_counter: 0,
        }
    }

    /// Crear nueva ventana
    pub fn create_window(&mut self, title: String, position: Position, size: Size) -> WindowId {
        let id = self.next_window_id;
        self.next_window_id += 1;

        let mut window = Window::new(id, title, position, size);
        window.z_order = self.z_order_counter;
        self.z_order_counter += 1;

        self.windows.insert(id, window);
        
        id
    }

    /// Destruir ventana
    pub fn destroy_window(&mut self, window_id: WindowId) -> Result<(), String> {
        if let Some(window) = self.windows.remove(&window_id) {
            // Remover de la lista de hijos del padre
            if let Some(parent_id) = window.parent {
                if let Some(parent) = self.windows.get_mut(&parent_id) {
                    parent.children.retain(|&id| id != window_id);
                }
            }

            // Destruir ventanas hijas
            for child_id in window.children {
                self.destroy_window(child_id)?;
            }

            // Limpiar foco si era la ventana enfocada
            if self.focused_window == Some(window_id) {
                self.focused_window = None;
            }

            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Obtener ventana
    pub fn get_window(&self, window_id: WindowId) -> Option<&Window> {
        self.windows.get(&window_id)
    }

    /// Obtener ventana mutable
    pub fn get_window_mut(&mut self, window_id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&window_id)
    }

    /// Listar todas las ventanas
    pub fn list_windows(&self) -> Vec<&Window> {
        self.windows.values().collect()
    }

    /// Establecer foco en ventana
    pub fn set_focus(&mut self, window_id: WindowId) -> Result<(), String> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            // Quitar foco de la ventana anterior
            if let Some(old_focus) = self.focused_window {
                if let Some(old_window) = self.windows.get_mut(&old_focus) {
                    old_window.set_focused(false);
                }
            }

            // Establecer nuevo foco
            window.set_focused(true);
            self.focused_window = Some(window_id);
            
            // Traer al frente
            self.bring_to_front(window_id)?;
            
            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Traer ventana al frente
    pub fn bring_to_front(&mut self, window_id: WindowId) -> Result<(), String> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.z_order = self.z_order_counter;
            self.z_order_counter += 1;
            window.dirty = true;
            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Mover ventana
    pub fn move_window(&mut self, window_id: WindowId, new_position: Position) -> Result<(), String> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            let old_position = window.position;
            window.move_to(new_position);
            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Redimensionar ventana
    pub fn resize_window(&mut self, window_id: WindowId, new_size: Size) -> Result<(), String> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            let old_size = window.size;
            window.resize_to(new_size);
            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Minimizar ventana
    pub fn minimize_window(&mut self, window_id: WindowId) -> Result<(), String> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.set_state(WindowState::Minimized);
            window.set_visible(false);
            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Maximizar ventana
    pub fn maximize_window(&mut self, window_id: WindowId) -> Result<(), String> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.set_state(WindowState::Maximized);
            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Restaurar ventana
    pub fn restore_window(&mut self, window_id: WindowId) -> Result<(), String> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.set_state(WindowState::Normal);
            window.set_visible(true);
            Ok(())
        } else {
            Err(format!("Ventana no encontrada: {}", window_id))
        }
    }

    /// Componer todas las ventanas en el framebuffer
    pub fn compose(&mut self, framebuffer: &mut FramebufferDriver) {
        // Ordenar ventanas por z_order
        let mut window_list: Vec<&Window> = self.windows.values().collect();
        window_list.sort_by_key(|w| w.z_order);

        // Limpiar framebuffer
        framebuffer.clear_screen();

        // Dibujar ventanas en orden
        for window in window_list {
            if window.visible && window.state != WindowState::Minimized {
                self.draw_window(framebuffer, window);
            }
        }
    }

    /// Dibujar una ventana específica
    fn draw_window(&self, framebuffer: &mut FramebufferDriver, window: &Window) {
        let rect = window.get_rectangle();
        
        // Dibujar borde de ventana
        let border_color = if window.focused { 0x00FF00 } else { 0x808080 }; // Verde si enfocada, gris si no
        framebuffer.draw_rectangle(rect.x, rect.y, rect.width, 1, border_color); // Borde superior
        framebuffer.draw_rectangle(rect.x, rect.y, 1, rect.height, border_color); // Borde izquierdo
        framebuffer.draw_rectangle(rect.x + rect.width as i32 - 1, rect.y, 1, rect.height, border_color); // Borde derecho
        framebuffer.draw_rectangle(rect.x, rect.y + rect.height as i32 - 1, rect.width, 1, border_color); // Borde inferior

        // Dibujar barra de título
        let title_bar_color = if window.focused { 0x4040FF } else { 0x404040 }; // Azul si enfocada, gris si no
        framebuffer.draw_rectangle(rect.x + 1, rect.y + 1, rect.width - 2, 20, title_bar_color);

        // Dibujar título
        let title_text = &window.title;
        if title_text.len() > 0 {
            framebuffer.write_text_kernel(title_text, crate::drivers::framebuffer::Color::WHITE);
        }

        // Dibujar contenido de la ventana
        for y in 0..window.size.height {
            for x in 0..window.size.width {
                let buffer_index = (y * window.size.width + x) as usize;
                if buffer_index < window.buffer.len() {
                    let pixel_color = window.buffer[buffer_index];
                    if pixel_color != 0 { // No dibujar píxeles transparentes
                        framebuffer.draw_pixel(rect.x + x as i32 + 1, rect.y + y as i32 + 21, pixel_color);
                    }
                }
            }
        }
    }

    /// Obtener ventana en una posición específica
    pub fn get_window_at(&self, x: i32, y: i32) -> Option<WindowId> {
        // Buscar desde la ventana con mayor z_order (la del frente)
        let mut window_list: Vec<&Window> = self.windows.values().collect();
        window_list.sort_by_key(|w| w.z_order);
        window_list.reverse(); // Orden descendente

        for window in window_list {
            if window.visible && window.state != WindowState::Minimized && window.contains_point(x, y) {
                return Some(window.id);
            }
        }
        None
    }

    /// Obtener estadísticas del compositor
    pub fn get_statistics(&self) -> WindowSystemStats {
        let total_windows = self.windows.len();
        let visible_windows = self.windows.values().filter(|w| w.visible).count();
        let focused_windows = self.windows.values().filter(|w| w.focused).count();
        let minimized_windows = self.windows.values().filter(|w| w.state == WindowState::Minimized).count();

        WindowSystemStats {
            total_windows,
            visible_windows,
            focused_windows,
            minimized_windows,
            next_window_id: self.next_window_id,
        }
    }
}

/// Estadísticas del sistema de ventanas
#[derive(Debug, Clone)]
pub struct WindowSystemStats {
    pub total_windows: usize,
    pub visible_windows: usize,
    pub focused_windows: usize,
    pub minimized_windows: usize,
    pub next_window_id: WindowId,
}

impl fmt::Display for WindowSystemStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Window System Stats: {} total, {} visible, {} focused, {} minimized",
            self.total_windows,
            self.visible_windows,
            self.focused_windows,
            self.minimized_windows
        )
    }
}
