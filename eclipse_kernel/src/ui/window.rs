//! Sistema de ventanas para Eclipse OS
//! 
//! Maneja la creación, gestión y renderizado de ventanas

use core::fmt;
use alloc::vec::Vec;
use alloc::string::String;

/// ID único de ventana
pub type WindowId = u32;

/// Estados de una ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowState {
    Normal,
    Minimized,
    Maximized,
    Hidden,
    Focused,
}

/// Tipos de ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowType {
    Application,
    Dialog,
    Tooltip,
    Menu,
    Desktop,
}

/// Estructura de una ventana
pub struct Window {
    pub id: WindowId,
    pub title: String,
    pub position: Point,
    pub size: Size,
    pub state: WindowState,
    pub window_type: WindowType,
    pub visible: bool,
    pub resizable: bool,
    pub movable: bool,
    pub has_title_bar: bool,
    pub has_border: bool,
    pub z_order: u32,
    pub buffer: Vec<u32>, // Buffer de píxeles
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

/// Color RGBA
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Window {
    /// Crear una nueva ventana
    pub fn new(id: WindowId, title: &str, width: u32, height: u32) -> Self {
        let buffer_size = (width * height) as usize;
        let mut buffer = Vec::with_capacity(buffer_size);
        for _ in 0..buffer_size {
            buffer.push(0xFF000000); // Negro por defecto
        }
        
        Self {
            id,
            title: String::from(title),
            position: Point { x: 100, y: 100 },
            size: Size { width, height },
            state: WindowState::Normal,
            window_type: WindowType::Application,
            visible: true,
            resizable: true,
            movable: true,
            has_title_bar: true,
            has_border: true,
            z_order: 0,
            buffer,
        }
    }
    
    /// Obtener el rectángulo de la ventana
    pub fn get_rect(&self) -> Rectangle {
        Rectangle {
            x: self.position.x,
            y: self.position.y,
            width: self.size.width,
            height: self.size.height,
        }
    }
    
    /// Obtener el rectángulo del área de contenido
    pub fn get_content_rect(&self) -> Rectangle {
        let border_width = if self.has_border { 2 } else { 0 };
        let title_height = if self.has_title_bar { 24 } else { 0 };
        
        Rectangle {
            x: self.position.x + border_width as i32,
            y: self.position.y + title_height as i32 + border_width as i32,
            width: self.size.width - (border_width * 2),
            height: self.size.height - title_height - (border_width * 2),
        }
    }
    
    /// Verificar si un punto está dentro de la ventana
    pub fn contains_point(&self, point: Point) -> bool {
        let rect = self.get_rect();
        point.x >= rect.x && point.x < rect.x + rect.width as i32 &&
        point.y >= rect.y && point.y < rect.y + rect.height as i32
    }
    
    /// Verificar si un punto está en el área de redimensionamiento
    pub fn is_resize_handle(&self, point: Point) -> bool {
        if !self.resizable {
            return false;
        }
        
        let rect = self.get_rect();
        let border = 8; // Tamaño del handle de redimensionamiento
        
        // Esquinas
        (point.x < rect.x + border && point.y < rect.y + border) ||
        (point.x >= rect.x + rect.width as i32 - border && point.y < rect.y + border) ||
        (point.x < rect.x + border && point.y >= rect.y + rect.height as i32 - border) ||
        (point.x >= rect.x + rect.width as i32 - border && point.y >= rect.y + rect.height as i32 - border) ||
        // Bordes
        (point.x < rect.x + border || point.x >= rect.x + rect.width as i32 - border) ||
        (point.y < rect.y + border || point.y >= rect.y + rect.height as i32 - border)
    }
    
    /// Verificar si un punto está en la barra de título
    pub fn is_title_bar(&self, point: Point) -> bool {
        if !self.has_title_bar {
            return false;
        }
        
        let rect = self.get_rect();
        point.x >= rect.x && point.x < rect.x + rect.width as i32 &&
        point.y >= rect.y && point.y < rect.y + 24
    }
    
    /// Mover la ventana
    pub fn move_to(&mut self, x: i32, y: i32) {
        self.position = Point { x, y };
    }
    
    /// Redimensionar la ventana
    pub fn resize_to(&mut self, width: u32, height: u32) {
        if !self.resizable {
            return;
        }
        
        self.size = Size { width, height };
        
        // Recrear el buffer con el nuevo tamaño
        let buffer_size = (width * height) as usize;
        self.buffer.clear();
        self.buffer.reserve(buffer_size);
        for _ in 0..buffer_size {
            self.buffer.push(0xFF000000);
        }
    }
    
    /// Establecer el estado de la ventana
    pub fn set_state(&mut self, state: WindowState) {
        self.state = state;
    }
    
    /// Mostrar/ocultar la ventana
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }
    
    /// Establecer el z-order
    pub fn set_z_order(&mut self, z_order: u32) {
        self.z_order = z_order;
    }
    
