#![no_std]

use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::boxed::Box;

use crate::drivers::input_system::{InputSystem, InputEvent, InputEventType};
use crate::drivers::usb_keyboard::{KeyboardEvent, UsbKeyCode, ModifierState};
use crate::drivers::usb_mouse::{MouseEvent, MouseButton, MousePosition, MouseButtonState};
use crate::drivers::acceleration_2d::{Acceleration2D, AccelerationOperation};
use crate::drivers::framebuffer::{FramebufferDriver, Color};
use crate::desktop_ai::{Point, Rect};

/// Integración del sistema de entrada con la aceleración 2D
/// Proporciona una interfaz unificada para aplicaciones gráficas

/// Estilo de ventana
#[derive(Debug, Clone)]
pub struct WindowStyle {
    pub background_color: Color,
    pub border_color: Color,
    pub border_width: u32,
    pub title_bar_color: Color,
    pub title_bar_height: u32,
    pub shadow_color: Color,
    pub shadow_offset: Point,
}

impl Default for WindowStyle {
    fn default() -> Self {
        Self {
            background_color: Color { r: 60, g: 60, b: 60, a: 255 },
            border_color: Color { r: 150, g: 150, b: 150, a: 255 },
            border_width: 2,
            title_bar_color: Color { r: 80, g: 80, b: 80, a: 255 },
            title_bar_height: 30,
            shadow_color: Color { r: 0, g: 0, b: 0, a: 100 },
            shadow_offset: Point { x: 3, y: 3 },
        }
    }
}

/// Ventana gráfica
#[derive(Debug, Clone)]
pub struct GuiWindow {
    pub id: u32,
    pub title: String,
    pub rect: Rect,
    pub style: WindowStyle,
    pub visible: bool,
    pub focused: bool,
    pub resizable: bool,
    pub movable: bool,
    pub min_size: Point,
    pub max_size: Point,
    pub content_rect: Rect,
}

impl GuiWindow {
    pub fn new(id: u32, title: String, rect: Rect) -> Self {
        let content_rect = Rect {
            x: rect.x + 2,
            y: rect.y + 32, // title_bar_height + border
            width: rect.width - 4,
            height: rect.height - 34,
        };
        
        Self {
            id,
            title,
            rect,
            style: WindowStyle::default(),
            visible: true,
            focused: false,
            resizable: true,
            movable: true,
            min_size: Point { x: 200, y: 150 },
            max_size: Point { x: 1920, y: 1080 },
            content_rect,
        }
    }
    
    pub fn contains_point(&self, point: Point) -> bool {
        point.x >= self.rect.x && point.x < self.rect.x + self.rect.width &&
        point.y >= self.rect.y && point.y < self.rect.y + self.rect.height
    }
    
    pub fn get_title_bar_rect(&self) -> Rect {
        Rect {
            x: self.rect.x,
            y: self.rect.y,
            width: self.rect.width,
            height: self.style.title_bar_height,
        }
    }
    
    pub fn move_to(&mut self, new_pos: Point) {
        let dx = new_pos.x - self.rect.x;
        let dy = new_pos.y - self.rect.y;
        
        self.rect.x = new_pos.x;
        self.rect.y = new_pos.y;
        
        self.content_rect.x += dx;
        self.content_rect.y += dy;
    }
    
    pub fn resize_to(&mut self, new_size: Point) {
        if new_size.x >= self.min_size.x && new_size.x <= self.max_size.x &&
           new_size.y >= self.min_size.y && new_size.y <= self.max_size.y {
            self.rect.width = new_size.x as u32;
            self.rect.height = new_size.y as u32;
            
            self.content_rect.width = self.rect.width - 4;
            self.content_rect.height = self.rect.height - 34;
        }
    }
}

/// Elemento de interfaz gráfica
pub trait GuiElement: core::fmt::Debug {
    fn get_id(&self) -> u32;
    fn get_rect(&self) -> Rect;
    fn is_visible(&self) -> bool;
    fn process_mouse_event(&mut self, event: &MouseEvent, window: &GuiWindow) -> bool;
    fn process_keyboard_event(&mut self, event: &KeyboardEvent, window: &GuiWindow) -> bool;
    fn render(&mut self, acceleration_2d: &mut Acceleration2D, window: &GuiWindow) -> Result<(), &'static str>;
}

/// Botón gráfico
#[derive(Debug)]
pub struct GuiButton {
    pub id: u32,
    pub rect: Rect,
    pub text: String,
    pub visible: bool,
    pub pressed: bool,
    pub hovered: bool,
    pub enabled: bool,
    pub style: ButtonStyle,
}

#[derive(Debug, Clone)]
pub struct ButtonStyle {
    pub background_color: Color,
    pub hover_color: Color,
    pub pressed_color: Color,
    pub disabled_color: Color,
    pub text_color: Color,
    pub border_color: Color,
    pub border_width: u32,
}

