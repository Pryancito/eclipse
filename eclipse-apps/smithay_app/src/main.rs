//! Smithay App - Xwayland Compositor
//! 
//! This application implements a Wayland compositor with Xwayland support
//! using Eclipse OS IPC and /dev/fb0 framebuffer device.

#![no_std]
#![no_main]



use eclipse_libc::{println, getpid, send, receive, yield_cpu, get_framebuffer_info, map_framebuffer, FramebufferInfo, get_gpu_display_info, set_cursor_position};
use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder, CornerRadii},
    text::{Text, TextStyle},
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
};

/// Kernel PHYS_MEM_OFFSET (Eclipse: physical X → kernel virt X + this)
/// Si map_framebuffer devuelve dir kernel por error, convertir a identity
const PHYS_MEM_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// IPC Message Types
const MSG_TYPE_GRAPHICS: u32 = 0x00000010;  // Graphics messages

#[allow(dead_code)]
const MSG_TYPE_INPUT: u32 = 0x00000040;     // Input messages

#[allow(dead_code)]
const MSG_TYPE_SIGNAL: u32 = 0x00000400;    // Signal messages

/// Status update interval (iterations between status prints)
const STATUS_UPDATE_INTERVAL: u64 = 1000000;

/// IPC message buffer size
const IPC_BUFFER_SIZE: usize = 256;

/// Ventana simple (rectángulo con barra de título)
/// Ref: xfwl4 workspaces minimizar
struct SimpleWindow {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// Oculta la ventana hasta restaurar (M minimiza, R restaura)
    minimized: bool,
}

impl SimpleWindow {
    const TITLE_H: i32 = 24;
    fn title_bar_contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w
            && py >= self.y && py < self.y + Self::TITLE_H
    }
    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w
            && py >= self.y && py < self.y + self.h
    }
}

/// Índice de siguiente ventana visible (no minimizada). Ref: xfwl4 cycle.
fn next_visible(from: usize, forward: bool, windows: &[SimpleWindow], count: usize) -> Option<usize> {
    let step = if forward { 1 } else { count.wrapping_sub(1) };
    let mut i = (from.wrapping_add(step)) % count;
    for _ in 0..count {
        if windows[i].w > 0 && !windows[i].minimized {
            return Some(i);
        }
        i = (i.wrapping_add(step)) % count;
    }
    None
}

/// Ventana bajo el cursor (z-order: última = arriba). Ref: xfwl4 focus.
/// Omitimos ventanas minimizadas.
fn focus_under_cursor(px: i32, py: i32, windows: &[SimpleWindow], count: usize) -> Option<usize> {
    for i in (0..count).rev() {
        let w = &windows[i];
        if w.w > 0 && !w.minimized && w.contains(px, py) {
            return Some(i);
        }
    }
    None
}

/// Input event layout shared with `userspace/input_service`
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    device_id: u32,
    event_type: u8,  // 0=key, 1=mouse_move, 2=mouse_button, 3=mouse_scroll
    code: u16,
    value: i32,
    timestamp: u64,
}

/// Cursor simple (cruz) - estable; 64x64 RGBA causaba crash al mover ratón
const CURSOR_SIZE: i32 = 10;

/// Acciones de teclado (referencia: xfwl4 input_handler KeyAction)
#[derive(Clone, Copy, PartialEq)]
enum KeyAction {
    None,
    Clear,
    SetColor(u8),
    CycleStrokeSize,
    SensitivityPlus,
    SensitivityMinus,
    InvertY,
    CenterCursor,
    NewWindow,
    CloseWindow,
    /// Ciclar focus hacia adelante (Tab), ref: xfwl4 cycle
    CycleForward,
    /// Ciclar focus hacia atrás (`), ref: xfwl4 cycle
    CycleBackward,
    /// Minimizar ventana enfocada (M)
    Minimize,
    /// Restaurar última minimizada (R)
    Restore,
}

