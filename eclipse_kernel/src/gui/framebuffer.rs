//! Framebuffer para el sistema gráfico
//! 
//! Gestiona el buffer de video para mostrar gráficos en pantalla

use core::ptr;

/// Información del framebuffer
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32, // bits per pixel
    pub red_shift: u32,
    pub green_shift: u32,
    pub blue_shift: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

impl FramebufferInfo {
    pub fn new() -> Self {
        Self {
            width: 1024,
            height: 768,
            pitch: 1024 * 4, // 4 bytes per pixel (RGBA)
            bpp: 32,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
            red_mask: 0xFF0000,
            green_mask: 0x00FF00,
            blue_mask: 0x0000FF,
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
    
    pub fn to_u32(&self) -> u32 {
        ((self.a as u32) << 24) | 
        ((self.r as u32) << 16) | 
        ((self.g as u32) << 8) | 
        (self.b as u32)
    }
}

/// Colores predefinidos
impl Color {
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const RED: Color = Color { r: 255, g: 0, b: 0, a: 255 };
    pub const GREEN: Color = Color { r: 0, g: 255, b: 0, a: 255 };
    pub const BLUE: Color = Color { r: 0, g: 0, b: 255, a: 255 };
    pub const YELLOW: Color = Color { r: 255, g: 255, b: 0, a: 255 };
    pub const CYAN: Color = Color { r: 0, g: 255, b: 255, a: 255 };
    pub const MAGENTA: Color = Color { r: 255, g: 0, b: 255, a: 255 };
    pub const GRAY: Color = Color { r: 128, g: 128, b: 128, a: 255 };
    pub const LIGHT_GRAY: Color = Color { r: 192, g: 192, b: 192, a: 255 };
    pub const DARK_GRAY: Color = Color { r: 64, g: 64, b: 64, a: 255 };
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
}

/// Punto 2D
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Rectángulo
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }
    
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x && 
        point.x < self.x + self.width as i32 &&
        point.y >= self.y && 
        point.y < self.y + self.height as i32
    }
    
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.width as i32 &&
        self.x + self.width as i32 > other.x &&
        self.y < other.y + other.height as i32 &&
        self.y + self.height as i32 > other.y
    }
}

/// Framebuffer principal
pub struct Framebuffer {
    pub info: FramebufferInfo,
    pub buffer: *mut u8,
    pub size: usize,
}

impl Framebuffer {
    /// Crear nuevo framebuffer
    pub fn new(info: FramebufferInfo) -> Self {
        let size = (info.height * info.pitch) as usize;
        
        // En un sistema real, esto sería una dirección de memoria mapeada del hardware
        // Para propósitos de demostración, usaremos 0xB8000 (VGA text mode buffer)
        // En un sistema gráfico real, esto sería algo como 0xFD000000
        let buffer = 0xB8000 as *mut u8;
        
        Self {
            info,
            buffer,
            size,
        }
    }
    
    /// Obtener puntero a pixel en coordenadas (x, y)
    fn get_pixel_ptr(&self, x: u32, y: u32) -> *mut u32 {
        if x >= self.info.width || y >= self.info.height {
            return ptr::null_mut();
        }
        
        let offset = (y * self.info.pitch + x * 4) as isize;
        unsafe { self.buffer.offset(offset) as *mut u32 }
    }
    
