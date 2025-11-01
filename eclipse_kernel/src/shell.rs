//! Shell Interactivo Básico para Eclipse OS
//!
//! Este módulo implementa un shell interactivo que permite:
//! - Interpretar comandos del usuario
//! - Ejecutar comandos del sistema
//! - Mostrar información del kernel
//! - Navegación básica por comandos
//! - Interfaz de usuario simple

#![no_std]
#![allow(unused_imports)]

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::vec;
use alloc::collections::VecDeque;
use core::fmt;

/// Estado del shell
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShellState {
    /// Shell activo y esperando comandos
    Active,
    /// Procesando un comando
    Processing,
    /// Shell inactivo
    Inactive,
}

/// Resultado de la ejecución de comandos
pub type CommandResult = Result<(), ShellError>;

/// Errores del shell
#[derive(Debug, Clone)]
pub enum ShellError {
    /// Comando desconocido
    UnknownCommand,
    /// Argumentos inválidos
    InvalidArguments,
    /// Error de ejecución
    ExecutionError(String),
    /// Comando no implementado
    NotImplemented,
    /// Error interno
    InternalError(String),
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellError::UnknownCommand => write!(f, "Comando desconocido"),
            ShellError::InvalidArguments => write!(f, "Argumentos inválidos"),
            ShellError::ExecutionError(msg) => write!(f, "Error de ejecución: {}", msg),
            ShellError::NotImplemented => write!(f, "Comando no implementado"),
            ShellError::InternalError(msg) => write!(f, "Error interno: {}", msg),
        }
    }
}

/// Comando del shell
#[derive(Debug, Clone)]
pub struct Command {
    /// Nombre del comando
    pub name: String,
    /// Argumentos del comando
    pub args: Vec<String>,
    /// Línea completa del comando
    pub line: String,
}

impl Command {
    /// Crear un comando desde una línea de texto
    pub fn from_line(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            return None;
        }

        let name = parts[0].to_string();
        let args = parts[1..].iter().map(|s| s.to_string()).collect();

        Some(Command {
            name,
            args,
            line: line.to_string(),
        })
    }
}

/// Handler de comandos
pub trait CommandHandler {
    /// Ejecutar un comando
    fn execute(&self, cmd: &Command, shell: &mut Shell) -> CommandResult;

    /// Obtener nombre del comando
    fn name(&self) -> &str;

    /// Obtener descripción del comando
    fn description(&self) -> &str;
}

/// Comando help
pub struct HelpCommand;

impl CommandHandler for HelpCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.println("Comandos disponibles:");
        shell.println("  help        - Muestra esta ayuda");
        shell.println("  info        - Información del sistema");
        shell.println("  ps          - Lista de procesos");
        shell.println("  mem         - Información de memoria");
        shell.println("  net         - Información de red");
        shell.println("  config      - Información de configuración");
        shell.println("  clear       - Limpia la pantalla");
        shell.println("  exit        - Salir del shell");
        shell.println("  history     - Historial de comandos");
        Ok(())
    }

    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "Muestra la ayuda del shell"
    }
}

/// Comando info
pub struct InfoCommand;

impl CommandHandler for InfoCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.println("Eclipse OS v0.1.0");
        shell.println("Sistema operativo experimental");
        shell.println("");
        shell.println("Subsistemas activos:");
        shell.println("  ✓ Sistema de logging estructurado");
        shell.println("  ✓ Sistema de recuperación de errores");
        shell.println("  ✓ Sistema de procesos básico");
        shell.println("  ✓ Sistema de módulos del kernel");
        shell.println("  ✓ Sistema de dispositivos virtuales");
        shell.println("  ✓ Sistema de configuración del kernel");
        shell.println("  ✓ Sistema de red TCP/IP completo");
        Ok(())
    }

    fn name(&self) -> &str {
        "info"
    }

    fn description(&self) -> &str {
        "Muestra información del sistema"
    }
}

/// Comando ps (procesos)
pub struct PsCommand;

impl CommandHandler for PsCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.println("Lista de procesos:");
        shell.println("  PID    Estado      Nombre");

        // En un sistema real, aquí obtendríamos la lista de procesos
        // Por ahora, mostramos procesos simulados
        shell.println("    1    Ejecutando  kernel_main");
        shell.println("    2    Ejecutando  shell");
        shell.println("    3    Dormido     demo_process_1");
        shell.println("    4    Dormido     demo_process_2");

        Ok(())
    }

    fn name(&self) -> &str {
        "ps"
    }

    fn description(&self) -> &str {
        "Muestra la lista de procesos"
    }
}

