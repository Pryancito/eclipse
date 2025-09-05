//! Sistema de terminal para Eclipse OS
//! 
//! Proporciona una interfaz de línea de comandos básica

use core::fmt;
use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::collections::VecDeque;
use alloc::format;

/// Terminal principal del sistema
pub struct Terminal {
    pub width: u32,
    pub height: u32,
    pub cursor_x: u32,
    pub cursor_y: u32,
    pub buffer: TerminalBuffer,
    pub history: VecDeque<String>,
    pub history_index: usize,
    pub current_line: String,
    pub prompt: String,
    pub scroll_offset: u32,
    pub max_history: usize,
}

/// Buffer del terminal
pub struct TerminalBuffer {
    pub lines: Vec<TerminalLine>,
    pub max_lines: usize,
}

/// Línea del terminal
pub struct TerminalLine {
    pub text: String,
    pub color: TerminalColor,
    pub style: TerminalStyle,
}

/// Colores del terminal
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TerminalColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Default,
}

/// Estilos del terminal
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TerminalStyle {
    Normal,
    Bold,
    Dim,
    Italic,
    Underline,
    Blink,
    Reverse,
    Strikethrough,
}

/// Cursor del terminal
pub struct TerminalCursor {
    pub x: u32,
    pub y: u32,
    pub visible: bool,
    pub blink: bool,
}

impl Terminal {
    /// Crear nuevo terminal
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            cursor_x: 0,
            cursor_y: 0,
            buffer: TerminalBuffer::new(height as usize),
            history: VecDeque::new(),
            history_index: 0,
            current_line: String::new(),
            prompt: String::from("eclipse$ "),
            scroll_offset: 0,
            max_history: 1000,
        }
    }
    
    /// Escribir texto en el terminal
    pub fn write(&mut self, text: &str) {
        for ch in text.chars() {
            self.write_char(ch);
        }
    }
    
    /// Escribir un carácter
    pub fn write_char(&mut self, ch: char) {
        match ch {
            '\n' => {
                self.new_line();
            }
            '\r' => {
                self.cursor_x = 0;
            }
            '\t' => {
                // Tab - expandir a 4 espacios
                for _ in 0..4 {
                    self.write_char(' ');
                }
            }
            '\x08' => { // Backspace
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                    self.current_line.pop();
                }
            }
            _ => {
                if self.cursor_x < self.width {
                    self.current_line.push(ch);
                    self.cursor_x += 1;
                }
            }
        }
    }
    
    /// Nueva línea
    pub fn new_line(&mut self) {
        // Agregar línea actual al buffer
        if !self.current_line.is_empty() {
            let line = TerminalLine {
                text: self.current_line.clone(),
                color: TerminalColor::Default,
                style: TerminalStyle::Normal,
            };
            self.buffer.add_line(line);
        }
        
        // Limpiar línea actual
        self.current_line.clear();
        
        // Mover cursor
        self.cursor_x = 0;
        self.cursor_y += 1;
        
        // Scroll si es necesario
        if self.cursor_y >= self.height {
            self.scroll_offset += 1;
            self.cursor_y = self.height - 1;
        }
    }
    
    /// Escribir línea con color
    pub fn write_line_color(&mut self, text: &str, color: TerminalColor) {
        let line = TerminalLine {
            text: text.to_string(),
            color,
            style: TerminalStyle::Normal,
        };
        self.buffer.add_line(line);
        self.new_line();
    }
    
    /// Escribir línea con estilo
    pub fn write_line_style(&mut self, text: &str, color: TerminalColor, style: TerminalStyle) {
        let line = TerminalLine {
            text: text.to_string(),
            color,
            style,
        };
        self.buffer.add_line(line);
        self.new_line();
    }
    
    /// Procesar comando
    pub fn process_command(&mut self, command: &str) {
        // Agregar comando al historial
        if !command.is_empty() {
            self.history.push_back(command.to_string());
            if self.history.len() > self.max_history {
                self.history.pop_front();
            }
            self.history_index = self.history.len();
        }
        
        // Mostrar prompt y comando
        let prompt = self.prompt.clone();
        self.write(&prompt);
        self.write(command);
        self.new_line();
        
        // Procesar comando
        let result = self.execute_command(command);
        if !result.is_empty() {
            self.write(&result);
            self.new_line();
        }
    }
    
    /// Ejecutar comando
    fn execute_command(&mut self, command: &str) -> String {
        let parts: Vec<&str> = command.trim().split_whitespace().collect();
        if parts.is_empty() {
            return String::new();
        }
        
        match parts[0] {
            "help" => {
                String::from("Comandos disponibles:\n  help - Mostrar esta ayuda\n  clear - Limpiar pantalla\n  echo <texto> - Mostrar texto\n  history - Mostrar historial\n  exit - Salir del terminal")
            }
            "clear" => {
                self.clear();
                String::new()
            }
            "echo" => {
                if parts.len() > 1 {
                    parts[1..].join(" ")
                } else {
                    String::new()
                }
            }
            "history" => {
                let mut result = String::from("Historial de comandos:\n");
                for (i, cmd) in self.history.iter().enumerate() {
                    result.push_str(&format!("  {}: {}\n", i + 1, cmd));
                }
                result
            }
            "exit" => {
                String::from("Saliendo del terminal...")
            }
            _ => {
                format!("Comando no encontrado: {}", parts[0])
            }
        }
    }
    
    /// Limpiar terminal
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.scroll_offset = 0;
        self.current_line.clear();
    }
    
    /// Obtener línea anterior del historial
    pub fn get_previous_history(&mut self) -> Option<&String> {
        if self.history_index > 0 {
            self.history_index -= 1;
            self.history.get(self.history_index)
        } else {
            None
        }
    }
    
    /// Obtener línea siguiente del historial
    pub fn get_next_history(&mut self) -> Option<&String> {
        if self.history_index < self.history.len() {
            let result = self.history.get(self.history_index);
            self.history_index += 1;
            result
        } else {
            None
        }
    }
    
    /// Establecer prompt
    pub fn set_prompt(&mut self, prompt: &str) {
        self.prompt = prompt.to_string();
    }
    
    /// Obtener cursor
    pub fn get_cursor(&self) -> TerminalCursor {
        TerminalCursor {
            x: self.cursor_x,
            y: self.cursor_y,
            visible: true,
            blink: true,
        }
    }
    
    /// Obtener estadísticas del terminal
    pub fn get_stats(&self) -> TerminalStats {
        TerminalStats {
            width: self.width,
            height: self.height,
            lines_in_buffer: self.buffer.lines.len(),
            history_size: self.history.len(),
            cursor_position: (self.cursor_x, self.cursor_y),
        }
    }
}