    /// Escribir un pixel
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        let pixel_ptr = self.get_pixel_ptr(x, y);
        if !pixel_ptr.is_null() {
            unsafe {
                *pixel_ptr = color.to_u32();
            }
        }
    }
    
    /// Leer un pixel
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        let pixel_ptr = self.get_pixel_ptr(x, y);
        if !pixel_ptr.is_null() {
            unsafe {
                let value = *pixel_ptr;
                Color::new(
                    ((value >> 16) & 0xFF) as u8,
                    ((value >> 8) & 0xFF) as u8,
                    (value & 0xFF) as u8,
                    ((value >> 24) & 0xFF) as u8,
                )
            }
        } else {
            Color::BLACK
        }
    }
    
    /// Llenar un rectángulo con color
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        let end_x = core::cmp::min((rect.x + rect.width as i32) as u32, self.info.width);
        let end_y = core::cmp::min((rect.y + rect.height as i32) as u32, self.info.height);
        
        let start_x = core::cmp::max(rect.x, 0) as u32;
        let start_y = core::cmp::max(rect.y, 0) as u32;
        
        for y in start_y..end_y {
            for x in start_x..end_x {
                self.put_pixel(x, y, color);
            }
        }
    }
    
    /// Limpiar toda la pantalla
    pub fn clear(&mut self, color: Color) {
        let rect = Rect::new(0, 0, self.info.width, self.info.height);
        self.fill_rect(rect, color);
    }
    
    /// Dibujar una línea
    pub fn draw_line(&mut self, start: Point, end: Point, color: Color) {
        let dx = (end.x - start.x).abs();
        let dy = (end.y - start.y).abs();
        let sx = if start.x < end.x { 1 } else { -1 };
        let sy = if start.y < end.y { 1 } else { -1 };
        let mut err = dx - dy;
        
        let mut x = start.x;
        let mut y = start.y;
        
        loop {
            if x >= 0 && x < self.info.width as i32 && y >= 0 && y < self.info.height as i32 {
                self.put_pixel(x as u32, y as u32, color);
            }
            
            if x == end.x && y == end.y {
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
    
    /// Dibujar el contorno de un rectángulo
    pub fn draw_rect(&mut self, rect: Rect, color: Color) {
        // Líneas horizontales
        let top_left = Point::new(rect.x, rect.y);
        let top_right = Point::new(rect.x + rect.width as i32 - 1, rect.y);
        let bottom_left = Point::new(rect.x, rect.y + rect.height as i32 - 1);
        let bottom_right = Point::new(rect.x + rect.width as i32 - 1, rect.y + rect.height as i32 - 1);
        
        self.draw_line(top_left, top_right, color);
        self.draw_line(bottom_left, bottom_right, color);
        
        // Líneas verticales
        self.draw_line(top_left, bottom_left, color);
        self.draw_line(top_right, bottom_right, color);
    }
    
    /// Copiar región de framebuffer
    pub fn blit(&mut self, src_rect: Rect, dst_point: Point, src_framebuffer: &Framebuffer) {
        let dst_x_end = core::cmp::min(
            (dst_point.x + src_rect.width as i32) as u32, 
            self.info.width
        );
        let dst_y_end = core::cmp::min(
            (dst_point.y + src_rect.height as i32) as u32, 
            self.info.height
        );
        
        let dst_x_start = core::cmp::max(dst_point.x, 0) as u32;
        let dst_y_start = core::cmp::max(dst_point.y, 0) as u32;
        
        for dst_y in dst_y_start..dst_y_end {
            for dst_x in dst_x_start..dst_x_end {
                let src_x = src_rect.x + (dst_x as i32 - dst_point.x);
                let src_y = src_rect.y + (dst_y as i32 - dst_point.y);
                
                if src_x >= 0 && src_x < src_framebuffer.info.width as i32 &&
                   src_y >= 0 && src_y < src_framebuffer.info.height as i32 {
                    let color = src_framebuffer.get_pixel(src_x as u32, src_y as u32);
                    self.put_pixel(dst_x, dst_y, color);
                }
            }
        }
    }
}

/// Framebuffer global del sistema
static mut SYSTEM_FRAMEBUFFER: Option<Framebuffer> = None;

/// Inicializar el framebuffer
pub fn init_framebuffer() {
    let info = FramebufferInfo::new();
    let framebuffer = Framebuffer::new(info);
    
    unsafe {
        SYSTEM_FRAMEBUFFER = Some(framebuffer);
    }
}

/// Obtener referencia al framebuffer del sistema
pub fn get_framebuffer() -> Option<&'static mut Framebuffer> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_mut()
    }
}

/// Obtener información del framebuffer
pub fn get_framebuffer_info() -> Option<FramebufferInfo> {
    unsafe {
        SYSTEM_FRAMEBUFFER.as_ref().map(|fb| fb.info)
    }
}
