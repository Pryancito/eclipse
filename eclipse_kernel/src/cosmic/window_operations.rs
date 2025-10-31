//! Operaciones avanzadas de ventanas para COSMIC Desktop Environment
//!
//! Implementa funcionalidades avanzadas como minimizar, maximizar, redimensionar
//! y gestión de estados de ventanas con integración completa con la barra de tareas.

use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Estados de una ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowState {
    Normal,
    Minimized,
    Maximized,
    Fullscreen,
    Hidden,
}

/// Operaciones que se pueden realizar en una ventana
#[derive(Debug, Clone)]
pub enum WindowOperation {
    Minimize,
    Maximize,
    Restore,
    Close,
    Move { x: i32, y: i32 },
    Resize { width: u32, height: u32 },
    Focus,
    Unfocus,
    PinToTop,
    UnpinFromTop,
}

/// Información detallada de una ventana
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub icon: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub state: WindowState,
    pub is_focused: bool,
    pub is_pinned: bool,
    pub can_resize: bool,
    pub can_move: bool,
    pub can_minimize: bool,
    pub can_maximize: bool,
    pub z_order: u32,
    pub workspace: u32,
}

/// Gestor de operaciones de ventanas
pub struct WindowOperationsManager {
    windows: BTreeMap<u32, WindowInfo>,
    next_window_id: u32,
    focused_window: Option<u32>,
    drag_state: Option<DragState>,
    resize_state: Option<ResizeState>,
}

/// Estado de arrastre de ventana
#[derive(Debug, Clone)]
struct DragState {
    window_id: u32,
    start_x: i32,
    start_y: i32,
    offset_x: i32,
    offset_y: i32,
}

/// Estado de redimensionamiento de ventana
#[derive(Debug, Clone)]
struct ResizeState {
    window_id: u32,
    start_width: u32,
    start_height: u32,
    start_x: i32,
    start_y: i32,
    resize_corner: ResizeCorner,
}

/// Esquinas de redimensionamiento
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResizeCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Top,
    Bottom,
    Left,
    Right,
}

