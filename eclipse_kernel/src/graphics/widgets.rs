//! Sistema de widgets y controles para Eclipse OS
//! 
//! Implementa widgets básicos como botones, etiquetas, campos de texto, etc.

use super::window_system::{Window, WindowId, Position, Size, Rectangle};
use crate::drivers::framebuffer::{FramebufferDriver, Color};
use core::fmt;
use crate::syslog;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;

/// ID único de widget
pub type WidgetId = u32;

/// Tipo de widget
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetType {
    Button,
    Label,
    TextField,
    Checkbox,
    RadioButton,
    Slider,
    ProgressBar,
    ListBox,
    ComboBox,
    Menu,
    Toolbar,
    StatusBar,
}

/// Estado de widget
#[derive(Debug, Clone, PartialEq)]
pub enum WidgetState {
    Normal,
    Hovered,
    Pressed,
    Disabled,
    Focused,
}

/// Evento de widget
#[derive(Debug, Clone)]
pub enum WidgetEvent {
    Clicked,
    DoubleClicked,
    RightClicked,
    TextChanged { old_text: String, new_text: String },
    ValueChanged { old_value: i32, new_value: i32 },
    Focused,
    Unfocused,
    Hovered,
    Unhovered,
}

/// Widget base
#[derive(Debug, Clone)]
pub struct Widget {
    pub id: WidgetId,
    pub widget_type: WidgetType,
    pub position: Position,
    pub size: Size,
    pub state: WidgetState,
    pub visible: bool,
    pub enabled: bool,
    pub text: String,
    pub value: i32,
    pub min_value: i32,
    pub max_value: i32,
    pub parent: Option<WindowId>,
    pub children: Vec<WidgetId>,
    pub style: WidgetStyle,
    pub callback: Option<fn(WidgetId, WidgetEvent)>,
}

/// Estilo de widget
#[derive(Debug, Clone)]
pub struct WidgetStyle {
    pub background_color: u32,
    pub text_color: u32,
    pub border_color: u32,
    pub hover_color: u32,
    pub pressed_color: u32,
    pub disabled_color: u32,
    pub border_width: u32,
    pub font_size: u32,
    pub padding: u32,
}

impl Default for WidgetStyle {
    fn default() -> Self {
        Self {
            background_color: 0xE0E0E0, // Gris claro
            text_color: 0x000000,       // Negro
            border_color: 0x808080,     // Gris
            hover_color: 0xD0D0D0,      // Gris más claro
            pressed_color: 0xC0C0C0,    // Gris medio
            disabled_color: 0xF0F0F0,   // Gris muy claro
            border_width: 1,
            font_size: 12,
            padding: 4,
        }
    }
}

impl Widget {
    /// Crear nuevo widget
    pub fn new(id: WidgetId, widget_type: WidgetType, position: Position, size: Size) -> Self {
        Self {
            id,
            widget_type,
            position,
            size,
            state: WidgetState::Normal,
            visible: true,
            enabled: true,
            text: String::new(),
            value: 0,
            min_value: 0,
            max_value: 100,
            parent: None,
            children: Vec::new(),
            style: WidgetStyle::default(),
            callback: None,
        }
    }

    /// Obtener rectángulo del widget
    pub fn get_rectangle(&self) -> Rectangle {
        Rectangle {
            x: self.position.x,
            y: self.position.y,
            width: self.size.width,
            height: self.size.height,
        }
    }

    /// Verificar si un punto está dentro del widget
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.position.x && 
        y >= self.position.y && 
        x < (self.position.x + self.size.width as i32) && 
        y < (self.position.y + self.size.height as i32)
    }

    /// Establecer texto
    pub fn set_text(&mut self, text: String) {
        self.text = text;
    }

    /// Establecer valor
    pub fn set_value(&mut self, value: i32) {
        let old_value = self.value;
        self.value = value.clamp(self.min_value, self.max_value);
        
        if old_value != self.value {
            self.trigger_event(WidgetEvent::ValueChanged { old_value, new_value: self.value });
        }
    }

    /// Establecer estado
    pub fn set_state(&mut self, state: WidgetState) {
        self.state = state;
    }

    /// Establecer habilitado
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.state = WidgetState::Disabled;
        }
    }

    /// Establecer visibilidad
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Establecer callback
    pub fn set_callback(&mut self, callback: fn(WidgetId, WidgetEvent)) {
        self.callback = Some(callback);
    }

    /// Disparar evento
    pub fn trigger_event(&self, event: WidgetEvent) {
        if let Some(callback) = self.callback {
            callback(self.id, event);
        }
    }

    /// Manejar clic
    pub fn handle_click(&mut self, x: i32, y: i32) {
        if self.enabled && self.visible && self.contains_point(x, y) {
            self.set_state(WidgetState::Pressed);
            self.trigger_event(WidgetEvent::Clicked);
        }
    }

    /// Manejar hover
    pub fn handle_hover(&mut self, x: i32, y: i32) {
        if self.enabled && self.visible {
            if self.contains_point(x, y) {
                if self.state != WidgetState::Hovered {
                    self.set_state(WidgetState::Hovered);
                    self.trigger_event(WidgetEvent::Hovered);
                }
            } else {
                if self.state == WidgetState::Hovered {
                    self.set_state(WidgetState::Normal);
                    self.trigger_event(WidgetEvent::Unhovered);
                }
            }
        }
    }
}