impl Default for ButtonStyle {
    fn default() -> Self {
        Self {
            background_color: Color { r: 100, g: 100, b: 100, a: 255 },
            hover_color: Color { r: 120, g: 120, b: 120, a: 255 },
            pressed_color: Color { r: 80, g: 80, b: 80, a: 255 },
            disabled_color: Color { r: 60, g: 60, b: 60, a: 255 },
            text_color: Color { r: 255, g: 255, b: 255, a: 255 },
            border_color: Color { r: 150, g: 150, b: 150, a: 255 },
            border_width: 1,
        }
    }
}

impl GuiButton {
    pub fn new(id: u32, rect: Rect, text: String) -> Self {
        Self {
            id,
            rect,
            text,
            visible: true,
            pressed: false,
            hovered: false,
            enabled: true,
            style: ButtonStyle::default(),
        }
    }
    
    pub fn contains_point(&self, point: Point) -> bool {
        point.x >= self.rect.x && point.x < self.rect.x + self.rect.width &&
        point.y >= self.rect.y && point.y < self.rect.y + self.rect.height
    }
}

impl GuiElement for GuiButton {
    fn get_id(&self) -> u32 {
        self.id
    }
    
    fn get_rect(&self) -> Rect {
        self.rect
    }
    
    fn is_visible(&self) -> bool {
        self.visible
    }
    
    fn process_mouse_event(&mut self, event: &MouseEvent, _window: &GuiWindow) -> bool {
        match event {
            MouseEvent::ButtonPress { button, position, .. } => {
                if *button == MouseButton::Left && self.contains_point(Point { x: position.x as u32, y: position.y as u32 }) && self.enabled {
                    self.pressed = true;
                    return true;
                }
            }
            MouseEvent::ButtonRelease { button, position, .. } => {
                if *button == MouseButton::Left {
                    if self.pressed && self.contains_point(Point { x: position.x as u32, y: position.y as u32 }) {
                        // Botón clickeado
                        self.pressed = false;
                        return true;
                    }
                    self.pressed = false;
                }
            }
            MouseEvent::Move { position, .. } => {
                self.hovered = self.contains_point(Point { x: position.x as u32, y: position.y as u32 });
            }
            _ => {}
        }
        false
    }
    
    fn process_keyboard_event(&mut self, _event: &KeyboardEvent, _window: &GuiWindow) -> bool {
        false
    }
    
    fn render(&mut self, acceleration_2d: &mut Acceleration2D, window: &GuiWindow) -> Result<(), &'static str> {
        if !self.visible {
            return Ok(());
        }
        
        // Calcular posición absoluta
        let absolute_rect = Rect {
            x: window.content_rect.x + self.rect.x,
            y: window.content_rect.y + self.rect.y,
            width: self.rect.width,
            height: self.rect.height,
        };
        
        // Determinar color del botón
        let bg_color = if !self.enabled {
            self.style.disabled_color
        } else if self.pressed {
            self.style.pressed_color
        } else if self.hovered {
            self.style.hover_color
        } else {
            self.style.background_color
        };
        
        // Dibujar fondo del botón
        let bg_operation = AccelerationOperation::FillRect(absolute_rect, bg_color);
        acceleration_2d.execute_operation(bg_operation);
        
        // Dibujar borde del botón
        let border_operation = AccelerationOperation::DrawRect(
            absolute_rect,
            self.style.border_color,
            self.style.border_width
        );
        acceleration_2d.execute_operation(border_operation);
        
        // En una implementación real, aquí se dibujaría el texto
        // Por simplicidad, solo dibujamos un rectángulo
        
        Ok(())
    }
}

/// Campo de texto gráfico
#[derive(Debug)]
pub struct GuiTextBox {
    pub id: u32,
    pub rect: Rect,
    pub text: String,
    pub visible: bool,
    pub focused: bool,
    pub cursor_position: usize,
    pub max_length: usize,
    pub style: TextBoxStyle,
}

#[derive(Debug, Clone)]
pub struct TextBoxStyle {
    pub background_color: Color,
    pub focused_color: Color,
    pub text_color: Color,
    pub cursor_color: Color,
    pub border_color: Color,
    pub focused_border_color: Color,
    pub border_width: u32,
}

impl Default for TextBoxStyle {
    fn default() -> Self {
        Self {
            background_color: Color { r: 40, g: 40, b: 40, a: 255 },
            focused_color: Color { r: 50, g: 50, b: 50, a: 255 },
            text_color: Color { r: 255, g: 255, b: 255, a: 255 },
            cursor_color: Color { r: 255, g: 255, b: 255, a: 255 },
            border_color: Color { r: 100, g: 100, b: 100, a: 255 },
            focused_border_color: Color { r: 150, g: 150, b: 150, a: 255 },
            border_width: 1,
        }
    }
}