/// Scancodes US QWERTY → KeyAction (tabla de keybindings)
fn scancode_to_action(scancode: u16) -> KeyAction {
    match scancode {
        0x21 => KeyAction::Clear,           // C
        0x02 => KeyAction::SetColor(0),     // 1
        0x03 => KeyAction::SetColor(1),     // 2
        0x04 => KeyAction::SetColor(2),     // 3
        0x05 => KeyAction::SetColor(3),     // 4
        0x06 => KeyAction::SetColor(4),     // 5
        0x0B => KeyAction::CycleStrokeSize, // 0
        0x0D => KeyAction::SensitivityPlus, // +
        0x0C => KeyAction::SensitivityMinus,// -
        0x17 => KeyAction::InvertY,         // I
        0x47 => KeyAction::CenterCursor,    // Home
        0x31 => KeyAction::NewWindow,       // N
        0x01 => KeyAction::CloseWindow,     // Escape
        0x0F => KeyAction::CycleForward,    // Tab
        0x29 => KeyAction::CycleBackward,   // ` (backtick, arriba de Tab)
        0x32 => KeyAction::Minimize,        // M
        0x13 => KeyAction::Restore,         // R
        _ => KeyAction::None,
    }
}

/// Paleta de colores para dibujar (índice 0–4)
const STROKE_COLORS: [Rgb888; 5] = [
    Rgb888::new(100, 200, 255),  // 0: azul
    Rgb888::new(255, 80, 80),    // 1: rojo
    Rgb888::new(80, 255, 120),   // 2: verde
    Rgb888::new(255, 230, 80),   // 3: amarillo
    Rgb888::new(255, 255, 255),  // 4: blanco
];

/// Simple input state (cursor + botones + focus, ref: xfwl4 focus)
struct InputState {
    cursor_x: i32,
    cursor_y: i32,
    /// Botones: bit0=left, bit1=right, bit2=middle (1=pressed)
    mouse_buttons: u8,
    /// Tecla 'c' pulsada: solicita limpiar el lienzo
    request_clear: bool,
    /// Color actual (índice 0–4)
    stroke_color: u8,
    /// Grosor del trazo: 2, 4 o 6 (cíclico con tecla 0)
    stroke_size: i32,
    /// Sensibilidad del ratón: 50–200, base 100
    mouse_sensitivity: i32,
    /// Invertir eje Y (algunos ratones PS/2 lo reportan al revés)
    invert_y: bool,
    /// Tecla Home: centrar cursor en pantalla
    request_center_cursor: bool,
    /// Índice de ventana siendo arrastrada (None = no)
    dragging_window: Option<usize>,
    /// Offset cursor-ventana al iniciar arrastre
    drag_offset_x: i32,
    drag_offset_y: i32,
    /// Solicitar nueva ventana (tecla N)
    request_new_window: bool,
    /// Cerrar ventana superior (tecla Escape)
    request_close_window: bool,
    /// Ciclar focus adelante (Tab)
    request_cycle_forward: bool,
    /// Ciclar focus atrás (`)
    request_cycle_backward: bool,
    /// Minimizar ventana enfocada (M)
    request_minimize: bool,
    /// Restaurar última minimizada (R)
    request_restore: bool,
    /// Ventana con focus (teclado/lógica), ref: xfwl4 KeyboardFocusTarget
    focused_window: Option<usize>,
}

impl InputState {
    fn new(width: i32, height: i32) -> Self {
        Self {
            cursor_x: width / 2,
            cursor_y: height / 2,
            mouse_buttons: 0,
            request_clear: false,
            stroke_color: 0,
            stroke_size: 4,
            mouse_sensitivity: 100,
            invert_y: false,
            request_center_cursor: false,
            dragging_window: None,
            drag_offset_x: 0,
            drag_offset_y: 0,
            request_new_window: false,
            request_close_window: false,
            request_cycle_forward: false,
            request_cycle_backward: false,
            request_minimize: false,
            request_restore: false,
            focused_window: None,
        }
    }

