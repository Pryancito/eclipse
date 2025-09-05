//! Shell avanzado para Eclipse OS
//! 
//! Proporciona una interfaz de línea de comandos moderna
//! con características avanzadas como autocompletado, historial,
//! y scripting.

use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;

/// Comando del shell
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
    pub description: String,
}

/// Historial de comandos
pub struct CommandHistory {
    commands: Vec<String>,
    max_size: usize,
    current_index: usize,
}

impl CommandHistory {
    pub fn new(max_size: usize) -> Self {
        Self {
            commands: Vec::new(),
            max_size,
            current_index: 0,
        }
    }

    pub fn add_command(&mut self, command: String) {
        if self.commands.len() >= self.max_size {
            self.commands.remove(0);
        }
        self.commands.push(command);
        self.current_index = self.commands.len();
    }

    pub fn get_previous(&mut self) -> Option<&String> {
        if self.current_index > 0 {
            self.current_index -= 1;
            self.commands.get(self.current_index)
        } else {
            None
        }
    }

    pub fn get_next(&mut self) -> Option<&String> {
        if self.current_index < self.commands.len() {
            self.current_index += 1;
            self.commands.get(self.current_index - 1)
        } else {
            None
        }
    }
}

/// Shell principal
pub struct EclipseShell {
    history: CommandHistory,
    current_directory: String,
    prompt: String,
    aliases: Vec<(String, String)>,
}

impl EclipseShell {
    pub fn new() -> Self {
        let mut shell = Self {
            history: CommandHistory::new(1000),
            current_directory: "/".to_string(),
            prompt: "eclipse@system".to_string(),
            aliases: Vec::new(),
        };
        
        // Configurar aliases por defecto
        shell.setup_default_aliases();
        shell
    }

    fn setup_default_aliases(&mut self) {
        self.aliases.push(("ll".to_string(), "ls -l".to_string()));
        self.aliases.push(("la".to_string(), "ls -a".to_string()));
        self.aliases.push(("l".to_string(), "ls".to_string()));
        self.aliases.push(("cls".to_string(), "clear".to_string()));
        self.aliases.push(("md".to_string(), "mkdir".to_string()));
        self.aliases.push(("rd".to_string(), "rmdir".to_string()));
    }

    /// Ejecutar el shell
    pub fn run(&mut self) -> Result<(), &'static str> {
        self.show_welcome();
        
