//! ReactOS Rust Kernel - Graphics System
//!
//! Sistema de gráficos del kernel.


/// Modos de video soportados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VideoMode {
    VgaText80x25,
    VgaGraphics320x200,
    VgaGraphics640x480,
    VgaGraphics800x600,
    VgaGraphics1024x768,
}

/// Colores básicos
#[derive(Debug, Clone, Copy, PartialEq)]
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

/// Estructura para representar un punto
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

/// Estructura para representar un rectángulo
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rectangle {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Estructura para representar una ventana
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub id: u32,
    pub title: [u8; 64],
    pub rect: Rectangle,
    pub visible: bool,
    pub focused: bool,
    pub background_color: Color,
    pub border_color: Color,
    pub title_color: Color,
    pub z_order: u32,
}

/// Driver VGA
pub struct VgaDriver {
    pub current_mode: VideoMode,
    pub width: u32,
    pub height: u32,
    pub buffer: *mut u8,
    pub buffer_size: usize,
    pub cursor_x: u32,
    pub cursor_y: u32,
    pub foreground_color: Color,
    pub background_color: Color,
}

/// Sistema de ventanas
pub struct WindowManager {
    pub windows: [Option<Window>; 32],
    pub next_window_id: u32,
    pub active_window: Option<u32>,
    pub desktop_background: Color,
    pub taskbar_height: u32,
}

/// Sistema de fuentes
pub struct FontSystem {
    pub current_font: Font,
    pub font_size: u32,
}

/// Fuente básica
#[derive(Debug, Clone, Copy)]
pub struct Font {
    pub width: u32,
    pub height: u32,
    pub data: [u8; 4096], // 16x16 caracteres * 16 bytes por carácter
}

impl VgaDriver {
    /// Crear un nuevo driver VGA
    pub fn new() -> Self {
        Self {
            current_mode: VideoMode::VgaText80x25,
            width: 80,
            height: 25,
            buffer: 0xB8000 as *mut u8, // Dirección VGA text mode
            buffer_size: 4000, // 80 * 25 * 2
            cursor_x: 0,
            cursor_y: 0,
            foreground_color: Color::LightGray,
            background_color: Color::Black,
        }
    }
    
    /// Inicializar driver VGA
    pub fn init(&mut self) {
        self.set_mode(VideoMode::VgaText80x25);
        self.clear_screen();
        self.hide_cursor();
    }
    
    /// Establecer modo de video
    pub fn set_mode(&mut self, mode: VideoMode) {
        self.current_mode = mode;
        
        match mode {
            VideoMode::VgaText80x25 => {
                self.width = 80;
                self.height = 25;
                self.buffer = 0xB8000 as *mut u8;
                self.buffer_size = 4000;
            },
            VideoMode::VgaGraphics320x200 => {
                self.width = 320;
                self.height = 200;
                self.buffer = 0xA0000 as *mut u8;
                self.buffer_size = 64000;
            },
            VideoMode::VgaGraphics640x480 => {
                self.width = 640;
                self.height = 480;
                self.buffer = 0xA0000 as *mut u8;
                self.buffer_size = 307200;
            },
            VideoMode::VgaGraphics800x600 => {
                self.width = 800;
                self.height = 600;
                self.buffer = 0xA0000 as *mut u8;
                self.buffer_size = 480000;
            },
            VideoMode::VgaGraphics1024x768 => {
                self.width = 1024;
                self.height = 768;
                self.buffer = 0xA0000 as *mut u8;
                self.buffer_size = 786432;
            },
        }
    }
    
