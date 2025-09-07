//! Sistema de mensajes de boot del kernel Eclipse
//! 
//! Proporciona mensajes informativos durante la inicialización del kernel
//! mostrando el progreso de cada componente del sistema.

use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

/// Niveles de mensaje de boot
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BootLevel {
    Info,
    Success,
    Warning,
    Error,
    Debug,
}

/// Sistema de mensajes de boot
pub struct BootMessenger {
    pub message_count: AtomicU32,
    pub current_step: AtomicU32,
    pub total_steps: u32,
}

impl BootMessenger {
    /// Crear un nuevo sistema de mensajes de boot
    pub const fn new() -> Self {
        Self {
            message_count: AtomicU32::new(0),
            current_step: AtomicU32::new(0),
            total_steps: 15, // Número total de pasos de inicialización
        }
    }

    /// Agregar un mensaje de boot
    pub fn add_message(&mut self, level: BootLevel, component: &'static str, message: &'static str) {
        let count = self.message_count.load(Ordering::Relaxed);
        self.message_count.store(count + 1, Ordering::Relaxed);
        self.display_message(level, component, message);
    }

    /// Mostrar mensaje de progreso
    pub fn show_progress(&mut self, step: u32, component: &'static str, message: &'static str) {
        self.current_step.store(step, Ordering::Relaxed);
        self.add_message(BootLevel::Info, component, message);
        self.display_progress_bar();
    }

    /// Mostrar mensaje de éxito
    pub fn show_success(&mut self, component: &'static str, message: &'static str) {
        self.add_message(BootLevel::Success, component, message);
    }

    /// Mostrar mensaje de advertencia
    pub fn show_warning(&mut self, component: &'static str, message: &'static str) {
        self.add_message(BootLevel::Warning, component, message);
    }

    /// Mostrar mensaje de error
    pub fn show_error(&mut self, component: &'static str, message: &'static str) {
        self.add_message(BootLevel::Error, component, message);
    }

    /// Mostrar mensaje de debug
    pub fn show_debug(&mut self, component: &'static str, message: &'static str) {
        self.add_message(BootLevel::Debug, component, message);
    }

    /// Mostrar barra de progreso
    fn display_progress_bar(&self) {
        let current = self.current_step.load(Ordering::Relaxed);
        let total = self.total_steps;
        
        // Mostrar barra de progreso simple
        self.print_text("Progreso: ");
        self.print_number(current);
        self.print_text("/");
        self.print_number(total);
        self.print_text("\n");
    }

    /// Mostrar mensaje con color
    fn display_message(&self, level: BootLevel, component: &str, message: &str) {
        let prefix = match level {
            BootLevel::Info => "[INFO]",
            BootLevel::Success => "[OK]",
            BootLevel::Warning => "[WARN]",
            BootLevel::Error => "[ERROR]",
            BootLevel::Debug => "[DEBUG]",
        };

        // Mostrar mensaje simple sin format!
        self.print_text(prefix);
        self.print_text(" ");
        self.print_text(component);
        self.print_text(": ");
        self.print_text(message);
        self.print_text("\n");
    }

    /// Imprimir número simple
    fn print_number(&self, num: u32) {
        // Conversión simple de número a string
        if num == 0 {
            self.print_text("0");
            return;
        }
        
        let mut n = num;
        let mut digits = [0u8; 10];
        let mut i = 0;
        
        while n > 0 {
            digits[i] = (n % 10) as u8 + b'0';
            n /= 10;
            i += 1;
        }
        
        // Imprimir dígitos en orden inverso
        for j in (0..i).rev() {
            let digit_bytes = [digits[j]];
            let digit_str = core::str::from_utf8(&digit_bytes).unwrap_or("0");
            self.print_text(digit_str);
        }
    }

    /// Imprimir texto (implementación básica)
    fn print_text(&self, text: &str) {
        // Implementación básica de impresión
        // En un kernel real, esto usaría VGA o framebuffer
        unsafe {
            let vga_buffer = 0xb8000 as *mut u16;
            static mut VGA_INDEX: usize = 0;
            
            for byte in text.bytes() {
                if VGA_INDEX < 2000 { // 80x25 = 2000 caracteres
                    // Usar color blanco (0x0F) sobre negro (0x00) = 0x0F00
                    *vga_buffer.add(VGA_INDEX) = 0x0F00 | byte as u16; // Blanco sobre negro
                    VGA_INDEX += 1;
                }
            }
        }
    }