impl TerminalBuffer {
    /// Crear nuevo buffer
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: Vec::new(),
            max_lines,
        }
    }
    
    /// Agregar línea
    pub fn add_line(&mut self, line: TerminalLine) {
        self.lines.push(line);
        
        // Mantener solo el número máximo de líneas
        if self.lines.len() > self.max_lines {
            self.lines.remove(0);
        }
    }
    
    /// Limpiar buffer
    pub fn clear(&mut self) {
        self.lines.clear();
    }
    
    /// Obtener líneas visibles
    pub fn get_visible_lines(&self, start: usize, count: usize) -> Vec<&TerminalLine> {
        let end = (start + count).min(self.lines.len());
        if start < self.lines.len() {
            self.lines[start..end].iter().collect()
        } else {
            Vec::new()
        }
    }
}

impl TerminalColor {
    /// Convertir a valor RGB
    pub fn to_rgb(&self) -> (u8, u8, u8) {
        match self {
            TerminalColor::Black => (0, 0, 0),
            TerminalColor::Red => (128, 0, 0),
            TerminalColor::Green => (0, 128, 0),
            TerminalColor::Yellow => (128, 128, 0),
            TerminalColor::Blue => (0, 0, 128),
            TerminalColor::Magenta => (128, 0, 128),
            TerminalColor::Cyan => (0, 128, 128),
            TerminalColor::White => (192, 192, 192),
            TerminalColor::BrightBlack => (64, 64, 64),
            TerminalColor::BrightRed => (255, 0, 0),
            TerminalColor::BrightGreen => (0, 255, 0),
            TerminalColor::BrightYellow => (255, 255, 0),
            TerminalColor::BrightBlue => (0, 0, 255),
            TerminalColor::BrightMagenta => (255, 0, 255),
            TerminalColor::BrightCyan => (0, 255, 255),
            TerminalColor::BrightWhite => (255, 255, 255),
            TerminalColor::Default => (192, 192, 192),
        }
    }
}

/// Estadísticas del terminal
#[derive(Debug, Clone, Copy)]
pub struct TerminalStats {
    pub width: u32,
    pub height: u32,
    pub lines_in_buffer: usize,
    pub history_size: usize,
    pub cursor_position: (u32, u32),
}

impl fmt::Display for TerminalStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Terminal: {}x{}, lines={}, history={}, cursor=({},{})",
               self.width, self.height, self.lines_in_buffer, 
               self.history_size, self.cursor_position.0, self.cursor_position.1)
    }
}

/// Instancia global del terminal
static mut TERMINAL: Option<Terminal> = None;

/// Inicializar el sistema de terminal
pub fn init_terminal_system() -> Result<(), &'static str> {
    unsafe {
        if TERMINAL.is_some() {
            return Ok(());
        }
        
        let terminal = Terminal::new(80, 25);
        TERMINAL = Some(terminal);
    }
    
    Ok(())
}

/// Obtener el terminal
pub fn get_terminal() -> Option<&'static mut Terminal> {
    unsafe { TERMINAL.as_mut() }
}

/// Escribir en el terminal
pub fn terminal_write(text: &str) {
    if let Some(terminal) = get_terminal() {
        terminal.write(text);
    }
}

/// Procesar comando en el terminal
pub fn terminal_process_command(command: &str) {
    if let Some(terminal) = get_terminal() {
        terminal.process_command(command);
    }
}

/// Obtener información del sistema de terminal
pub fn get_terminal_system_info() -> Option<TerminalStats> {
    get_terminal().map(|terminal| terminal.get_stats())
}