    /// Limpiar pantalla
    pub fn clear_screen(&mut self) {
        match self.current_mode {
            VideoMode::VgaText80x25 => {
                let color_byte = ((self.background_color as u8) << 4) | (self.foreground_color as u8);
                unsafe {
                    for i in 0..self.buffer_size {
                        *self.buffer.add(i) = if i % 2 == 0 { b' ' } else { color_byte };
                    }
                }
            },
            _ => {
                // Modo gráfico - llenar con color de fondo
                unsafe {
                    for i in 0..self.buffer_size {
                        *self.buffer.add(i) = self.background_color as u8;
                    }
                }
            },
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
    }
    
    /// Establecer colores
    pub fn set_colors(&mut self, foreground: Color, background: Color) {
        self.foreground_color = foreground;
        self.background_color = background;
    }
    
    /// Escribir carácter en pantalla
    pub fn put_char(&mut self, ch: u8) {
        match self.current_mode {
            VideoMode::VgaText80x25 => {
                if ch == b'\n' {
                    self.cursor_x = 0;
                    self.cursor_y += 1;
                } else if ch == b'\r' {
                    self.cursor_x = 0;
                } else if ch == b'\t' {
                    self.cursor_x = (self.cursor_x + 8) & !7;
                } else {
                    let color_byte = ((self.background_color as u8) << 4) | (self.foreground_color as u8);
                    let offset = (self.cursor_y * self.width + self.cursor_x) as usize * 2;
                    
                    if offset + 1 < self.buffer_size {
                        unsafe {
                            *self.buffer.add(offset) = ch;
                            *self.buffer.add(offset + 1) = color_byte;
                        }
                    }
                    
                    self.cursor_x += 1;
                }
                
                // Scroll si es necesario
                if self.cursor_y >= self.height {
                    self.scroll_up();
                    self.cursor_y = self.height - 1;
                }
                
                // Wrap horizontal
                if self.cursor_x >= self.width {
                    self.cursor_x = 0;
                    self.cursor_y += 1;
                }
            },
            _ => {
                // Modo gráfico - dibujar carácter
                self.draw_char_graphics(ch, self.cursor_x, self.cursor_y);
                self.cursor_x += 8; // Ancho de carácter
                if self.cursor_x >= self.width {
                    self.cursor_x = 0;
                    self.cursor_y += 16; // Alto de carácter
                }
            },
        }
    }
    
    /// Escribir string en pantalla
    pub fn put_string(&mut self, s: &str) {
        for byte in s.bytes() {
            self.put_char(byte);
        }
    }
    
    /// Scroll hacia arriba
    fn scroll_up(&mut self) {
        match self.current_mode {
            VideoMode::VgaText80x25 => {
                unsafe {
                    // Mover líneas hacia arriba
                    for y in 1..self.height {
                        for x in 0..self.width {
                            let src_offset = (y * self.width + x) as usize * 2;
                            let dst_offset = ((y - 1) * self.width + x) as usize * 2;
                            if src_offset + 1 < self.buffer_size && dst_offset + 1 < self.buffer_size {
                                *self.buffer.add(dst_offset) = *self.buffer.add(src_offset);
                                *self.buffer.add(dst_offset + 1) = *self.buffer.add(src_offset + 1);
                            }
                        }
                    }
                    
                    // Limpiar última línea
                    let color_byte = ((self.background_color as u8) << 4) | (self.foreground_color as u8);
                    for x in 0..self.width {
                        let offset = ((self.height - 1) * self.width + x) as usize * 2;
                        if offset + 1 < self.buffer_size {
                            *self.buffer.add(offset) = b' ';
                            *self.buffer.add(offset + 1) = color_byte;
                        }
                    }
                }
            },
            _ => {
                // Modo gráfico - scroll
                unsafe {
                    let line_size = self.width as usize;
                    for y in 1..self.height {
                        for x in 0..self.width {
                            let src_offset = (y * self.width + x) as usize;
                            let dst_offset = ((y - 1) * self.width + x) as usize;
                            if src_offset < self.buffer_size && dst_offset < self.buffer_size {
                                *self.buffer.add(dst_offset) = *self.buffer.add(src_offset);
                            }
                        }
                    }
                    
                    // Limpiar última línea
                    for x in 0..self.width {
                        let offset = ((self.height - 1) * self.width + x) as usize;
                        if offset < self.buffer_size {
                            *self.buffer.add(offset) = self.background_color as u8;
                        }
                    }
                }
            },
        }
    }
    
    /// Dibujar carácter en modo gráfico
    fn draw_char_graphics(&mut self, ch: u8, x: u32, y: u32) {
        // Implementación básica de fuente 8x16
        let font_data = self.get_font_data(ch);
        
        for row in 0..16 {
            let byte = font_data[row as usize];
            for col in 0..8 {
                if (byte & (0x80 >> col)) != 0 {
                    let pixel_x = x + col;
                    let pixel_y = y + row;
                    if pixel_x < self.width && pixel_y < self.height {
                        self.set_pixel(pixel_x, pixel_y, self.foreground_color);
                    }
                }
            }
        }
    }

    fn get_font_data(&self, ch: u8) -> [u8; 16] {
        // Fuente básica 8x16 - solo algunos caracteres
        match ch {
            b'A' => [0x00, 0x00, 0x18, 0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
            b'B' => [0x00, 0x00, 0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x66, 0x66, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
            b'C' => [0x00, 0x00, 0x3C, 0x66, 0x60, 0x60, 0x60, 0x60, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
            b' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            _ => [0x00, 0x00, 0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00], // Carácter por defecto
        }
    }
    
    /// Establecer pixel en modo gráfico
    pub fn set_pixel(&mut self, x: u32, y: u32, color: Color) {
        if x < self.width && y < self.height {
            let offset = (y * self.width + x) as usize;
            if offset < self.buffer_size {
                unsafe {
                    *self.buffer.add(offset) = color as u8;
                }
            }
        }
    }
    
    /// Obtener pixel en modo gráfico
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        if x < self.width && y < self.height {
            let offset = (y * self.width + x) as usize;
            if offset < self.buffer_size {
                unsafe {
                    return Color::from_u8(*self.buffer.add(offset));
                }
            }
        }
        Color::Black
    }
    
    /// Dibujar línea
    pub fn draw_line(&mut self, x1: u32, y1: u32, x2: u32, y2: u32, color: Color) {
        let dx = (x2 as i32 - x1 as i32).abs() as u32;
        let dy = (y2 as i32 - y1 as i32).abs() as u32;
        let sx = if x1 < x2 { 1 } else { -1 };
        let sy = if y1 < y2 { 1 } else { -1 };
        let mut err = dx as i32 - dy as i32;
        
        let mut x = x1 as i32;
        let mut y = y1 as i32;
        
        loop {
            self.set_pixel(x as u32, y as u32, color);
            
            if x == x2 as i32 && y == y2 as i32 {
                break;
            }
            
            let e2 = 2 * err;
            if e2 > -(dy as i32) {
                err -= dy as i32;
                x += sx;
            }
            if e2 < dx as i32 {
                err += dx as i32;
                y += sy;
            }
        }
    }
    
    /// Dibujar rectángulo
    pub fn draw_rectangle(&mut self, rect: Rectangle, color: Color) {
        // Líneas horizontales
        self.draw_line(rect.x, rect.y, rect.x + rect.width - 1, rect.y, color);
        self.draw_line(rect.x, rect.y + rect.height - 1, rect.x + rect.width - 1, rect.y + rect.height - 1, color);
        
        // Líneas verticales
        self.draw_line(rect.x, rect.y, rect.x, rect.y + rect.height - 1, color);
        self.draw_line(rect.x + rect.width - 1, rect.y, rect.x + rect.width - 1, rect.y + rect.height - 1, color);
    }
    
    /// Rellenar rectángulo
    pub fn fill_rectangle(&mut self, rect: Rectangle, color: Color) {
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                self.set_pixel(x, y, color);
            }
        }
    }
    
    /// Mostrar cursor
    pub fn show_cursor(&mut self) {
        // TODO: Implementar cursor visible
    }
    
    /// Ocultar cursor
    pub fn hide_cursor(&mut self) {
        // TODO: Implementar cursor invisible
    }
    
    /// Establecer posición del cursor
    pub fn set_cursor_position(&mut self, x: u32, y: u32) {
        self.cursor_x = x;
        self.cursor_y = y;
    }
}

impl Color {
    /// Convertir u8 a Color
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Color::Black,
            1 => Color::Blue,
            2 => Color::Green,
            3 => Color::Cyan,
            4 => Color::Red,
            5 => Color::Magenta,
            6 => Color::Brown,
            7 => Color::LightGray,
            8 => Color::DarkGray,
            9 => Color::LightBlue,
            10 => Color::LightGreen,
            11 => Color::LightCyan,
            12 => Color::LightRed,
            13 => Color::LightMagenta,
            14 => Color::Yellow,
            15 => Color::White,
            _ => Color::Black,
        }
    }
}