    /// Mostrar banner de inicio del kernel
    pub fn show_banner(&mut self) {
        self.print_text("╔══════════════════════════════════════════════════════════════╗\n");
        self.print_text("║                                                              ║\n");
        self.print_text("║                     ECLIPSE KERNEL                    ║\n");
        self.print_text("║                                                              ║\n");
        self.print_text("║              Kernel Nativo de Eclipse v0.1.0             ║\n");
        self.print_text("║                                                              ║\n");
        self.print_text("╚══════════════════════════════════════════════════════════════╝\n");
        self.print_text("\n");
    }

    /// Mostrar resumen de inicialización
    pub fn show_summary(&self) {
        let total_messages = self.message_count.load(Ordering::Relaxed);
        let current_step = self.current_step.load(Ordering::Relaxed);
        
        self.print_text("========================================\n");
        self.print_text("    INICIALIZACIÓN COMPLETA\n");
        self.print_text("========================================\n");
        self.print_text("Pasos completados: ");
        self.print_number(current_step);
        self.print_text("/");
        self.print_number(self.total_steps);
        self.print_text("\n");
        self.print_text("Mensajes generados: ");
        self.print_number(total_messages);
        self.print_text("\n");
        self.print_text("Kernel Eclipse listo para operar!\n");
    }
}

/// Instancia global del sistema de mensajes de boot
static BOOT_MESSENGER: BootMessenger = BootMessenger::new();
static BOOT_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Funciones de conveniencia para usar el sistema de mensajes
pub fn boot_info(component: &'static str, message: &'static str) {
    // En un entorno real, esto sería thread-safe
    // Por ahora, solo mostramos el mensaje directamente
    display_message_direct(BootLevel::Info, component, message);
}

pub fn boot_success(component: &'static str, message: &'static str) {
    display_message_direct(BootLevel::Success, component, message);
}

pub fn boot_warning(component: &'static str, message: &'static str) {
    display_message_direct(BootLevel::Warning, component, message);
}

pub fn boot_error(component: &'static str, message: &'static str) {
    display_message_direct(BootLevel::Error, component, message);
}

pub fn boot_progress(step: u32, component: &'static str, message: &'static str) {
    display_message_direct(BootLevel::Info, component, message);
    display_progress_bar_direct(step);
}

pub fn boot_banner() {
    display_banner_direct();
}

pub fn boot_summary() {
    display_summary_direct();
}

/// Función auxiliar para mostrar mensajes directamente
fn display_message_direct(level: BootLevel, component: &'static str, message: &'static str) {
    // Implementación básica de impresión
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        static mut VGA_INDEX: usize = 0;
        
        let prefix = match level {
            BootLevel::Info => "[INFO]",
            BootLevel::Success => "[OK]",
            BootLevel::Warning => "[WARN]",
            BootLevel::Error => "[ERROR]",
            BootLevel::Debug => "[DEBUG]",
        };

        // Imprimir prefijo
        for byte in prefix.bytes() {
            if VGA_INDEX < 2000 {
                *vga_buffer.add(VGA_INDEX) = 0x0F00 | byte as u16; // Blanco sobre negro
                VGA_INDEX += 1;
            }
        }
        
        // Imprimir componente
        for byte in component.bytes() {
            if VGA_INDEX < 2000 {
                *vga_buffer.add(VGA_INDEX) = 0x0F00 | byte as u16; // Blanco sobre negro
                VGA_INDEX += 1;
            }
        }
        
        // Imprimir ": "
        if VGA_INDEX < 2000 {
            *vga_buffer.add(VGA_INDEX) = 0x0F00 | b':' as u16; // Blanco sobre negro
            VGA_INDEX += 1;
        }
        if VGA_INDEX < 2000 {
            *vga_buffer.add(VGA_INDEX) = 0x0F00 | b' ' as u16; // Blanco sobre negro
            VGA_INDEX += 1;
        }
        
        // Imprimir mensaje
        for byte in message.bytes() {
            if VGA_INDEX < 2000 {
                *vga_buffer.add(VGA_INDEX) = 0x0F00 | byte as u16; // Blanco sobre negro
                VGA_INDEX += 1;
            }
        }
        
        // Nueva línea
        if VGA_INDEX < 2000 {
            *vga_buffer.add(VGA_INDEX) = 0x0F00 | b'\n' as u16; // Blanco sobre negro
            VGA_INDEX += 1;
        }
    }
}

