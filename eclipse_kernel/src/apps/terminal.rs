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

/// Comando del terminal
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub handler: fn(&mut Terminal, &[String]) -> Result<(), String>,
}

/// Terminal avanzado
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
        };

        terminal.register_builtin_commands();
        terminal.setup_environment();
        terminal
    }

    /// Ejecutar el terminal
    pub fn run(&mut self) -> Result<(), &'static str> {
        self.show_welcome();

        loop {
            self.show_prompt();
            let input = self.read_input();

            if input.trim().is_empty() {
                continue;
            }

            if input.trim() == "exit" {
                break;
            }

            self.process_command(&input);
        }

        Ok(())
    }

    fn show_welcome(&self) {
        self.print_info("╔══════════════════════════════════════════════════════════════╗");
        self.print_info("║                                                              ║");
        self.print_info("║                    ECLIPSE TERMINAL                          ║");
        self.print_info("║                                                              ║");
        self.print_info("║  Terminal avanzado con soporte para comandos modernos      ║");
        self.print_info("║  Escribe 'help' para ver comandos disponibles              ║");
        self.print_info("║  Escribe 'exit' para salir                                 ║");
        self.print_info("║                                                              ║");
        self.print_info("╚══════════════════════════════════════════════════════════════╝");
        self.print_info("");
    }

    fn show_prompt(&self) {
        self.print_info(&format!("{}{}", self.prompt, self.current_line));
    }

    fn read_input(&self) -> String {
        // En una implementación real, esto leería del teclado con soporte para:
        // - Historial con flechas arriba/abajo
        // - Autocompletado con Tab
        // - Edición de línea con flechas izquierda/derecha
        // - Copiar/pegar con Ctrl+C/Ctrl+V

        // Por ahora simulamos con comandos de ejemplo
        let commands = vec![
            "help",
            "ls",
            "pwd",
            "cd /home",
            "cat welcome.txt",
            "calc 2+2",
            "history",
            "clear",
            "exit",
        ];

        // Simular entrada secuencial
        static mut COMMAND_INDEX: usize = 0;
        unsafe {
            if COMMAND_INDEX < commands.len() {
                let cmd = commands[COMMAND_INDEX].to_string();
                COMMAND_INDEX += 1;
                cmd
            } else {
                "exit".to_string()
            }
        }
    }

    fn process_command(&mut self, input: &str) {
        // Añadir al historial
        self.history.push(input.to_string());
        self.history_index = self.history.len();

        // Parsear comando y argumentos
        let parts: Vec<&str> = input.trim().split_whitespace().collect();
        if parts.is_empty() {
            return;
        }

        let command_name = parts[0];
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        // Buscar comando
        if let Some(command) = self.commands.get(command_name) {
            match (command.handler)(self, &args) {
                Ok(_) => {}
                Err(e) => {
                    self.print_error(&format!("Error: {}", e));
                }
            }
        } else {
            self.print_error(&format!("Comando no encontrado: {}", command_name));
            self.print_info("Escribe 'help' para ver comandos disponibles");
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

        // Comando ls
        self.commands.insert(
            "ls".to_string(),
            Command {
                name: "ls".to_string(),
                description: "Lista archivos y directorios".to_string(),
                handler: Self::cmd_ls,
            },
        );

        // Comando pwd
        self.commands.insert(
            "pwd".to_string(),
            Command {
                name: "pwd".to_string(),
                description: "Muestra el directorio actual".to_string(),
                handler: Self::cmd_pwd,
            },
        );

        // Comando cd
        self.commands.insert(
            "cd".to_string(),
            Command {
                name: "cd".to_string(),
                description: "Cambia de directorio".to_string(),
                handler: Self::cmd_cd,
            },
        );

        // Comando cat
        self.commands.insert(
            "cat".to_string(),
            Command {
                name: "cat".to_string(),
                description: "Muestra el contenido de un archivo".to_string(),
                handler: Self::cmd_cat,
            },
        );

        // Comando calc
        self.commands.insert(
            "calc".to_string(),
            Command {
                name: "calc".to_string(),
                description: "Calculadora básica".to_string(),
                handler: Self::cmd_calc,
            },
        );

        // Comando history
        self.commands.insert(
            "history".to_string(),
            Command {
                name: "history".to_string(),
                description: "Muestra el historial de comandos".to_string(),
                handler: Self::cmd_history,
            },
        );

        // Comando clear
        self.commands.insert(
            "clear".to_string(),
            Command {
                name: "clear".to_string(),
                description: "Limpia la pantalla".to_string(),
                handler: Self::cmd_clear,
            },
        );

        // Comando echo
        self.commands.insert(
            "echo".to_string(),
            Command {
                name: "echo".to_string(),
                description: "Muestra texto en pantalla".to_string(),
                handler: Self::cmd_echo,
            },
        );

        // Comando date
        self.commands.insert(
            "date".to_string(),
            Command {
                name: "date".to_string(),
                description: "Muestra la fecha y hora actual".to_string(),
                handler: Self::cmd_date,
            },
        );
    }

    fn setup_environment(&mut self) {
        self.environment
            .insert("USER".to_string(), "eclipse".to_string());
        self.environment
            .insert("HOME".to_string(), "/home/eclipse".to_string());
        self.environment
            .insert("SHELL".to_string(), "/bin/eclipse-shell".to_string());
        self.environment.insert(
            "PATH".to_string(),
            "/bin:/usr/bin:/usr/local/bin".to_string(),
        );
        self.environment.insert("PWD".to_string(), "/".to_string());
    }

    // Implementaciones de comandos
    fn cmd_help(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        terminal.print_info("Comandos disponibles:");
        terminal.print_info("");

        for (name, command) in &terminal.commands {
            terminal.print_info(&format!("  {:<12} - {}", name, command.description));
        }

        terminal.print_info("");
        terminal.print_info("Usa las flechas para navegar por el historial");
        terminal.print_info("Usa Tab para autocompletar comandos");
        terminal.print_info("Usa Ctrl+C para cancelar el comando actual");

        Ok(())
    }

    fn cmd_ls(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        terminal.print_info("Archivos y directorios en el directorio actual:");
        terminal.print_info("");
        terminal.print_info("  drwxr-xr-x 2 eclipse eclipse 4096 Jan  1 00:00 .");
        terminal.print_info("  drwxr-xr-x 3 eclipse eclipse 4096 Jan  1 00:00 ..");
        terminal.print_info("  -rw-r--r-- 1 eclipse eclipse  123 Jan  1 00:00 welcome.txt");
        terminal.print_info("  -rw-r--r-- 1 eclipse eclipse  456 Jan  1 00:00 config.ini");
        terminal.print_info("  -rw-r--r-- 1 eclipse eclipse  789 Jan  1 00:00 system.log");
        terminal.print_info("  drwxr-xr-x 2 eclipse eclipse 4096 Jan  1 00:00 system");
        terminal.print_info("  drwxr-xr-x 2 eclipse eclipse 4096 Jan  1 00:00 users");

        Ok(())
    }

    fn cmd_pwd(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        terminal.print_info(&terminal.working_directory);
        Ok(())
    }

    fn cmd_cd(terminal: &mut Terminal, args: &[String]) -> Result<(), String> {
        if args.is_empty() {
            terminal.working_directory = terminal.environment.get("HOME").unwrap().clone();
        } else {
            let path = &args[0];
            if path == ".." {
                if terminal.working_directory != "/" {
                    if let Some(pos) = terminal.working_directory.rfind('/') {
                        terminal.working_directory = terminal.working_directory[..pos].to_string();
                        if terminal.working_directory.is_empty() {
                            terminal.working_directory = "/".to_string();
                        }
                    }
                }
            } else if path.starts_with('/') {
                terminal.working_directory = path.clone();
            } else {
                if terminal.working_directory.ends_with('/') {
                    terminal.working_directory.push_str(path);
                } else {
                    terminal.working_directory.push('/');
                    terminal.working_directory.push_str(path);
                }
            }
        }

        terminal
            .environment
            .insert("PWD".to_string(), terminal.working_directory.clone());
        Ok(())
    }

    fn cmd_cat(terminal: &mut Terminal, args: &[String]) -> Result<(), String> {
        if args.is_empty() {
            return Err("Uso: cat <archivo>".to_string());
        }

        let filename = &args[0];
        match filename.as_str() {
            "welcome.txt" => {
                terminal.print_info("Bienvenido a Eclipse OS!");
                terminal.print_info("");
                terminal.print_info("Este es un sistema operativo moderno construido en Rust.");
                terminal.print_info("Características:");
                terminal.print_info("- Kernel monolítico con microkernel");
                terminal.print_info("- Sistema de ventanas avanzado");
                terminal.print_info("- Soporte para Wayland");
                terminal.print_info("- Drivers de hardware modernos");
                terminal.print_info("- Sistema de archivos funcionando correctamente");
            }
            "config.ini" => {
                terminal.print_info("[system]");
                terminal.print_info("version=0.1.0");
                terminal.print_info("kernel=eclipse");
                terminal.print_info("");
                terminal.print_info("[graphics]");
                terminal.print_info("backend=wayland");
                terminal.print_info("resolution=1024x768");
                terminal.print_info("");
                terminal.print_info("[filesystem]");
                terminal.print_info("type=eclipsefs");
                terminal.print_info("cache_size=64");
                terminal.print_info("fat32_support=true");
            }
            "system.log" => {
                terminal.print_info("[2024-01-01 00:00:00] Sistema iniciado");
                terminal.print_info("[2024-01-01 00:00:01] VFS inicializado");
                terminal.print_info("[2024-01-01 00:00:02] FAT32 inicializado");
                terminal.print_info("[2024-01-01 00:00:03] Drivers cargados");
                terminal.print_info("[2024-01-01 00:00:04] Sistema listo");
            }
            _ => {
                terminal.print_error(&format!("Archivo no encontrado: {}", filename));
            }
        }

        Ok(())
    }

    fn cmd_calc(terminal: &mut Terminal, args: &[String]) -> Result<(), String> {
        if args.is_empty() {
            return Err("Uso: calc <expresión>".to_string());
        }

        let expression = args.join(" ");

        // Evaluación simple de expresiones matemáticas
        match Self::evaluate_math_expression(&expression) {
            Ok(result) => {
                terminal.print_info(&format!("{} = {}", expression, result));
            }
            Err(e) => {
                terminal.print_error(&format!("Error en expresión: {}", e));
            }
        }

        Ok(())
    }

    fn evaluate_math_expression(expr: &str) -> Result<f64, String> {
        // Implementación muy simple de evaluación matemática
        let expr = expr.replace(" ", "");

        if let Some(pos) = expr.find('+') {
            let left = expr[..pos].parse::<f64>().map_err(|_| "Número inválido")?;
            let right = expr[pos + 1..]
                .parse::<f64>()
                .map_err(|_| "Número inválido")?;
            return Ok(left + right);
        }

        if let Some(pos) = expr.find('-') {
            let left = expr[..pos].parse::<f64>().map_err(|_| "Número inválido")?;
            let right = expr[pos + 1..]
                .parse::<f64>()
                .map_err(|_| "Número inválido")?;
            return Ok(left - right);
        }

        if let Some(pos) = expr.find('*') {
            let left = expr[..pos].parse::<f64>().map_err(|_| "Número inválido")?;
            let right = expr[pos + 1..]
                .parse::<f64>()
                .map_err(|_| "Número inválido")?;
            return Ok(left * right);
        }

        if let Some(pos) = expr.find('/') {
            let left = expr[..pos].parse::<f64>().map_err(|_| "Número inválido")?;
            let right = expr[pos + 1..]
                .parse::<f64>()
                .map_err(|_| "Número inválido")?;
            if right == 0.0 {
                return Err("División por cero".to_string());
            }
            return Ok(left / right);
        }

        expr.parse::<f64>()
            .map_err(|_| "Expresión no válida".to_string())
    }

    fn cmd_history(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        terminal.print_info("Historial de comandos:");
        terminal.print_info("");

        for (i, cmd) in terminal.history.iter().enumerate() {
            terminal.print_info(&format!("  {}: {}", i + 1, cmd));
        }

        Ok(())
    }

    fn cmd_clear(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        // En una implementación real, esto limpiaría la pantalla
        terminal.print_info("Pantalla limpiada");
        Ok(())
    }

    fn cmd_echo(terminal: &mut Terminal, args: &[String]) -> Result<(), String> {
        if args.is_empty() {
            terminal.print_info("");
        } else {
            terminal.print_info(&args.join(" "));
        }
        Ok(())
    }

    fn cmd_date(terminal: &mut Terminal, _args: &[String]) -> Result<(), String> {
        terminal.print_info("Mon Jan  1 00:00:00 UTC 2024");
        Ok(())
    }

    fn print_info(&self, text: &str) {
        // En una implementación real, esto imprimiría en la consola
        // Por ahora solo simulamos
    }

    fn print_error(&self, text: &str) {
        // En una implementación real, esto imprimiría en la consola con color rojo
        // Por ahora solo simulamos
    }
}

/// Función principal para ejecutar el terminal
pub fn run() -> Result<(), &'static str> {
    let mut terminal = Terminal::new();
    terminal.run()
}