impl WindowManager {
    /// Crear un nuevo gestor de ventanas
    pub fn new() -> Self {
        Self {
            windows: [None; 32],
            next_window_id: 1,
            active_window: None,
            desktop_background: Color::DarkGray,
            taskbar_height: 30,
        }
    }
    
    /// Inicializar gestor de ventanas
    pub fn init(&mut self) {
        // Crear ventana de escritorio
        self.create_window("Desktop", Rectangle { x: 0, y: 0, width: 800, height: 600 });
    }
    
    /// Crear ventana
    pub fn create_window(&mut self, title: &str, rect: Rectangle) -> Option<u32> {
        for i in 0..32 {
            if self.windows[i].is_none() {
                let mut title_bytes = [0u8; 64];
                let title_len = core::cmp::min(title.len(), 63);
                title_bytes[..title_len].copy_from_slice(&title.as_bytes()[..title_len]);
                
                let window = Window {
                    id: self.next_window_id,
                    title: title_bytes,
                    rect,
                    visible: true,
                    focused: false,
                    background_color: Color::LightGray,
                    border_color: Color::DarkGray,
                    title_color: Color::Black,
                    z_order: self.next_window_id,
                };
                
                self.windows[i] = Some(window);
                self.next_window_id += 1;
                return Some(window.id);
            }
        }
        None
    }
    
