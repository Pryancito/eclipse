//! Driver de pantalla mejorado para Eclipse OS
//! 
//! Soporta tanto VGA text mode como framebuffer UEFI

use core::fmt::Write;

// Información del framebuffer
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub pixel_format: u32,
}

// Driver de pantalla unificado
pub struct DisplayDriver {
    vga_enabled: bool,
    framebuffer_enabled: bool,
    framebuffer_info: FramebufferInfo,
    current_x: u32,
    current_y: u32,
    font_width: u32,
    font_height: u32,
}

impl DisplayDriver {
    pub const fn new() -> Self {
        Self {
            vga_enabled: true,
            framebuffer_enabled: false,
            framebuffer_info: FramebufferInfo {
                base_address: 0,
                width: 0,
                height: 0,
                pixels_per_scan_line: 0,
                pixel_format: 0,
            },
            current_x: 0,
            current_y: 0,
            font_width: 8,
            font_height: 16,
        }
    }

    pub fn init(&mut self, framebuffer_base: u64, framebuffer_width: u32, framebuffer_height: u32, framebuffer_pixels_per_scan_line: u32, framebuffer_pixel_format: u32) {
        self.framebuffer_info = FramebufferInfo {
            base_address: framebuffer_base,
            width: framebuffer_width,
            height: framebuffer_height,
            pixels_per_scan_line: framebuffer_pixels_per_scan_line,
            pixel_format: framebuffer_pixel_format,
        };

        // Si tenemos información válida del framebuffer, usarlo
        if framebuffer_base != 0 && framebuffer_width > 0 && framebuffer_height > 0 {
            self.framebuffer_enabled = true;
            self.vga_enabled = false; // Priorizar framebuffer sobre VGA
            self.clear_screen();
        } else {
            // Usar VGA como fallback
            self.framebuffer_enabled = false;
            self.vga_enabled = true;
        }
    }

    pub fn clear_screen(&mut self) {
        if self.framebuffer_enabled {
            self.clear_framebuffer();
        } else if self.vga_enabled {
            self.clear_vga();
        }
        self.current_x = 0;
        self.current_y = 0;
    }

    fn clear_framebuffer(&self) {
        if self.framebuffer_info.base_address == 0 {
            return;
        }

        unsafe {
            let buffer = core::slice::from_raw_parts_mut(
                self.framebuffer_info.base_address as *mut u32,
                (self.framebuffer_info.width * self.framebuffer_info.height) as usize,
            );
            
            // Limpiar con color negro (0x000000)
            for pixel in buffer.iter_mut() {
                *pixel = 0x000000;
            }
        }
    }

