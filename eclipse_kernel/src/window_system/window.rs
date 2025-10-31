//! Definición de ventana individual
//!
//! Representa una ventana individual en el sistema de ventanas.

use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::geometry::{Point, Rectangle, Size};
use super::protocol::WindowFlags;
use super::{ClientId, WindowId};

/// Estado de una ventana
#[derive(Debug, Clone, PartialEq)]
pub enum WindowState {
    Created,
    Mapped,
    Unmapped,
    Minimized,
    Maximized,
    Destroyed,
}

/// Tipo de ventana
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowType {
    Normal,
    Dialog,
    Tooltip,
    Menu,
    Popup,
}

/// Información de una ventana
#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub client_id: ClientId,
    pub title: String,
    pub geometry: Rectangle,
    pub state: WindowState,
    pub window_type: WindowType,
    pub flags: WindowFlags,
    pub mapped: bool,
    pub visible: bool,
    pub focused: bool,
    pub needs_redraw: bool,
}

impl Window {
    /// Crear nueva ventana
    pub fn new(
        id: WindowId,
        client_id: ClientId,
        title: String,
        geometry: Rectangle,
        flags: WindowFlags,
        window_type: WindowType,
    ) -> Self {
        Self {
            id,
            client_id,
            title,
            geometry,
            state: WindowState::Created,
            window_type,
            flags,
            mapped: false,
            visible: false,
            focused: false,
            needs_redraw: true,
        }
    }

    /// Mapear la ventana (hacerla visible)
    pub fn map(&mut self) {
        self.mapped = true;
        self.visible = true;
        self.state = WindowState::Mapped;
        self.needs_redraw = true;
    }

    /// Desmapear la ventana (ocultarla)
    pub fn unmap(&mut self) {
        self.mapped = false;
        self.visible = false;
        self.state = WindowState::Unmapped;
    }

    /// Minimizar la ventana
    pub fn minimize(&mut self) {
        self.visible = false;
        self.state = WindowState::Minimized;
    }

    /// Maximizar la ventana
    pub fn maximize(&mut self) {
        self.state = WindowState::Maximized;
        self.needs_redraw = true;
    }

    /// Restaurar la ventana
    pub fn restore(&mut self) {
        match self.state {
            WindowState::Minimized => {
                self.visible = true;
                self.state = WindowState::Mapped;
            }
            WindowState::Maximized => {
                self.state = WindowState::Mapped;
            }
            _ => {}
        }
        self.needs_redraw = true;
    }

    /// Dar foco a la ventana
    pub fn focus(&mut self) {
        self.focused = true;
    }

    /// Quitar foco de la ventana
    pub fn unfocus(&mut self) {
        self.focused = false;
    }

    /// Mover la ventana
    pub fn move_to(&mut self, x: i32, y: i32) {
        if self.flags.movable {
            self.geometry.x = x;
            self.geometry.y = y;
            self.needs_redraw = true;
        }
    }

    /// Redimensionar la ventana
    pub fn resize(&mut self, width: u32, height: u32) {
        if self.flags.resizable {
            self.geometry.width = width;
            self.geometry.height = height;
            self.needs_redraw = true;
        }
    }

    /// Cambiar título de la ventana
    pub fn set_title(&mut self, title: String) {
        self.title = title;
        self.needs_redraw = true;
    }

    /// Verificar si la ventana puede ser redimensionada
    pub fn can_resize(&self) -> bool {
        self.flags.resizable
    }

    /// Verificar si la ventana puede ser movida
    pub fn can_move(&self) -> bool {
        self.flags.movable
    }

    /// Verificar si la ventana puede ser minimizada
    pub fn can_minimize(&self) -> bool {
        self.flags.minimizable
    }

    /// Verificar si la ventana puede ser maximizada
    pub fn can_maximize(&self) -> bool {
        self.flags.maximizable
    }

    /// Verificar si la ventana puede ser cerrada
    pub fn can_close(&self) -> bool {
        self.flags.closeable
    }

    /// Verificar si la ventana está siempre encima
    pub fn is_always_on_top(&self) -> bool {
        self.flags.always_on_top
    }

    /// Verificar si la ventana es transparente
    pub fn is_transparent(&self) -> bool {
        self.flags.transparent
    }

    /// Obtener posición de la ventana
    pub fn position(&self) -> Point {
        Point::new(self.geometry.x, self.geometry.y)
    }

    /// Obtener tamaño de la ventana
    pub fn size(&self) -> Size {
        Size::new(self.geometry.width, self.geometry.height)
    }

    /// Obtener geometría de la ventana
    pub fn geometry(&self) -> Rectangle {
        self.geometry
    }

    /// Verificar si un punto está dentro de la ventana
    pub fn contains_point(&self, point: &Point) -> bool {
        self.geometry.contains_point(point)
    }

    /// Verificar si la ventana intersecta con un rectángulo
    pub fn intersects(&self, rect: &Rectangle) -> bool {
        self.geometry.intersects(rect)
    }

    /// Obtener área de la ventana
    pub fn area(&self) -> u32 {
        self.geometry.area()
    }

    /// Marcar ventana como que necesita redibujado
    pub fn mark_dirty(&mut self) {
        self.needs_redraw = true;
    }

    /// Marcar ventana como que no necesita redibujado
    pub fn mark_clean(&mut self) {
        self.needs_redraw = false;
    }

    /// Verificar si la ventana necesita redibujado
    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    /// Verificar si la ventana está visible
    pub fn is_visible(&self) -> bool {
        self.visible && self.mapped
    }

    /// Verificar si la ventana está mapeada
    pub fn is_mapped(&self) -> bool {
        self.mapped
    }

    /// Verificar si la ventana tiene foco
    pub fn has_focus(&self) -> bool {
        self.focused
    }

    /// Obtener estado de la ventana
    pub fn state(&self) -> &WindowState {
        &self.state
    }

    /// Obtener tipo de ventana
    pub fn window_type(&self) -> WindowType {
        self.window_type
    }

    /// Obtener flags de la ventana
    pub fn flags(&self) -> &WindowFlags {
        &self.flags
    }

    /// Obtener ID de la ventana
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// Obtener ID del cliente propietario
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// Obtener título de la ventana
    pub fn title(&self) -> &str {
        &self.title
    }
}