    fn apply_event(&mut self, ev: &InputEvent, fb_width: i32, fb_height: i32, windows: &mut [SimpleWindow], window_count: &mut usize) {
        match ev.event_type {
            // Keyboard: usar KeyAction (ref: xfwl4 process_common_key_action)
            0 => {
                if ev.value != 1 { return; }
                match scancode_to_action(ev.code) {
                    KeyAction::None => {}
                    KeyAction::Clear => self.request_clear = true,
                    KeyAction::SetColor(c) => self.stroke_color = c.min(4),
                    KeyAction::CycleStrokeSize => {
                        self.stroke_size = match self.stroke_size {
                            2 => 4,
                            4 => 6,
                            _ => 2,
                        };
                    }
                    KeyAction::SensitivityPlus => self.mouse_sensitivity = (self.mouse_sensitivity + 25).min(200),
                    KeyAction::SensitivityMinus => self.mouse_sensitivity = (self.mouse_sensitivity - 25).max(50),
                    KeyAction::InvertY => self.invert_y = !self.invert_y,
                    KeyAction::CenterCursor => self.request_center_cursor = true,
                    KeyAction::NewWindow => self.request_new_window = true,
                    KeyAction::CloseWindow => self.request_close_window = true,
                    KeyAction::CycleForward => self.request_cycle_forward = true,
                    KeyAction::CycleBackward => self.request_cycle_backward = true,
                    KeyAction::Minimize => self.request_minimize = true,
                    KeyAction::Restore => self.request_restore = true,
                }
            }
            // Mouse move: code 0 = X, code 1 = Y (con sensibilidad e inversión Y)
            1 => {
                let delta = (ev.value * self.mouse_sensitivity) / 100;
                if ev.code == 0 {
                    self.cursor_x = (self.cursor_x + delta)
                        .clamp(0, fb_width.saturating_sub(1));
                    // Arrastrar ventana
                    if let Some(idx) = self.dragging_window {
                        if idx < *window_count {
                            let dx = self.cursor_x - (windows[idx].x + self.drag_offset_x);
                            windows[idx].x = (windows[idx].x + dx)
                                .clamp(0, fb_width.saturating_sub(windows[idx].w));
                        }
                    }
                } else if ev.code == 1 {
                    let dy = if self.invert_y { -delta } else { delta };
                    self.cursor_y = (self.cursor_y + dy)
                        .clamp(0, fb_height.saturating_sub(1));
                    if let Some(idx) = self.dragging_window {
                        if idx < *window_count {
                            let dy_act = self.cursor_y - (windows[idx].y + self.drag_offset_y);
                            windows[idx].y = (windows[idx].y + dy_act)
                                .clamp(0, fb_height.saturating_sub(windows[idx].h));
                        }
                    }
                }
            }
            // Mouse button: code 0=left, 1=right, 2=middle; value 0=release, 1=press
            2 => {
                let mask = 1u8 << (ev.code as u8);
                if ev.value != 0 {
                    self.mouse_buttons |= mask;
                    if ev.code == 0 {
                        // Clic izquierdo: focus + arrastre si es en barra de título (z-order: de arriba abajo)
                        let under = focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count);
                        if let Some(i) = under {
                            self.focused_window = Some(i);
                            if windows[i].title_bar_contains(self.cursor_x, self.cursor_y) {
                                self.dragging_window = Some(i);
                                self.drag_offset_x = self.cursor_x - windows[i].x;
                                self.drag_offset_y = self.cursor_y - windows[i].y;
                            }
                        } else {
                            self.focused_window = None;
                        }
                    }
                } else {
                    self.mouse_buttons &= !mask;
                    if ev.code == 0 {
                        self.dragging_window = None;
                    }
                }
            }
            // Mouse scroll: code 0 = vertical, value = +/-1
            3 => {
                if ev.code == 0 && ev.value != 0 {
                    self.mouse_sensitivity = (self.mouse_sensitivity + ev.value * 10)
                        .clamp(50, 200);
                }
            }
            _ => {}
        }

        // Clamp cursor to screen
        self.cursor_x = self.cursor_x.clamp(0, fb_width.saturating_sub(1));
        self.cursor_y = self.cursor_y.clamp(0, fb_height.saturating_sub(1));
    }

    /// Cursor al estilo xfwl4: solo dibujar encima, sin save/restore de fondo.
    /// xfwl4 redibuja todo el frame; aquí evitamos read_region/write_region que causaban crash.
    fn draw_cursor(&mut self, fb: &mut FramebufferState) {
        let size = CURSOR_SIZE;
        let width = fb.info.width as i32;
        let height = fb.info.height as i32;
        let max_x = width.saturating_sub(size).max(0);
        let max_y = height.saturating_sub(size).max(0);
        let x = self.cursor_x.clamp(0, max_x);
        let y = self.cursor_y.clamp(0, max_y);

        let (r, g, b) = if self.mouse_buttons & 1 != 0 {
            let c = STROKE_COLORS[self.stroke_color.min(4) as usize];
            (c.r(), c.g(), c.b())
        } else {
            (230u8, 230, 230)
        };
        let raw = 0xFF000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let half = size / 2;
        fb.draw_cross_raw(x + half, y + half, half, raw);
    }
}