/// Gestor de widgets
pub struct WidgetManager {
    widgets: BTreeMap<WidgetId, Widget>,
    next_widget_id: WidgetId,
    focused_widget: Option<WidgetId>,
}

impl WidgetManager {
    /// Crear nuevo gestor de widgets
    pub fn new() -> Self {
        Self {
            widgets: BTreeMap::new(),
            next_widget_id: 1,
            focused_widget: None,
        }
    }

    /// Crear nuevo widget
    pub fn create_widget(&mut self, widget_type: WidgetType, position: Position, size: Size) -> WidgetId {
        let id = self.next_widget_id;
        self.next_widget_id += 1;

        let widget = Widget::new(id, widget_type, position, size);
        self.widgets.insert(id, widget);
        
        id
    }

    /// Obtener widget
    pub fn get_widget(&self, widget_id: WidgetId) -> Option<&Widget> {
        self.widgets.get(&widget_id)
    }

    /// Obtener widget mutable
    pub fn get_widget_mut(&mut self, widget_id: WidgetId) -> Option<&mut Widget> {
        self.widgets.get_mut(&widget_id)
    }

    /// Destruir widget
    pub fn destroy_widget(&mut self, widget_id: WidgetId) -> Result<(), String> {
        if let Some(widget) = self.widgets.remove(&widget_id) {
            // Destruir widgets hijos
            for child_id in widget.children {
                self.destroy_widget(child_id)?;
            }

            // Limpiar foco si era el widget enfocado
            if self.focused_widget == Some(widget_id) {
                self.focused_widget = None;
            }

            Ok(())
        } else {
            Err(format!("Widget no encontrado: {}", widget_id))
        }
    }

    /// Dibujar widget
    pub fn draw_widget(&self, framebuffer: &mut FramebufferDriver, widget_id: WidgetId) -> Result<(), String> {
        if let Some(widget) = self.widgets.get(&widget_id) {
            if widget.visible {
                self.draw_widget_internal(framebuffer, widget);
            }
            Ok(())
        } else {
            Err(format!("Widget no encontrado: {}", widget_id))
        }
    }

    /// Dibujar widget interno
    fn draw_widget_internal(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        let rect = widget.get_rectangle();
        
        // Determinar color de fondo
        let bg_color = match widget.state {
            WidgetState::Normal => widget.style.background_color,
            WidgetState::Hovered => widget.style.hover_color,
            WidgetState::Pressed => widget.style.pressed_color,
            WidgetState::Disabled => widget.style.disabled_color,
            WidgetState::Focused => widget.style.background_color,
        };

        // Dibujar fondo
        framebuffer.draw_rect(rect.x as u32, rect.y as u32, rect.width as u32, rect.height as u32, Color::from_hex(bg_color));

        // Dibujar borde
        if widget.style.border_width > 0 {
            framebuffer.draw_rect(rect.x as u32, rect.y as u32, rect.width as u32, widget.style.border_width as u32, Color::from_hex(widget.style.border_color));
            framebuffer.draw_rect(rect.x as u32, rect.y as u32, widget.style.border_width as u32, rect.height as u32, Color::from_hex(widget.style.border_color));
            framebuffer.draw_rect((rect.x + rect.width as i32 - widget.style.border_width as i32) as u32, rect.y as u32, widget.style.border_width as u32, rect.height as u32, Color::from_hex(widget.style.border_color));
            framebuffer.draw_rect(rect.x as u32, (rect.y + rect.height as i32 - widget.style.border_width as i32) as u32, rect.width as u32, widget.style.border_width as u32, Color::from_hex(widget.style.border_color));
        }

        // Dibujar contenido específico del widget
        match widget.widget_type {
            WidgetType::Button => self.draw_button(framebuffer, widget),
            WidgetType::Label => self.draw_label(framebuffer, widget),
            WidgetType::TextField => self.draw_text_field(framebuffer, widget),
            WidgetType::Checkbox => self.draw_checkbox(framebuffer, widget),
            WidgetType::RadioButton => self.draw_radio_button(framebuffer, widget),
            WidgetType::Slider => self.draw_slider(framebuffer, widget),
            WidgetType::ProgressBar => self.draw_progress_bar(framebuffer, widget),
            _ => {
                // Para otros tipos, dibujar texto genérico
                if !widget.text.is_empty() {
                    framebuffer.write_text_kernel(&widget.text, Color::BLACK);
                }
            }
        }
    }

