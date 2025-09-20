//! Shell integrado con comandos de IA para Eclipse OS
//! 
//! Este módulo proporciona un shell interactivo que incluye
//! comandos de IA integrados en el sistema operativo.

#![no_std]

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use crate::ai_commands::{execute_ai_command, get_ai_command_manager};
use crate::{KernelResult, KernelError, syslog_info, syslog_warn, syslog_err};

/// Shell integrado con IA
pub struct AIShell {
    pub is_running: bool,
    pub command_history: Vec<String>,
    pub variables: BTreeMap<String, String>,
    pub prompt: String,
}

impl AIShell {
    pub fn new() -> Self {
        Self {
            is_running: false,
            command_history: Vec::new(),
            variables: BTreeMap::new(),
            prompt: "eclipse-ai> ".to_string(),
        }
    }

    /// Inicializar el shell
    pub fn initialize(&mut self) -> KernelResult<()> {
        syslog_info!("AI_SHELL", "Inicializando shell con IA integrada");
        
        // Configurar variables de entorno por defecto
        self.variables.insert("PS1".to_string(), "eclipse-ai> ".to_string());
        self.variables.insert("PATH".to_string(), "/bin:/usr/bin:/usr/local/bin".to_string());
        self.variables.insert("HOME".to_string(), "/home/user".to_string());
        self.variables.insert("USER".to_string(), "eclipse".to_string());
        
        self.is_running = true;
        syslog_info!("AI_SHELL", "Shell con IA inicializado correctamente");
        Ok(())
    }

