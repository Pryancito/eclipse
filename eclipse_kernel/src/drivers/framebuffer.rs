//! Driver de Framebuffer para Eclipse OS
//! 
//! Implementa un sistema de framebuffer robusto basado en las prácticas de Linux
//! para hardware real.

use core::ptr;
use core::mem;

/// Información del framebuffer obtenida del hardware
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,        // bytes per scanline
    pub bpp: u8,           // bits per pixel
    pub red_offset: u8,
    pub green_offset: u8,
    pub blue_offset: u8,
    pub alpha_offset: u8,
    pub red_length: u8,
    pub green_length: u8,
    pub blue_length: u8,
    pub alpha_length: u8,
    pub pixel_format: PixelFormat,
}

/// Formatos de pixel soportados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelFormat {
    RGB888,     // 24-bit RGB
    RGBA8888,   // 32-bit RGBA
    BGR888,     // 24-bit BGR
    BGRA8888,   // 32-bit BGRA
    RGB565,     // 16-bit RGB
    BGR565,     // 16-bit BGR
    Unknown,
}

impl PixelFormat {
    pub fn from_uefi_format(format: u32) -> Self {
        match format {
            0 => PixelFormat::RGB888,      // PixelRedGreenBlueReserved8BitPerColor
            1 => PixelFormat::BGR888,      // PixelBlueGreenRedReserved8BitPerColor
            2 => PixelFormat::RGB565,      // PixelBitMask
            3 => PixelFormat::BGR565,      // PixelBltOnly
            _ => PixelFormat::Unknown,
        }
    }
    
    pub fn bytes_per_pixel(&self) -> u8 {
        match self {
            PixelFormat::RGB888 => 3,
            PixelFormat::RGBA8888 => 4,
            PixelFormat::BGR888 => 3,
            PixelFormat::BGRA8888 => 4,
            PixelFormat::RGB565 => 2,
            PixelFormat::BGR565 => 2,
            PixelFormat::Unknown => 4, // Default to 4 bytes
        }
    }
}