    /// Dibujar botón
    fn draw_button(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        let rect = widget.get_rectangle();
        
        // Dibujar texto del botón centrado
        if !widget.text.is_empty() {
            let text_x = rect.x + (rect.width / 2) as i32 - (widget.text.len() as i32 * 6) / 2;
            let text_y = rect.y + (rect.height / 2) as i32 - 6;
            framebuffer.write_text_kernel(&widget.text, Color::BLACK);
        }
    }

    /// Dibujar etiqueta
    fn draw_label(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        if !widget.text.is_empty() {
            framebuffer.write_text_kernel(&widget.text, Color::BLACK);
        }
    }

    /// Dibujar campo de texto
    fn draw_text_field(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        let rect = widget.get_rectangle();
        
        // Dibujar texto del campo
        if !widget.text.is_empty() {
            framebuffer.write_text_kernel(&widget.text, Color::BLACK);
        }
        
        // Dibujar cursor si está enfocado
        if widget.state == WidgetState::Focused {
            let cursor_x = rect.x + (widget.text.len() as i32 * 6) + 2;
            let cursor_y = rect.y + 2;
            framebuffer.draw_rect(cursor_x as u32, cursor_y as u32, 1 as u32, rect.height - 4 as u32, Color::from_hex(0x000000));
        }
    }

    /// Dibujar checkbox
    fn draw_checkbox(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        let rect = widget.get_rectangle();
        let checkbox_size = 16;
        let checkbox_x = rect.x + 2;
        let checkbox_y = rect.y + (rect.height - checkbox_size) as i32 / 2;
        
        // Dibujar cuadrado del checkbox
        framebuffer.draw_rect(checkbox_x as u32, checkbox_y as u32, checkbox_size as u32, checkbox_size as u32, Color::from_hex(0xFFFFFF));
        framebuffer.draw_rect(checkbox_x as u32, checkbox_y as u32, checkbox_size as u32, 1 as u32, Color::from_hex(0x000000));
        framebuffer.draw_rect(checkbox_x as u32, checkbox_y as u32, 1 as u32, checkbox_size as u32, Color::from_hex(0x000000));
            framebuffer.draw_rect((checkbox_x + checkbox_size as i32 - 1) as u32, checkbox_y as u32, 1 as u32, checkbox_size as u32, Color::from_hex(0x000000));
            framebuffer.draw_rect(checkbox_x as u32, (checkbox_y + checkbox_size as i32 - 1) as u32, checkbox_size as u32, 1 as u32, Color::from_hex(0x000000));
        
        // Dibujar marca si está marcado
        if widget.value != 0 {
            framebuffer.draw_rect((checkbox_x + 3) as u32, (checkbox_y + 3) as u32, 10 as u32, 2 as u32, Color::from_hex(0x000000));
            framebuffer.draw_rect((checkbox_x + 3) as u32, (checkbox_y + 5) as u32, 2 as u32, 6 as u32, Color::from_hex(0x000000));
            framebuffer.draw_rect((checkbox_x + 5) as u32, (checkbox_y + 7) as u32, 6 as u32, 2 as u32, Color::from_hex(0x000000));
        }
        
        // Dibujar texto
        if !widget.text.is_empty() {
            let text_x = checkbox_x + checkbox_size as i32 + 4;
            let text_y = checkbox_y + 4;
            framebuffer.write_text_kernel(&widget.text, Color::BLACK);
        }
    }

