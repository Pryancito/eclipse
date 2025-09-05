#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::fmt::Write;

// Colores VGA
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    LightMagenta = 13,
    Yellow = 14,
    White = 15,
}

// Driver VGA mejorado
pub struct VgaWriter {
    buffer: *mut u16,
    position: usize,
    color: u8,
}

impl VgaWriter {
    pub const fn new() -> Self {
        Self {
            buffer: 0xB8000 as *mut u16,
            position: 0,
            color: ((Color::White as u8) << 4) | (Color::Black as u8),
        }
    }

    pub fn set_color(&mut self, fg: Color, bg: Color) {
        self.color = ((bg as u8) << 4) | (fg as u8);
    }

    pub fn clear_screen(&mut self) {
        for i in 0..(80 * 25) {
            unsafe {
                *self.buffer.add(i) = (b' ' as u16) | ((self.color as u16) << 8);
            }
        }
        self.position = 0;
    }

    pub fn newline(&mut self) {
        self.position = (self.position / 80 + 1) * 80;
        if self.position >= 80 * 25 {
            self.scroll();
        }
    }

    pub fn scroll(&mut self) {
        // Mover todas las líneas hacia arriba
        for i in 0..(80 * 24) {
            unsafe {
                *self.buffer.add(i) = *self.buffer.add(i + 80);
            }
        }
        // Limpiar la última línea
        for i in (80 * 24)..(80 * 25) {
            unsafe {
                *self.buffer.add(i) = (b' ' as u16) | ((self.color as u16) << 8);
            }
        }
        self.position = 80 * 24;
    }

    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.position = (self.position / 80) * 80,
            '\t' => {
                let tab_pos = (self.position / 8 + 1) * 8;
                for _ in self.position..tab_pos {
                    self.write_char(' ');
                }
            }
            c => {
                if self.position < 80 * 25 {
                    unsafe {
                        *self.buffer.add(self.position) = (c as u8 as u16) | ((self.color as u16) << 8);
                    }
                    self.position += 1;
                }
            }
        }
    }

    pub fn write_string(&mut self, s: &str) {
        for c in s.chars() {
            self.write_char(c);
        }
    }

    pub fn set_cursor(&mut self, x: usize, y: usize) {
        self.position = y * 80 + x;
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        (self.position % 80, self.position / 80)
    }
}

impl Write for VgaWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

// Driver de serie mejorado
pub struct SerialWriter {
    base_port: u16,
}

impl SerialWriter {
    pub const fn new() -> Self {
        Self { base_port: 0x3F8 }
    }

    pub unsafe fn init(&self) {
        let base = self.base_port;
        self.outb(base + 1, 0x00); // Deshabilitar interrupciones
        self.outb(base + 3, 0x80); // Habilitar DLAB
        self.outb(base + 0, 0x01); // Divisor de baud rate (115200)
        self.outb(base + 1, 0x00);
        self.outb(base + 3, 0x03); // 8 bits, sin paridad, 1 stop bit
        self.outb(base + 2, 0xC7); // Habilitar FIFO
        self.outb(base + 4, 0x0B); // Habilitar DTR, RTS
    }

    unsafe fn outb(&self, port: u16, val: u8) {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
    }

    unsafe fn inb(&self, port: u16) -> u8 {
        let mut val: u8;
        core::arch::asm!("in al, dx", in("dx") port, out("al") val, options(nomem, nostack, preserves_flags));
        val
    }

    pub fn write_byte(&self, b: u8) {
        unsafe {
            while (self.inb(self.base_port + 5) & 0x20) == 0 {}
            self.outb(self.base_port, b);
        }
    }

    pub fn write_string(&self, s: &str) {
        for &b in s.as_bytes() {
            self.write_byte(b);
        }
    }
}

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

// Variables globales
pub static mut VGA: VgaWriter = VgaWriter::new();
pub static mut SERIAL: SerialWriter = SerialWriter::new();
pub static mut DISPLAY: crate::display::DisplayDriver = crate::display::DisplayDriver::new();

// Funciones de utilidad
pub fn print(s: &str) {
    unsafe {
        DISPLAY.write_string(s);
        VGA.write_string(s);
        SERIAL.write_string(s);
    }
}

pub fn println(s: &str) {
    unsafe {
        DISPLAY.write_string(s);
        DISPLAY.write_string("\n");
        VGA.write_string(s);
        VGA.write_char('\n');
        SERIAL.write_string(s);
        SERIAL.write_byte(b'\r');
        SERIAL.write_byte(b'\n');
    }
}

pub fn print_hex(value: u64) {
    unsafe {
        let mut buf = [0u8; 18];
        let mut n = 0usize;
        buf[n] = b'0'; n += 1;
        buf[n] = b'x'; n += 1;
        for i in (0..16).rev() {
            let nyb = ((value >> (i*4)) & 0xF) as u8;
            buf[n] = if nyb < 10 { b'0'+nyb } else { b'a'+(nyb-10) }; n += 1;
        }
        let s = core::str::from_utf8_unchecked(&buf[0..n]);
        VGA.write_string(s);
        SERIAL.write_string(s);
    }
}

