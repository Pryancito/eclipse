//! Sistema de ventanas
//!
//! Gestiona ventanas, decoraciones y jerarquía de ventanas

use crate::gui::framebuffer::{Color, Point, Rect};
use core::ptr::NonNull;

/// ID único de ventana
pub type WindowId = u32;

/// Estados de ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowState {
    Normal,
    Minimized,
    Maximized,
    Hidden,
    Closed,
}

/// Flags de ventana
#[derive(Debug, Clone, Copy)]
pub struct WindowFlags {
    pub resizable: bool,
    pub movable: bool,
    pub closable: bool,
    pub minimizable: bool,
    pub maximizable: bool,
    pub always_on_top: bool,
    pub no_title_bar: bool,
    pub no_border: bool,
}

impl WindowFlags {
    pub fn default() -> Self {
        Self {
            resizable: true,
            movable: true,
            closable: true,
            minimizable: true,
            maximizable: true,
            always_on_top: false,
            no_title_bar: false,
            no_border: false,
        }
    }

    pub fn dialog() -> Self {
        Self {
            resizable: false,
            movable: true,
            closable: true,
            minimizable: false,
            maximizable: false,
            always_on_top: true,
            no_title_bar: false,
            no_border: false,
        }
    }

    pub fn popup() -> Self {
        Self {
            resizable: false,
            movable: false,
            closable: false,
            minimizable: false,
            maximizable: false,
            always_on_top: true,
            no_title_bar: true,
            no_border: true,
        }
    }
}

/// Estructura de ventana
#[derive(Debug)]
pub struct Window {
    pub id: WindowId,
    pub title: [u8; 256],
    pub title_len: usize,
    pub rect: Rect,
    pub client_rect: Rect,
    pub state: WindowState,
    pub flags: WindowFlags,
    pub background_color: Color,
    pub border_color: Color,
    pub title_bar_color: Color,
    pub text_color: Color,
    pub z_order: u32,
    pub parent: Option<WindowId>,
    pub children: [Option<WindowId>; 32],
    pub child_count: usize,
    pub next: Option<NonNull<Window>>,
    pub prev: Option<NonNull<Window>>,
    pub is_focused: bool,
    pub is_visible: bool,
    pub is_dirty: bool,
}

impl Window {
    /// Crear nueva ventana
    pub fn new(id: WindowId, title: &str, rect: Rect, flags: WindowFlags) -> Self {
        let mut title_array = [0u8; 256];
        let title_bytes = title.as_bytes();
        let copy_len = core::cmp::min(title_bytes.len(), 255);
        title_array[..copy_len].copy_from_slice(&title_bytes[..copy_len]);

        // Calcular rectángulo del cliente (interior de la ventana)
        let title_bar_height = if flags.no_title_bar { 0 } else { 24 };
        let border_width = if flags.no_border { 0 } else { 2 };

        let client_rect = Rect::new(
            rect.x + border_width,
            rect.y + title_bar_height + border_width,
            rect.width.saturating_sub((border_width * 2) as u32),
            rect.height
                .saturating_sub((title_bar_height + border_width * 2) as u32),
        );

        Self {
            id,
            title: title_array,
            title_len: copy_len,
            rect,
            client_rect,
            state: WindowState::Normal,
            flags,
            background_color: Color::WHITE,
            border_color: Color::DARK_GRAY,
            title_bar_color: Color::BLUE,
            text_color: Color::BLACK,
            z_order: 0,
            parent: None,
            children: [None; 32],
            child_count: 0,
            next: None,
            prev: None,
            is_focused: false,
            is_visible: true,
            is_dirty: true,
        }
    }

    /// Obtener título como string slice
    pub fn get_title(&self) -> &str {
        let title_bytes = &self.title[..self.title_len];
        core::str::from_utf8(title_bytes).unwrap_or("")
    }

    /// Establecer título
    pub fn set_title(&mut self, title: &str) {
        let title_bytes = title.as_bytes();
        let copy_len = core::cmp::min(title_bytes.len(), 255);
        self.title[..copy_len].copy_from_slice(&title_bytes[..copy_len]);
        self.title_len = copy_len;
        self.is_dirty = true;
    }

    /// Mover ventana
    pub fn move_to(&mut self, x: i32, y: i32) {
        if !self.flags.movable {
            return;
        }

        let dx = x - self.rect.x;
        let dy = y - self.rect.y;

        self.rect.x = x;
        self.rect.y = y;
        self.client_rect.x += dx;
        self.client_rect.y += dy;
        self.is_dirty = true;
    }

