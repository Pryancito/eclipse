//! # Shell del Sistema Userland

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub command: String,
    pub args: Vec<String>,
    pub working_directory: String,
}

pub struct Shell {
    pub current_directory: String,
    pub environment_variables: std::collections::HashMap<String, String>,
    pub command_history: Vec<String>,
}

impl Shell {
    pub fn new() -> Self {
        Self {
            current_directory: "C:\\".to_string(),
            environment_variables: std::collections::HashMap::new(),
            command_history: Vec::new(),
        }
    }
    
    pub fn initialize(&mut self) -> Result<()> {
        // Configurar variables de entorno básicas
        self.environment_variables.insert("PATH".to_string(), "C:\\Windows\\System32;C:\\Windows".to_string());
        self.environment_variables.insert("USERNAME".to_string(), "Administrator".to_string());
        self.environment_variables.insert("COMPUTERNAME".to_string(), "REACTOS-RUST".to_string());
        self.environment_variables.insert("OS".to_string(), "Windows_NT".to_string());
        
        Ok(())
    }
    
    pub fn execute_command(&mut self, command_line: &str) -> Result<()> {
        let parts: Vec<&str> = command_line.trim().split_whitespace().collect();
        if parts.is_empty() {
            return Ok(());
        }
        
        let command = parts[0];
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        
        // Agregar a historial
        self.command_history.push(command_line.to_string());
        
        match command {
            "cd" => self.change_directory(&args),
            "dir" => self.list_directory(),
            "echo" => self.echo_command(&args),
            "help" => self.show_help(),
            "exit" => self.exit_shell(),
            _ => self.execute_external_command(command, &args),
        }
    }
    
    fn change_directory(&mut self, args: &[String]) -> Result<()> {
        if args.is_empty() {
            self.current_directory = "C:\\".to_string();
        } else {
            let new_dir = &args[0];
            if new_dir.starts_with("C:\\") || new_dir.starts_with("D:\\") {
                self.current_directory = new_dir.clone();
            } else {
                self.current_directory = format!("{}\\{}", self.current_directory, new_dir);
            }
        }
        println!("Current directory: {}", self.current_directory);
        Ok(())
    }
    
    fn list_directory(&self) -> Result<()> {
        println!("Directory of {}", self.current_directory);
        println!("<DIR>  .");
        println!("<DIR>  ..");
        println!("<DIR>  Windows");
        println!("<DIR>  Program Files");
        println!("<DIR>  Users");
        println!("<DIR>  Documents and Settings");
        Ok(())
    }
    
    fn echo_command(&self, args: &[String]) -> Result<()> {
        println!("{}", args.join(" "));
        Ok(())
    }
    
    fn show_help(&self) -> Result<()> {
        println!("ReactOS Rust Shell - Available commands:");
        println!("  cd [directory]  - Change directory");
        println!("  dir             - List directory contents");
        println!("  echo [text]     - Display text");
        println!("  help            - Show this help");
        println!("  exit            - Exit shell");
        Ok(())
    }
    
    fn exit_shell(&self) -> Result<()> {
        println!("Goodbye!");
        std::process::exit(0);
    }
    
    fn execute_external_command(&self, command: &str, _args: &[String]) -> Result<()> {
        println!("Command '{}' not found. Type 'help' for available commands.", command);
        Ok(())
    }
    
    pub fn get_prompt(&self) -> String {
        format!("{}> ", self.current_directory)
    }
}

static mut SHELL: Option<Shell> = None;

pub fn initialize() -> Result<()> {
    unsafe {
        SHELL = Some(Shell::new());
        if let Some(ref mut shell) = SHELL {
            shell.initialize()?;
        }
    }
    Ok(())
}

pub fn execute_command(command_line: &str) -> Result<()> {
    unsafe {
        if let Some(ref mut shell) = SHELL {
            shell.execute_command(command_line)
        } else {
            Err(anyhow::anyhow!("Shell not initialized"))
        }
    }
}

pub fn get_prompt() -> String {
    unsafe {
        if let Some(ref shell) = SHELL {
            shell.get_prompt()
        } else {
            "C:\\> ".to_string()
        }
    }
}