/// Framebuffer state
struct FramebufferState {
    info: FramebufferInfo,
    base_addr: usize,
}

impl FramebufferState {
    /// Initialize framebuffer by getting info and mapping it
    fn init() -> Option<Self> {
        println!("[SMITHAY] Initializing framebuffer access...");
        
        let fb_info = match get_framebuffer_info() {
            Some(info) => {
                println!("[SMITHAY]   - Framebuffer: {}x{} @ {} bpp", 
                    info.width, info.height, info.bpp);
                info
            }
            None => {
                println!("[SMITHAY]   - ERROR: Failed to get framebuffer info");
                return None;
            }
        };
        
        let fb_base = match map_framebuffer() {
            Some(addr) => {
                // Si el kernel devuelve dir kernel por error, convertir a identity
                let addr_u64 = addr as u64;
                let base = if addr_u64 >= PHYS_MEM_OFFSET {
                    (addr_u64 - PHYS_MEM_OFFSET) as usize
                } else {
                    addr
                };
                // Rechazar direcciones en la primera página (evita crash CR2=0x3 por ptr inválido)
                if base < 0x1000 {
                    println!("[SMITHAY]   - ERROR: map_framebuffer returned invalid address 0x{:x}", base);
                    return None;
                }
                println!("[SMITHAY]   - Framebuffer mapped at address: 0x{:x}", base);
                base
            }
            None => {
                println!("[SMITHAY]   - ERROR: Failed to map framebuffer");
                return None;
            }
        };
        
        // CRÍTICO: Usar SIEMPRE base_addr de map_framebuffer para dibujar. NUNCA info.address,
        // que podría contener una dirección kernel si hay un bug. Sobrescribir para consistencia.
        let mut info = fb_info;
        info.address = fb_base as u64;
        
        Some(FramebufferState {
            info,
            base_addr: fb_base,
        })
    }

    /// Read a rectangular region of raw u32 pixels (ARGB8888). Stride uses width.
    fn read_region_u32(&self, x: i32, y: i32, w: i32, h: i32, out: &mut [u32]) {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        if self.base_addr < 0x1000 || x < 0 || y < 0 || w <= 0 || h <= 0
            || x + w > width || y + h > height
            || (w as usize) * (h as usize) > out.len()
        {
            return;
        }
        let ptr = self.base_addr as *const u32;
        let mut idx = 0;
        for py in y..(y + h) {
            for px in x..(x + w) {
                if px >= 0 && px < width && py >= 0 && py < height && idx < out.len() {
                    let offset = (py * width + px) as usize;
                    unsafe {
                        out[idx] = core::ptr::read_volatile(ptr.add(offset));
                    }
                }
                idx += 1;
            }
        }
    }

    /// Dibuja una cruz (vertical + horizontal) con escrituras directas; evita embedded_graphics
    fn draw_cross_raw(&mut self, cx: i32, cy: i32, half: i32, raw_color: u32) {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        if self.base_addr < 0x1000 { return; }
        let ptr = self.base_addr as *mut u32;
        // Vertical: (cx, cy-half) .. (cx, cy+half)
        for py in (cy - half)..=(cy + half) {
            if py >= 0 && py < height {
                let offset = (py * width + cx) as usize;
                unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
            }
        }
        // Horizontal: (cx-half, cy) .. (cx+half, cy)
        for px in (cx - half)..=(cx + half) {
            if px >= 0 && px < width {
                let offset = (cy * width + px) as usize;
                unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
            }
        }
    }