    /// Redimensionar ventana
    pub fn resize(&mut self, width: u32, height: u32) {
        if !self.flags.resizable {
            return;
        }

        self.rect.width = width;
        self.rect.height = height;

        // Recalcular rectángulo del cliente
        let title_bar_height = if self.flags.no_title_bar { 0 } else { 24 };
        let border_width = if self.flags.no_border { 0 } else { 2 };

        self.client_rect.width = width.saturating_sub((border_width * 2) as u32);
        self.client_rect.height =
            height.saturating_sub((title_bar_height + border_width * 2) as u32);
        self.is_dirty = true;
    }

    /// Establecer estado de ventana
    pub fn set_state(&mut self, state: WindowState) {
        if self.state != state {
            self.state = state;
            self.is_dirty = true;
        }
    }

    /// Enfocar ventana
    pub fn focus(&mut self) {
        self.is_focused = true;
        self.is_dirty = true;
    }

    /// Desenfocar ventana
    pub fn unfocus(&mut self) {
        self.is_focused = false;
        self.is_dirty = true;
    }

    /// Mostrar ventana
    pub fn show(&mut self) {
        self.is_visible = true;
        self.is_dirty = true;
    }

    /// Ocultar ventana
    pub fn hide(&mut self) {
        self.is_visible = false;
        self.is_dirty = true;
    }

    /// Verificar si un punto está dentro de la ventana
    pub fn contains_point(&self, point: Point) -> bool {
        self.rect.contains(point)
    }

    /// Verificar si un punto está en la barra de título
    pub fn point_in_title_bar(&self, point: Point) -> bool {
        if self.flags.no_title_bar {
            return false;
        }

        let title_rect = Rect::new(self.rect.x, self.rect.y, self.rect.width, 24);
        title_rect.contains(point)
    }

    /// Verificar si un punto está en el área del cliente
    pub fn point_in_client_area(&self, point: Point) -> bool {
        self.client_rect.contains(point)
    }

    /// Agregar ventana hija
    pub fn add_child(&mut self, child_id: WindowId) -> bool {
        if self.child_count < self.children.len() {
            self.children[self.child_count] = Some(child_id);
            self.child_count += 1;
            true
        } else {
            false
        }
    }

    /// Remover ventana hija
    pub fn remove_child(&mut self, child_id: WindowId) {
        for i in 0..self.child_count {
            if self.children[i] == Some(child_id) {
                // Mover el último elemento a esta posición
                self.children[i] = self.children[self.child_count - 1];
                self.children[self.child_count - 1] = None;
                self.child_count -= 1;
                break;
            }
        }
    }
}

/// Lista de ventanas
pub struct WindowList {
    pub head: Option<NonNull<Window>>,
    pub tail: Option<NonNull<Window>>,
    pub count: usize,
}

impl WindowList {
    pub fn new() -> Self {
        Self {
            head: None,
            tail: None,
            count: 0,
        }
    }

    /// Agregar ventana al final de la lista
    pub fn push_back(&mut self, window: NonNull<Window>) {
        unsafe {
            let window_ptr = window.as_ptr();
            (*window_ptr).next = None;
            (*window_ptr).prev = self.tail;

            if let Some(tail) = self.tail {
                (*tail.as_ptr()).next = Some(window);
            } else {
                self.head = Some(window);
            }

            self.tail = Some(window);
            self.count += 1;
        }
    }

    /// Agregar ventana al principio de la lista (mayor Z-order)
    pub fn push_front(&mut self, window: NonNull<Window>) {
        unsafe {
            let window_ptr = window.as_ptr();
            (*window_ptr).next = self.head;
            (*window_ptr).prev = None;

            if let Some(head) = self.head {
                (*head.as_ptr()).prev = Some(window);
            } else {
                self.tail = Some(window);
            }

            self.head = Some(window);
            self.count += 1;
        }
    }

    /// Remover ventana de la lista
    pub fn remove(&mut self, window: NonNull<Window>) {
        unsafe {
            let window_ptr = window.as_ptr();

            if let Some(prev) = (*window_ptr).prev {
                (*prev.as_ptr()).next = (*window_ptr).next;
            } else {
                self.head = (*window_ptr).next;
            }

            if let Some(next) = (*window_ptr).next {
                (*next.as_ptr()).prev = (*window_ptr).prev;
            } else {
                self.tail = (*window_ptr).prev;
            }

            (*window_ptr).next = None;
            (*window_ptr).prev = None;
            self.count -= 1;
        }
    }