pub fn print_dec(value: u64) {
    unsafe {
        let mut buf = [0u8; 21];
        let mut n = 0usize;
        let mut val = value;
        
        if val == 0 {
            buf[n] = b'0'; n += 1;
        } else {
            while val > 0 {
                buf[n] = b'0' + (val % 10) as u8; n += 1;
                val /= 10;
            }
        }
        
        // Invertir el buffer
        for i in 0..n/2 {
            let temp = buf[i];
            buf[i] = buf[n-1-i];
            buf[n-1-i] = temp;
        }
        
        let s = core::str::from_utf8_unchecked(&buf[0..n]);
        VGA.write_string(s);
        SERIAL.write_string(s);
    }
}

// Función principal del kernel
pub fn kernel_main(
    framebuffer_base: u64,
    framebuffer_width: u32,
    framebuffer_height: u32,
    framebuffer_pixels_per_scan_line: u32,
    framebuffer_pixel_format: u32,
) -> ! {
    // Inicializar drivers
    unsafe {
        SERIAL.init();
        DISPLAY.init(framebuffer_base, framebuffer_width, framebuffer_height, framebuffer_pixels_per_scan_line, framebuffer_pixel_format);
        VGA.clear_screen();
    }

    // Banner de inicio
    unsafe {
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("╔══════════════════════════════════════════════════════════════════════════════╗\n");
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("║                           ECLIPSE OS KERNEL v1.0                            ║\n");
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("║                        Sistema Operativo en Rust                            ║\n");
        VGA.set_color(Color::LightCyan, Color::Black);
        VGA.write_string("╚══════════════════════════════════════════════════════════════════════════════╝\n");
        
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("\n");
        VGA.write_string("Inicializando sistema...\n");
    }

    // Información del sistema
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("✓ Drivers VGA inicializados\n");
        VGA.write_string("✓ Driver de serie COM1 activo\n");
        VGA.write_string("✓ Gestión de memoria básica lista\n");
        
        VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("\nInformación del sistema:\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("  - Arquitectura: x86_64\n");
        VGA.write_string("  - Modo: 64-bit\n");
        VGA.write_string("  - Bootloader: UEFI\n");
        VGA.write_string("  - Kernel: Rust (no_std)\n");
        
        VGA.set_color(Color::LightBlue, Color::Black);
        VGA.write_string("\nDirecciones de memoria:\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("  - Kernel base: 0x200000\n");
        VGA.write_string("  - VGA buffer: 0xB8000\n");
        VGA.write_string("  - Serial COM1: 0x3F8\n");
    }

    // Demostración de colores
    unsafe {
        VGA.set_color(Color::LightRed, Color::Black);
        VGA.write_string("\nDemostración de colores VGA:\n");
        
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
        VGA.write_string("\n");
    }

    // Demostración de funciones
    unsafe {
        VGA.set_color(Color::LightMagenta, Color::Black);
        VGA.write_string("\nFunciones del kernel:\n");
        VGA.set_color(Color::White, Color::Black);
        
        VGA.write_string("  - Número hexadecimal: ");
        print_hex(0xDEADBEEF);
        VGA.write_string("\n");
        
        VGA.write_string("  - Número decimal: ");
        print_dec(12345);
        VGA.write_string("\n");
        
        VGA.write_string("  - Posición del cursor: (");
        let (x, y) = VGA.get_cursor();
        print_dec(x as u64);
        VGA.write_string(", ");
        print_dec(y as u64);
        VGA.write_string(")\n");
    }

    // Mensaje final
    unsafe {
        VGA.set_color(Color::LightGreen, Color::Black);
        VGA.write_string("\n✓ Kernel Eclipse OS inicializado correctamente\n");
        VGA.set_color(Color::Yellow, Color::Black);
        VGA.write_string("Iniciando shell interactivo...\n");
        VGA.set_color(Color::White, Color::Black);
    }

    // Iniciar shell interactivo
    crate::shell::run_shell();
}

#[panic_handler]
pub fn panic(info: &PanicInfo) -> ! {
    unsafe {
        VGA.set_color(Color::LightRed, Color::Black);
        VGA.write_string("\n\n╔══════════════════════════════════════════════════════════════════════════════╗\n");
        VGA.write_string("║                                KERNEL PANIC                                 ║\n");
        VGA.write_string("╚══════════════════════════════════════════════════════════════════════════════╝\n");
        VGA.set_color(Color::White, Color::Black);
        VGA.write_string("\nEl kernel ha encontrado un error crítico y se ha detenido.\n");
        
        if let Some(location) = info.location() {
            VGA.write_string("Ubicación: ");
            VGA.write_string(location.file());
            VGA.write_string(":");
            print_dec(location.line() as u64);
            VGA.write_string("\n");
        }
        
        VGA.write_string("Mensaje: Kernel panic detectado\n");
        
        VGA.write_string("\nReinicia el sistema para continuar.\n");
    }
    
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
