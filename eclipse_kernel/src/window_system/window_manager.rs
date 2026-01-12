//! Gestor de ventanas del sistema de ventanas
//!
//! Maneja la creación, destrucción y gestión de ventanas,
//! similar al window manager de X11/Wayland.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::client_api::{get_client_api, ClientAPI};
use super::compositor::{get_window_compositor, WindowCompositor};
use super::geometry::{Point, Rectangle, Size};
use super::protocol::WindowFlags;
use super::window::{Window, WindowState, WindowType};
use super::{ClientId, WindowId};

/// Gestor de ventanas
pub struct WindowManager {
    /// Ventanas del sistema
    windows: BTreeMap<WindowId, Window>,
    /// Ventana con foco
    focused_window: Option<WindowId>,
    /// Ventana bajo el cursor
    window_under_cursor: Option<WindowId>,
    /// Orden de apilamiento de ventanas
    window_stack: Vec<WindowId>,
    /// Próximo ID de ventana
    next_window_id: AtomicU32,
    /// Gestor inicializado
    initialized: AtomicBool,
}

impl WindowManager {
    pub fn new() -> Result<Self, &'static str> {
        Ok(Self {
            windows: BTreeMap::new(),
            focused_window: None,
            window_under_cursor: None,
            window_stack: Vec::new(),
            next_window_id: AtomicU32::new(1),
            initialized: AtomicBool::new(false),
        })
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Crear una nueva ventana
    pub fn create_window(
        &mut self,
        client_id: ClientId,
        title: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        flags: WindowFlags,
        window_type: WindowType,
    ) -> Result<WindowId, &'static str> {
        crate::debug::serial_write_str("WM: create_window start\n");
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Gestor de ventanas no inicializado");
        }

        let window_id = self.next_window_id.fetch_add(1, Ordering::SeqCst);
        let geometry = Rectangle::new(x, y, width, height);

        crate::debug::serial_write_str("WM: Allocating Window struct...\n");
        let window = Window::new(
            window_id,
            client_id,
            title.clone(),
            geometry,
            flags,
            window_type,
        );
        crate::debug::serial_write_str("WM: Window struct allocated.\n");

        self.windows.insert(window_id, window);
        self.window_stack.push(window_id);

        // Registrar ventana en el compositor
        if let Ok(compositor) = get_window_compositor() {
            crate::debug::serial_write_str("WM: Registering with compositor...\n");
            compositor.register_window(window_id, geometry, window_type)?;
            crate::debug::serial_write_str("WM: Registered with compositor.\n");
        }

        // Registrar ventana en la API de cliente
        if let Ok(client_api) = get_client_api() {
            client_api.create_window(client_id, title, x, y, width, height, flags)?;
        }

        Ok(window_id)
    }

    /// Destruir una ventana
    pub fn destroy_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(_window) = self.windows.remove(&window_id) {
            // Remover del stack
            self.window_stack.retain(|&id| id != window_id);

            // Actualizar foco si era la ventana con foco
            if self.focused_window == Some(window_id) {
                self.focused_window = None;
            }

            // Actualizar ventana bajo cursor si era esta
            if self.window_under_cursor == Some(window_id) {
                self.window_under_cursor = None;
            }

            // Desregistrar del compositor
            if let Ok(compositor) = get_window_compositor() {
                compositor.unregister_window(window_id)?;
            }

            // Desregistrar de la API de cliente
            if let Ok(client_api) = get_client_api() {
                client_api.destroy_window(window_id)?;
            }
        }

        Ok(())
    }

    /// Mapear una ventana (hacerla visible)
    pub fn map_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.map();

            // Marcar para redibujado en el compositor
            if let Ok(compositor) = get_window_compositor() {
                compositor.mark_window_dirty(window_id)?;
            }

            // Notificar a la API de cliente
            if let Ok(client_api) = get_client_api() {
                client_api.map_window(window_id)?;
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Desmapear una ventana (ocultarla)
    pub fn unmap_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.unmap();

            // Quitar foco si era la ventana con foco
            if self.focused_window == Some(window_id) {
                self.focused_window = None;
            }

            // Notificar a la API de cliente
            if let Ok(client_api) = get_client_api() {
                client_api.unmap_window(window_id)?;
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Mover una ventana
    pub fn move_window(&mut self, window_id: WindowId, x: i32, y: i32) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.move_to(x, y);

            // Actualizar geometría en el compositor
            if let Ok(compositor) = get_window_compositor() {
                compositor.update_window_geometry(window_id, window.geometry)?;
            }

            // Notificar a la API de cliente
            if let Ok(client_api) = get_client_api() {
                client_api.move_window(window_id, x, y)?;
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Redimensionar una ventana
    pub fn resize_window(
        &mut self,
        window_id: WindowId,
        width: u32,
        height: u32,
    ) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.resize(width, height);

            // Actualizar geometría en el compositor
            if let Ok(compositor) = get_window_compositor() {
                compositor.update_window_geometry(window_id, window.geometry)?;
            }

            // Notificar a la API de cliente
            if let Ok(client_api) = get_client_api() {
                client_api.resize_window(window_id, width, height)?;
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Establecer título de una ventana
    pub fn set_window_title(
        &mut self,
        window_id: WindowId,
        title: String,
    ) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.set_title(title.clone());

            // Marcar para redibujado
            if let Ok(compositor) = get_window_compositor() {
                compositor.mark_window_dirty(window_id)?;
            }

            // Notificar a la API de cliente
            if let Ok(client_api) = get_client_api() {
                client_api.set_window_title(window_id, title)?;
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Dar foco a una ventana
    pub fn focus_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get(&window_id) {
            if window.is_visible() {
                // Quitar foco de la ventana anterior
                if let Some(old_focused) = self.focused_window {
                    if let Some(old_window) = self.windows.get_mut(&old_focused) {
                        old_window.unfocus();
                    }
                }

                // Dar foco a la nueva ventana
                if let Some(new_window) = self.windows.get_mut(&window_id) {
                    new_window.focus();
                }

                self.focused_window = Some(window_id);

                // Traer ventana al frente
                self.bring_window_to_front(window_id)?;

                // Notificar a la API de cliente
                if let Ok(client_api) = get_client_api() {
                    client_api.focus_window(window_id)?;
                }
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Traer ventana al frente
    pub fn bring_window_to_front(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        // Remover de la posición actual en el stack
        self.window_stack.retain(|&id| id != window_id);

        // Agregar al final (frente)
        self.window_stack.push(window_id);

        // Actualizar Z-order en el compositor
        if let Ok(compositor) = get_window_compositor() {
            let z_order = self.window_stack.len() as i32;
            compositor.set_window_z_order(window_id, z_order)?;
        }

        Ok(())
    }

    /// Minimizar una ventana
    pub fn minimize_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            if window.can_minimize() {
                window.minimize();

                // Quitar foco si era la ventana con foco
                if self.focused_window == Some(window_id) {
                    self.focused_window = None;
                }

                // Notificar a la API de cliente
                if let Ok(client_api) = get_client_api() {
                    client_api.unmap_window(window_id)?; // Minimizar es como desmapear
                }
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Maximizar una ventana
    pub fn maximize_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            if window.can_maximize() {
                window.maximize();

                // Marcar para redibujado
                if let Ok(compositor) = get_window_compositor() {
                    compositor.mark_window_dirty(window_id)?;
                }
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Restaurar una ventana
    pub fn restore_window(&mut self, window_id: WindowId) -> Result<(), &'static str> {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.restore();

            // Marcar para redibujado
            if let Ok(compositor) = get_window_compositor() {
                compositor.mark_window_dirty(window_id)?;
            }

            // Notificar a la API de cliente si se está restaurando desde minimizada
            if matches!(window.state(), WindowState::Mapped) {
                if let Ok(client_api) = get_client_api() {
                    client_api.map_window(window_id)?;
                }
            }
        } else {
            return Err("Ventana no encontrada");
        }

        Ok(())
    }

    /// Obtener ventana bajo un punto
    pub fn get_window_at(&self, point: Point) -> Option<WindowId> {
        // Buscar desde la ventana más al frente hacia atrás
        for &window_id in self.window_stack.iter().rev() {
            if let Some(window) = self.windows.get(&window_id) {
                if window.is_visible() && window.contains_point(&point) {
                    return Some(window_id);
                }
            }
        }
        None
    }

    /// Obtener ventanas en un área
    pub fn get_windows_in_area(&self, area: Rectangle) -> Vec<WindowId> {
        let mut windows = Vec::new();

        for &window_id in &self.window_stack {
            if let Some(window) = self.windows.get(&window_id) {
                if window.is_visible() && window.intersects(&area) {
                    windows.push(window_id);
                }
            }
        }

        windows
    }

    /// Obtener ventana con foco
    pub fn get_focused_window(&self) -> Option<WindowId> {
        self.focused_window
    }

    /// Obtener ventana bajo el cursor
    pub fn get_window_under_cursor(&self) -> Option<WindowId> {
        self.window_under_cursor
    }

    /// Establecer ventana bajo el cursor
    pub fn set_window_under_cursor(&mut self, window_id: Option<WindowId>) {
        self.window_under_cursor = window_id;
    }

    /// Obtener información de una ventana
    pub fn get_window(&self, window_id: WindowId) -> Option<&Window> {
        self.windows.get(&window_id)
    }

    /// Obtener información mutable de una ventana
    pub fn get_window_mut(&mut self, window_id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&window_id)
    }

    /// Obtener todas las ventanas
    pub fn get_all_windows(&self) -> Vec<WindowId> {
        self.window_stack.clone()
    }

    /// Obtener ventanas visibles
    pub fn get_visible_windows(&self) -> Vec<WindowId> {
        self.window_stack
            .iter()
            .filter(|&&window_id| {
                self.windows
                    .get(&window_id)
                    .map(|w| w.is_visible())
                    .unwrap_or(false)
            })
            .copied()
            .collect()
    }

    /// Obtener número de ventanas
    pub fn get_window_count(&self) -> u32 {
        self.windows.len() as u32
    }

    /// Obtener número de ventanas visibles
    pub fn get_visible_window_count(&self) -> u32 {
        self.get_visible_windows().len() as u32
    }

    /// Obtener estadísticas del gestor de ventanas
    pub fn get_stats(&self) -> WindowManagerStats {
        WindowManagerStats {
            total_windows: self.windows.len(),
            visible_windows: self.get_visible_windows().len(),
            focused_window: self.focused_window,
            window_under_cursor: self.window_under_cursor,
        }
    }
}

/// Estadísticas del gestor de ventanas
#[derive(Debug, Clone)]
pub struct WindowManagerStats {
    pub total_windows: usize,
    pub visible_windows: usize,
    pub focused_window: Option<WindowId>,
    pub window_under_cursor: Option<WindowId>,
}

/// Instancia global del gestor de ventanas
static mut WINDOW_MANAGER: Option<WindowManager> = None;

/// Inicializar el gestor de ventanas global
pub fn init_window_manager() -> Result<(), &'static str> {
    unsafe {
        if WINDOW_MANAGER.is_some() {
            return Err("Gestor de ventanas ya inicializado");
        }

        let mut manager = WindowManager::new()?;
        manager.initialize()?;
        WINDOW_MANAGER = Some(manager);
    }
    Ok(())
}

/// Obtener referencia al gestor de ventanas
pub fn get_window_manager() -> Result<&'static mut WindowManager, &'static str> {
    unsafe {
        WINDOW_MANAGER
            .as_mut()
            .ok_or("Gestor de ventanas no inicializado")
    }
}

/// Verificar si el gestor de ventanas está inicializado
pub fn is_window_manager_initialized() -> bool {
    unsafe { WINDOW_MANAGER.is_some() }
}

/// Crear ventana globalmente
pub fn create_global_window(
    client_id: ClientId,
    title: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    flags: WindowFlags,
    window_type: WindowType,
) -> Result<WindowId, &'static str> {
    let manager = super::get_window_manager()?;
    crate::debug::serial_write_str(&alloc::format!("WM: Creating global window '{}' size {}x{}\n", title, width, height));
    manager.create_window(client_id, title, x, y, width, height, flags, window_type)
}

/// Destruir ventana globalmente
pub fn destroy_global_window(window_id: WindowId) -> Result<(), &'static str> {
    let manager = super::get_window_manager()?;
    manager.destroy_window(window_id)
}

/// Obtener ventana bajo punto globalmente
pub fn get_global_window_at(point: Point) -> Result<Option<WindowId>, &'static str> {
    let manager = super::get_window_manager()?;
    Ok(manager.get_window_at(point))
}