    /// Write a rectangular region of raw u32 pixels (ARGB8888). Stride uses width.
    fn write_region_u32(&mut self, x: i32, y: i32, w: i32, h: i32, pixels: &[u32]) {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        if self.base_addr < 0x1000 || x < 0 || y < 0 || w <= 0 || h <= 0
            || x + w > width || y + h > height
            || (w as usize) * (h as usize) > pixels.len()
        {
            return;
        }
        let ptr = self.base_addr as *mut u32;
        let mut idx = 0;
        for py in y..(y + h) {
            for px in x..(x + w) {
                if px >= 0 && px < width && py >= 0 && py < height && idx < pixels.len() {
                    let offset = (py * width + px) as usize;
                    unsafe {
                        core::ptr::write_volatile(ptr.add(offset), pixels[idx]);
                    }
                }
                idx += 1;
            }
        }
    }
}

/// DrawTarget implementation for our Framebuffer
impl DrawTarget for FramebufferState {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        if self.base_addr < 0x1000 {
            return Ok(());
        }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let fb_ptr = self.base_addr as *mut u32;

        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < width && coord.y >= 0 && coord.y < height {
                let offset = (coord.y * width + coord.x) as usize;
                // Convert Rgb888 to ARGB8888 (0xFFRRGGBB)
                let raw_color = 0xFF000000 | 
                    ((color.r() as u32) << 16) | 
                    ((color.g() as u32) << 8) | 
                    (color.b() as u32);
                
                unsafe {
                    core::ptr::write_volatile(fb_ptr.add(offset), raw_color);
                }
            }
        }
        Ok(())
    }
}

impl OriginDimensions for FramebufferState {
    fn size(&self) -> Size {
        Size::new(self.info.width as u32, self.info.height as u32)
    }
}

/// IPC Communication Handler
struct IpcHandler {
    message_count: u64,
}

impl IpcHandler {
    fn new() -> Self {
        Self {
            message_count: 0,
        }
    }
    
    /// Process one IPC message (if any) and return a possible `InputEvent`.
    fn process_messages(&mut self) -> Option<InputEvent> {
        let mut buffer = [0u8; IPC_BUFFER_SIZE];
        
        let (len, sender_pid) = receive(&mut buffer);
        
        if len > 0 {
            self.message_count += 1;
            // Check if this is a binary input event from input_service
            if len == core::mem::size_of::<InputEvent>() {
                let mut ev = InputEvent {
                    device_id: 0,
                    event_type: 0,
                    code: 0,
                    value: 0,
                    timestamp: 0,
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer.as_ptr(),
                        &mut ev as *mut InputEvent as *mut u8,
                        core::mem::size_of::<InputEvent>(),
                    );
                }
                return Some(ev);
            }

            let response = b"ACK";
            if send(sender_pid, MSG_TYPE_GRAPHICS, response) != 0 {
                println!("[SMITHAY] WARNING: Failed to send ACK to PID {}", sender_pid);
            }
        }

        None
    }
}

/// Dibuja las ventanas (focused = borde más brillante). Omitimos minimizadas.
fn draw_windows(fb: &mut FramebufferState, windows: &[SimpleWindow], window_count: usize, focused_window: Option<usize>) {
    for (i, w) in windows.iter().take(window_count).enumerate() {
        if w.w <= 0 || w.h <= 0 || w.minimized { continue; }
        let is_focused = focused_window == Some(i);
        let stroke = if is_focused {
            Rgb888::new(120, 150, 255)  // borde azul claro para focus
        } else {
            Rgb888::new(80, 80, 150)
        };
        let title_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(60, 60, 100))
            .stroke_color(stroke)
            .stroke_width(if is_focused { 2 } else { 1 })
            .build();
        let body_style = PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::new(40, 40, 50))
            .stroke_color(Rgb888::new(100, 100, 100))
            .stroke_width(1)
            .build();
        let _ = Rectangle::new(Point::new(w.x, w.y), Size::new(w.w as u32, SimpleWindow::TITLE_H as u32))
            .into_styled(title_style)
            .draw(fb);
        let body_h = (w.h - SimpleWindow::TITLE_H).max(0);
        let _ = Rectangle::new(Point::new(w.x, w.y + SimpleWindow::TITLE_H), Size::new(w.w as u32, body_h as u32))
            .into_styled(body_style)
            .draw(fb);
    }
}

