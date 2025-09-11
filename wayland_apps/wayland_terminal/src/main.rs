//! Aplicación de terminal Wayland para Eclipse OS
//! 
//! Esta aplicación implementa un terminal básico que se conecta al compositor Wayland
//! y proporciona una interfaz de línea de comandos simple.

#![no_std]
#![no_main]

extern crate alloc;

use core::panic::PanicInfo;
use core::fmt::Write;
use core::str::FromStr;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use linked_list_allocator::LockedHeap;

/// Tamaño del heap (1MB)
const HEAP_SIZE: usize = 1024 * 1024;

/// Heap global
#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

/// Inicializa el allocator
fn init_allocator() {
    unsafe {
        static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
        HEAP.lock().init(HEAP_MEM.as_mut_ptr(), HEAP_SIZE);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { serial_write_str("KERNEL: PANIC\r\n"); }
    // TEMPORALMENTE DESHABILITADO: hlt causa opcode inválido
    loop {
        // Simular espera sin hlt para evitar opcode inválido
        unsafe { core::arch::asm!("nop"); }
    }
}



/// Estructura para manejar la aplicación de terminal Wayland
pub struct WaylandTerminal {
    /// ID de la superficie Wayland
    surface_id: u32,
    /// Buffer de texto del terminal
    text_buffer: String,
    /// Línea de comando actual
    current_line: String,
    /// Historial de comandos
    command_history: Vec<String>,
    /// Posición del cursor
    cursor_position: usize,
    /// Ancho de la ventana
    width: u32,
    /// Alto de la ventana
    height: u32,
}

impl WaylandTerminal {
    /// Crea una nueva instancia del terminal Wayland
    pub fn new() -> Self {
        Self {
            surface_id: 0,
            text_buffer: String::new(),
            current_line: String::new(),
            command_history: Vec::new(),
            cursor_position: 0,
            width: 80,
            height: 24,
        }
    }

    /// Inicializa el terminal y se conecta al compositor Wayland
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        // Simular conexión al compositor Wayland
        self.surface_id = 1;
        
        // Mostrar mensaje de bienvenida
        self.write_line("=== Eclipse OS Terminal ===");
        self.write_line("Conectado al compositor Wayland");
        self.write_line("Escriba 'help' para ver comandos disponibles");
        self.write_line("");
        
        Ok(())
    }

    /// Escribe una línea en el buffer del terminal
    pub fn write_line(&mut self, line: &str) {
        self.text_buffer.push_str(line);
        self.text_buffer.push('\n');
    }

    /// Procesa un comando ingresado por el usuario
    pub fn process_command(&mut self, command: &str) {
        self.write_line(&format!("$ {}", command));
        
        match command {
            "help" => {
                self.write_line("Comandos disponibles:");
                self.write_line("  help     - Muestra esta ayuda");
                self.write_line("  clear    - Limpia la pantalla");
                self.write_line("  echo     - Repite el texto");
                self.write_line("  date     - Muestra la fecha y hora");
                self.write_line("  exit     - Sale del terminal");
            },
            "clear" => {
                self.text_buffer.clear();
                self.write_line("=== Eclipse OS Terminal ===");
            },
            "echo" => {
                self.write_line("Echo: comando no implementado completamente");
            },
            "date" => {
                self.write_line("Fecha: 2024-01-01 12:00:00 (simulado)");
            },
            "exit" => {
                self.write_line("Cerrando terminal...");
                // En una implementación real, aquí se cerraría la aplicación
            },
            _ => {
                self.write_line(&format!("Comando no encontrado: {}", command));
                self.write_line("Escriba 'help' para ver comandos disponibles");
            }
        }
        
        // Agregar comando al historial
        self.command_history.push(command.to_string());
    }

    /// Renderiza el contenido del terminal en la superficie Wayland
    pub fn render(&mut self) -> Result<(), &'static str> {
        // Simular renderizado en la superficie Wayland
        // En una implementación real, aquí se actualizaría el buffer de la superficie
        
        // Limitar el buffer a las líneas que caben en la pantalla
        let mut lines: Vec<&str> = Vec::new();
        for line in self.text_buffer.split('\n') {
            lines.push(line);
        }
        
        let start_line = if lines.len() > self.height as usize {
            lines.len() - self.height as usize
        } else {
            0
        };
        
        // Renderizar líneas visibles
        for i in start_line..lines.len() {
            if let Some(line) = lines.get(i) {
                // En una implementación real, aquí se dibujaría la línea en la superficie
                // Por ahora solo simulamos el renderizado
            }
        }
        
        Ok(())
    }

    /// Maneja eventos de entrada (teclado, ratón)
    pub fn handle_input(&mut self, input_type: InputType, data: &[u8]) {
        match input_type {
            InputType::KeyPress => {
                if let Some(key) = data.get(0) {
                    match *key {
                        b'\n' | b'\r' => {
                            // Enter - procesar comando
                            if !self.current_line.is_empty() {
                                let command = self.current_line.clone();
                                self.process_command(&command);
                                self.current_line.clear();
                                self.cursor_position = 0;
                            }
                        },
                        b'\x08' | b'\x7f' => {
                            // Backspace
                            if self.cursor_position > 0 {
                                self.cursor_position -= 1;
                                self.current_line.pop();
                            }
                        },
                        _ => {
                            // Carácter normal
                            if let Ok(ch) = core::str::from_utf8(&[*key]) {
                                self.current_line.push_str(ch);
                                self.cursor_position += 1;
                            }
                        }
                    }
                }
            },
            InputType::MouseClick => {
                // Manejar clics del ratón
                self.write_line("Click del ratón detectado");
            },
            InputType::Resize => {
                // Manejar redimensionamiento de ventana
                if data.len() >= 8 {
                    let width = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    let height = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                    self.width = width;
                    self.height = height;
                    self.write_line(&format!("Ventana redimensionada: {}x{}", width, height));
                }
            }
        }
    }

    /// Obtiene el contenido actual del buffer de texto
    pub fn get_text_buffer(&self) -> &str {
        &self.text_buffer
    }

    /// Obtiene la línea de comando actual
    pub fn get_current_line(&self) -> &str {
        &self.current_line
    }

    /// Obtiene la posición del cursor
    pub fn get_cursor_position(&self) -> usize {
        self.cursor_position
    }
}

