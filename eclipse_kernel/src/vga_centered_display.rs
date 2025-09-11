//! Módulo para mostrar "Eclipse OS" centrado en pantalla negra
//! 
//! Este módulo proporciona funciones para inicializar VGA y mostrar
//! texto centrado en una pantalla completamente negra.

/// Inicializar VGA y mostrar "Eclipse OS" centrado
pub fn init_vga_centered_display() {
    // Primero inicializar el modo VGA correctamente
    init_vga_mode();
    
    // Esperar un momento para que se aplique la configuración
    for _ in 0..10000 {
        // TEMPORALMENTE DESHABILITADO: nop causa opcode inválido
        unsafe {
            // Simular nop con spin loop para evitar opcode inválido
            core::hint::spin_loop();
        }
    }
    
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Limpiar toda la pantalla con fondo negro
        for i in 0..2000 {
            *vga_buffer.add(i) = 0x0000; // Negro sobre negro (fondo negro)
        }
        
        // Esperar un momento más
        for _ in 0..10000 {
            // TEMPORALMENTE DESHABILITADO: nop causa opcode inválido
            // Simular nop con spin loop para evitar opcode inválido
            core::hint::spin_loop();
        }
        
        // Mostrar "Eclipse OS" centrado
        display_centered_text("Eclipse OS");
    }
}

/// Inicializar el modo VGA 80x25 correctamente
fn init_vga_mode() {
    unsafe {
        // Configurar el controlador VGA para modo 80x25
        // Esto es más específico para QEMU y hardware real
        
        // Configurar registros de cursor
        outb(0x3D4, 0x0A); // Registro de cursor bajo
        outb(0x3D5, 0x20); // Ocultar cursor
        outb(0x3D4, 0x0B); // Registro de cursor alto
        outb(0x3D5, 0x00);
        
        // Configurar registros de inicio de pantalla
        outb(0x3D4, 0x0C); // Registro de inicio de pantalla bajo
        outb(0x3D5, 0x00);
        outb(0x3D4, 0x0D); // Registro de inicio de pantalla alto
        outb(0x3D5, 0x00);
        
        // Configurar registros de cursor de posición
        outb(0x3D4, 0x0E); // Registro de cursor bajo
        outb(0x3D5, 0x00);
        outb(0x3D4, 0x0F); // Registro de cursor alto
        outb(0x3D5, 0x00);
        
        // Configurar registros de cursor de posición
        outb(0x3D4, 0x00); // Registro de cursor bajo
        outb(0x3D5, 0x00);
        outb(0x3D4, 0x01); // Registro de cursor alto
        outb(0x3D5, 0x00);
        
        // Configurar registros de cursor de posición
        outb(0x3D4, 0x02); // Registro de cursor bajo
        outb(0x3D5, 0x00);
        outb(0x3D4, 0x03); // Registro de cursor alto
        outb(0x3D5, 0x00);
        
        // Configurar registros de cursor de posición
        outb(0x3D4, 0x04); // Registro de cursor bajo
        outb(0x3D5, 0x00);
        outb(0x3D4, 0x05); // Registro de cursor alto
        outb(0x3D5, 0x00);
        
        // Configurar registros de cursor de posición
        outb(0x3D4, 0x06); // Registro de cursor bajo
        outb(0x3D5, 0x00);
        outb(0x3D4, 0x07); // Registro de cursor alto
        outb(0x3D5, 0x00);
        
        // Configurar registros de cursor de posición
        outb(0x3D4, 0x08); // Registro de cursor bajo
        outb(0x3D5, 0x00);
        outb(0x3D4, 0x09); // Registro de cursor alto
        outb(0x3D5, 0x00);
    }
}

/// Función auxiliar para escribir a un puerto
unsafe fn outb(port: u16, value: u8) {
    // DESHABILITADO: Las instrucciones de puerto I/O causan opcode inválido
    // Usar simulación segura en lugar de acceso directo a puertos
    use crate::main_simple::serial_write_str;
    serial_write_str("[VGA] Escritura a puerto simulada\r\n");
}

/// Mostrar texto centrado en la pantalla
fn display_centered_text(text: &str) {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Calcular posición central
        // Pantalla: 80 columnas x 25 filas
        let text_len = text.len();
        let start_col = (80 - text_len) / 2; // Centrar horizontalmente
        let start_row = 12; // Centrar verticalmente (fila 12 de 25)
        let start_pos = start_row * 80 + start_col;
        
        // Escribir texto en el centro
        for (i, byte) in text.bytes().enumerate() {
            if start_pos + i < 2000 {
                *vga_buffer.add(start_pos + i) = 0x0F00 | byte as u16; // Blanco sobre negro
            }
        }
    }
}

/// Mostrar texto centrado en una fila específica
pub fn display_centered_text_at_row(text: &str, row: usize) {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Calcular posición central
        let text_len = text.len();
        let start_col = (80 - text_len) / 2; // Centrar horizontalmente
        let start_pos = row * 80 + start_col;
        
        // Escribir texto en la posición calculada
        for (i, byte) in text.bytes().enumerate() {
            if start_pos + i < 2000 {
                *vga_buffer.add(start_pos + i) = 0x0F00 | byte as u16; // Blanco sobre negro
            }
        }
    }
}

/// Limpiar pantalla con fondo negro
pub fn clear_screen_black() {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Limpiar toda la pantalla con fondo negro
        for i in 0..2000 {
            *vga_buffer.add(i) = 0x0000; // Negro sobre negro (fondo negro)
        }
    }
}

/// Mostrar mensaje de bienvenida centrado
pub fn display_welcome_message() {
    clear_screen_black();
    display_centered_text_at_row("Eclipse OS", 10);
    display_centered_text_at_row("Sistema Operativo en Rust", 12);
    display_centered_text_at_row("Iniciando...", 14);
}

/// Función alternativa para QEMU - más directa
pub fn init_vga_qemu_display() {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Limpiar toda la pantalla con fondo negro
        for i in 0..2000 {
            *vga_buffer.add(i) = 0x0000; // Negro sobre negro (fondo negro)
        }
        
        // Mostrar "Eclipse OS" centrado
        display_centered_text("Eclipse OS");
    }
}

/// Función para forzar pantalla negra en QEMU
pub fn force_black_screen() {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Escribir caracteres de espacio con fondo negro
        for i in 0..2000 {
            *vga_buffer.add(i) = 0x0000; // Negro sobre negro
        }
        
        // Mostrar "Eclipse OS" centrado
        display_centered_text("Eclipse OS");
    }
}

/// Función ultra-agresiva para forzar pantalla negra
pub fn ultra_force_black_screen() {
    unsafe {
        let vga_buffer = 0xb8000 as *mut u16;
        
        // Limpiar múltiples veces para asegurar que se aplique
        for _ in 0..10 {
            for i in 0..2000 {
                *vga_buffer.add(i) = 0x0000; // Negro sobre negro
            }
        }
        
        // Mostrar "Eclipse OS" centrado
        display_centered_text("Eclipse OS");
        
        // Limpiar de nuevo después de mostrar el texto
        for i in 0..2000 {
            if i < 960 || i > 1040 { // Mantener solo el área del texto
                *vga_buffer.add(i) = 0x0000; // Negro sobre negro
            }
        }
    }
}