/// Redibuja el fondo estático (header, footer) para limpiar el lienzo
fn draw_static_ui(fb: &mut FramebufferState) {
    let _ = fb.clear(Rgb888::new(26, 26, 26));
    let rect_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(45, 45, 45))
        .stroke_color(Rgb888::new(100, 100, 255))
        .stroke_width(2)
        .build();
    let _ = Rectangle::new(Point::new(50, 50), Size::new(700, 100))
        .into_styled(rect_style)
        .draw(fb);
    let text_style = MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE);
    let _ = Text::new("Eclipse OS - Rust Display Server", Point::new(80, 110), text_style)
        .draw(fb);
    let _ = Rectangle::new(Point::new(0, fb.info.height as i32 - 40), Size::new(fb.info.width as u32, 40))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 80, 150)).build())
        .draw(fb);
    let _ = Text::new("1-5 color | 0 size | N/Esc win | Tab/` cycle | M min R rest | +/- sens | Home center | C clear", Point::new(20, fb.info.height as i32 - 15),
        MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE))
        .draw(fb);
}

/// Ask init (PID 1) for the PID of `input_service`.
fn query_input_service_pid() -> Option<u32> {
    const INIT_PID: u32 = 1;
    const REQUEST: &[u8] = b"GET_INPUT_PID";

    if send(INIT_PID, MSG_TYPE_INPUT, REQUEST) != 0 {
        println!("[SMITHAY] ERROR: Failed to send GET_INPUT_PID to init");
        return None;
    }

    let mut buffer = [0u8; IPC_BUFFER_SIZE];

    // Small non-blocking wait loop
    for _ in 0..1000 {
        let (len, sender_pid) = receive(&mut buffer);
        if len >= 8 && sender_pid == INIT_PID && &buffer[0..4] == b"INPT" {
            let mut id_bytes = [0u8; 4];
            id_bytes.copy_from_slice(&buffer[4..8]);
            let pid = u32::from_le_bytes(id_bytes);
            if pid != 0 {
                println!("[SMITHAY] input_service PID discovered: {}", pid);
                return Some(pid);
            }
        }
        yield_cpu();
    }

    println!("[SMITHAY] WARNING: Could not get input_service PID from init");
    None
}