    /// Dibujar ventana
    pub fn draw_window(&self, window_id: u32, vga: &mut VgaDriver) {
        if let Some(window) = self.find_window(window_id) {
            // Dibujar borde
            vga.draw_rectangle(window.rect, window.border_color);
            
            // Dibujar área de contenido
            let content_rect = Rectangle {
                x: window.rect.x + 1,
                y: window.rect.y + 1,
                width: window.rect.width - 2,
                height: window.rect.height - 2,
            };
            vga.fill_rectangle(content_rect, window.background_color);
            
            // Dibujar barra de título
            let title_rect = Rectangle {
                x: window.rect.x + 1,
                y: window.rect.y + 1,
                width: window.rect.width - 2,
                height: 20,
            };
            vga.fill_rectangle(title_rect, window.border_color);
            
            // Dibujar título
            let title_str = core::str::from_utf8(&window.title).unwrap_or("Window");
            vga.set_cursor_position(window.rect.x + 5, window.rect.y + 2);
            vga.set_colors(window.title_color, window.border_color);
            vga.put_string(title_str);
        }
    }
    
    /// Encontrar ventana por ID
    fn find_window(&self, window_id: u32) -> Option<&Window> {
        for i in 0..32 {
            if let Some(ref window) = self.windows[i] {
                if window.id == window_id {
                    return Some(window);
                }
            }
        }
        None
    }
    
    /// Dibujar todas las ventanas
    pub fn draw_all_windows(&self, vga: &mut VgaDriver) {
        // Limpiar pantalla
        vga.clear_screen();
        
        // Dibujar ventanas en orden de z-order
        let mut windows_to_draw = [None; 32];
        let mut count = 0;
        
        for i in 0..32 {
            if let Some(ref window) = self.windows[i] {
                if window.visible {
                    windows_to_draw[count] = Some(*window);
                    count += 1;
                }
            }
        }
        
        // Ordenar por z-order
        for i in 0..count {
            for j in i + 1..count {
                if let (Some(w1), Some(w2)) = (windows_to_draw[i], windows_to_draw[j]) {
                    if w1.z_order > w2.z_order {
                        windows_to_draw[i] = Some(w2);
                        windows_to_draw[j] = Some(w1);
                    }
                }
            }
        }
        
        // Dibujar ventanas
        for i in 0..count {
            if let Some(window) = windows_to_draw[i] {
                self.draw_window(window.id, vga);
            }
        }
    }
}

impl FontSystem {
    /// Crear un nuevo sistema de fuentes
    pub fn new() -> Self {
        Self {
            current_font: Font {
                width: 8,
                height: 16,
                data: [0; 4096],
            },
            font_size: 16,
        }
    }
    
    /// Inicializar sistema de fuentes
    pub fn init(&mut self) {
        // Cargar fuente básica
        self.load_basic_font();
    }
    
    /// Cargar fuente básica
    fn load_basic_font(&mut self) {
        // TODO: Implementar carga de fuente desde memoria
        // Por ahora, usar fuente básica hardcodeada
    }
}

/// Instancia global del driver VGA
static mut VGA_DRIVER: Option<VgaDriver> = None;

/// Instancia global del gestor de ventanas
static mut WINDOW_MANAGER: Option<WindowManager> = None;

/// Instancia global del sistema de fuentes
static mut FONT_SYSTEM: Option<FontSystem> = None;

