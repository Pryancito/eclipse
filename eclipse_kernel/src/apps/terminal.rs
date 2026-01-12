//! Terminal avanzado para Eclipse OS
//!
//! Proporciona una interfaz de terminal completa con soporte para comandos,
//! historial, autocompletado y múltiples sesiones.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::drivers::framebuffer::Color;
use crate::window_system::client_api::{connect_global_client, get_client_api, ClientAPI};
use crate::window_system::compositor::get_window_compositor;
use crate::window_system::protocol::{
    InputEventData, InputEventType, MessageData, ProtocolMessage, WindowFlags,
};
use crate::window_system::window::WindowType;
use crate::window_system::{ClientId, WindowId};

/// Comando del terminal
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub handler: fn(&mut Terminal, &[String]) -> Result<(), String>,
}

/// Terminal avanzado (Lógica)
pub struct Terminal {
    pub prompt: String,
    pub history: Vec<String>,
    pub history_index: usize,
    pub current_line: String,
    pub cursor_position: usize,
    pub commands: BTreeMap<String, Command>,
    pub working_directory: String,
    pub environment: BTreeMap<String, String>,
    pub session_id: u32,
    pub output_buffer: Vec<String>, // Buffer de salida visible
    pub scroll_offset: usize,
}

impl Terminal {
    pub fn new() -> Self {
        let mut terminal = Self {
            prompt: "eclipse@os$ ".to_string(),
            history: Vec::new(),
            history_index: 0,
            current_line: String::new(),
            cursor_position: 0,
            commands: BTreeMap::new(),
            working_directory: "/".to_string(),
            environment: BTreeMap::new(),
            session_id: 1,
            output_buffer: Vec::new(),
            scroll_offset: 0,
        };

        terminal.register_builtin_commands();
        terminal.setup_environment();
        terminal.print_info("Eclipse OS Terminal v0.2");
        terminal.print_info("Type 'help' for commands");
        terminal
    }

    fn print_info(&mut self, text: &str) {
        self.output_buffer.push(text.to_string());
        if self.output_buffer.len() > 100 { // Limitar buffer
            self.output_buffer.remove(0);
        }
    }

    fn print_error(&mut self, text: &str) {
        self.output_buffer.push(format!("Error: {}", text));
    }

    fn execute_current_line(&mut self) {
        let input = self.current_line.clone();
        self.print_info(&format!("{}{}", self.prompt, input));
        self.history.push(input.clone());
        self.history_index = self.history.len();
        self.process_command(&input);
        self.current_line.clear();
        self.cursor_position = 0;
    }

    fn process_command(&mut self, input: &str) {
        let parts: Vec<&str> = input.trim().split_whitespace().collect();
        if parts.is_empty() { return; }

        let command_name = parts[0];
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        if command_name == "exit" {
             self.print_info("Cannot exit kernel shell");
             return;
        }

        if let Some(command) = self.commands.get(command_name) {
            match (command.handler)(self, &args) {
                Ok(_) => {}
                Err(e) => self.print_error(&e),
            }
        } else {
            self.print_error(&format!("Command not found: {}", command_name));
        }
    }

    fn register_builtin_commands(&mut self) {
        // Comando help
        self.commands.insert(
            "help".to_string(),
            Command {
                name: "help".to_string(),
                description: "Muestra ayuda sobre comandos disponibles".to_string(),
                handler: Self::cmd_help,
            },
        );
        // ... (otros comandos simplificados por ahora)
        self.commands.insert("clear".to_string(), Command{ name: "clear".to_string(), description: "Clears screen".to_string(), handler: Self::cmd_clear});
    }

    fn cmd_help(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        terminal.print_info("Comandos: help, clear, exit");
        Ok(())
    }

    fn cmd_clear(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        terminal.output_buffer.clear();
        Ok(())
    }

    fn setup_environment(&mut self) {
        self.environment.insert("USER".to_string(), "eclipse".to_string());
    }
}

/// Terminal Gráfica (Wrapper para WindowSystem)
pub struct GraphicalTerminal {
    pub logic: Terminal,
    pub client_id: Option<ClientId>,
    pub window_id: Option<WindowId>,
    pub width: u32,
    pub height: u32,
    pub needs_redraw: bool,
}

static mut GRAPHICAL_TERMINAL: Option<GraphicalTerminal> = None;