    /// Mover ventana al frente (mayor Z-order)
    pub fn bring_to_front(&mut self, window: NonNull<Window>) {
        self.remove(window);
        self.push_front(window);
    }

    /// Mover ventana al fondo (menor Z-order)
    pub fn send_to_back(&mut self, window: NonNull<Window>) {
        self.remove(window);
        self.push_back(window);
    }
}

/// Gestor de ventanas
pub struct WindowManager {
    pub windows: WindowList,
    pub focused_window: Option<WindowId>,
    pub window_counter: u32,
    pub desktop_color: Color,
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            windows: WindowList::new(),
            focused_window: None,
            window_counter: 1,
            desktop_color: Color::BLUE,
        }
    }

    /// Crear nueva ventana
    pub fn create_window(&mut self, title: &str, rect: Rect, flags: WindowFlags) -> WindowId {
        let id = self.window_counter;
        self.window_counter += 1;

        let window = Window::new(id, title, rect, flags);
        let window_ptr = unsafe { core::ptr::null_mut::<Window>() as *mut Window }; //  alloc(Layout::new::<Window>()) as *mut Window
        let window_non_null = unsafe { NonNull::new_unchecked(window_ptr) };

        self.windows.push_front(window_non_null);

        // Enfocar la nueva ventana
        self.focus_window(id);

        id
    }

    /// Cerrar ventana
    pub fn close_window(&mut self, id: WindowId) {
        // TODO: Implementar cierre de ventana
        // Esto requeriría encontrar la ventana en la lista y removerla
    }

    /// Enfocar ventana
    pub fn focus_window(&mut self, id: WindowId) {
        // Desenfocar ventana anterior
        if let Some(old_focused) = self.focused_window {
            // TODO: Encontrar y desenfocar la ventana anterior
        }

        // Enfocar nueva ventana
        self.focused_window = Some(id);
        // TODO: Encontrar y enfocar la nueva ventana
    }

    /// Encontrar ventana en punto
    pub fn window_at_point(&self, point: Point) -> Option<WindowId> {
        // Iterar desde el frente hacia atrás (mayor a menor Z-order)
        let mut current = self.windows.head;

        while let Some(window_ptr) = current {
            unsafe {
                let window = &*window_ptr.as_ptr();
                if window.is_visible && window.contains_point(point) {
                    return Some(window.id);
                }
                current = window.next;
            }
        }

        None
    }

    /// Obtener ventana por ID
    pub fn get_window(&self, id: WindowId) -> Option<&Window> {
        let mut current = self.windows.head;

        while let Some(window_ptr) = current {
            unsafe {
                let window = &*window_ptr.as_ptr();
                if window.id == id {
                    return Some(window);
                }
                current = window.next;
            }
        }

        None
    }

    /// Obtener ventana mutable por ID
    pub fn get_window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        let mut current = self.windows.head;

        while let Some(window_ptr) = current {
            unsafe {
                let window = &mut *window_ptr.as_ptr();
                if window.id == id {
                    return Some(window);
                }
                current = window.next;
            }
        }

        None
    }
}

/// Gestor de ventanas global
static mut WINDOW_MANAGER: Option<WindowManager> = None;

/// Inicializar el gestor de ventanas
pub fn init_window_manager() {
    let manager = WindowManager::new();
    unsafe {
        WINDOW_MANAGER = Some(manager);
    }
}

/// Obtener referencia al gestor de ventanas
pub fn get_window_manager() -> Option<&'static mut WindowManager> {
    unsafe { WINDOW_MANAGER.as_mut() }
}

/// Crear nueva ventana
pub fn create_window(title: &str, rect: Rect, flags: WindowFlags) -> Option<WindowId> {
    get_window_manager().map(|wm| wm.create_window(title, rect, flags))
}

/// Cerrar ventana
pub fn close_window(id: WindowId) {
    if let Some(wm) = get_window_manager() {
        wm.close_window(id);
    }
}

/// Enfocar ventana
pub fn focus_window(id: WindowId) {
    if let Some(wm) = get_window_manager() {
        wm.focus_window(id);
    }
}

/// Encontrar ventana en punto
pub fn window_at_point(point: Point) -> Option<WindowId> {
    get_window_manager()?.window_at_point(point)
}