impl WindowOperationsManager {
    pub fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
            next_window_id: 1,
            focused_window: None,
            drag_state: None,
            resize_state: None,
        }
    }

    /// Crear una nueva ventana
    pub fn create_window(
        &mut self,
        title: String,
        icon: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> u32 {
        let id = self.next_window_id;
        self.next_window_id += 1;

        let window_info = WindowInfo {
            id,
            title,
            icon,
            x,
            y,
            width,
            height,
            state: WindowState::Normal,
            is_focused: false,
            is_pinned: false,
            can_resize: true,
            can_move: true,
            can_minimize: true,
            can_maximize: true,
            z_order: id,
            workspace: 0,
        };

        self.windows.insert(id, window_info);
        self.focus_window(id);
        id
    }

    /// Ejecutar una operación en una ventana
    pub fn execute_operation(
        &mut self,
        window_id: u32,
        operation: WindowOperation,
    ) -> Result<(), String> {
        let window = self
            .windows
            .get_mut(&window_id)
            .ok_or_else(|| "Ventana no encontrada".to_string())?;

        match operation {
            WindowOperation::Minimize => {
                if window.can_minimize {
                    window.state = WindowState::Minimized;
                    window.is_focused = false;
                    if self.focused_window == Some(window_id) {
                        self.focused_window = None;
                    }
                }
            }
            WindowOperation::Maximize => {
                if window.can_maximize {
                    window.state = WindowState::Maximized;
                    // En una implementación real, aquí se guardaría el tamaño original
                    window.width = 1024; // Tamaño de pantalla completo
                    window.height = 768;
                    window.x = 0;
                    window.y = 0;
                }
            }
            WindowOperation::Restore => {
                window.state = WindowState::Normal;
                // En una implementación real, aquí se restauraría el tamaño original
            }
            WindowOperation::Close => {
                self.windows.remove(&window_id);
                if self.focused_window == Some(window_id) {
                    self.focused_window = None;
                }
                return Ok(());
            }
            WindowOperation::Move { x, y } => {
                if window.can_move {
                    window.x = x;
                    window.y = y;
                }
            }
            WindowOperation::Resize { width, height } => {
                if window.can_resize && window.state != WindowState::Maximized {
                    window.width = width;
                    window.height = height;
                }
            }
            WindowOperation::Focus => {
                self.focus_window(window_id);
            }
            WindowOperation::Unfocus => {
                window.is_focused = false;
                if self.focused_window == Some(window_id) {
                    self.focused_window = None;
                }
            }
            WindowOperation::PinToTop => {
                window.is_pinned = true;
                window.z_order = u32::MAX;
            }
            WindowOperation::UnpinFromTop => {
                window.is_pinned = false;
                window.z_order = window_id;
            }
        }

        Ok(())
    }

    /// Enfocar una ventana
    pub fn focus_window(&mut self, window_id: u32) {
        // Desenfocar ventana anterior
        if let Some(old_focused) = self.focused_window {
            if let Some(old_window) = self.windows.get_mut(&old_focused) {
                old_window.is_focused = false;
            }
        }

        // Enfocar nueva ventana
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.is_focused = true;
            window.z_order = self.next_window_id;
            self.focused_window = Some(window_id);
            self.next_window_id += 1;
        }
    }

    /// Obtener información de una ventana
    pub fn get_window_info(&self, window_id: u32) -> Option<&WindowInfo> {
        self.windows.get(&window_id)
    }

    /// Obtener todas las ventanas
    pub fn get_all_windows(&self) -> Vec<&WindowInfo> {
        self.windows.values().collect()
    }

    /// Obtener ventana enfocada
    pub fn get_focused_window(&self) -> Option<&WindowInfo> {
        self.focused_window.and_then(|id| self.windows.get(&id))
    }

    /// Iniciar arrastre de ventana
    pub fn start_drag(&mut self, window_id: u32, start_x: i32, start_y: i32) -> Result<(), String> {
        let window = self
            .windows
            .get(&window_id)
            .ok_or_else(|| "Ventana no encontrada".to_string())?;

        if !window.can_move {
            return Err("La ventana no se puede mover".to_string());
        }

        self.drag_state = Some(DragState {
            window_id,
            start_x,
            start_y,
            offset_x: start_x - window.x,
            offset_y: start_y - window.y,
        });

        Ok(())
    }

    /// Actualizar posición durante arrastre
    pub fn update_drag(&mut self, current_x: i32, current_y: i32) -> Result<(), String> {
        if let Some(ref drag_state) = self.drag_state {
            let new_x = current_x - drag_state.offset_x;
            let new_y = current_y - drag_state.offset_y;

            self.execute_operation(
                drag_state.window_id,
                WindowOperation::Move { x: new_x, y: new_y },
            )?;
        }
        Ok(())
    }

    /// Finalizar arrastre de ventana
    pub fn end_drag(&mut self) {
        self.drag_state = None;
    }

    /// Iniciar redimensionamiento
    pub fn start_resize(
        &mut self,
        window_id: u32,
        corner: ResizeCorner,
        start_x: i32,
        start_y: i32,
    ) -> Result<(), String> {
        let window = self
            .windows
            .get(&window_id)
            .ok_or_else(|| "Ventana no encontrada".to_string())?;

        if !window.can_resize || window.state == WindowState::Maximized {
            return Err("La ventana no se puede redimensionar".to_string());
        }

        self.resize_state = Some(ResizeState {
            window_id,
            start_width: window.width,
            start_height: window.height,
            start_x,
            start_y,
            resize_corner: corner,
        });

        Ok(())
    }

    /// Actualizar tamaño durante redimensionamiento
    pub fn update_resize(&mut self, current_x: i32, current_y: i32) -> Result<(), String> {
        if let Some(resize_state) = self.resize_state.clone() {
            let delta_x = current_x - resize_state.start_x;
            let delta_y = current_y - resize_state.start_y;

            let (new_width, new_height, new_x, new_y) = match resize_state.resize_corner {
                ResizeCorner::BottomRight => {
                    let width = (resize_state.start_width as i32 + delta_x).max(100) as u32;
                    let height = (resize_state.start_height as i32 + delta_y).max(100) as u32;
                    (width, height, resize_state.start_x, resize_state.start_y)
                }
                ResizeCorner::BottomLeft => {
                    let width = (resize_state.start_width as i32 - delta_x).max(100) as u32;
                    let height = (resize_state.start_height as i32 + delta_y).max(100) as u32;
                    let x = resize_state.start_x + (resize_state.start_width as i32 - width as i32);
                    (width, height, x, resize_state.start_y)
                }
                ResizeCorner::TopRight => {
                    let width = (resize_state.start_width as i32 + delta_x).max(100) as u32;
                    let height = (resize_state.start_height as i32 - delta_y).max(100) as u32;
                    let y =
                        resize_state.start_y + (resize_state.start_height as i32 - height as i32);
                    (width, height, resize_state.start_x, y)
                }
                ResizeCorner::TopLeft => {
                    let width = (resize_state.start_width as i32 - delta_x).max(100) as u32;
                    let height = (resize_state.start_height as i32 - delta_y).max(100) as u32;
                    let x = resize_state.start_x + (resize_state.start_width as i32 - width as i32);
                    let y =
                        resize_state.start_y + (resize_state.start_height as i32 - height as i32);
                    (width, height, x, y)
                }
                ResizeCorner::Right => {
                    let width = (resize_state.start_width as i32 + delta_x).max(100) as u32;
                    (
                        width,
                        resize_state.start_height,
                        resize_state.start_x,
                        resize_state.start_y,
                    )
                }
                ResizeCorner::Left => {
                    let width = (resize_state.start_width as i32 - delta_x).max(100) as u32;
                    let x = resize_state.start_x + (resize_state.start_width as i32 - width as i32);
                    (width, resize_state.start_height, x, resize_state.start_y)
                }
                ResizeCorner::Bottom => {
                    let height = (resize_state.start_height as i32 + delta_y).max(100) as u32;
                    (
                        resize_state.start_width,
                        height,
                        resize_state.start_x,
                        resize_state.start_y,
                    )
                }
                ResizeCorner::Top => {
                    let height = (resize_state.start_height as i32 - delta_y).max(100) as u32;
                    let y =
                        resize_state.start_y + (resize_state.start_height as i32 - height as i32);
                    (resize_state.start_width, height, resize_state.start_x, y)
                }
            };

            self.execute_operation(
                resize_state.window_id,
                WindowOperation::Resize {
                    width: new_width,
                    height: new_height,
                },
            )?;
            self.execute_operation(
                resize_state.window_id,
                WindowOperation::Move { x: new_x, y: new_y },
            )?;
        }
        Ok(())
    }

    /// Finalizar redimensionamiento
    pub fn end_resize(&mut self) {
        self.resize_state = None;
    }

    /// Detectar si un punto está en el área de redimensionamiento
    pub fn detect_resize_corner(&self, window_id: u32, x: i32, y: i32) -> Option<ResizeCorner> {
        let window = self.windows.get(&window_id)?;

        if window.state == WindowState::Maximized || !window.can_resize {
            return None;
        }

        let resize_margin = 8; // Margen para detectar redimensionamiento

        let left = x >= window.x && x <= window.x + resize_margin;
        let right = x >= window.x + window.width as i32 - resize_margin
            && x <= window.x + window.width as i32;
        let top = y >= window.y && y <= window.y + resize_margin;
        let bottom = y >= window.y + window.height as i32 - resize_margin
            && y <= window.y + window.height as i32;

        match (left, right, top, bottom) {
            (true, false, true, false) => Some(ResizeCorner::TopLeft),
            (false, true, true, false) => Some(ResizeCorner::TopRight),
            (true, false, false, true) => Some(ResizeCorner::BottomLeft),
            (false, true, false, true) => Some(ResizeCorner::BottomRight),
            (false, false, true, false) => Some(ResizeCorner::Top),
            (false, false, false, true) => Some(ResizeCorner::Bottom),
            (true, false, false, false) => Some(ResizeCorner::Left),
            (false, true, false, false) => Some(ResizeCorner::Right),
            _ => None,
        }
    }

    /// Obtener ventanas ordenadas por Z-order
    pub fn get_windows_by_z_order(&self) -> Vec<&WindowInfo> {
        let mut windows: Vec<&WindowInfo> = self.windows.values().collect();
        windows.sort_by_key(|w| w.z_order);
        windows.reverse(); // Ventanas con mayor Z-order primero
        windows
    }

    /// Minimizar todas las ventanas
    pub fn minimize_all(&mut self) {
        for window in self.windows.values_mut() {
            if window.can_minimize && window.state != WindowState::Minimized {
                window.state = WindowState::Minimized;
                window.is_focused = false;
            }
        }
        self.focused_window = None;
    }

    /// Mostrar todas las ventanas minimizadas
    pub fn restore_all(&mut self) {
        for window in self.windows.values_mut() {
            if window.state == WindowState::Minimized {
                window.state = WindowState::Normal;
            }
        }
    }

    /// Cambiar a ventana siguiente
    pub fn switch_to_next_window(&mut self) {
        let windows: Vec<u32> = self.windows.keys().cloned().collect();
        if windows.is_empty() {
            return;
        }

        let current_focused = self.focused_window.unwrap_or(0);
        let current_index = windows
            .iter()
            .position(|&id| id == current_focused)
            .unwrap_or(0);
        let next_index = (current_index + 1) % windows.len();

        if let Some(&next_window_id) = windows.get(next_index) {
            self.focus_window(next_window_id);
        }
    }

    /// Cambiar a ventana anterior
    pub fn switch_to_previous_window(&mut self) {
        let windows: Vec<u32> = self.windows.keys().cloned().collect();
        if windows.is_empty() {
            return;
        }

        let current_focused = self.focused_window.unwrap_or(0);
        let current_index = windows
            .iter()
            .position(|&id| id == current_focused)
            .unwrap_or(0);
        let prev_index = if current_index == 0 {
            windows.len() - 1
        } else {
            current_index - 1
        };

        if let Some(&prev_window_id) = windows.get(prev_index) {
            self.focus_window(prev_window_id);
        }
    }
}