impl GuiTextBox {
    pub fn new(id: u32, rect: Rect, max_length: usize) -> Self {
        Self {
            id,
            rect,
            text: String::new(),
            visible: true,
            focused: false,
            cursor_position: 0,
            max_length,
            style: TextBoxStyle::default(),
        }
    }
    
    pub fn contains_point(&self, point: Point) -> bool {
        point.x >= self.rect.x && point.x < self.rect.x + self.rect.width &&
        point.y >= self.rect.y && point.y < self.rect.y + self.rect.height
    }
    
    pub fn insert_char(&mut self, ch: char) {
        if self.text.len() < self.max_length {
            self.text.insert(self.cursor_position, ch);
            self.cursor_position += 1;
        }
    }
    
    pub fn delete_char(&mut self) {
        if self.cursor_position > 0 && self.cursor_position <= self.text.len() {
            self.cursor_position -= 1;
            self.text.remove(self.cursor_position);
        }
    }
}

impl GuiElement for GuiTextBox {
    fn get_id(&self) -> u32 {
        self.id
    }
    
    fn get_rect(&self) -> Rect {
        self.rect
    }
    
    fn is_visible(&self) -> bool {
        self.visible
    }
    
    fn process_mouse_event(&mut self, event: &MouseEvent, _window: &GuiWindow) -> bool {
        match event {
            MouseEvent::ButtonPress { button, position, .. } => {
                if *button == MouseButton::Left {
                    self.focused = self.contains_point(Point { x: position.x as u32, y: position.y as u32 });
                    return self.focused;
                }
            }
            _ => {}
        }
        false
    }
    
    fn process_keyboard_event(&mut self, event: &KeyboardEvent, _window: &GuiWindow) -> bool {
        if !self.focused {
            return false;
        }
        
        if event.pressed {
            match event.key_code {
                    UsbKeyCode::Backspace => {
                        self.delete_char();
                        return true;
                    }
                    UsbKeyCode::Left => {
                        if self.cursor_position > 0 {
                            self.cursor_position -= 1;
                        }
                        return true;
                    }
                    UsbKeyCode::Right => {
                        if self.cursor_position < self.text.len() {
                            self.cursor_position += 1;
                        }
                        return true;
                    }
                    _ => {
                        if let Some(ch) = event.key_code.to_ascii(event.modifiers.shift_pressed(), false) {
                            self.insert_char(ch);
                            return true;
                        }
                    }
                }
        }
        false
    }
    
    fn render(&mut self, acceleration_2d: &mut Acceleration2D, window: &GuiWindow) -> Result<(), &'static str> {
        if !self.visible {
            return Ok(());
        }
        
        // Calcular posición absoluta
        let absolute_rect = Rect {
            x: window.content_rect.x + self.rect.x,
            y: window.content_rect.y + self.rect.y,
            width: self.rect.width,
            height: self.rect.height,
        };
        
        // Determinar color de fondo
        let bg_color = if self.focused {
            self.style.focused_color
        } else {
            self.style.background_color
        };
        
        // Dibujar fondo
        let bg_operation = AccelerationOperation::FillRect(absolute_rect, bg_color);
        acceleration_2d.execute_operation(bg_operation);
        
        // Dibujar borde
        let border_color = if self.focused {
            self.style.focused_border_color
        } else {
            self.style.border_color
        };
        
        let border_operation = AccelerationOperation::DrawRect(
            absolute_rect,
            border_color,
            self.style.border_width
        );
        acceleration_2d.execute_operation(border_operation);
        
        // En una implementación real, aquí se dibujaría el texto y el cursor
        // Por simplicidad, solo dibujamos el rectángulo
        
        Ok(())
    }
}

/// Gestor de interfaz gráfica
#[derive(Debug)]
pub struct GuiManager {
    pub windows: Vec<GuiWindow>,
    pub elements: Vec<Box<dyn GuiElement>>,
    pub focused_window: Option<u32>,
    pub mouse_position: MousePosition,
    pub mouse_buttons: MouseButtonState,
    pub keyboard_modifiers: ModifierState,
    pub initialized: bool,
}