    /// Dibujar un píxel en el buffer
    pub fn draw_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.size.width && y < self.size.height {
            let index = (y * self.size.width + x) as usize;
            if index < self.buffer.len() {
                self.buffer[index] = color;
            }
        }
    }
    
    /// Obtener un píxel del buffer
    pub fn get_pixel(&self, x: u32, y: u32) -> u32 {
        if x < self.size.width && y < self.size.height {
            let index = (y * self.size.width + x) as usize;
            if index < self.buffer.len() {
                self.buffer[index]
            } else {
                0xFF000000
            }
        } else {
            0xFF000000
        }
    }
}

/// Gestor de ventanas
pub struct WindowManager {
    windows: Vec<Window>,
    next_window_id: WindowId,
    focused_window: Option<WindowId>,
    desktop_window: Option<WindowId>,
}

impl WindowManager {
    /// Crear nuevo gestor de ventanas
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            next_window_id: 1,
            focused_window: None,
            desktop_window: None,
        }
    }
    
    /// Crear una nueva ventana
    pub fn create_window(&mut self, title: &str, width: u32, height: u32) -> WindowId {
        let id = self.next_window_id;
        self.next_window_id += 1;
        
        let window = Window::new(id, title, width, height);
        self.windows.push(window);
        
        id
    }
    
    /// Obtener una ventana por ID
    pub fn get_window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }
    
    /// Obtener una ventana mutable por ID
    pub fn get_window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }
    
    /// Cerrar una ventana
    pub fn close_window(&mut self, id: WindowId) -> bool {
        if let Some(pos) = self.windows.iter().position(|w| w.id == id) {
            self.windows.remove(pos);
            if self.focused_window == Some(id) {
                self.focused_window = None;
            }
            true
        } else {
            false
        }
    }
    
    /// Obtener todas las ventanas visibles ordenadas por z-order
    pub fn get_visible_windows(&self) -> Vec<&Window> {
        let mut visible_windows: Vec<&Window> = self.windows
            .iter()
            .filter(|w| w.visible)
            .collect();
        
        visible_windows.sort_by_key(|w| w.z_order);
        visible_windows
    }
    
    /// Establecer ventana enfocada
    pub fn set_focused_window(&mut self, id: WindowId) {
        if let Some(window) = self.get_window_mut(id) {
            window.set_state(WindowState::Focused);
            self.focused_window = Some(id);
        }
    }
    
    /// Obtener ventana enfocada
    pub fn get_focused_window(&self) -> Option<WindowId> {
        self.focused_window
    }
    
    /// Obtener ventana en una posición específica
    pub fn get_window_at(&self, point: Point) -> Option<WindowId> {
        let visible_windows = self.get_visible_windows();
        
        // Buscar desde la ventana con mayor z-order (última en la lista)
        for window in visible_windows.iter().rev() {
            if window.contains_point(point) {
                return Some(window.id);
            }
        }
        
        None
    }
    
    /// Minimizar ventana
    pub fn minimize_window(&mut self, id: WindowId) {
        if let Some(window) = self.get_window_mut(id) {
            window.set_state(WindowState::Minimized);
        }
    }
    
    /// Maximizar ventana
    pub fn maximize_window(&mut self, id: WindowId) {
        if let Some(window) = self.get_window_mut(id) {
            window.set_state(WindowState::Maximized);
        }
    }
    
    /// Restaurar ventana
    pub fn restore_window(&mut self, id: WindowId) {
        if let Some(window) = self.get_window_mut(id) {
            window.set_state(WindowState::Normal);
        }
    }
    
    /// Obtener estadísticas del gestor
    pub fn get_stats(&self) -> WindowManagerStats {
        WindowManagerStats {
            total_windows: self.windows.len(),
            visible_windows: self.windows.iter().filter(|w| w.visible).count(),
            focused_window: self.focused_window,
        }
    }
}

/// Estadísticas del gestor de ventanas
#[derive(Debug, Clone, Copy)]
pub struct WindowManagerStats {
    pub total_windows: usize,
    pub visible_windows: usize,
    pub focused_window: Option<WindowId>,
}

impl fmt::Display for WindowManagerStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Window Manager: total={}, visible={}, focused={:?}",
               self.total_windows, self.visible_windows, self.focused_window)
    }
}

/// Instancia global del gestor de ventanas
static mut WINDOW_MANAGER: Option<WindowManager> = None;

/// Inicializar el gestor de ventanas
pub fn init_window_manager() -> Result<(), &'static str> {
    unsafe {
        if WINDOW_MANAGER.is_some() {
            return Ok(());
        }
        
        WINDOW_MANAGER = Some(WindowManager::new());
    }
    
    Ok(())
}

/// Obtener el gestor de ventanas
pub fn get_window_manager() -> Option<&'static mut WindowManager> {
    unsafe { WINDOW_MANAGER.as_mut() }
}

/// Crear una nueva ventana
pub fn create_window(title: &str, width: u32, height: u32) -> Option<WindowId> {
    get_window_manager().map(|manager| manager.create_window(title, width, height))
}

/// Obtener información del sistema de ventanas
pub fn get_window_system_info() -> Option<WindowManagerStats> {
    get_window_manager().map(|manager| manager.get_stats())
}
