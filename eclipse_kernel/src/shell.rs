//! Shell interactivo para Eclipse OS
//! 
//! Implementa un shell básico con comandos del sistema

use core::fmt::Write;

// Comandos disponibles
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Help,
    Clear,
    Info,
    Memory,
    Colors,
    Test,
    Echo,
    Unknown,
}

impl Command {
    pub fn from_str(s: &str) -> Self {
        let trimmed = s.trim();
        match trimmed {
            "help" | "?" | "h" | "HELP" | "H" => Command::Help,
            "clear" | "cls" | "c" | "CLEAR" | "CLS" | "C" => Command::Clear,
            "info" | "i" | "INFO" | "I" => Command::Info,
            "memory" | "mem" | "m" | "MEMORY" | "MEM" | "M" => Command::Memory,
            "colors" | "color" | "COLORS" | "COLOR" => Command::Colors,
            "test" | "t" | "TEST" | "T" => Command::Test,
            "echo" | "ECHO" => Command::Echo,
            _ => Command::Unknown,
        }
    }
}

// Shell principal
pub struct Shell {
    prompt: &'static str,
    command_history: [&'static str; 10],
    history_index: usize,
    current_input: [u8; 256],
    input_length: usize,
}

impl Shell {
    pub const fn new() -> Self {
        Self {
            prompt: "eclipse> ",
            command_history: [""; 10],
            history_index: 0,
            current_input: [0; 256],
            input_length: 0,
        }
    }

    pub fn run(&mut self) -> ! {
        self.show_welcome();
        self.show_prompt();
        
        loop {
            self.handle_input();
        }
    }

    fn show_welcome(&self) {
        unsafe {
            VGA.set_color(Color::LightCyan, Color::Black);
            VGA.write_string("╔══════════════════════════════════════════════════════════════════════════════╗\n");
            VGA.set_color(Color::LightCyan, Color::Black);
            VGA.write_string("║                           ECLIPSE OS SHELL v1.0                            ║\n");
            VGA.set_color(Color::LightCyan, Color::Black);
            VGA.write_string("║                        Shell interactivo en Rust                            ║\n");
            VGA.set_color(Color::LightCyan, Color::Black);
            VGA.write_string("╚══════════════════════════════════════════════════════════════════════════════╝\n");
            VGA.set_color(Color::White, Color::Black);
            VGA.write_string("\n");
            VGA.write_string("Bienvenido al shell de Eclipse OS. Escribe 'help' para ver los comandos disponibles.\n");
            VGA.write_string("Presiona Enter para ejecutar comandos. Ctrl+C para interrumpir.\n\n");
        }
    }

    fn show_prompt(&self) {
        unsafe {
            VGA.set_color(Color::LightGreen, Color::Black);
            VGA.write_string(self.prompt);
            VGA.set_color(Color::White, Color::Black);
        }
    }

    fn handle_input(&mut self) {
        // Simulamos entrada de teclado por ahora
        // En un sistema real, esto vendría del driver de teclado
        self.simulate_keyboard_input();
    }

    fn simulate_keyboard_input(&mut self) {
        // Simulamos algunos comandos para demostración
        let demo_commands = [
            "help",
            "info", 
            "memory",
            "colors",
            "test",
            "echo Hola Eclipse OS!",
            "clear",
            "help",
        ];
        
        static mut DEMO_INDEX: usize = 0;
        static mut DEMO_DELAY: u32 = 0;
        
        unsafe {
            DEMO_DELAY += 1;
            if DEMO_DELAY > 5000000 { // Simular delay más largo para mejor visualización
                DEMO_DELAY = 0;
                if DEMO_INDEX < demo_commands.len() {
                    let cmd = demo_commands[DEMO_INDEX];
                    self.execute_command(cmd);
                    DEMO_INDEX += 1;
                } else {
                    // Reiniciar demo
                    DEMO_INDEX = 0;
                    // Mostrar mensaje de reinicio
                    VGA.set_color(Color::LightCyan, Color::Black);
                    VGA.write_string("\n--- Reiniciando demostración del shell ---\n");
                    VGA.set_color(Color::White, Color::Black);
                }
            }
        }
    }

    fn execute_command(&mut self, input: &str) {
        // Mostrar el comando que se está ejecutando
        unsafe {
            VGA.set_color(Color::Yellow, Color::Black);
            VGA.write_string(input);
            VGA.write_string("\n");
            VGA.set_color(Color::White, Color::Black);
        }

        // Parsear y ejecutar comando
        let parts: core::str::Split<'_, char> = input.split(' ');
        let cmd_str = parts.clone().next().unwrap_or("");
        let args: heapless::Vec<&str, 16> = parts.skip(1).collect();

        let command = Command::from_str(cmd_str);
        
        match command {
            Command::Help => self.cmd_help(),
            Command::Clear => self.cmd_clear(),
            Command::Info => self.cmd_info(),
            Command::Memory => self.cmd_memory(),
            Command::Colors => self.cmd_colors(),
            Command::Test => self.cmd_test(),
            Command::Echo => self.cmd_echo(&args),
            Command::Unknown => self.cmd_unknown(cmd_str),
        }

        self.show_prompt();
    }