impl GuiManager {
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            elements: Vec::new(),
            focused_window: None,
            mouse_position: MousePosition::new(),
            mouse_buttons: MouseButtonState::new(),
            keyboard_modifiers: ModifierState::new(),
            initialized: false,
        }
    }
    
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized = true;
        Ok(())
    }
    
    pub fn create_window(&mut self, id: u32, title: String, rect: Rect) -> Result<(), &'static str> {
        let window = GuiWindow::new(id, title, rect);
        self.windows.push(window);
        Ok(())
    }
    
    pub fn add_element(&mut self, element: Box<dyn GuiElement>) -> Result<(), &'static str> {
        self.elements.push(element);
        Ok(())
    }
    
    pub fn process_input_event(&mut self, event: &InputEvent) -> Result<(), &'static str> {
        match &event.event_type {
            InputEventType::Mouse(mouse_event) => {
                self.process_mouse_event(mouse_event)?;
            }
            InputEventType::Keyboard(keyboard_event) => {
                self.process_keyboard_event(keyboard_event)?;
            }
            _ => {}
        }
        Ok(())
    }
    
    fn process_mouse_event(&mut self, event: &MouseEvent) -> Result<(), &'static str> {
        match event {
            MouseEvent::Move { position, buttons } => {
                self.mouse_position = *position;
                self.mouse_buttons = *buttons;
                
                // Verificar si el mouse está sobre alguna ventana
                for window in &mut self.windows {
                    if window.contains_point(Point { x: position.x as u32, y: position.y as u32 }) {
                        window.focused = true;
                        self.focused_window = Some(window.id);
                    } else {
                        window.focused = false;
                    }
                }
            }
            MouseEvent::ButtonPress { button, position, .. } => {
                // Procesar click en ventanas
                for window in &mut self.windows {
                    if window.contains_point(Point { x: position.x as u32, y: position.y as u32 }) {
                        window.focused = true;
                        self.focused_window = Some(window.id);
                        break;
                    }
                }
                
                // Procesar click en elementos
                if let Some(focused_window) = self.focused_window {
                    if let Some(window) = self.windows.iter().find(|w| w.id == focused_window) {
                        for element in &mut self.elements {
                            if element.process_mouse_event(event, window) {
                                break;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    
    fn process_keyboard_event(&mut self, event: &KeyboardEvent) -> Result<(), &'static str> {
        if event.pressed {
            self.keyboard_modifiers = event.modifiers;
                
            // Procesar teclado en ventana enfocada
            if let Some(focused_window) = self.focused_window {
                if let Some(window) = self.windows.iter().find(|w| w.id == focused_window) {
                    for element in &mut self.elements {
                        if element.process_keyboard_event(event, window) {
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    
    pub fn render(&mut self, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str> {
        // Renderizar ventanas
        let window_ids: Vec<_> = self.windows.iter().filter(|w| w.visible).map(|w| w.id).collect();
        for window_id in window_ids {
            self.render_window_by_id(window_id, acceleration_2d)?;
        }
        
        // Renderizar elementos
        for element in &mut self.elements {
            if element.is_visible() {
                if let Some(focused_window) = self.focused_window {
                    if let Some(window) = self.windows.iter().find(|w| w.id == focused_window) {
                        element.render(acceleration_2d, window)?;
                    }
                }
            }
        }
        
        Ok(())
    }
    
    fn render_window_by_id(&mut self, window_id: u32, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str> {
        let window_data = self.windows.iter().find(|w| w.id == window_id).cloned();
        if let Some(window) = window_data {
            self.render_window(&window, acceleration_2d)?;
        }
        Ok(())
    }
    
    fn render_window(&mut self, window: &GuiWindow, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str> {
        // Dibujar sombra
        let shadow_rect = Rect {
            x: window.rect.x + window.style.shadow_offset.x,
            y: window.rect.y + window.style.shadow_offset.y,
            width: window.rect.width,
            height: window.rect.height,
        };
        
        let shadow_operation = AccelerationOperation::FillRect(shadow_rect, window.style.shadow_color);
        acceleration_2d.execute_operation(shadow_operation);
        
        // Dibujar ventana
        let window_operation = AccelerationOperation::FillRect(window.rect, window.style.background_color);
        acceleration_2d.execute_operation(window_operation);
        
        // Dibujar borde de la ventana
        let border_operation = AccelerationOperation::DrawRect(
            window.rect,
            window.style.border_color,
            window.style.border_width
        );
        acceleration_2d.execute_operation(border_operation);
        
        // Dibujar barra de título
        let title_bar_rect = window.get_title_bar_rect();
        let title_bar_operation = AccelerationOperation::FillRect(
            title_bar_rect,
            window.style.title_bar_color
        );
        acceleration_2d.execute_operation(title_bar_operation);
        
        // Dibujar borde de la barra de título
        let title_border_operation = AccelerationOperation::DrawRect(
            title_bar_rect,
            window.style.border_color,
            1
        );
        acceleration_2d.execute_operation(title_border_operation);
        
        // En una implementación real, aquí se dibujaría el título de la ventana
        // Por simplicidad, solo dibujamos la estructura básica
        
        Ok(())
    }
    
    pub fn get_window_count(&self) -> usize {
        self.windows.len()
    }
    
    pub fn get_element_count(&self) -> usize {
        self.elements.len()
    }
    
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

/// Función de conveniencia para crear el gestor de GUI
pub fn create_gui_manager() -> GuiManager {
    GuiManager::new()
}