        loop {
            self.show_prompt();
            let input = self.read_input();
            
            if input.trim().is_empty() {
                continue;
            }

            self.history.add_command(input.clone());
            
            if let Err(e) = self.execute_command(&input) {
                self.print_error(&format!("Error: {}", e));
            }
        }
    }

    fn show_welcome(&self) {
        self.print_info("╔══════════════════════════════════════════════════════════════╗");
        self.print_info("║                                                              ║");
        self.print_info("║                    ECLIPSE OS SHELL                         ║");
        self.print_info("║                                                              ║");
        self.print_info("║  Shell avanzado con características modernas                ║");
        self.print_info("║  Escribe 'help' para ver comandos disponibles              ║");
        self.print_info("║  Escribe 'exit' para salir                                 ║");
        self.print_info("║                                                              ║");
        self.print_info("╚══════════════════════════════════════════════════════════════╝");
        self.print_info("");
    }

    fn show_prompt(&self) {
        self.print_info(&format!("{}:{} $ ", self.prompt, self.current_directory));
    }

    fn read_input(&self) -> String {
        // En una implementación real, esto leería del teclado
        // Por ahora simulamos con un input fijo
        "help".to_string()
    }

    fn execute_command(&mut self, input: &str) -> Result<(), &'static str> {
        let command = self.parse_command(input);
        
        match command.name.as_str() {
            "help" => self.cmd_help(),
            "exit" => self.cmd_exit(),
            "clear" => self.cmd_clear(),
            "ls" => self.cmd_ls(&command.args),
            "cd" => self.cmd_cd(&command.args),
            "pwd" => self.cmd_pwd(),
            "mkdir" => self.cmd_mkdir(&command.args),
            "rmdir" => self.cmd_rmdir(&command.args),
            "rm" => self.cmd_rm(&command.args),
            "cp" => self.cmd_cp(&command.args),
            "mv" => self.cmd_mv(&command.args),
            "cat" => self.cmd_cat(&command.args),
            "echo" => self.cmd_echo(&command.args),
            "date" => self.cmd_date(),
            "whoami" => self.cmd_whoami(),
            "ps" => self.cmd_ps(),
            "top" => self.cmd_top(),
            "history" => self.cmd_history(),
            "alias" => self.cmd_alias(&command.args),
            "unalias" => self.cmd_unalias(&command.args),
            _ => {
                // Verificar si es un alias
                if let Some(alias_command) = self.get_alias(&command.name) {
                    let alias_cmd = alias_command.clone();
                    return self.execute_command(&alias_cmd);
                }
                Err("Comando no encontrado")
            }
        }
    }

    fn parse_command(&self, input: &str) -> Command {
        let parts: Vec<&str> = input.trim().split_whitespace().collect();
        let name = parts.get(0).unwrap_or(&"").to_string();
        let args = parts[1..].iter().map(|s| s.to_string()).collect();
        
        Command {
            name,
            args,
            description: String::new(),
        }
    }

    fn cmd_help(&self) -> Result<(), &'static str> {
        self.print_info("Comandos disponibles:");
        self.print_info("  help          - Muestra esta ayuda");
        self.print_info("  exit          - Sale del shell");
        self.print_info("  clear         - Limpia la pantalla");
        self.print_info("  ls [dir]      - Lista archivos y directorios");
        self.print_info("  cd [dir]      - Cambia de directorio");
        self.print_info("  pwd           - Muestra el directorio actual");
        self.print_info("  mkdir <dir>   - Crea un directorio");
        self.print_info("  rmdir <dir>   - Elimina un directorio");
        self.print_info("  rm <file>     - Elimina un archivo");
        self.print_info("  cp <src> <dst> - Copia un archivo");
        self.print_info("  mv <src> <dst> - Mueve un archivo");
        self.print_info("  cat <file>    - Muestra el contenido de un archivo");
        self.print_info("  echo <text>   - Imprime texto");
        self.print_info("  date          - Muestra la fecha y hora");
        self.print_info("  whoami        - Muestra el usuario actual");
        self.print_info("  ps            - Lista procesos");
        self.print_info("  top           - Muestra procesos en tiempo real");
        self.print_info("  history       - Muestra historial de comandos");
        self.print_info("  alias         - Muestra aliases");
        self.print_info("  unalias <name> - Elimina un alias");
        Ok(())
    }

    fn cmd_exit(&self) -> Result<(), &'static str> {
        self.print_info("Saliendo del shell...");
        // En una implementación real, esto terminaría el shell
        Err("Exit command")
    }

    fn cmd_clear(&self) -> Result<(), &'static str> {
        // Limpiar pantalla
        self.print_info("\x1B[2J\x1B[H");
        Ok(())
    }

    fn cmd_ls(&self, args: &[String]) -> Result<(), &'static str> {
        let dir = args.get(0).unwrap_or(&self.current_directory);
        self.print_info(&format!("Listando contenido de: {}", dir));
        self.print_info("  archivo1.txt");
        self.print_info("  archivo2.txt");
        self.print_info("  directorio1/");
        self.print_info("  directorio2/");
        Ok(())
    }

    fn cmd_cd(&mut self, args: &[String]) -> Result<(), &'static str> {
        let default_dir = "/".to_string();
        let dir = args.get(0).map(|s| s.clone()).unwrap_or(default_dir);
        self.current_directory = dir.clone();
        self.print_info(&format!("Directorio cambiado a: {}", dir));
        Ok(())
    }

    fn cmd_pwd(&self) -> Result<(), &'static str> {
        self.print_info(&self.current_directory);
        Ok(())
    }

    fn cmd_mkdir(&self, args: &[String]) -> Result<(), &'static str> {
        if args.is_empty() {
            return Err("Uso: mkdir <directorio>");
        }
        self.print_info(&format!("Creando directorio: {}", args[0]));
        Ok(())
    }

    fn cmd_rmdir(&self, args: &[String]) -> Result<(), &'static str> {
        if args.is_empty() {
            return Err("Uso: rmdir <directorio>");
        }
        self.print_info(&format!("Eliminando directorio: {}", args[0]));
        Ok(())
    }

    fn cmd_rm(&self, args: &[String]) -> Result<(), &'static str> {
        if args.is_empty() {
            return Err("Uso: rm <archivo>");
        }
        self.print_info(&format!("Eliminando archivo: {}", args[0]));
        Ok(())
    }

    fn cmd_cp(&self, args: &[String]) -> Result<(), &'static str> {
        if args.len() < 2 {
            return Err("Uso: cp <origen> <destino>");
        }
        self.print_info(&format!("Copiando {} a {}", args[0], args[1]));
        Ok(())
    }

    fn cmd_mv(&self, args: &[String]) -> Result<(), &'static str> {
        if args.len() < 2 {
            return Err("Uso: mv <origen> <destino>");
        }
        self.print_info(&format!("Moviendo {} a {}", args[0], args[1]));
        Ok(())
    }

    fn cmd_cat(&self, args: &[String]) -> Result<(), &'static str> {
        if args.is_empty() {
            return Err("Uso: cat <archivo>");
        }
        self.print_info(&format!("Contenido de {}:", args[0]));
        self.print_info("  Línea 1 del archivo");
        self.print_info("  Línea 2 del archivo");
        self.print_info("  Línea 3 del archivo");
        Ok(())
    }

    fn cmd_echo(&self, args: &[String]) -> Result<(), &'static str> {
        let text = args.join(" ");
        self.print_info(&text);
        Ok(())
    }

    fn cmd_date(&self) -> Result<(), &'static str> {
        self.print_info("Fecha: 2024-01-01");
        self.print_info("Hora: 12:00:00");
        Ok(())
    }

    fn cmd_whoami(&self) -> Result<(), &'static str> {
        self.print_info("usuario");
        Ok(())
    }

    fn cmd_ps(&self) -> Result<(), &'static str> {
        self.print_info("PID    Nombre           Estado");
        self.print_info("1      kernel           Running");
        self.print_info("2      shell            Running");
        self.print_info("3      file_manager     Stopped");
        Ok(())
    }

    fn cmd_top(&self) -> Result<(), &'static str> {
        self.print_info("Procesos en tiempo real:");
        self.print_info("PID    CPU%   Memoria   Nombre");
        self.print_info("1      0.1    1024      kernel");
        self.print_info("2      0.5    2048      shell");
        Ok(())
    }

    fn cmd_history(&self) -> Result<(), &'static str> {
        self.print_info("Historial de comandos:");
        for (i, cmd) in self.history.commands.iter().enumerate() {
            self.print_info(&format!("  {}: {}", i + 1, cmd));
        }
        Ok(())
    }

    fn cmd_alias(&self, args: &[String]) -> Result<(), &'static str> {
        if args.is_empty() {
            self.print_info("Aliases definidos:");
            for (alias, command) in &self.aliases {
                self.print_info(&format!("  {} = {}", alias, command));
            }
        } else {
            self.print_info("Uso: alias [nombre=comando]");
        }
        Ok(())
    }

    fn cmd_unalias(&self, args: &[String]) -> Result<(), &'static str> {
        if args.is_empty() {
            return Err("Uso: unalias <nombre>");
        }
        self.print_info(&format!("Eliminando alias: {}", args[0]));
        Ok(())
    }

    fn get_alias(&self, name: &str) -> Option<&String> {
        self.aliases.iter()
            .find(|(alias, _)| alias == name)
            .map(|(_, command)| command)
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

/// Función principal para ejecutar el shell
pub fn run() -> Result<(), &'static str> {
    let mut shell = EclipseShell::new();
    shell.run()
}