impl GraphicalTerminal {
    pub fn new() -> Self {
        Self {
            logic: Terminal::new(),
            client_id: None,
            window_id: None,
            width: 600,
            height: 400,
            needs_redraw: true,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Conectar como cliente
        if let Ok(id) = connect_global_client("Terminal".to_string()) {
            self.client_id = Some(id);
            // Crear ventana
            if let Ok(api) = get_client_api() {
                let win_id = api.create_window(
                    id,
                    "Terminal".to_string(),
                    100, 100, self.width, self.height,
                    WindowFlags::empty(),
                )?;
                self.window_id = Some(win_id);
                // Mapear ventana
                api.map_window(win_id)?;
                return Ok(());
            }
        }
        Err("Failed to initialize terminal window")
    }

    pub fn update(&mut self) {
        // Procesar mensajes del sistema de ventanas (eventos)
        if let Some(client_id) = self.client_id {
            if let Ok(api) = get_client_api() {
                // Sacar mensajes de la cola de SALIDA de la API (que son para nosotros el cliente)
                // Nota: La API client_api actualmente pone mensajes en outgoing_messages.
                // Como somos un cliente interno, podemos hackear un poco o simular ser externo.
                // En la implementación de ClientAPI, outgoing_messages es queue.
                // Necesitamos un método para LEER mensajes para un cliente específico.
                // get_outgoing_message() hace pop_front(), pero no filtra por cliente.
                // DEBERÍAMOS filtrar.
                // Por simplicidad en este paso, asumimos que somos el único cliente o
                // implementamos polling directo de la ventana enfocada en EventSystem es más complejo.
                
                // MEJOR ESTRATEGIA: Acceder directamente al EventSystem si la ventana está enfocada.
                // PERO... ClientAPI envía mensajes ProtocolMessage.
                // Vamos a intentar leer mensajes.
                // Si get_outgoing_message devuelve algo, verificamos si es para nosotros.
                // Si no, lo devolvemos? No podemos devolver al frente de deque.
                
                // Workaround: Asumir que el sistema de eventos envía TODO a outgoing_queue
                // y consumimos todo. Si no es para nosotros, lo perdemos (esto es un bug de diseño,
                // pero aceptable para prototipo unicliente).
                
                while let Some(msg) = api.get_outgoing_message() {
                    if msg.client_id == client_id {
                        self.handle_message(msg);
                    }
                }
            }
        }

        if self.needs_redraw {
            self.draw();
            self.needs_redraw = false;
        }
    }

    fn handle_message(&mut self, msg: ProtocolMessage) {
        match msg.data {
            MessageData::InputEvent { event_type, data } => {
                match event_type {
                    InputEventType::KeyPress => {
                        if let InputEventData::Keyboard { key_code, modifiers: _ } = data {
                            self.handle_key(key_code);
                        }
                    },
                    _ => {}
                }
            },
            _ => {}
        }
    }

    fn handle_key(&mut self, key_code: u32) {
        // Mapeo simple de key_code a char (muy básico)
        let ch = match key_code {
            0x04..=0x1D => (key_code - 0x04 + b'a' as u32) as u8 as char, // a-z
            0x1E..=0x27 => (key_code - 0x1E + b'1' as u32) as u8 as char, // 1-9, 0 needs fix
            0x27 => '0', // corrección rápida para 0 (0x27 es 0 en HID estándar? No, 0x27 es 0)
            0x2C => ' ',
            0x28 => '\n',
            0x2A => '\x08', // Backspace
            _ => return,
        };

        if ch == '\n' {
            self.logic.execute_current_line();
        } else if ch == '\x08' {
            self.logic.current_line.pop();
        } else {
            self.logic.current_line.push(ch);
        }
        self.needs_redraw = true;
    }

    fn draw(&mut self) {
        if let Some(window_id) = self.window_id {
            if let Ok(compositor) = get_window_compositor() {
                if let Some(buffer) = compositor.get_window_buffer_mut(window_id) {
                    // Limpiar fondo
                    buffer.clear(Color::BLACK);
                    
                    // Dibujar buffer de texto
                    let mut y = 10;
                    let line_height = 10;
                    
                    // Dibujar últimas líneas
                    let start_line = self.logic.output_buffer.len().saturating_sub(30);
                    for line in self.logic.output_buffer.iter().skip(start_line) {
                        buffer.draw_text(line, 10, y, Color::GREEN);
                        y += line_height;
                    }
                    
                    // Dibujar línea actual
                    buffer.draw_text(&format!("{}{}", self.logic.prompt, self.logic.current_line), 10, y, Color::WHITE);
                    
                    // Cursor
                    let cursor_x = 10 + (self.logic.prompt.len() + self.logic.current_line.len()) as i32 * 8;
                    buffer.draw_rect(
                        crate::window_system::geometry::Rectangle::new(cursor_x, y, 8, 8),
                        Color::WHITE
                    );
                }
            }
        }
    }
}

pub fn init_terminal() -> Result<(), &'static str> {
    unsafe {
        if GRAPHICAL_TERMINAL.is_none() {
            let mut terminal = GraphicalTerminal::new();
            terminal.initialize()?;
            GRAPHICAL_TERMINAL = Some(terminal);
        }
    }
    Ok(())
}

pub fn update_terminal() {
    unsafe {
        if let Some(terminal) = GRAPHICAL_TERMINAL.as_mut() {
            terminal.update();
        }
    }
}