/// Inicializar sistema de gráficos
pub fn init_graphics() {
    unsafe {
        VGA_DRIVER = Some(VgaDriver::new());
        WINDOW_MANAGER = Some(WindowManager::new());
        FONT_SYSTEM = Some(FontSystem::new());
        
        if let Some(ref mut vga) = VGA_DRIVER {
            vga.init();
        }
        
        if let Some(ref mut wm) = WINDOW_MANAGER {
            wm.init();
        }
        
        if let Some(ref mut fs) = FONT_SYSTEM {
            fs.init();
        }
    }
}

/// Obtener driver VGA
pub fn get_vga_driver() -> Option<&'static mut VgaDriver> {
    unsafe {
        VGA_DRIVER.as_mut()
    }
}

/// Obtener gestor de ventanas
pub fn get_window_manager() -> Option<&'static mut WindowManager> {
    unsafe {
        WINDOW_MANAGER.as_mut()
    }
}

/// Escribir string en pantalla
pub fn print_string(s: &str) {
    unsafe {
        if let Some(ref mut vga) = VGA_DRIVER {
            vga.put_string(s);
        }
    }
}

/// Limpiar pantalla
pub fn clear_screen() {
    unsafe {
        if let Some(ref mut vga) = VGA_DRIVER {
            vga.clear_screen();
        }
    }
}

/// Establecer colores
pub fn set_colors(foreground: Color, background: Color) {
    unsafe {
        if let Some(ref mut vga) = VGA_DRIVER {
            vga.set_colors(foreground, background);
        }
    }
}

/// Escribir string al framebuffer (similar a VGA.write_string)
pub fn framebuffer_write_string(s: &str) {
    use crate::drivers::framebuffer::{get_framebuffer, Color as FbColor};
    
    if let Some(ref mut fb) = get_framebuffer() {
        // Intentar usar aceleración NVIDIA si está disponible
        if let Some(nvidia_driver) = crate::drivers::nvidia_graphics::detect_nvidia_gpu() {
            // Usar aceleración NVIDIA para renderizado de texto
            framebuffer_write_string_nvidia(s, fb, &nvidia_driver);
        } else {
            // Usar renderizado estándar
            framebuffer_write_string_standard(s, fb);
        }
    }
}

/// Renderizado de texto estándar (sin aceleración)
fn framebuffer_write_string_standard(s: &str, fb: &mut crate::drivers::framebuffer::FramebufferDriver) {
    use crate::drivers::framebuffer::Color as FbColor;
    
    // Usar posición fija para el texto (esquina superior izquierda)
    let mut x = 10;
    let mut y = 30;
    let char_width = 8;
    let char_height = 16;
    
    for ch in s.chars() {
        if ch == '\n' {
            x = 10;
            y += char_height + 2;
        } else if ch == '\r' {
            x = 10;
        } else if ch == '\t' {
            x += char_width * 4;
        } else {
            // Dibujar carácter en el framebuffer
            draw_char_to_framebuffer(fb, ch, x, y, FbColor::WHITE, FbColor::BLACK);
            x += char_width;
            
            // Salto de línea si llegamos al borde
            if x > (fb.info.width - char_width) {
                x = 10;
                y += char_height + 2;
            }
        }
    }
}

/// Renderizado de texto con aceleración NVIDIA
fn framebuffer_write_string_nvidia(s: &str, fb: &mut crate::drivers::framebuffer::FramebufferDriver, nvidia: &crate::drivers::nvidia_graphics::NvidiaGraphicsDriver) {
    use crate::drivers::framebuffer::Color as FbColor;
    use crate::desktop_ai::{Point, Rect};
    
    // Usar posición fija para el texto (esquina superior izquierda)
    let mut x = 10;
    let mut y = 30;
    let char_width = 8;
    let char_height = 16;
    
    for ch in s.chars() {
        if ch == '\n' {
            x = 10;
            y += char_height + 2;
        } else if ch == '\r' {
            x = 10;
        } else if ch == '\t' {
            x += char_width * 4;
        } else {
            // Crear operación 2D para NVIDIA
            let text_rect = Rect {
                x: x as u32,
                y: y as u32,
                width: char_width as u32,
                height: char_height as u32,
            };
            
            // Usar NVIDIA para renderizar el carácter (simulado)
            // En una implementación real, esto usaría la API de NVIDIA
            draw_char_to_framebuffer(fb, ch, x, y, FbColor::WHITE, FbColor::BLACK);
            x += char_width;
            
            // Salto de línea si llegamos al borde
            if x > (fb.info.width - char_width) {
                x = 10;
                y += char_height + 2;
            }
        }
    }
}