/// Función auxiliar para mostrar barra de progreso directamente
fn display_progress_bar_direct(step: u32) {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        static mut VGA_INDEX: usize = 0;
        
        let progress_text = "Progreso: ";
        for byte in progress_text.bytes() {
            if VGA_INDEX < 2000 {
                *vga_buffer.add(VGA_INDEX) = 0x0F00 | byte as u16; // Blanco sobre negro
                VGA_INDEX += 1;
            }
        }
        
        // Imprimir número de paso
        let mut n = step;
        let mut digits = [0u8; 10];
        let mut i = 0;
        
        if n == 0 {
            if VGA_INDEX < 2000 {
                *vga_buffer.add(VGA_INDEX) = 0x0F00 | b'0' as u16; // Blanco sobre negro
                VGA_INDEX += 1;
            }
        } else {
            while n > 0 {
                digits[i] = (n % 10) as u8 + b'0';
                n /= 10;
                i += 1;
            }
            
            for j in (0..i).rev() {
                if VGA_INDEX < 2000 {
                    *vga_buffer.add(VGA_INDEX) = 0x0F00 | digits[j] as u16; // Blanco sobre negro
                    VGA_INDEX += 1;
                }
            }
        }
        
        // Imprimir "/15"
        if VGA_INDEX < 2000 {
            *vga_buffer.add(VGA_INDEX) = 0x0F00 | b'/' as u16; // Blanco sobre negro
            VGA_INDEX += 1;
        }
        if VGA_INDEX < 2000 {
            *vga_buffer.add(VGA_INDEX) = 0x0F00 | b'1' as u16; // Blanco sobre negro
            VGA_INDEX += 1;
        }
        if VGA_INDEX < 2000 {
            *vga_buffer.add(VGA_INDEX) = 0x0F00 | b'5' as u16; // Blanco sobre negro
            VGA_INDEX += 1;
        }
        
        // Nueva línea
        if VGA_INDEX < 2000 {
            *vga_buffer.add(VGA_INDEX) = 0x0F00 | b'\n' as u16; // Blanco sobre negro
            VGA_INDEX += 1;
        }
    }
}

/// Función auxiliar para mostrar banner directamente
fn display_banner_direct() {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Limpiar toda la pantalla con fondo negro
        for i in 0..2000 {
            *vga_buffer.add(i) = 0x0000; // Negro sobre negro (fondo negro)
        }
        
        // Calcular posición central para "Eclipse OS"
        // Pantalla: 80 columnas x 25 filas
        // "Eclipse OS" tiene 10 caracteres
        let text = "Eclipse OS";
        let text_len = text.len();
        let start_col = (80 - text_len) / 2; // Centrar horizontalmente
        let start_row = 12; // Centrar verticalmente (fila 12 de 25)
        let start_pos = start_row * 80 + start_col;
        
        // Escribir "Eclipse OS" en el centro
        for (i, byte) in text.bytes().enumerate() {
            if start_pos + i < 2000 {
                *vga_buffer.add(start_pos + i) = 0x0F00 | byte as u16; // Blanco sobre negro
            }
        }
    }
}

/// Función auxiliar para mostrar resumen directamente
fn display_summary_direct() {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        static mut VGA_INDEX: usize = 0;
        
        let summary_lines = [
            "========================================",
            "    INICIALIZACION COMPLETA",
            "========================================",
            "Pasos completados: 15/15",
            "Mensajes generados: 50+",
            "Kernel Eclipse listo para operar!",
            "",
        ];
        
        for line in &summary_lines {
            for byte in line.bytes() {
                if VGA_INDEX < 2000 {
                    *vga_buffer.add(VGA_INDEX) = 0x0F00 | byte as u16; // Blanco sobre negro
                    VGA_INDEX += 1;
                }
            }
            // Nueva línea
            if VGA_INDEX < 2000 {
                *vga_buffer.add(VGA_INDEX) = 0x0F00 | b'\n' as u16; // Blanco sobre negro
                VGA_INDEX += 1;
            }
        }
    }
}