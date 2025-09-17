#![no_std]

use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::boxed::Box;

use crate::drivers::input_system::{InputSystem, InputEvent, InputEventType};
use crate::drivers::usb_keyboard::{UsbKeyCode, ModifierState};
use crate::drivers::input_system::KeyboardEvent;
use crate::drivers::usb_mouse::{MouseButton, MousePosition, MouseButtonState};
use crate::drivers::input_system::MouseEvent;
use crate::drivers::acceleration_2d::{Acceleration2D, AccelerationOperation};
use crate::drivers::framebuffer::{FramebufferDriver, Color};
use crate::desktop_ai::{Point, Rect};

/// Aplicación interactiva base
pub trait InteractiveApp: core::fmt::Debug {
    fn name(&self) -> &str;
    fn initialize(&mut self) -> Result<(), &'static str>;
    fn process_input(&mut self, event: &InputEvent) -> Result<(), &'static str>;
    fn update(&mut self) -> Result<(), &'static str>;
    fn render(&mut self, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str>;
    fn cleanup(&mut self) -> Result<(), &'static str>;
}

/// Aplicación de texto simple
#[derive(Debug)]
pub struct TextEditor {
    pub name: String,
    pub text_buffer: String,
    pub cursor_position: usize,
    pub scroll_offset: u32,
    pub window_rect: Rect,
    pub background_color: Color,
    pub text_color: Color,
    pub cursor_color: Color,
    pub initialized: bool,
}

impl TextEditor {
    pub fn new() -> Self {
        Self {
            name: String::from("Text Editor"),
            text_buffer: String::new(),
            cursor_position: 0,
            scroll_offset: 0,
            window_rect: Rect { x: 100, y: 100, width: 600, height: 400 },
            background_color: Color { r: 40, g: 40, b: 40, a: 255 },
            text_color: Color { r: 200, g: 200, b: 200, a: 255 },
            cursor_color: Color { r: 255, g: 255, b: 255, a: 255 },
            initialized: false,
        }
    }
    
    fn insert_char(&mut self, ch: char) {
        if self.cursor_position <= self.text_buffer.len() {
            self.text_buffer.insert(self.cursor_position, ch);
            self.cursor_position += 1;
        }
    }
    
    fn delete_char(&mut self) {
        if self.cursor_position > 0 && self.cursor_position <= self.text_buffer.len() {
            self.cursor_position -= 1;
            self.text_buffer.remove(self.cursor_position);
        }
    }
    
    fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
        }
    }
    
    fn move_cursor_right(&mut self) {
        if self.cursor_position < self.text_buffer.len() {
            self.cursor_position += 1;
        }
    }
}