/// Escribir string con color al framebuffer
pub fn framebuffer_write_string_colored(s: &str, fg: Color, bg: Color) {
    use crate::drivers::framebuffer::{get_framebuffer, Color as FbColor};
    
    if let Some(ref mut fb) = get_framebuffer() {
        let mut x = 10;
        let mut y = 30;
        let char_width = 8;
        let char_height = 16;
        
        // Convertir colores VGA a colores framebuffer
        let fb_fg = convert_vga_to_framebuffer_color(fg);
        let fb_bg = convert_vga_to_framebuffer_color(bg);
        
        for ch in s.chars() {
            if ch == '\n' {
                x = 10;
                y += char_height + 2;
            } else if ch == '\r' {
                x = 10;
            } else if ch == '\t' {
                x += char_width * 4;
            } else {
                draw_char_to_framebuffer(fb, ch, x, y, fb_fg, fb_bg);
                x += char_width;
                
                if x > (fb.info.width - char_width) {
                    x = 10;
                    y += char_height + 2;
                }
            }
        }
    }
}

/// Dibujar un carácter en el framebuffer
fn draw_char_to_framebuffer(fb: &mut crate::drivers::framebuffer::FramebufferDriver, ch: char, x: u32, y: u32, fg: crate::drivers::framebuffer::Color, bg: crate::drivers::framebuffer::Color) {
    let char_width = 8;
    let char_height = 16;
    
    // Verificar límites
    if x + char_width > fb.info.width || y + char_height > fb.info.height {
        return;
    }
    
    // Obtener datos de fuente para el carácter
    let font_data = get_font_data(ch);
    
    // Dibujar el carácter
    for row in 0..char_height {
        let byte = font_data[row as usize];
        for col in 0..char_width {
            let pixel_x = x + col;
            let pixel_y = y + row;
            
            if pixel_x < fb.info.width && pixel_y < fb.info.height {
                if (byte & (0x80 >> col)) != 0 {
                    fb.put_pixel(pixel_x, pixel_y, fg);
                } else {
                    fb.put_pixel(pixel_x, pixel_y, bg);
                }
            }
        }
    }
}