/// Color RGBA
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 255)
    }
    
    /// Convertir color a pixel según el formato
    pub fn to_pixel(&self, format: PixelFormat) -> u32 {
        match format {
            PixelFormat::RGBA8888 => {
                ((self.a as u32) << 24) | 
                ((self.r as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.b as u32)
            },
            PixelFormat::BGRA8888 => {
                ((self.a as u32) << 24) | 
                ((self.b as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.r as u32)
            },
            PixelFormat::RGB888 => {
                ((self.r as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.b as u32)
            },
            PixelFormat::BGR888 => {
                ((self.b as u32) << 16) | 
                ((self.g as u32) << 8) | 
                (self.r as u32)
            },
            PixelFormat::RGB565 => {
                (((self.r as u32) >> 3) << 11) |
                (((self.g as u32) >> 2) << 5) |
                ((self.b as u32) >> 3)
            },
            PixelFormat::BGR565 => {
                (((self.b as u32) >> 3) << 11) |
                (((self.g as u32) >> 2) << 5) |
                ((self.r as u32) >> 3)
            },
            PixelFormat::Unknown => 0,
        }
    }
}

impl Color {
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const RED: Color = Color { r: 255, g: 0, b: 0, a: 255 };
    pub const GREEN: Color = Color { r: 0, g: 255, b: 0, a: 255 };
    pub const BLUE: Color = Color { r: 0, g: 0, b: 255, a: 255 };
    pub const YELLOW: Color = Color { r: 255, g: 255, b: 0, a: 255 };
    pub const CYAN: Color = Color { r: 0, g: 255, b: 255, a: 255 };
    pub const MAGENTA: Color = Color { r: 255, g: 0, b: 255, a: 255 };
    
    // Colores adicionales para el escritorio
    pub const DARK_BLUE: Color = Color { r: 0, g: 0, b: 128, a: 255 };
    pub const DARKER_BLUE: Color = Color { r: 0, g: 0, b: 64, a: 255 };
    pub const GRAY: Color = Color { r: 128, g: 128, b: 128, a: 255 };
    pub const DARK_GRAY: Color = Color { r: 64, g: 64, b: 64, a: 255 };
    pub const LIGHT_GRAY: Color = Color { r: 192, g: 192, b: 192, a: 255 };
}

/// Driver de Framebuffer
#[derive(Debug, Clone)]
pub struct FramebufferDriver {
    pub info: FramebufferInfo,
    buffer: *mut u8,
    is_initialized: bool,
}

impl FramebufferDriver {
    /// Crear nuevo driver de framebuffer
    pub fn new() -> Self {
        Self {
            info: FramebufferInfo {
                base_address: 0,
                size: 0,
                width: 0,
                height: 0,
                pitch: 0,
                bpp: 0,
                red_offset: 0,
                green_offset: 0,
                blue_offset: 0,
                alpha_offset: 0,
                red_length: 0,
                green_length: 0,
                blue_length: 0,
                alpha_length: 0,
                pixel_format: PixelFormat::Unknown,
            },
            buffer: ptr::null_mut(),
            is_initialized: false,
        }
    }
    
    /// Inicializar framebuffer con información de UEFI
    pub fn init_from_uefi(&mut self, 
                          base_address: u64,
                          width: u32,
                          height: u32,
                          pixels_per_scan_line: u32,
                          pixel_format: u32,
                          pixel_bitmask: u32) -> Result<(), &'static str> {
        
        // Validar parámetros básicos
        if base_address == 0 || width == 0 || height == 0 {
            return Err("Invalid framebuffer parameters");
        }
        
        // Determinar formato de pixel
        let format = PixelFormat::from_uefi_format(pixel_format);
        if format == PixelFormat::Unknown {
            return Err("Unsupported pixel format");
        }
        
        // Calcular pitch (bytes per scanline)
        let bytes_per_pixel = format.bytes_per_pixel();
        let pitch = if pixels_per_scan_line > 0 {
            pixels_per_scan_line * bytes_per_pixel as u32
        } else {
            width * bytes_per_pixel as u32
        };
        
        // Calcular tamaño total del buffer
        let size = (height * pitch) as u64;
        
        // Configurar información del framebuffer
        self.info = FramebufferInfo {
            base_address,
            size,
            width,
            height,
            pitch,
            bpp: bytes_per_pixel * 8,
            red_offset: 0,    // Se configurará según el formato
            green_offset: 0,
            blue_offset: 0,
            alpha_offset: 0,
            red_length: 8,
            green_length: 8,
            blue_length: 8,
            alpha_length: 8,
            pixel_format: format,
        };
        
        // Configurar offsets según el formato
        self.configure_pixel_offsets();
        
        // Mapear memoria del framebuffer
        self.buffer = base_address as *mut u8;
        
        // Validar que el mapeo sea válido
        if self.buffer.is_null() {
            return Err("Failed to map framebuffer memory");
        }
        
        self.is_initialized = true;
        
        // Limpiar pantalla
        self.clear_screen(Color::BLACK);
        
        Ok(())
    }
    
    /// Configurar offsets de pixel según el formato
    fn configure_pixel_offsets(&mut self) {
        match self.info.pixel_format {
            PixelFormat::RGBA8888 => {
                self.info.red_offset = 0;
                self.info.green_offset = 8;
                self.info.blue_offset = 16;
                self.info.alpha_offset = 24;
            },
            PixelFormat::BGRA8888 => {
                self.info.blue_offset = 0;
                self.info.green_offset = 8;
                self.info.red_offset = 16;
                self.info.alpha_offset = 24;
            },
            PixelFormat::RGB888 => {
                self.info.red_offset = 0;
                self.info.green_offset = 8;
                self.info.blue_offset = 16;
                self.info.alpha_offset = 0;
            },
            PixelFormat::BGR888 => {
                self.info.blue_offset = 0;
                self.info.green_offset = 8;
                self.info.red_offset = 16;
                self.info.alpha_offset = 0;
            },
            PixelFormat::RGB565 => {
                self.info.red_offset = 11;
                self.info.green_offset = 5;
                self.info.blue_offset = 0;
                self.info.alpha_offset = 0;
                self.info.red_length = 5;
                self.info.green_length = 6;
                self.info.blue_length = 5;
                self.info.alpha_length = 0;
            },
            PixelFormat::BGR565 => {
                self.info.blue_offset = 11;
                self.info.green_offset = 5;
                self.info.red_offset = 0;
                self.info.alpha_offset = 0;
                self.info.red_length = 5;
                self.info.green_length = 6;
                self.info.blue_length = 5;
                self.info.alpha_length = 0;
            },
            PixelFormat::Unknown => {},
        }
    }
    
    /// Verificar si el framebuffer está inicializado
    pub fn is_initialized(&self) -> bool {
        self.is_initialized
    }
    
    /// Obtener información del framebuffer
    pub fn get_info(&self) -> &FramebufferInfo {
        &self.info
    }
    
    /// Obtener puntero a pixel en coordenadas (x, y)
    fn get_pixel_ptr(&self, x: u32, y: u32) -> *mut u8 {
        if !self.is_initialized || x >= self.info.width || y >= self.info.height {
            return ptr::null_mut();
        }
        
        let bytes_per_pixel = self.info.pixel_format.bytes_per_pixel();
        let offset = (y * self.info.pitch + x * bytes_per_pixel as u32) as isize;
        
        unsafe { self.buffer.offset(offset) }
    }
    
    /// Escribir un pixel
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        let pixel_ptr = self.get_pixel_ptr(x, y);
        if !pixel_ptr.is_null() {
            let pixel_value = color.to_pixel(self.info.pixel_format);
            let bytes_per_pixel = self.info.pixel_format.bytes_per_pixel();
            
            unsafe {
                match bytes_per_pixel {
                    1 => *pixel_ptr = pixel_value as u8,
                    2 => {
                        let pixel_ptr_16 = pixel_ptr as *mut u16;
                        *pixel_ptr_16 = pixel_value as u16;
                    },
                    3 => {
                        *pixel_ptr = (pixel_value & 0xFF) as u8;
                        *pixel_ptr.offset(1) = ((pixel_value >> 8) & 0xFF) as u8;
                        *pixel_ptr.offset(2) = ((pixel_value >> 16) & 0xFF) as u8;
                    },
                    4 => {
                        let pixel_ptr_32 = pixel_ptr as *mut u32;
                        *pixel_ptr_32 = pixel_value;
                    },
                    _ => {},
                }
            }
        }
    }
    
    /// Leer un pixel
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        let pixel_ptr = self.get_pixel_ptr(x, y);
        if !pixel_ptr.is_null() {
            let bytes_per_pixel = self.info.pixel_format.bytes_per_pixel();
            
            unsafe {
                let pixel_value = match bytes_per_pixel {
                    1 => *pixel_ptr as u32,
                    2 => {
                        let pixel_ptr_16 = pixel_ptr as *mut u16;
                        *pixel_ptr_16 as u32
                    },
                    3 => {
                        (*pixel_ptr as u32) |
                        ((*pixel_ptr.offset(1) as u32) << 8) |
                        ((*pixel_ptr.offset(2) as u32) << 16)
                    },
                    4 => {
                        let pixel_ptr_32 = pixel_ptr as *mut u32;
                        *pixel_ptr_32
                    },
                    _ => 0,
                };
                
                self.pixel_to_color(pixel_value)
            }
        } else {
            Color::BLACK
        }
    }
    
    /// Convertir valor de pixel a color
    fn pixel_to_color(&self, pixel_value: u32) -> Color {
        match self.info.pixel_format {
            PixelFormat::RGBA8888 => Color::new(
                ((pixel_value >> self.info.red_offset) & ((1 << self.info.red_length) - 1)) as u8,
                ((pixel_value >> self.info.green_offset) & ((1 << self.info.green_length) - 1)) as u8,
                ((pixel_value >> self.info.blue_offset) & ((1 << self.info.blue_length) - 1)) as u8,
                ((pixel_value >> self.info.alpha_offset) & ((1 << self.info.alpha_length) - 1)) as u8,
            ),
            PixelFormat::BGRA8888 => Color::new(
                ((pixel_value >> self.info.red_offset) & ((1 << self.info.red_length) - 1)) as u8,
                ((pixel_value >> self.info.green_offset) & ((1 << self.info.green_length) - 1)) as u8,
                ((pixel_value >> self.info.blue_offset) & ((1 << self.info.blue_length) - 1)) as u8,
                ((pixel_value >> self.info.alpha_offset) & ((1 << self.info.alpha_length) - 1)) as u8,
            ),
            _ => Color::BLACK, // Implementar otros formatos si es necesario
        }
    }
    
    /// Llenar rectángulo con color
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        let end_x = core::cmp::min(x + width, self.info.width);
        let end_y = core::cmp::min(y + height, self.info.height);
        
        for py in y..end_y {
            for px in x..end_x {
                self.put_pixel(px, py, color);
            }
        }
    }
    
    /// Limpiar pantalla con color
    pub fn clear_screen(&mut self, color: Color) {
        self.fill_rect(0, 0, self.info.width, self.info.height, color);
    }
    
    /// Dibujar línea usando algoritmo de Bresenham
    pub fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, color: Color) {
        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        let sx = if x1 < x2 { 1 } else { -1 };
        let sy = if y1 < y2 { 1 } else { -1 };
        let mut err = dx - dy;
        
        let mut x = x1;
        let mut y = y1;
        
        loop {
            if x >= 0 && x < self.info.width as i32 && y >= 0 && y < self.info.height as i32 {
                self.put_pixel(x as u32, y as u32, color);
            }
            
            if x == x2 && y == y2 {
                break;
            }
            
            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }
    }
    
    /// Dibujar rectángulo
    pub fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        // Líneas horizontales
        self.draw_line(x as i32, y as i32, (x + width - 1) as i32, y as i32, color);
        self.draw_line(x as i32, (y + height - 1) as i32, (x + width - 1) as i32, (y + height - 1) as i32, color);
        
        // Líneas verticales
        self.draw_line(x as i32, y as i32, x as i32, (y + height - 1) as i32, color);
        self.draw_line((x + width - 1) as i32, y as i32, (x + width - 1) as i32, (y + height - 1) as i32, color);
    }
    
    /// Copiar región de framebuffer
    pub fn blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, width: u32, height: u32, src_fb: &FramebufferDriver) {
        let end_x = core::cmp::min(dst_x + width, self.info.width);
        let end_y = core::cmp::min(dst_y + height, self.info.height);
        
        for y in 0..height {
            for x in 0..width {
                let src_px = src_x + x;
                let src_py = src_y + y;
                let dst_px = dst_x + x;
                let dst_py = dst_y + y;
                
                if src_px < src_fb.info.width && src_py < src_fb.info.height &&
                   dst_px < self.info.width && dst_py < self.info.height {
                    let color = src_fb.get_pixel(src_px, src_py);
                    self.put_pixel(dst_px, dst_py, color);
                }
            }
        }
    }

    /// Escribir texto usando fuente simple de 8x16 pixels
    pub fn write_text(&mut self, x: u32, y: u32, text: &str, color: Color) {
        let mut current_x = x;
        let char_width = 8;
        let char_height = 16;

        for ch in text.chars() {
            if current_x + char_width > self.info.width {
                break; // Salir si no cabe más texto
            }

            self.draw_char(current_x, y, ch, color);
            current_x += char_width;
        }
    }

    pub fn write_line(&mut self, text: &str, color: Color) {
        self.write_text(10, 10, text, color);
    }
    
    /// Dibujar un carácter usando fuente simple
    fn draw_char(&mut self, x: u32, y: u32, ch: char, color: Color) {
        // Fuente simple 8x16 - representación básica de caracteres ASCII
        let bitmap = match ch {
            'A'..='Z' => self.get_char_bitmap(ch as u8 - b'A' + 10),
            'a'..='z' => self.get_char_bitmap(ch as u8 - b'a' + 10),
            '0'..='9' => self.get_char_bitmap(ch as u8 - b'0'),
            ' ' => [0; 16],
            _ => self.get_char_bitmap(36), // Carácter desconocido
        };

        for row in 0..16 {
            let bits = bitmap[row];
            for col in 0..8 {
                if (bits & (1 << (7 - col))) != 0 {
                    let px = x + col;
                    let py = y + row as u32;
                    if px < self.info.width && py < self.info.height {
                        self.put_pixel(px, py, color);
                    }
                }
            }
        }
    }

    /// Obtener bitmap de un carácter (fuente simple)
    fn get_char_bitmap(&self, index: u8) -> [u8; 16] {
        // Bitmaps simples de caracteres ASCII básicos
        // Cada fila representa 8 pixels horizontales
        match index {
            0 => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0
            1 => [0x18, 0x38, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 1
            2 => [0x3C, 0x66, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 2
            3 => [0x3C, 0x66, 0x06, 0x1C, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 3
            4 => [0x0C, 0x1C, 0x2C, 0x4C, 0x7E, 0x0C, 0x0C, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 4
            5 => [0x7E, 0x60, 0x60, 0x7C, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 5
            6 => [0x3C, 0x66, 0x60, 0x7C, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 6
            7 => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x30, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 7
            8 => [0x3C, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 8
            9 => [0x3C, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 9
            10 => [0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // A
            11 => [0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x66, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // B
            12 => [0x3C, 0x66, 0x60, 0x60, 0x60, 0x60, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // C
            13 => [0x7C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // D
            14 => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // E
            15 => [0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // F
            16 => [0x3C, 0x66, 0x60, 0x6E, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // G
            17 => [0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // H
            18 => [0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // I
            19 => [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x6C, 0x6C, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // J
            20 => [0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // K
            21 => [0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // L
            22 => [0x66, 0x7E, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // M
            23 => [0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // N
            24 => [0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // O
            25 => [0x7C, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // P
            26 => [0x3C, 0x66, 0x66, 0x66, 0x6E, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Q
            27 => [0x7C, 0x66, 0x66, 0x7C, 0x78, 0x6C, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // R
            28 => [0x3C, 0x66, 0x60, 0x3C, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // S
            29 => [0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // T
            30 => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // U
            31 => [0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // V
            32 => [0x66, 0x66, 0x66, 0x66, 0x66, 0x7E, 0x7E, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // W
            33 => [0x66, 0x66, 0x3C, 0x18, 0x18, 0x3C, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // X
            34 => [0x66, 0x66, 0x66, 0x3C, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Y
            35 => [0x7E, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Z
            _ => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // Espacio o desconocido
        }
    }
}

/// Framebuffer global del sistema
static mut SYSTEM_FRAMEBUFFER: Option<FramebufferDriver> = None;

/// Inicializar framebuffer del sistema
pub fn init_framebuffer(base_address: u64,
                       width: u32,
                       height: u32,
                       pixels_per_scan_line: u32,
                       pixel_format: u32,
                       pixel_bitmask: u32) -> Result<(), &'static str> {
    let mut fb = FramebufferDriver::new();
    fb.init_from_uefi(base_address, width, height, pixels_per_scan_line, pixel_format, pixel_bitmask)?;
    
    unsafe {
        SYSTEM_FRAMEBUFFER = Some(fb);
    }
    
    Ok(())
}

/// Obtener referencia al framebuffer del sistema
pub fn get_framebuffer() -> Option<&'static mut FramebufferDriver> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_mut()
    }
}

/// Verificar si el framebuffer está disponible
pub fn is_framebuffer_available() -> bool {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_ref().map_or(false, |fb| fb.is_initialized())
    }
}

/// Obtener información del framebuffer
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_ref().map(|fb| fb.info)
    }
}

/// Inicializar framebuffer con información UEFI
/// Esta función es llamada desde el punto de entrada UEFI
pub fn init_framebuffer_from_uefi(uefi_fb_info: &crate::entry_simple::FramebufferInfo) -> Result<(), &'static str> {
    // Convertir formato de pixel UEFI a nuestro formato
    let pixel_format = match uefi_fb_info.pixel_format {
        0 => 0, // RGB
        1 => 1, // BGR
        _ => 0, // Default to RGB
    };

    // Crear bitmask basado en las máscaras de color
    let pixel_bitmask = (uefi_fb_info.red_mask & 0xFF) |
                       ((uefi_fb_info.green_mask & 0xFF) << 8) |
                       ((uefi_fb_info.blue_mask & 0xFF) << 16) |
                       ((uefi_fb_info.reserved_mask & 0xFF) << 24);

    init_framebuffer(
        uefi_fb_info.base_address,
        uefi_fb_info.width,
        uefi_fb_info.height,
        uefi_fb_info.pixels_per_scan_line,
        pixel_format,
        pixel_bitmask
    )
}

/// Escribir texto en el framebuffer usando fuente simple
pub fn write_text(x: u32, y: u32, text: &str, color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.write_text(x, y, text, color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}

/// Limpiar pantalla del framebuffer
pub fn clear_screen(color: Color) -> Result<(), &'static str> {
    if let Some(fb) = get_framebuffer() {
        fb.clear_screen(color);
        Ok(())
    } else {
        Err("Framebuffer not initialized")
    }
}