/// Tipos de entrada que puede manejar el terminal
#[derive(Debug, Clone, Copy)]
pub enum InputType {
    KeyPress,
    MouseClick,
    Resize,
}

/// Función principal del terminal Wayland
#[no_mangle]
pub extern "C" fn wayland_terminal_main() {
    let mut terminal = WaylandTerminal::new();
    
    // Inicializar el terminal
    if let Err(e) = terminal.initialize() {
        // En caso de error, mostrar mensaje y salir
        return;
    }
    
    // Loop principal del terminal
    loop {
        // Simular entrada del usuario
        // En una implementación real, aquí se recibirían eventos del compositor Wayland
        
        // Renderizar el terminal
        if let Err(_) = terminal.render() {
            break;
        }
        
        // Simular procesamiento de eventos
        // En una implementación real, aquí se procesarían eventos del compositor
    }
}

/// Función principal de la aplicación
#[no_mangle]
pub extern "C" fn main() -> ! {
    // Inicializar el allocator
    init_allocator();
    
    let mut terminal = WaylandTerminal::new();
    
    // Inicializar el terminal
    if let Err(e) = terminal.initialize() {
        // En una implementación real, aquí se manejaría el error
        loop {}
    }
    
    // Simular entrada de usuario
    terminal.handle_input(InputType::KeyPress, b"h");
    terminal.handle_input(InputType::KeyPress, b"e");
    terminal.handle_input(InputType::KeyPress, b"l");
    terminal.handle_input(InputType::KeyPress, b"p");
    terminal.handle_input(InputType::KeyPress, b"\n");
    
    // Renderizar el terminal
    if let Err(e) = terminal.render() {
        // En una implementación real, aquí se manejaría el error
        loop {}
    }
    
    // Bucle infinito
    loop {}
}