/// Send a subscription message to `input_service` with our own PID.
fn subscribe_to_input_service(input_pid: u32, self_pid: u32) {
    let mut msg = [0u8; 8];
    msg[0..4].copy_from_slice(b"SUBS");
    msg[4..8].copy_from_slice(&self_pid.to_le_bytes());
    let res = send(input_pid, MSG_TYPE_INPUT, &msg);
    if res != 0 {
        println!(
            "[SMITHAY] WARNING: Failed to send SUBS to input_service (PID {}), code={}",
            input_pid, res
        );
    } else {
        println!(
            "[SMITHAY] Subscribed to input_service events (PID {})",
            input_pid
        );
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Re-align stack to 16 bytes for SSE instructions.
    // Some linkers/stubs might have CALLED us, misaligning RSP by 8.
    unsafe {
        core::arch::asm!("and rsp, -16", options(nomem, nostack));
    }
    
    let pid = getpid();
    
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║         SMITHAY XWAYLAND COMPOSITOR v0.2.1                   ║");
    println!("║         (Rust Native Display Server Prototype)               ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    
    // Initialize framebuffer
    let mut fb = match FramebufferState::init() {
        Some(fb) => fb,
        None => {
            println!("[SMITHAY] CRITICAL: Cannot start without display");
            loop { yield_cpu(); }
        }
    };

    // Simple input state (logical cursor centered on screen)
    let mut input_state = InputState::new(
        fb.info.width as i32,
        fb.info.height as i32,
    );
    // Clear screen
    fb.clear(Rgb888::new(26, 26, 26)).unwrap();
    
    // Draw Header
    let rect_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(45, 45, 45))
        .stroke_color(Rgb888::new(100, 100, 255))
        .stroke_width(2)
        .build();
    
    Rectangle::new(Point::new(50, 50), Size::new(700, 100))
        .into_styled(rect_style)
        .draw(&mut fb).unwrap();

    let text_style = MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE);
    Text::new("Eclipse OS - Rust Display Server", Point::new(80, 110), text_style)
        .draw(&mut fb).unwrap();

    // Draw Footer/Status Bar
    Rectangle::new(Point::new(0, fb.info.height as i32 - 40), Size::new(fb.info.width as u32, 40))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 80, 150)).build())
        .draw(&mut fb).unwrap();

    Text::new("1-5 color | 0 size | N/Esc win | Tab/` cycle | M min R rest | Home center | C clear", Point::new(20, fb.info.height as i32 - 15),
              MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE))
        .draw(&mut fb).unwrap();

    // Discover input_service PID and subscribe to its events
    if let Some(input_pid) = query_input_service_pid() {
        subscribe_to_input_service(input_pid, pid);
    }

    // Initialize IPC handler
    let mut ipc = IpcHandler::new();

    // Ventanas (máx 8), una demo inicial
    const MAX_WINDOWS: usize = 8;
    let mut windows: [SimpleWindow; MAX_WINDOWS] = [
        SimpleWindow { x: 100, y: 180, w: 250, h: 150, minimized: false },
        SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false },  // vacía
        SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false },
        SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false },
        SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false },
        SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false },
        SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false },
        SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false },
    ];
    let mut window_count: usize = 1;

    // VirtIO GPU hardware cursor (evita redibujar cursor en software cada frame)
    let use_hw_cursor = {
        let mut dims = [0u32, 0u32];
        get_gpu_display_info(&mut dims)
    };
    if use_hw_cursor {
        println!("[SMITHAY] Using VirtIO GPU hardware cursor");
    }

    // Dibujar cursor desde el primer frame (antes del loop)
    if use_hw_cursor {
        let _ = set_cursor_position(input_state.cursor_x as u32, input_state.cursor_y as u32);
    } else {
        input_state.draw_cursor(&mut fb);
    }

    println!("[SMITHAY] Compositor ready and running (N=new window, drag title bar)");

    let mut counter: u64 = 0;
    let mut last_status_counter: u64 = 0;
    // Solo actualizar cursor VirtIO cuando cambie (evita saturar la cola y congelar el ratón)
    let mut last_hw_cursor_x: i32 = input_state.cursor_x;
    let mut last_hw_cursor_y: i32 = input_state.cursor_y;
    
    loop {
        counter = counter.wrapping_add(1);
        let mut had_events = false;
        // Procesar todos los eventos IPC pendientes (cursor más fluido)
        while let Some(ev) = ipc.process_messages() {
            had_events = true;
            input_state.apply_event(
                &ev,
                fb.info.width as i32,
                fb.info.height as i32,
                &mut windows,
                &mut window_count,
            );
        }

        // Reducir CPU cuando idle: varios yield si no hubo eventos
        if !had_events {
            for _ in 0..8 {
                yield_cpu();
            }
        }

        // Tecla 'c': limpiar lienzo
        if input_state.request_clear {
            draw_static_ui(&mut fb);
            input_state.request_clear = false;
        }

        // Tecla Escape: cerrar ventana con focus o la superior
        if input_state.request_close_window && window_count > 1 {
            let to_close = input_state.focused_window
                .filter(|i| *i < window_count)
                .unwrap_or(window_count - 1);
            // Mover la ventana a cerrar al final y eliminar
            windows.swap(to_close, window_count - 1);
            windows[window_count - 1] = SimpleWindow { x: 0, y: 0, w: 0, h: 0, minimized: false };
            window_count -= 1;
            input_state.dragging_window = None;
            input_state.focused_window = input_state.focused_window.and_then(|i| {
                if i == to_close { None }
                else if i == window_count - 1 { Some(to_close) }  // el que era último pasó a to_close
                else { Some(i) }
            });
            input_state.request_close_window = false;
        } else if input_state.request_close_window {
            input_state.request_close_window = false;
        }

        // Tab / `: ciclar focus entre ventanas visibles (ref: xfwl4 cycle)
        if input_state.request_cycle_forward && window_count > 1 {
            let current = input_state.focused_window.unwrap_or(0);
            if let Some(next) = next_visible(current, true, &windows, window_count) {
                let top = window_count - 1;
                windows.swap(next, top);
                if let Some(d) = input_state.dragging_window {
                    if d == next { input_state.dragging_window = Some(top); }
                    else if d == top { input_state.dragging_window = Some(next); }
                }
                input_state.focused_window = Some(top);
            }
            input_state.request_cycle_forward = false;
        } else if input_state.request_cycle_forward {
            input_state.request_cycle_forward = false;
        }
        if input_state.request_cycle_backward && window_count > 1 {
            let current = input_state.focused_window.unwrap_or(window_count - 1);
            if let Some(prev) = next_visible(current, false, &windows, window_count) {
                let top = window_count - 1;
                windows.swap(prev, top);
                if let Some(d) = input_state.dragging_window {
                    if d == prev { input_state.dragging_window = Some(top); }
                    else if d == top { input_state.dragging_window = Some(prev); }
                }
                input_state.focused_window = Some(top);
            }
            input_state.request_cycle_backward = false;
        } else if input_state.request_cycle_backward {
            input_state.request_cycle_backward = false;
        }

        // M: minimizar ventana enfocada
        if input_state.request_minimize {
            if let Some(i) = input_state.focused_window {
                if i < window_count && windows[i].w > 0 && !windows[i].minimized {
                    windows[i].minimized = true;
                    input_state.focused_window = None;
                    input_state.dragging_window = None;
                }
            }
            input_state.request_minimize = false;
        }

        // R: restaurar última ventana minimizada
        if input_state.request_restore {
            if let Some(i) = (0..window_count).rev().find(|&i| windows[i].w > 0 && windows[i].minimized) {
                windows[i].minimized = false;
                input_state.focused_window = Some(i);
            }
            input_state.request_restore = false;
        }

        // Tecla N: nueva ventana
        if input_state.request_new_window && window_count < MAX_WINDOWS {
            let w = 200;
            let h = 120;
            windows[window_count] = SimpleWindow {
                x: 50 + (window_count as i32) * 30,
                y: 150 + (window_count as i32) * 25,
                w,
                h,
                minimized: false,
            };
            window_count += 1;
            input_state.request_new_window = false;
        }

        // Tecla Home: centrar cursor
        if input_state.request_center_cursor {
            let w = fb.info.width as i32;
            let h = fb.info.height as i32;
            input_state.cursor_x = w / 2;
            input_state.cursor_y = h / 2;
            input_state.request_center_cursor = false;
        }

        // Dibujar ventanas (antes del trazo y cursor)
        draw_windows(&mut fb, &windows, window_count, input_state.focused_window);

        // Si botón izquierdo pulsado y no arrastrando: dibujar trazo
        if input_state.mouse_buttons & 1 != 0 && input_state.dragging_window.is_none() {
            let dot_size = input_state.stroke_size.clamp(2, 8);
            let x = input_state.cursor_x.clamp(0, fb.info.width as i32 - dot_size);
            let y = input_state.cursor_y.clamp(0, fb.info.height as i32 - dot_size);
            let color = STROKE_COLORS[input_state.stroke_color.min(4) as usize];
            let dot_style = PrimitiveStyleBuilder::new().fill_color(color).build();
            let _ = Rectangle::new(Point::new(x, y), Size::new(dot_size as u32, dot_size as u32))
                .into_styled(dot_style)
                .draw(&mut fb);
        }

        // Draw cursor each iteration (hardware cursor via VirtIO GPU, or software fallback)
        if use_hw_cursor {
            // Solo enviar MOVE_CURSOR cuando cambie; evita saturar VirtIO y congelar el ratón
            if input_state.cursor_x != last_hw_cursor_x || input_state.cursor_y != last_hw_cursor_y {
                last_hw_cursor_x = input_state.cursor_x;
                last_hw_cursor_y = input_state.cursor_y;
                let _ = set_cursor_position(input_state.cursor_x as u32, input_state.cursor_y as u32);
            }
        } else {
            input_state.draw_cursor(&mut fb);
        }
        
        if counter.wrapping_sub(last_status_counter) >= STATUS_UPDATE_INTERVAL {
            // Solo barra, sin Text (evitar posible acceso a dir kernel en MonoTextStyle/font)
            Rectangle::new(Point::new(0, fb.info.height as i32 - 40), Size::new(fb.info.width as u32, 40))
                .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 80, 150)).build())
                .draw(&mut fb).unwrap();
            last_status_counter = counter;
        }
        
        yield_cpu();
    }
}

// Panic handler is provided by eclipse-libc