impl InteractiveApp for TextEditor {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized = true;
        Ok(())
    }
    
    fn process_input(&mut self, event: &InputEvent) -> Result<(), &'static str> {
        if let InputEventType::Keyboard(keyboard_event) = &event.event_type {
            match keyboard_event {
                KeyboardEvent { key_code: key, pressed: true, .. } => {
                    match key {
                        UsbKeyCode::Backspace => {
                            self.delete_char();
                        }
                        UsbKeyCode::Left => {
                            self.move_cursor_left();
                        }
                        UsbKeyCode::Right => {
                            self.move_cursor_right();
                        }
                        UsbKeyCode::Enter => {
                            self.insert_char('\n');
                        }
                        UsbKeyCode::Tab => {
                            self.insert_char('\t');
                        }
                        _ => {
                            // Por ahora, no usamos shift (se puede mejorar más tarde)
                            if let Some(ch) = key.to_ascii(false, false) {
                                self.insert_char(ch);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    
    fn update(&mut self) -> Result<(), &'static str> {
        // Actualizar lógica de la aplicación
        Ok(())
    }
    
    fn render(&mut self, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str> {
        // Dibujar ventana
        let window_operation = AccelerationOperation::FillRect(
            self.window_rect,
            self.background_color
        );
        acceleration_2d.execute_operation(window_operation);
        
        // Dibujar borde de la ventana
        let border_operation = AccelerationOperation::DrawRect(
            self.window_rect,
            self.text_color,
            2
        );
        acceleration_2d.execute_operation(border_operation);
        
        // Dibujar texto (simplificado)
        let text_rect = Rect {
            x: self.window_rect.x + 10,
            y: self.window_rect.y + 10,
            width: self.window_rect.width - 20,
            height: self.window_rect.height - 20,
        };
        
        // Simular renderizado de texto
        let text_bg_operation = AccelerationOperation::FillRect(
            text_rect,
            Color { r: 20, g: 20, b: 20, a: 255 }
        );
        acceleration_2d.execute_operation(text_bg_operation);
        
        Ok(())
    }
    
    fn cleanup(&mut self) -> Result<(), &'static str> {
        self.initialized = false;
        Ok(())
    }
}

/// Aplicación de dibujo simple
#[derive(Debug)]
pub struct DrawingApp {
    pub name: String,
    pub canvas: Vec<Vec<Color>>,
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub current_color: Color,
    pub brush_size: u32,
    pub mouse_pressed: bool,
    pub last_mouse_pos: MousePosition,
    pub window_rect: Rect,
    pub initialized: bool,
}

impl DrawingApp {
    pub fn new() -> Self {
        let canvas_width = 400;
        let canvas_height = 300;
        let mut canvas = Vec::with_capacity(canvas_height as usize);
        for _ in 0..canvas_height {
            let mut row = Vec::with_capacity(canvas_width as usize);
            for _ in 0..canvas_width {
                row.push(Color { r: 0, g: 0, b: 0, a: 255 }); // Negro
            }
            canvas.push(row);
        }
        
        Self {
            name: String::from("Drawing App"),
            canvas,
            canvas_width,
            canvas_height,
            current_color: Color { r: 255, g: 255, b: 255, a: 255 }, // Blanco
            brush_size: 3,
            mouse_pressed: false,
            last_mouse_pos: (0, 0),
            window_rect: Rect { x: 200, y: 150, width: canvas_width + 40, height: canvas_height + 40 },
            initialized: false,
        }
    }
    
    fn draw_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x >= 0 && x < self.canvas_width as i32 && y >= 0 && y < self.canvas_height as i32 {
            self.canvas[y as usize][x as usize] = color;
        }
    }
    
    fn draw_line(&mut self, start: MousePosition, end: MousePosition, color: Color) {
        let dx = (end.0 - start.0).abs();
        let dy = (end.1 - start.1).abs();
        let sx = if start.0 < end.0 { 1 } else { -1 };
        let sy = if start.1 < end.1 { 1 } else { -1 };
        let mut err = dx - dy;
        
        let mut x = start.0;
        let mut y = start.1;
        
        loop {
            self.draw_pixel(x, y, color);
            
            if x == end.0 && y == end.1 {
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
    
    fn clear_canvas(&mut self) {
        for row in &mut self.canvas {
            for pixel in row {
                *pixel = Color { r: 0, g: 0, b: 0, a: 255 };
            }
        }
    }
}

impl InteractiveApp for DrawingApp {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized = true;
        Ok(())
    }
    
    fn process_input(&mut self, event: &InputEvent) -> Result<(), &'static str> {
        match &event.event_type {
            InputEventType::Mouse(mouse_event) => {
                match mouse_event {
                    MouseEvent { button: Some(button), pressed: true, .. } => {
                        if *button == MouseButton::Left {
                            self.mouse_pressed = true;
                            self.last_mouse_pos = mouse_event.position;
                        }
                    }
                    MouseEvent { button: Some(button), pressed: false, .. } => {
                        if *button == MouseButton::Left {
                            self.mouse_pressed = false;
                        }
                    }
                    MouseEvent { button: None, .. } => {
                        if self.mouse_pressed {
                            self.draw_line(self.last_mouse_pos, mouse_event.position, self.current_color);
                            self.last_mouse_pos = mouse_event.position;
                        }
                    }
                    _ => {}
                }
            }
            InputEventType::Keyboard(keyboard_event) => {
                match keyboard_event {
                    KeyboardEvent { key_code: key, pressed: true, .. } => {
                        match key {
                            UsbKeyCode::C => {
                                self.clear_canvas();
                            }
                            UsbKeyCode::Num1 => {
                                self.current_color = Color { r: 255, g: 0, b: 0, a: 255 }; // Rojo
                            }
                            UsbKeyCode::Num2 => {
                                self.current_color = Color { r: 0, g: 255, b: 0, a: 255 }; // Verde
                            }
                            UsbKeyCode::Num3 => {
                                self.current_color = Color { r: 0, g: 0, b: 255, a: 255 }; // Azul
                            }
                            UsbKeyCode::Num4 => {
                                self.current_color = Color { r: 255, g: 255, b: 255, a: 255 }; // Blanco
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }
    
    fn update(&mut self) -> Result<(), &'static str> {
        // Actualizar lógica de la aplicación
        Ok(())
    }
    
    fn render(&mut self, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str> {
        // Dibujar ventana
        let window_operation = AccelerationOperation::FillRect(
            self.window_rect,
            Color { r: 60, g: 60, b: 60, a: 255 }
        );
        acceleration_2d.execute_operation(window_operation);
        
        // Dibujar borde de la ventana
        let border_operation = AccelerationOperation::DrawRect(
            self.window_rect,
            Color { r: 200, g: 200, b: 200, a: 255 },
            2
        );
        acceleration_2d.execute_operation(border_operation);
        
        // Dibujar canvas
        let canvas_rect = Rect {
            x: self.window_rect.x + 20,
            y: self.window_rect.y + 20,
            width: self.canvas_width,
            height: self.canvas_height,
        };
        
        let canvas_bg_operation = AccelerationOperation::FillRect(
            canvas_rect,
            Color { r: 0, g: 0, b: 0, a: 255 }
        );
        acceleration_2d.execute_operation(canvas_bg_operation);
        
        // Dibujar contenido del canvas (simplificado)
        // En una implementación real, aquí se renderizaría pixel por pixel
        
        Ok(())
    }
    
    fn cleanup(&mut self) -> Result<(), &'static str> {
        self.initialized = false;
        Ok(())
    }
}

/// Aplicación de calculadora
#[derive(Debug)]
pub struct CalculatorApp {
    pub name: String,
    pub display: String,
    pub operation: Option<char>,
    pub first_number: f64,
    pub second_number: f64,
    pub result: f64,
    pub window_rect: Rect,
    pub initialized: bool,
}

impl CalculatorApp {
    pub fn new() -> Self {
        Self {
            name: String::from("Calculator"),
            display: String::from("0"),
            operation: None,
            first_number: 0.0,
            second_number: 0.0,
            result: 0.0,
            window_rect: Rect { x: 300, y: 200, width: 200, height: 300 },
            initialized: false,
        }
    }
    
    fn input_number(&mut self, num: char) {
        if self.display == "0" {
            self.display = String::from(num);
        } else {
            self.display.push(num);
        }
    }
    
    fn input_operation(&mut self, op: char) {
        if let Ok(num) = self.display.parse::<f64>() {
            self.first_number = num;
            self.operation = Some(op);
            self.display = String::from("0");
        }
    }
    
    fn calculate(&mut self) {
        if let Ok(num) = self.display.parse::<f64>() {
            self.second_number = num;
            
            if let Some(op) = self.operation {
                self.result = match op {
                    '+' => self.first_number + self.second_number,
                    '-' => self.first_number - self.second_number,
                    '*' => self.first_number * self.second_number,
                    '/' => {
                        if self.second_number != 0.0 {
                            self.first_number / self.second_number
                        } else {
                            f64::NAN
                        }
                    }
                    _ => self.first_number,
                };
                
                self.display = alloc::format!("{}", self.result);
                self.operation = None;
            }
        }
    }
    
    fn clear(&mut self) {
        self.display = String::from("0");
        self.operation = None;
        self.first_number = 0.0;
        self.second_number = 0.0;
        self.result = 0.0;
    }
}

impl InteractiveApp for CalculatorApp {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn initialize(&mut self) -> Result<(), &'static str> {
        self.initialized = true;
        Ok(())
    }
    
    fn process_input(&mut self, event: &InputEvent) -> Result<(), &'static str> {
        if let InputEventType::Keyboard(keyboard_event) = &event.event_type {
            match keyboard_event {
                KeyboardEvent { key_code: key, pressed: true, .. } => {
                    match key {
                        UsbKeyCode::Num0 => self.input_number('0'),
                        UsbKeyCode::Num1 => self.input_number('1'),
                        UsbKeyCode::Num2 => self.input_number('2'),
                        UsbKeyCode::Num3 => self.input_number('3'),
                        UsbKeyCode::Num4 => self.input_number('4'),
                        UsbKeyCode::Num5 => self.input_number('5'),
                        UsbKeyCode::Num6 => self.input_number('6'),
                        UsbKeyCode::Num7 => self.input_number('7'),
                        UsbKeyCode::Num8 => self.input_number('8'),
                        UsbKeyCode::Num9 => self.input_number('9'),
                        UsbKeyCode::Equal => self.calculate(),
                        UsbKeyCode::Minus => self.input_operation('-'),
                        UsbKeyCode::NumPadPlus => self.input_operation('+'),
                        UsbKeyCode::NumPadStar => self.input_operation('*'),
                        UsbKeyCode::Slash => self.input_operation('/'),
                        UsbKeyCode::Escape => self.clear(),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    
    fn update(&mut self) -> Result<(), &'static str> {
        // Actualizar lógica de la aplicación
        Ok(())
    }
    
    fn render(&mut self, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str> {
        // Dibujar ventana
        let window_operation = AccelerationOperation::FillRect(
            self.window_rect,
            Color { r: 50, g: 50, b: 50, a: 255 }
        );
        acceleration_2d.execute_operation(window_operation);
        
        // Dibujar borde de la ventana
        let border_operation = AccelerationOperation::DrawRect(
            self.window_rect,
            Color { r: 150, g: 150, b: 150, a: 255 },
            2
        );
        acceleration_2d.execute_operation(border_operation);
        
        // Dibujar display
        let display_rect = Rect {
            x: self.window_rect.x + 10,
            y: self.window_rect.y + 10,
            width: self.window_rect.width - 20,
            height: 40,
        };
        
        let display_bg_operation = AccelerationOperation::FillRect(
            display_rect,
            Color { r: 0, g: 0, b: 0, a: 255 }
        );
        acceleration_2d.execute_operation(display_bg_operation);
        
        // Dibujar borde del display
        let display_border_operation = AccelerationOperation::DrawRect(
            display_rect,
            Color { r: 100, g: 100, b: 100, a: 255 },
            1
        );
        acceleration_2d.execute_operation(display_border_operation);
        
        Ok(())
    }
    
    fn cleanup(&mut self) -> Result<(), &'static str> {
        self.initialized = false;
        Ok(())
    }
}

/// Gestor de aplicaciones interactivas
#[derive(Debug)]
pub struct InteractiveAppManager {
    pub apps: Vec<Box<dyn InteractiveApp>>,
    pub current_app: Option<usize>,
    pub initialized: bool,
}

impl InteractiveAppManager {
    pub fn new() -> Self {
        Self {
            apps: Vec::new(),
            current_app: None,
            initialized: false,
        }
    }
    
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Crear aplicaciones por defecto
        self.apps.push(Box::new(TextEditor::new()));
        self.apps.push(Box::new(DrawingApp::new()));
        self.apps.push(Box::new(CalculatorApp::new()));
        
        // Inicializar todas las aplicaciones
        for app in &mut self.apps {
            app.initialize()?;
        }
        
        self.initialized = true;
        Ok(())
    }
    
    pub fn add_app(&mut self, app: Box<dyn InteractiveApp>) -> Result<(), &'static str> {
        self.apps.push(app);
        Ok(())
    }
    
    pub fn switch_app(&mut self, app_index: usize) -> Result<(), &'static str> {
        if app_index < self.apps.len() {
            self.current_app = Some(app_index);
            Ok(())
        } else {
            Err("Índice de aplicación inválido")
        }
    }
    
    pub fn process_input(&mut self, event: &InputEvent) -> Result<(), &'static str> {
        if let Some(current_app) = self.current_app {
            if let Some(app) = self.apps.get_mut(current_app) {
                app.process_input(event)?;
            }
        }
        Ok(())
    }
    
    pub fn update(&mut self) -> Result<(), &'static str> {
        if let Some(current_app) = self.current_app {
            if let Some(app) = self.apps.get_mut(current_app) {
                app.update()?;
            }
        }
        Ok(())
    }
    
    pub fn render(&mut self, acceleration_2d: &mut Acceleration2D) -> Result<(), &'static str> {
        if let Some(current_app) = self.current_app {
            if let Some(app) = self.apps.get_mut(current_app) {
                app.render(acceleration_2d)?;
            }
        }
        Ok(())
    }
    
    pub fn get_app_count(&self) -> usize {
        self.apps.len()
    }
    
    pub fn get_current_app_name(&self) -> Option<&str> {
        if let Some(current_app) = self.current_app {
            if let Some(app) = self.apps.get(current_app) {
                Some(app.name())
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// Función de conveniencia para crear el gestor de aplicaciones
pub fn create_app_manager() -> InteractiveAppManager {
    InteractiveAppManager::new()
}