/// Renderizar controles de ventana
pub fn render_window_controls(
    fb: &mut FramebufferDriver,
    window: &WindowInfo,
) -> Result<(), String> {
    let titlebar_height = 30u32;
    let button_size = 24u32;
    let button_margin = 3u32;

    // Colores
    let titlebar_color = if window.is_focused {
        Color::from_hex(0x0066aa)
    } else {
        Color::from_hex(0x004488)
    };

    let button_color = Color::from_hex(0x0f0f1a);
    let button_hover_color = Color::from_hex(0x1a1a2e);
    let close_button_color = Color::from_hex(0xff4444);
    let text_color = Color::from_hex(0xffffff);

    // Fondo de la barra de título
    fb.draw_rect(
        window.x as u32,
        window.y as u32,
        window.width,
        titlebar_height,
        titlebar_color,
    );

    // Título de la ventana
    fb.draw_text_simple(
        (window.x + 8) as u32,
        (window.y + 8) as u32,
        &window.title,
        text_color,
    );

    // Botones de control
    let button_start_x = window.x + window.width as i32 - (button_size + button_margin) as i32 * 3;

    // Botón minimizar
    if window.can_minimize {
        let minimize_x = button_start_x;
        let minimize_color = if window.state == WindowState::Minimized {
            button_hover_color
        } else {
            button_color
        };

        fb.draw_rect(
            minimize_x as u32,
            (window.y + button_margin as i32) as u32,
            button_size,
            button_size,
            minimize_color,
        );
        fb.draw_text_simple(
            (minimize_x + 8) as u32,
            (window.y + 8) as u32,
            "−",
            text_color,
        );
    }

    // Botón maximizar/restaurar
    if window.can_maximize {
        let maximize_x = button_start_x + (button_size + button_margin) as i32;
        let maximize_color = if window.state == WindowState::Maximized {
            button_hover_color
        } else {
            button_color
        };

        fb.draw_rect(
            maximize_x as u32,
            (window.y + button_margin as i32) as u32,
            button_size,
            button_size,
            maximize_color,
        );
        let icon = if window.state == WindowState::Maximized {
            "❐"
        } else {
            "□"
        };
        fb.draw_text_simple(
            (maximize_x + 8) as u32,
            (window.y + 8) as u32,
            icon,
            text_color,
        );
    }

    // Botón cerrar
    let close_x = button_start_x + (button_size + button_margin) as i32 * 2;
    fb.draw_rect(
        close_x as u32,
        (window.y + button_margin as i32) as u32,
        button_size,
        button_size,
        close_button_color,
    );
    fb.draw_text_simple((close_x + 8) as u32, (window.y + 8) as u32, "×", text_color);

    // Borde de la ventana
    let border_color = if window.is_focused {
        Color::from_hex(0x0088cc)
    } else {
        Color::from_hex(0x0066aa)
    };

    fb.draw_rect(
        window.x as u32,
        window.y as u32,
        window.width,
        window.height,
        border_color,
    );

    Ok(())
}