/// Comando mem (memoria)
pub struct MemCommand;

impl CommandHandler for MemCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.println("Información de memoria:");
        shell.println("  Total:     4 GB");
        shell.println("  Usada:     128 MB");
        shell.println("  Libre:     3.87 GB");
        shell.println("  Kernel:    64 MB");
        shell.println("  Buffers:   8 MB");
        Ok(())
    }

    fn name(&self) -> &str {
        "mem"
    }

    fn description(&self) -> &str {
        "Muestra información de memoria"
    }
}

/// Comando net (red)
pub struct NetCommand;

impl CommandHandler for NetCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.println("Información de red:");
        let info = crate::network::get_network_system_info();
        shell.println(&alloc::format!("  {}", info));
        shell.println("  Interfaces: 1");
        shell.println("  Conexiones: 0");
        shell.println("  Estado: Activo");
        Ok(())
    }

    fn name(&self) -> &str {
        "net"
    }

    fn description(&self) -> &str {
        "Muestra información de red"
    }
}

/// Comando config
pub struct ConfigCommand;

impl CommandHandler for ConfigCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.println("Configuración del sistema:");

        // Mostrar algunas configuraciones importantes
        if let Some(hostname) = crate::config::get_hostname() {
            shell.println(&alloc::format!("  Hostname: {}", hostname));
        }

        if let Some(heap_size) = crate::config::get_heap_size() {
            shell.println(&alloc::format!("  Heap size: {} KB", heap_size / 1024));
        }

        shell.println(&alloc::format!("  Debug mode: {}", crate::config::is_debug_mode()));
        shell.println(&alloc::format!("  Networking: {}", crate::config::is_networking_enabled()));

        Ok(())
    }

    fn name(&self) -> &str {
        "config"
    }

    fn description(&self) -> &str {
        "Muestra configuración del sistema"
    }
}

/// Comando clear
pub struct ClearCommand;

impl CommandHandler for ClearCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.clear_screen();
        Ok(())
    }

    fn name(&self) -> &str {
        "clear"
    }

    fn description(&self) -> &str {
        "Limpia la pantalla"
    }
}

/// Comando history
pub struct HistoryCommand;

impl CommandHandler for HistoryCommand {
    fn execute(&self, _cmd: &Command, shell: &mut Shell) -> CommandResult {
        shell.println("Historial de comandos:");
        let history: Vec<String> = shell.history.iter().cloned().collect();
        for (i, cmd) in history.iter().enumerate() {
            shell.println(&alloc::format!("  {}: {}", i + 1, cmd));
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "history"
    }

    fn description(&self) -> &str {
        "Muestra el historial de comandos"
    }
}

/// Shell interactivo
pub struct Shell {
    /// Estado del shell
    state: ShellState,
    /// Historial de comandos
    history: VecDeque<String>,
    /// Comando actual
    current_command: String,
    /// Posición del cursor en el comando actual
    cursor_pos: usize,
    /// Handlers de comandos registrados
    command_handlers: Vec<Box<dyn CommandHandler>>,
    /// Capacidad máxima del historial
    max_history: usize,
}

impl Shell {
    /// Crear un nuevo shell
    pub fn new() -> Self {
        let mut shell = Shell {
            state: ShellState::Inactive,
            history: VecDeque::new(),
            current_command: String::new(),
            cursor_pos: 0,
            command_handlers: Vec::new(),
            max_history: 100,
        };

        // Registrar comandos básicos
        shell.register_command(Box::new(HelpCommand));
        shell.register_command(Box::new(InfoCommand));
        shell.register_command(Box::new(PsCommand));
        shell.register_command(Box::new(MemCommand));
        shell.register_command(Box::new(NetCommand));
        shell.register_command(Box::new(ConfigCommand));
        shell.register_command(Box::new(ClearCommand));
        shell.register_command(Box::new(HistoryCommand));

        shell
    }

    /// Inicializar el shell
    pub fn init(&mut self) {
        self.state = ShellState::Active;
        self.println("Eclipse OS Shell v0.1.0");
        self.println("Escribe 'help' para ver los comandos disponibles");
        self.show_prompt();
    }

    /// Registrar un nuevo comando
    pub fn register_command(&mut self, handler: Box<dyn CommandHandler>) {
        self.command_handlers.push(handler);
    }