    fn clear_vga(&self) {
        unsafe {
            let vga_buffer = core::slice::from_raw_parts_mut(0xB8000 as *mut u16, 80 * 25);
            for cell in vga_buffer.iter_mut() {
                *cell = 0x0720; // Blanco sobre negro, espacio
            }
        }
    }

    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            self.write_char(byte as char);
        }
    }

    pub fn write_char(&mut self, c: char) {
        if c == '\n' {
            self.new_line();
            return;
        }

        if self.framebuffer_enabled {
            self.write_char_framebuffer(c);
        } else if self.vga_enabled {
            self.write_char_vga(c);
        }
    }

    fn write_char_framebuffer(&mut self, c: char) {
        if self.framebuffer_info.base_address == 0 {
            return;
        }

        // Verificar límites con protección contra división por cero
        if self.font_height > 0 && self.framebuffer_info.height > 0 {
            if self.current_y >= self.framebuffer_info.height / self.font_height {
                self.scroll_framebuffer();
            }
        }

        // Dibujar carácter en framebuffer (implementación básica)
        let char_code = c as u8;
        let x = self.current_x * self.font_width;
        let y = self.current_y * self.font_height;

        // Dibujar un rectángulo simple para cada carácter
        self.draw_rect(x, y, self.font_width, self.font_height, 0xFFFFFF); // Blanco

        self.current_x += 1;
        // Verificar que el framebuffer esté inicializado y evitar división por cero
        if self.framebuffer_enabled && self.font_width > 0 && self.framebuffer_info.width > 0 {
            if self.current_x >= self.framebuffer_info.width / self.font_width {
                self.new_line();
            }
        }
    }

    fn write_char_vga(&mut self, c: char) {
        unsafe {
            let vga_buffer = core::slice::from_raw_parts_mut(0xB8000 as *mut u16, 80 * 25);
            let index = (self.current_y * 80 + self.current_x) as usize;
            
            if index < vga_buffer.len() {
                vga_buffer[index] = 0x0700 | (c as u16); // Blanco sobre negro
            }
        }

        self.current_x += 1;
        if self.current_x >= 80 {
            self.new_line();
        }
    }

    fn new_line(&mut self) {
        self.current_x = 0;
        self.current_y += 1;
        
        if self.framebuffer_enabled {
            // Verificar que no haya división por cero
            if self.font_height > 0 && self.framebuffer_info.height > 0 {
                if self.current_y >= self.framebuffer_info.height / self.font_height {
                    self.scroll_framebuffer();
                }
            }
        } else if self.vga_enabled {
            if self.current_y >= 25 {
                self.scroll_vga();
            }
        }
    }

    fn scroll_framebuffer(&mut self) {
        if self.framebuffer_info.base_address == 0 {
            return;
        }

        unsafe {
            let buffer = core::slice::from_raw_parts_mut(
                self.framebuffer_info.base_address as *mut u32,
                (self.framebuffer_info.width * self.framebuffer_info.height) as usize,
            );
            
            let line_size = self.framebuffer_info.pixels_per_scan_line as usize;
            let scroll_lines = self.font_height as usize;
            
            // Mover líneas hacia arriba
            for y in scroll_lines..(self.framebuffer_info.height as usize) {
                let src_start = y * line_size;
                let dst_start = (y - scroll_lines) * line_size;
                
                if src_start + line_size <= buffer.len() && dst_start + line_size <= buffer.len() {
                    buffer.copy_within(src_start..src_start + line_size, dst_start);
                }
            }
            
            // Limpiar la última línea
            let last_line_start = (self.framebuffer_info.height as usize - scroll_lines) * line_size;
            for i in 0..(scroll_lines * line_size) {
                if last_line_start + i < buffer.len() {
                    buffer[last_line_start + i] = 0x000000; // Negro
                }
            }
        }
        
        // Calcular la nueva posición Y con protección contra división por cero
        if self.font_height > 0 && self.framebuffer_info.height > 0 {
            self.current_y = (self.framebuffer_info.height / self.font_height) - 1;
        } else {
            self.current_y = 0; // Valor por defecto si hay problemas
        }
    }

    fn scroll_vga(&mut self) {
        unsafe {
            let vga_buffer = core::slice::from_raw_parts_mut(0xB8000 as *mut u16, 80 * 25);
            
            // Mover líneas hacia arriba
            for i in 0..(80 * 24) {
                vga_buffer[i] = vga_buffer[i + 80];
            }
            
            // Limpiar la última línea
            for i in (80 * 24)..(80 * 25) {
                vga_buffer[i] = 0x0720; // Blanco sobre negro, espacio
            }
        }
        
        self.current_y = 24;
    }

    fn draw_rect(&self, x: u32, y: u32, width: u32, height: u32, color: u32) {
        if self.framebuffer_info.base_address == 0 {
            return;
        }

        unsafe {
            let buffer = core::slice::from_raw_parts_mut(
                self.framebuffer_info.base_address as *mut u32,
                (self.framebuffer_info.width * self.framebuffer_info.height) as usize,
            );
            
            let line_size = self.framebuffer_info.pixels_per_scan_line as usize;
            
            for dy in 0..height {
                for dx in 0..width {
                    let pixel_x = x + dx;
                    let pixel_y = y + dy;
                    
                    if pixel_x < self.framebuffer_info.width && pixel_y < self.framebuffer_info.height {
                        let index = (pixel_y as usize * line_size + pixel_x as usize) as usize;
                        if index < buffer.len() {
                            buffer[index] = color;
                        }
                    }
                }
            }
        }
    }

    pub fn set_color(&mut self, fg: u32, bg: u32) {
        // Por ahora, solo implementar para VGA
        if self.vga_enabled {
            // Implementación básica - en un driver real esto sería más complejo
        }
    }

    pub fn get_info(&self) -> &FramebufferInfo {
        &self.framebuffer_info
    }

    pub fn is_framebuffer_enabled(&self) -> bool {
        self.framebuffer_enabled
    }

    pub fn is_vga_enabled(&self) -> bool {
        self.vga_enabled
    }
}

// Implementar Write para compatibilidad con print!
impl Write for DisplayDriver {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

// Variable global del driver de pantalla
pub static mut DISPLAY: DisplayDriver = DisplayDriver::new();