    /// Dibujar radio button
    fn draw_radio_button(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        let rect = widget.get_rectangle();
        let radio_size = 16;
        let radio_x = rect.x + 2;
        let radio_y = rect.y + (rect.height - radio_size) as i32 / 2;
        
        // Dibujar círculo del radio button (simulado como cuadrado)
        framebuffer.draw_rect(radio_x as u32, radio_y as u32, radio_size as u32, radio_size as u32, Color::from_hex(0xFFFFFF));
        framebuffer.draw_rect(radio_x as u32, radio_y as u32, radio_size as u32, 1 as u32, Color::from_hex(0x000000));
        framebuffer.draw_rect(radio_x as u32, radio_y as u32, 1 as u32, radio_size as u32, Color::from_hex(0x000000));
            framebuffer.draw_rect((radio_x + radio_size as i32 - 1) as u32, radio_y as u32, 1 as u32, radio_size as u32, Color::from_hex(0x000000));
            framebuffer.draw_rect(radio_x as u32, (radio_y + radio_size as i32 - 1) as u32, radio_size as u32, 1 as u32, Color::from_hex(0x000000));
        
        // Dibujar punto si está seleccionado
        if widget.value != 0 {
            framebuffer.draw_rect((radio_x + 4) as u32, (radio_y + 4) as u32, 8 as u32, 8 as u32, Color::from_hex(0x000000));
        }
        
        // Dibujar texto
        if !widget.text.is_empty() {
            let text_x = radio_x + radio_size as i32 + 4;
            let text_y = radio_y + 4;
            framebuffer.write_text_kernel(&widget.text, Color::BLACK);
        }
    }

    /// Dibujar slider
    fn draw_slider(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        let rect = widget.get_rectangle();
        let slider_height = 4;
        let slider_y = rect.y + (rect.height - slider_height) as i32 / 2;
        
        // Dibujar pista del slider
        framebuffer.draw_rect(rect.x as u32, slider_y as u32, rect.width as u32, slider_height as u32, Color::from_hex(0xCCCCCC));
        
        // Calcular posición del thumb
        let thumb_width = 16;
        let thumb_x = rect.x + ((widget.value - widget.min_value) as u32 * (rect.width - thumb_width) / (widget.max_value - widget.min_value) as u32) as i32;
        let thumb_y = rect.y + (rect.height - 16) as i32 / 2;
        
        // Dibujar thumb
        framebuffer.draw_rect(thumb_x as u32, thumb_y as u32, thumb_width as u32, 16 as u32, Color::from_hex(0x666666));
    }

    /// Dibujar barra de progreso
    fn draw_progress_bar(&self, framebuffer: &mut FramebufferDriver, widget: &Widget) {
        let rect = widget.get_rectangle();
        
        // Dibujar fondo de la barra
        framebuffer.draw_rect(rect.x as u32, rect.y as u32, rect.width as u32, rect.height as u32, Color::from_hex(0xCCCCCC));
        
        // Calcular ancho de la barra de progreso
        let progress_width = ((widget.value - widget.min_value) as u32 * rect.width / (widget.max_value - widget.min_value) as u32) as u32;
        
        // Dibujar barra de progreso
        if progress_width > 0 {
            framebuffer.draw_rect(rect.x as u32, rect.y as u32, progress_width as u32, rect.height as u32, Color::from_hex(0x00AA00));
        }
    }

    /// Manejar clic en widget
    pub fn handle_click(&mut self, x: i32, y: i32) -> Option<WidgetId> {
        // Buscar widget en la posición
        for (widget_id, widget) in &mut self.widgets {
            if widget.visible && widget.enabled && widget.contains_point(x, y) {
                widget.handle_click(x, y);
                return Some(*widget_id);
            }
        }
        None
    }

    /// Manejar hover en widget
    pub fn handle_hover(&mut self, x: i32, y: i32) {
        for (_, widget) in &mut self.widgets {
            widget.handle_hover(x, y);
        }
    }

    /// Obtener estadísticas del gestor de widgets
    pub fn get_statistics(&self) -> WidgetSystemStats {
        let total_widgets = self.widgets.len();
        let visible_widgets = self.widgets.values().filter(|w| w.visible).count();
        let enabled_widgets = self.widgets.values().filter(|w| w.enabled).count();
        let focused_widgets = self.widgets.values().filter(|w| w.state == WidgetState::Focused).count();

        WidgetSystemStats {
            total_widgets,
            visible_widgets,
            enabled_widgets,
            focused_widgets,
            next_widget_id: self.next_widget_id,
        }
    }
}

/// Estadísticas del sistema de widgets
#[derive(Debug, Clone)]
pub struct WidgetSystemStats {
    pub total_widgets: usize,
    pub visible_widgets: usize,
    pub enabled_widgets: usize,
    pub focused_widgets: usize,
    pub next_widget_id: WidgetId,
}

impl fmt::Display for WidgetSystemStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Widget System Stats: {} total, {} visible, {} enabled, {} focused",
            self.total_widgets,
            self.visible_widgets,
            self.enabled_widgets,
            self.focused_widgets
        )
    }
}