/// Obtener datos de fuente para un carácter (fuente 8x16 básica)
fn get_font_data(ch: char) -> [u8; 16] {
    match ch {
        'A' => [0x00, 0x00, 0x18, 0x3C, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        'B' => [0x00, 0x00, 0x7C, 0x66, 0x66, 0x7C, 0x66, 0x66, 0x66, 0x66, 0x7C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'C' => [0x00, 0x00, 0x3C, 0x66, 0x60, 0x60, 0x60, 0x60, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'D' => [0x00, 0x00, 0x78, 0x6C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x6C, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00],
        'E' => [0x00, 0x00, 0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00],
        'F' => [0x00, 0x00, 0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00],
        'G' => [0x00, 0x00, 0x3C, 0x66, 0x60, 0x60, 0x6E, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'H' => [0x00, 0x00, 0x66, 0x66, 0x66, 0x7E, 0x66, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        'I' => [0x00, 0x00, 0x3C, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'J' => [0x00, 0x00, 0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x6C, 0x6C, 0x38, 0x00, 0x00, 0x00, 0x00, 0x00],
        'K' => [0x00, 0x00, 0x66, 0x6C, 0x78, 0x70, 0x78, 0x6C, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        'L' => [0x00, 0x00, 0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00],
        'M' => [0x00, 0x00, 0x63, 0x77, 0x7F, 0x6B, 0x63, 0x63, 0x63, 0x63, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00],
        'N' => [0x00, 0x00, 0x66, 0x76, 0x7E, 0x7E, 0x6E, 0x66, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        'O' => [0x00, 0x00, 0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'P' => [0x00, 0x00, 0x7C, 0x66, 0x66, 0x66, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00],
        'Q' => [0x00, 0x00, 0x3C, 0x66, 0x66, 0x66, 0x66, 0x66, 0x6A, 0x6C, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00],
        'R' => [0x00, 0x00, 0x7C, 0x66, 0x66, 0x66, 0x7C, 0x6C, 0x66, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        'S' => [0x00, 0x00, 0x3C, 0x66, 0x60, 0x30, 0x18, 0x0C, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'T' => [0x00, 0x00, 0x7E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
        'U' => [0x00, 0x00, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        'V' => [0x00, 0x00, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x3C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
        'W' => [0x00, 0x00, 0x63, 0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00],
        'X' => [0x00, 0x00, 0x66, 0x66, 0x3C, 0x3C, 0x18, 0x3C, 0x3C, 0x66, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00],
        'Y' => [0x00, 0x00, 0x66, 0x66, 0x66, 0x3C, 0x3C, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
        'Z' => [0x00, 0x00, 0x7E, 0x06, 0x0C, 0x0C, 0x18, 0x30, 0x30, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00],
        '0' => [0x00, 0x00, 0x3C, 0x66, 0x6E, 0x76, 0x66, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        '1' => [0x00, 0x00, 0x18, 0x38, 0x78, 0x18, 0x18, 0x18, 0x18, 0x18, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00],
        '2' => [0x00, 0x00, 0x3C, 0x66, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00],
        '3' => [0x00, 0x00, 0x3C, 0x66, 0x06, 0x06, 0x3C, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        '4' => [0x00, 0x00, 0x0C, 0x1C, 0x3C, 0x6C, 0xCC, 0xFE, 0x0C, 0x0C, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00],
        '5' => [0x00, 0x00, 0x7E, 0x60, 0x60, 0x7C, 0x06, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        '6' => [0x00, 0x00, 0x3C, 0x66, 0x60, 0x60, 0x7C, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        '7' => [0x00, 0x00, 0x7E, 0x06, 0x06, 0x0C, 0x0C, 0x18, 0x18, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00],
        '8' => [0x00, 0x00, 0x3C, 0x66, 0x66, 0x66, 0x3C, 0x66, 0x66, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        '9' => [0x00, 0x00, 0x3C, 0x66, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x66, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x00],
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '!' => [0x00, 0x00, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
        '?' => [0x00, 0x00, 0x3C, 0x66, 0x06, 0x0C, 0x18, 0x18, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30, 0x00, 0x00, 0x00, 0x00],
        ':' => [0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],
        ';' => [0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00, 0x00, 0x00, 0x18, 0x18, 0x30, 0x00, 0x00, 0x00, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '=' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x00, 0x00, 0x18, 0x18, 0x7E, 0x7E, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        _ => [0x00, 0x00, 0x7E, 0x60, 0x60, 0x7C, 0x60, 0x60, 0x60, 0x60, 0x7E, 0x00, 0x00, 0x00, 0x00, 0x00], // Carácter por defecto
    }
}

/// Convertir color VGA a color framebuffer
fn convert_vga_to_framebuffer_color(vga_color: Color) -> crate::drivers::framebuffer::Color {
    use crate::drivers::framebuffer::Color as FbColor;
    
    match vga_color {
        Color::Black => FbColor::BLACK,
        Color::Blue => FbColor::BLUE,
        Color::Green => FbColor::GREEN,
        Color::Cyan => FbColor::CYAN,
        Color::Red => FbColor::RED,
        Color::Magenta => FbColor::MAGENTA,
        Color::Brown => FbColor::new(139, 69, 19, 255), // Brown
        Color::LightGray => FbColor::new(192, 192, 192, 255), // Light Gray
        Color::DarkGray => FbColor::new(64, 64, 64, 255), // Dark Gray
        Color::LightBlue => FbColor::new(173, 216, 230, 255), // Light Blue
        Color::LightGreen => FbColor::new(144, 238, 144, 255), // Light Green
        Color::LightCyan => FbColor::new(224, 255, 255, 255), // Light Cyan
        Color::LightRed => FbColor::new(255, 182, 193, 255), // Light Red
        Color::LightMagenta => FbColor::new(255, 182, 255, 255), // Light Magenta
        Color::Yellow => FbColor::YELLOW,
        Color::White => FbColor::WHITE,
    }
}

/// Limpiar framebuffer con color
pub fn framebuffer_clear(color: Color) {
    use crate::drivers::framebuffer::{get_framebuffer, Color as FbColor};
    
    if let Some(ref mut fb) = get_framebuffer() {
        let fb_color = convert_vga_to_framebuffer_color(color);
        fb.clear_screen(fb_color);
    }
}

/// Dibujar líneas de referencia en el centro del framebuffer
pub fn framebuffer_draw_center_reference_lines() {
    use crate::main_simple::VGA;
    
    // Debug en VGA
    unsafe {
        VGA.write_string("Debug: Dibujando líneas de referencia con NVIDIA...\n");
    }
}