    /// Procesar entrada del usuario
    pub fn process_input(&mut self, input: &str) {
        if self.state != ShellState::Active {
            return;
        }

        for ch in input.chars() {
            match ch {
                '\n' | '\r' => {
                    self.execute_command();
                    self.show_prompt();
                }
                '\x08' | '\x7f' => { // Backspace
                    if self.cursor_pos > 0 {
                        self.current_command.remove(self.cursor_pos - 1);
                        self.cursor_pos -= 1;
                    }
                }
                '\x1b' => {
                    // Secuencias de escape (flechas, etc.)
                    // Por simplicidad, ignoramos por ahora
                }
                ch if ch.is_ascii() && !ch.is_control() => {
                    self.current_command.insert(self.cursor_pos, ch);
                    self.cursor_pos += 1;
                }
                _ => {} // Ignorar otros caracteres
            }
        }
    }

    /// Ejecutar el comando actual
    fn execute_command(&mut self) {
        let command_line = self.current_command.trim().to_string();

        if command_line.is_empty() {
            return;
        }

        // Agregar al historial
        self.add_to_history(command_line.clone());

        // Resetear comando actual
        self.current_command.clear();
        self.cursor_pos = 0;

        // Procesar comando
        self.state = ShellState::Processing;

        if let Some(cmd) = Command::from_line(&command_line) {
            if cmd.name == "exit" {
                self.println("Saliendo del shell...");
                self.state = ShellState::Inactive;
                return;
            }

            // Buscar handler apropiado
            let handler_index = self.command_handlers.iter().position(|h| h.name() == cmd.name);
            let result = if let Some(index) = handler_index {
                // Necesitamos una referencia mutable al handler
                // Para evitar problemas de borrowing, usamos un enfoque diferente
                let handler_name = self.command_handlers[index].name();
                if handler_name == cmd.name {
                    // Creamos una copia temporal del handler para ejecutar
                    // En un sistema real, esto se optimizaría
                    match index {
                        0 => HelpCommand.execute(&cmd, self),
                        1 => InfoCommand.execute(&cmd, self),
                        2 => PsCommand.execute(&cmd, self),
                        3 => MemCommand.execute(&cmd, self),
                        4 => NetCommand.execute(&cmd, self),
                        5 => ConfigCommand.execute(&cmd, self),
                        6 => ClearCommand.execute(&cmd, self),
                        7 => HistoryCommand.execute(&cmd, self),
                        _ => Err(ShellError::UnknownCommand),
                    }
                } else {
                    Err(ShellError::UnknownCommand)
                }
            } else {
                Err(ShellError::UnknownCommand)
            };

            if let Err(e) = result {
                self.println(&alloc::format!("Error: {}", e));
            }
        }

        self.state = ShellState::Active;
    }

    /// Mostrar prompt del shell
    fn show_prompt(&mut self) {
        self.print("eclipse-os> ");
    }

    /// Imprimir texto (con nueva línea)
    pub fn println(&mut self, text: &str) {
        // En un sistema real, aquí se enviaría a la interfaz gráfica o serial
        // Logging disabled
    }

    /// Imprimir texto (sin nueva línea)
    pub fn print(&mut self, text: &str) {
        // En un sistema real, aquí se enviaría a la interfaz gráfica o serial
        // Por simplicidad, usamos la misma función que println
        // Logging disabled
    }

    /// Limpiar pantalla
    pub fn clear_screen(&mut self) {
        // En un sistema real, aquí se limpiaría la pantalla
        self.println("Pantalla limpiada");
    }

    /// Agregar comando al historial
    fn add_to_history(&mut self, command: String) {
        if self.history.len() >= self.max_history {
            self.history.pop_front();
        }
        self.history.push_back(command);
    }

    /// Obtener estado del shell
    pub fn get_state(&self) -> ShellState {
        self.state
    }

    /// Verificar si el shell está activo
    pub fn is_active(&self) -> bool {
        self.state == ShellState::Active
    }

    /// Obtener línea de comando actual
    pub fn get_current_command(&self) -> &str {
        &self.current_command
    }

    /// Obtener historial de comandos
    pub fn get_history(&self) -> &VecDeque<String> {
        &self.history
    }
}

/// Instancia global del shell
static mut GLOBAL_SHELL: Option<Shell> = None;

/// Inicializar el shell global
pub fn init_shell() -> Result<(), ShellError> {
    unsafe {
        GLOBAL_SHELL = Some(Shell::new());
    }

    // Logging disabled
    Ok(())
}

/// Obtener referencia al shell global
pub fn get_shell() -> Option<&'static mut Shell> {
    unsafe {
        GLOBAL_SHELL.as_mut()
    }
}

/// Procesar entrada en el shell global
pub fn process_shell_input(input: &str) {
    if let Some(shell) = get_shell() {
        shell.process_input(input);
    }
}

// logging removido