    /// Ejecutar una línea de comando
    pub fn execute_line(&mut self, line: &str) -> KernelResult<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }

        // Agregar al historial
        self.command_history.push(trimmed.to_string());

        // Parsear comando y argumentos
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.is_empty() {
            return Ok(String::new());
        }

        let command = parts[0];
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

        // Ejecutar comando
        self.execute_command(command, &args)
    }

    /// Ejecutar un comando específico
    fn execute_command(&mut self, command: &str, args: &[String]) -> KernelResult<String> {
        match command {
            // Comandos del sistema
            "exit" | "quit" => {
                self.is_running = false;
                Ok("Saliendo del shell Eclipse AI...".to_string())
            }
            "help" => {
                self.help_command()
            }
            "clear" => {
                Ok("\x1B[2J\x1B[H".to_string()) // ANSI clear screen
            }
            "history" => {
                self.history_command()
            }
            "echo" => {
                Ok(args.join(" "))
            }
            "set" => {
                self.set_command(args)
            }
            "get" => {
                self.get_command(args)
            }
            "env" => {
                self.env_command()
            }
            "pwd" => {
                Ok(self.variables.get("PWD").unwrap_or(&"/".to_string()).clone())
            }
            "cd" => {
                self.cd_command(args)
            }
            "ls" => {
                self.ls_command(args)
            }
            "ps" => {
                self.ps_command()
            }
            "top" => {
                self.top_command()
            }
            "free" => {
                self.free_command()
            }
            "df" => {
                self.df_command()
            }
            "uptime" => {
                self.uptime_command()
            }
            "uname" => {
                self.uname_command()
            }
            "whoami" => {
                Ok(self.variables.get("USER").unwrap_or(&"eclipse".to_string()).clone())
            }
            "date" => {
                self.date_command()
            }
            // Comandos de IA
            _ => {
                // Intentar ejecutar como comando de IA
                match execute_ai_command(command, args) {
                    Ok(result) => Ok(result),
                    Err(_) => {
                        // Si no es un comando de IA, mostrar error
                        Ok(alloc::format!("Comando '{}' no encontrado. Escriba 'help' para ver comandos disponibles.", command))
                    }
                }
            }
        }
    }

    /// Comando help
    fn help_command(&self) -> KernelResult<String> {
        let mut output = String::new();
        output.push_str("=== ECLIPSE OS AI SHELL ===\n\n");
        
        output.push_str("Comandos del Sistema:\n");
        output.push_str("  help          - Mostrar esta ayuda\n");
        output.push_str("  clear         - Limpiar pantalla\n");
        output.push_str("  exit/quit     - Salir del shell\n");
        output.push_str("  history       - Mostrar historial de comandos\n");
        output.push_str("  echo <text>   - Mostrar texto\n");
        output.push_str("  set <var=val> - Establecer variable\n");
        output.push_str("  get <var>     - Obtener variable\n");
        output.push_str("  env           - Mostrar variables de entorno\n");
        output.push_str("  pwd           - Directorio actual\n");
        output.push_str("  cd <dir>      - Cambiar directorio\n");
        output.push_str("  ls [dir]      - Listar archivos\n");
        output.push_str("  ps            - Procesos activos\n");
        output.push_str("  top           - Procesos en tiempo real\n");
        output.push_str("  free          - Uso de memoria\n");
        output.push_str("  df            - Uso de disco\n");
        output.push_str("  uptime        - Tiempo de actividad\n");
        output.push_str("  uname         - Información del sistema\n");
        output.push_str("  whoami        - Usuario actual\n");
        output.push_str("  date          - Fecha y hora\n\n");
        
        output.push_str("Comandos de IA:\n");
        if let Some(manager) = get_ai_command_manager() {
            for command_name in manager.list_commands() {
                if let Some(command) = manager.get_command_info(&command_name) {
                    output.push_str(&alloc::format!("  {} - {}\n", command.name, command.description));
                }
            }
        }
        
        output.push_str("\nEscriba 'ai-help' para más información sobre comandos de IA.\n");
        
        Ok(output)
    }

    /// Comando history
    fn history_command(&self) -> KernelResult<String> {
        let mut output = String::new();
        output.push_str("=== HISTORIAL DE COMANDOS ===\n\n");
        
        for (i, cmd) in self.command_history.iter().enumerate() {
            output.push_str(&alloc::format!("{:3}: {}\n", i + 1, cmd));
        }
        
        Ok(output)
    }

    /// Comando set
    fn set_command(&mut self, args: &[String]) -> KernelResult<String> {
        if args.len() != 1 {
            return Ok("Uso: set <variable=valor>".to_string());
        }

        let assignment = &args[0];
        if let Some(eq_pos) = assignment.find('=') {
            let var = &assignment[..eq_pos];
            let val = &assignment[eq_pos + 1..];
            self.variables.insert(var.to_string(), val.to_string());
            Ok(alloc::format!("Variable {} establecida a '{}'", var, val))
        } else {
            Ok("Formato incorrecto. Use: variable=valor".to_string())
        }
    }

    /// Comando get
    fn get_command(&self, args: &[String]) -> KernelResult<String> {
        if args.len() != 1 {
            return Ok("Uso: get <variable>".to_string());
        }

        let var = &args[0];
        if let Some(val) = self.variables.get(var) {
            Ok(val.clone())
        } else {
            Ok(alloc::format!("Variable '{}' no encontrada", var))
        }
    }

    /// Comando env
    fn env_command(&self) -> KernelResult<String> {
        let mut output = String::new();
        output.push_str("=== VARIABLES DE ENTORNO ===\n\n");
        
        for (var, val) in &self.variables {
            output.push_str(&alloc::format!("{}={}\n", var, val));
        }
        
        Ok(output)
    }

    /// Comando cd
    fn cd_command(&mut self, args: &[String]) -> KernelResult<String> {
        let dir = if args.is_empty() {
            self.variables.get("HOME").unwrap_or(&"/".to_string()).clone()
        } else {
            args[0].clone()
        };

        // Simular cambio de directorio
        self.variables.insert("PWD".to_string(), dir.clone());
        Ok(alloc::format!("Directorio cambiado a: {}", dir))
    }

    /// Comando ls
    fn ls_command(&self, args: &[String]) -> KernelResult<String> {
        let dir = if args.is_empty() {
            self.variables.get("PWD").unwrap_or(&"/".to_string()).clone()
        } else {
            args[0].clone()
        };

        let mut output = String::new();
        output.push_str(&alloc::format!("Contenido de {}:\n\n", dir));
        output.push_str("drwxr-xr-x 2 root root 4096 Jan  1 00:00 bin\n");
        output.push_str("drwxr-xr-x 2 root root 4096 Jan  1 00:00 etc\n");
        output.push_str("drwxr-xr-x 2 root root 4096 Jan  1 00:00 home\n");
        output.push_str("drwxr-xr-x 2 root root 4096 Jan  1 00:00 lib\n");
        output.push_str("drwxr-xr-x 2 root root 4096 Jan  1 00:00 usr\n");
        output.push_str("drwxr-xr-x 2 root root 4096 Jan  1 00:00 var\n");
        output.push_str("-rw-r--r-- 1 root root 1024 Jan  1 00:00 kernel.log\n");
        output.push_str("-rw-r--r-- 1 root root 2048 Jan  1 00:00 system.log\n");

        Ok(output)
    }

    /// Comando ps
    fn ps_command(&self) -> KernelResult<String> {
        let mut output = String::new();
        output.push_str("=== PROCESOS ACTIVOS ===\n\n");
        output.push_str("  PID  PPID  CMD\n");
        output.push_str("----- ----- --------------------\n");
        output.push_str("    1     0  kernel_init\n");
        output.push_str("    2     0  ai_services\n");
        output.push_str("    3     0  ai_commands\n");
        output.push_str("    4     0  shell\n");
        output.push_str("    5     0  syslog\n");
        output.push_str("    6     0  memory_manager\n");
        output.push_str("    7     0  process_manager\n");

        Ok(output)
    }

    /// Comando top
    fn top_command(&self) -> KernelResult<String> {
        let mut output = String::new();
        output.push_str("=== PROCESOS EN TIEMPO REAL ===\n\n");
        output.push_str("  PID  CPU%  MEM%  CMD\n");
        output.push_str("----- ----- ----- --------------------\n");
        output.push_str("    1  15.2  12.5  kernel_init\n");
        output.push_str("    2   8.7   8.3  ai_services\n");
        output.push_str("    3   5.1   4.2  ai_commands\n");
        output.push_str("    4   2.3   3.1  shell\n");
        output.push_str("    5   1.8   2.7  syslog\n");
        output.push_str("    6   1.2   2.1  memory_manager\n");
        output.push_str("    7   0.9   1.8  process_manager\n");

        Ok(output)
    }

    /// Comando free
    fn free_command(&self) -> KernelResult<String> {
        let mut output = String::new();
        output.push_str("=== USO DE MEMORIA ===\n\n");
        output.push_str("              total        used        free      shared\n");
        output.push_str("Memoria:     8388608     4194304     4194304          0\n");
        output.push_str("Swap:             0           0           0\n");
        output.push_str("Cache:      1048576      524288      524288\n");

        Ok(output)
    }

    /// Comando df
    fn df_command(&self) -> KernelResult<String> {
        let mut output = String::new();
        output.push_str("=== USO DE DISCO ===\n\n");
        output.push_str("Filesystem     1K-blocks    Used Available Use% Mounted on\n");
        output.push_str("/dev/sda1       10485760  5242880   5242880  50% /\n");
        output.push_str("/dev/sda2        5242880  1048576   4194304  20% /home\n");
        output.push_str("/dev/sda3        2097152   524288   1572864  25% /var\n");

        Ok(output)
    }

    /// Comando uptime
    fn uptime_command(&self) -> KernelResult<String> {
        Ok("Sistema activo desde: 00:00:01, tiempo de actividad: 1 día, 2 horas, 30 minutos".to_string())
    }

    /// Comando uname
    fn uname_command(&self) -> KernelResult<String> {
        Ok("Eclipse OS 0.6.0 x86_64 GNU/Linux".to_string())
    }

    /// Comando date
    fn date_command(&self) -> KernelResult<String> {
        Ok("Lun Ene  1 00:00:01 UTC 2024".to_string())
    }

    /// Ejecutar shell interactivo
    pub fn run_interactive(&mut self) -> KernelResult<()> {
        syslog_info!("AI_SHELL", "Iniciando shell interactivo con IA");
        
        // Mostrar banner de bienvenida
        self.show_welcome_banner()?;
        
        // Bucle principal del shell
        while self.is_running {
            // En una implementación real, aquí se leería input del usuario
            // Por ahora, simulamos algunos comandos de demostración
            self.run_demo_commands()?;
            break; // Salir después de la demostración
        }
        
        syslog_info!("AI_SHELL", "Shell interactivo finalizado");
        Ok(())
    }

    /// Mostrar banner de bienvenida
    fn show_welcome_banner(&self) -> KernelResult<()> {
        let banner = r#"
╔══════════════════════════════════════════════════════════════╗
║                    ECLIPSE OS AI SHELL                      ║
║                                                              ║
║  Sistema Operativo Eclipse con Inteligencia Artificial     ║
║  Versión 0.6.0 - Kernel Nativo en Rust                     ║
║                                                              ║
║  Comandos disponibles:                                       ║
║    - Comandos del sistema: help, ls, ps, top, etc.          ║
║    - Comandos de IA: ai-status, ai-optimize, ai-security    ║
║                                                              ║
║  Escriba 'help' para ver todos los comandos disponibles     ║
║  Escriba 'ai-help' para comandos específicos de IA          ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
"#;
        
        syslog_info!("AI_SHELL", banner);
        Ok(())
    }

    /// Ejecutar comandos de demostración
    fn run_demo_commands(&mut self) -> KernelResult<()> {
        let demo_commands = [
            "ai-status",
            "ai-models",
            "ai-optimize",
            "ai-security",
            "ps",
            "free",
            "uname",
            "ai-help",
        ].to_vec();

        for cmd in demo_commands {
            syslog_info!("AI_SHELL", &alloc::format!("Ejecutando: {}", cmd));
            match self.execute_line(cmd) {
                Ok(output) => {
                    if !output.is_empty() {
                        syslog_info!("AI_SHELL", &output);
                    }
                }
                Err(e) => {
                    syslog_warn!("AI_SHELL", &alloc::format!("Error ejecutando '{}': {}", cmd, e));
                }
            }
        }

        Ok(())
    }
}

/// Instancia global del shell con IA
static mut AI_SHELL: Option<AIShell> = None;

/// Inicializar shell con IA
pub fn init_ai_shell() -> KernelResult<()> {
    syslog_info!("AI_SHELL", "Inicializando shell con IA integrada");
    
    unsafe {
        AI_SHELL = Some(AIShell::new());
        if let Some(ref mut shell) = AI_SHELL {
            shell.initialize()?;
        }
    }
    
    syslog_info!("AI_SHELL", "Shell con IA inicializado correctamente");
    Ok(())
}

/// Obtener el shell con IA
pub fn get_ai_shell() -> Option<&'static mut AIShell> {
    unsafe { AI_SHELL.as_mut() }
}

/// Ejecutar shell interactivo
pub fn run_ai_shell() -> KernelResult<()> {
    if let Some(shell) = get_ai_shell() {
        shell.run_interactive()
    } else {
        Err("Shell con IA no inicializado".into())
    }
}