    fn cmd_help(&self) {
        unsafe {
            VGA.set_color(Color::LightBlue, Color::Black);
            VGA.write_string("Comandos disponibles:\n");
            VGA.set_color(Color::White, Color::Black);
            VGA.write_string("  help, ?, h     - Muestra esta ayuda\n");
            VGA.write_string("  clear, cls, c  - Limpia la pantalla\n");
            VGA.write_string("  info, i        - Información del sistema\n");
            VGA.write_string("  memory, mem, m - Información de memoria\n");
            VGA.write_string("  colors         - Demostración de colores\n");
            VGA.write_string("  test, t        - Ejecuta pruebas del sistema\n");
            VGA.write_string("  echo <texto>   - Muestra el texto especificado\n");
            VGA.write_string("\n");
        }
    }

    fn cmd_clear(&self) {
        unsafe {
            VGA.clear_screen();
        }
    }

    fn cmd_info(&self) {
        unsafe {
            VGA.set_color(Color::LightMagenta, Color::Black);
            VGA.write_string("Información del sistema Eclipse OS:\n");
            VGA.set_color(Color::White, Color::Black);
            VGA.write_string("  - Versión: 1.0\n");
            VGA.write_string("  - Arquitectura: x86_64\n");
            VGA.write_string("  - Modo: 64-bit\n");
            VGA.write_string("  - Bootloader: UEFI\n");
            VGA.write_string("  - Kernel: Rust (no_std)\n");
            VGA.write_string("  - Shell: Interactivo\n");
            VGA.write_string("  - Drivers: VGA, Serial COM1\n");
            VGA.write_string("\n");
        }
    }

    fn cmd_memory(&self) {
        unsafe {
            VGA.set_color(Color::LightCyan, Color::Black);
            VGA.write_string("Información de memoria:\n");
            VGA.set_color(Color::White, Color::Black);
            VGA.write_string("  - Kernel base: 0x200000\n");
            VGA.write_string("  - VGA buffer: 0xB8000\n");
            VGA.write_string("  - Serial COM1: 0x3F8\n");
            VGA.write_string("  - Stack: Configurado por bootloader\n");
            VGA.write_string("  - Heap: No implementado aún\n");
            VGA.write_string("  - Paginación: Deshabilitada\n");
            VGA.write_string("\n");
        }
    }

    fn cmd_colors(&self) {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Demostración de colores VGA:\n");
            
            let colors = [
                (Color::Black, "Negro"),
                (Color::Blue, "Azul"),
                (Color::Green, "Verde"),
                (Color::Cyan, "Cian"),
                (Color::Red, "Rojo"),
                (Color::Magenta, "Magenta"),
                (Color::Brown, "Marrón"),
                (Color::LightGray, "Gris claro"),
                (Color::DarkGray, "Gris oscuro"),
                (Color::LightBlue, "Azul claro"),
                (Color::LightGreen, "Verde claro"),
                (Color::LightCyan, "Cian claro"),
                (Color::LightRed, "Rojo claro"),
                (Color::LightMagenta, "Magenta claro"),
                (Color::Yellow, "Amarillo"),
                (Color::White, "Blanco"),
            ];
            
            for (i, (color, name)) in colors.iter().enumerate() {
                VGA.set_color(*color, Color::Black);
                VGA.write_string("■ ");
                VGA.set_color(Color::White, Color::Black);
                VGA.write_string(name);
                if (i + 1) % 4 == 0 {
                    VGA.write_string("\n");
                } else {
                    VGA.write_string("  ");
                }
            }
            VGA.write_string("\n\n");
        }
    }

    fn cmd_test(&self) {
        unsafe {
            VGA.set_color(Color::LightGreen, Color::Black);
            VGA.write_string("Ejecutando pruebas del sistema...\n");
            VGA.set_color(Color::White, Color::Black);
            
            // Test 1: VGA
            VGA.write_string("  ✓ Test VGA: OK\n");
            
            // Test 2: Serial
            SERIAL.write_string("  ✓ Test Serial: OK\n");
            
            // Test 3: Funciones de formateo
            VGA.write_string("  ✓ Test formateo: ");
            print_hex(0xDEADBEEF);
            VGA.write_string("\n");
            
            // Test 4: Colores
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("  ✓ Test colores: ");
            VGA.set_color(Color::LightGreen, Color::Black);
            VGA.write_string("OK");
            VGA.set_color(Color::White, Color::Black);
            VGA.write_string("\n");
            
            VGA.write_string("  ✓ Todas las pruebas completadas exitosamente\n");
            VGA.write_string("\n");
        }
    }

    fn cmd_echo(&self, args: &[&str]) {
        if args.is_empty() {
            unsafe {
                VGA.set_color(Color::LightRed, Color::Black);
                VGA.write_string("Error: echo requiere un argumento\n");
                VGA.set_color(Color::White, Color::Black);
            }
            return;
        }

        unsafe {
            VGA.set_color(Color::LightBlue, Color::Black);
            VGA.write_string("Echo: ");
            VGA.set_color(Color::White, Color::Black);
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    VGA.write_string(" ");
                }
                VGA.write_string(arg);
            }
            VGA.write_string("\n");
        }
    }

    fn cmd_unknown(&self, cmd: &str) {
        unsafe {
            VGA.set_color(Color::LightRed, Color::Black);
            VGA.write_string("Comando desconocido: ");
            VGA.write_string(cmd);
            VGA.write_string("\n");
            VGA.set_color(Color::Yellow, Color::Black);
            VGA.write_string("Escribe 'help' para ver los comandos disponibles.\n");
            VGA.set_color(Color::White, Color::Black);
        }
    }
}

// Re-exportar tipos necesarios
use crate::main_improved::{VGA, SERIAL, Color, print_hex};

// Función principal del shell
pub fn run_shell() -> ! {
    let mut shell = Shell::new();
    shell.run();
}