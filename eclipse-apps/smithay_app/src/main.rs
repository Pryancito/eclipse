#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

const HEAP_SIZE: usize = 8 * 1024 * 1024; // 8MB — bump allocator, no dealloc
const STACK_GUARD_MARGIN: usize = 64 * 1024; // 64KB safety zone

#[repr(align(4096))]
struct Heap([u8; HEAP_SIZE]);
static mut HEAP: Heap = Heap([0u8; HEAP_SIZE]);
static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

/// Read the current RSP value.
#[inline(always)]
unsafe fn read_rsp() -> usize {
    let rsp: usize;
    core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    rsp
}

struct StaticAllocator;
unsafe impl GlobalAlloc for StaticAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();
        loop {
            let current = HEAP_PTR.load(Ordering::SeqCst);
            let aligned = (current + align - 1) & !(align - 1);
            if aligned + size > HEAP_SIZE {
                // OOM — log and return null (caller will panic via OOM handler)
                // Cannot use println! here as it may itself allocate
                return core::ptr::null_mut();
            }
            if HEAP_PTR.compare_exchange(current, aligned + size, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                let ptr = HEAP.0.as_mut_ptr().add(aligned);
                // Stack-proximity warning (non-fatal: just warn and continue)
                let rsp = read_rsp();
                let ptr_addr = ptr as usize;
                if ptr_addr < rsp && rsp.wrapping_sub(ptr_addr) < STACK_GUARD_MARGIN {
                    // Print warning but DO NOT panic — the kernel's DANGER is separate
                    // and panicking here would kill the compositor unnecessarily
                    core::hint::black_box(ptr_addr); // keep the check visible
                }
                return ptr;
            }
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: StaticAllocator = StaticAllocator;




use eclipse_libc::{println, getpid, send, receive, yield_cpu, get_framebuffer_info, map_framebuffer, FramebufferInfo, get_gpu_display_info, set_cursor_position, mmap, munmap, open, close, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS, MAP_SHARED, O_RDWR, InputEvent};
use sidewind_core::{SideWindMessage, SWND_OP_CREATE, SWND_OP_DESTROY, SWND_OP_UPDATE, SWND_OP_COMMIT, SideWindEvent, SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_MOUSE_MOVE, SWND_EVENT_TYPE_MOUSE_BUTTON, SWND_EVENT_TYPE_RESIZE, SIDEWIND_TAG};
use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder},
    text::Text,
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


struct ExternalSurface {
    id: u32,
    pid: u32,
    vaddr: usize,
    buffer_size: usize,
    active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WindowContent {
    None,
    InternalDemo,
    External(u32), // Index into surfaces array
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ShellWindow {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    minimized: bool,
    content: WindowContent,
}

impl ShellWindow {
    const TITLE_H: i32 = 26;
    fn title_bar_contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w
            && py >= self.y && py < self.y + Self::TITLE_H
    }
    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w
            && py >= self.y && py < self.y + self.h
    }
    const RESIZE_HANDLE_SIZE: i32 = 16;
    fn resize_handle_contains(&self, px: i32, py: i32) -> bool {
        px >= self.x + self.w - Self::RESIZE_HANDLE_SIZE && px < self.x + self.w
            && py >= self.y + self.h - Self::RESIZE_HANDLE_SIZE && py < self.y + self.h
    }
}

/// Índice de siguiente ventana visible (no minimizada). Ref: xfwl4 cycle.
/// índice de siguiente ventana visible (no minimizada). Ref: xfwl4 cycle.
fn next_visible(from: usize, forward: bool, windows: &[ShellWindow], count: usize) -> Option<usize> {
    if count == 0 { return None; }
    let step = if forward { 1 } else { count.wrapping_sub(1) };
    let mut i = (from.wrapping_add(step)) % count;
    for _ in 0..count {
        if windows[i].content != WindowContent::None && !windows[i].minimized {
            return Some(i);
        }
        i = (i.wrapping_add(step)) % count;
    }
    None
}

/// Ventana bajo el cursor (z-order: última = arriba). Ref: xfwl4 focus.
/// Omitimos ventanas minimizadas.
fn focus_under_cursor(px: i32, py: i32, windows: &[ShellWindow], count: usize) -> Option<usize> {
    for i in (0..count).rev() {
        let w = &windows[i];
        if w.content != WindowContent::None && !w.minimized && w.contains(px, py) {
            return Some(i);
        }
    }
    None
}

/// InputEvent from eclipse_libc (shared with input_service)

#[derive(Clone)]
enum CompositorEvent {
    Input(InputEvent),
    SideWind(SideWindMessage, u32), // message, sender_pid
    Wayland(alloc::vec::Vec<u8>, u32), // data, sender_pid
    X11(alloc::vec::Vec<u8>, u32), // data, sender_pid
}

struct WaylandClient {
    pid: u32,
    conn: sidewind_wayland::WaylandConnection,
}

/// Cursor simple (cruz) - estable; 64x64 RGBA causaba crash al mover ratón
const CURSOR_SIZE: i32 = 10;
const MAX_EXTERNAL_SURFACES: usize = 16;

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
        0x2E => KeyAction::Clear,           // C (was incorrectly 0x21 which is F)
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
    request_new_window: bool,
    request_close_window: bool,
    request_cycle_forward: bool,
    request_cycle_backward: bool,
    request_minimize: bool,
    request_restore: bool,
    dragging_window: Option<usize>,
    resizing_window: Option<usize>,
    drag_offset_x: i32,
    drag_offset_y: i32,
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
            request_new_window: false,
            request_close_window: false,
            request_cycle_forward: false,
            request_cycle_backward: false,
            request_minimize: false,
            request_restore: false,
            dragging_window: None,
            resizing_window: None,
            drag_offset_x: 0,
            drag_offset_y: 0,
            focused_window: None,
        }
    }

    fn apply_event(&mut self, ev: &InputEvent, fb_width: i32, fb_height: i32, windows: &mut [ShellWindow], window_count: &mut usize, surfaces: &[ExternalSurface]) {
        match ev.event_type {
            // Keyboard: usar KeyAction (ref: xfwl4 process_common_key_action)
            0 => {
                if ev.value != 1 { return; }
                let action = scancode_to_action(ev.code);
                match action {
                    KeyAction::None => {
                        // Forward to client if focused
                        if let Some(f_idx) = self.focused_window {
                            if let WindowContent::External(s_idx) = windows[f_idx].content {
                                let pid = surfaces[s_idx as usize].pid;
                                let event = SideWindEvent {
                                    event_type: SWND_EVENT_TYPE_KEY,
                                    data1: ev.code as i32,
                                    data2: ev.value as i32,
                                    data3: 0,
                                };
                                let _ = send(pid, MSG_TYPE_INPUT, unsafe {
                                    core::slice::from_raw_parts(&event as *const _ as *const u8, core::mem::size_of::<SideWindEvent>())
                                });
                            }
                        }
                    }
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
            // Mouse move: code 0 = X, code 1 = Y
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
                    if let Some(idx) = self.resizing_window {
                        if idx < *window_count {
                            let new_w = (self.cursor_x - windows[idx].x + 8).max(50);
                            windows[idx].w = new_w;
                        }
                    }
                } else if ev.code == 1 {
                    let y_delta = if self.invert_y { -delta } else { delta };
                    self.cursor_y = (self.cursor_y + y_delta)
                        .clamp(0, fb_height.saturating_sub(1));
                    if let Some(idx) = self.dragging_window {
                        if idx < *window_count {
                            let dy = self.cursor_y - (windows[idx].y + self.drag_offset_y);
                            windows[idx].y = (windows[idx].y + dy)
                                .clamp(0, fb_height.saturating_sub(windows[idx].h));
                        }
                    }
                    if let Some(idx) = self.resizing_window {
                        if idx < *window_count {
                            let new_h = (self.cursor_y - windows[idx].y + 8).max(ShellWindow::TITLE_H + 20);
                            windows[idx].h = new_h;
                        }
                    }
                }
                
                // If resizing, notify client
                if let Some(f_idx) = self.resizing_window {
                     if let WindowContent::External(s_idx) = windows[f_idx].content {
                        let pid = surfaces[s_idx as usize].pid;
                        let event = SideWindEvent {
                            event_type: SWND_EVENT_TYPE_RESIZE,
                            data1: windows[f_idx].w,
                            data2: windows[f_idx].h - ShellWindow::TITLE_H,
                            data3: 0,
                        };
                        let _ = send(pid, MSG_TYPE_INPUT, unsafe {
                            core::slice::from_raw_parts(&event as *const _ as *const u8, core::mem::size_of::<SideWindEvent>())
                        });
                    }
                }
                
                // Forward move to client if focused
                if let Some(f_idx) = self.focused_window {
                    if let WindowContent::External(s_idx) = windows[f_idx].content {
                        let pid = surfaces[s_idx as usize].pid;
                        let rel_x = self.cursor_x - windows[f_idx].x;
                        let rel_y = self.cursor_y - (windows[f_idx].y + ShellWindow::TITLE_H);
                        let event = SideWindEvent {
                            event_type: SWND_EVENT_TYPE_MOUSE_MOVE,
                            data1: rel_x,
                            data2: rel_y,
                            data3: 0,
                        };
                        let _ = send(pid, MSG_TYPE_INPUT, unsafe {
                            core::slice::from_raw_parts(&event as *const _ as *const u8, core::mem::size_of::<SideWindEvent>())
                        });
                    }
                }
            }
            // Mouse button: bit0=L, bit1=R, bit2=M
            2 => {
                let btn = ev.code as u8;
                let pressed = ev.value != 0;
                let old_buttons = self.mouse_buttons;
                if pressed {
                    self.mouse_buttons |= 1 << btn;
                } else {
                    self.mouse_buttons &= !(1 << btn);
                }

                // Click izquierdo inicial: focus y arrastre (ref: xfwl4 start_drag)
                if (self.mouse_buttons & 1 != 0) && (old_buttons & 1 == 0) {
                    if let Some(idx) = focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count) {
                        // Click to front: mover ventana al final de la pila (Z-order top)
                        let top = *window_count - 1;
                        if idx != top {
                            windows.swap(idx, top);
                            self.focused_window = Some(top);
                            // Si pinchamos en la barra de título, iniciar arrastre
                            if windows[top].title_bar_contains(self.cursor_x, self.cursor_y) {
                                self.dragging_window = Some(top);
                                self.drag_offset_x = self.cursor_x - windows[top].x;
                                self.drag_offset_y = self.cursor_y - windows[top].y;
                            } else if windows[top].resize_handle_contains(self.cursor_x, self.cursor_y) {
                                self.resizing_window = Some(top);
                            }
                        } else {
                            self.focused_window = Some(idx);
                            if windows[idx].title_bar_contains(self.cursor_x, self.cursor_y) {
                                self.dragging_window = Some(idx);
                                self.drag_offset_x = self.cursor_x - windows[idx].x;
                                self.drag_offset_y = self.cursor_y - windows[idx].y;
                            } else if windows[idx].resize_handle_contains(self.cursor_x, self.cursor_y) {
                                self.resizing_window = Some(idx);
                            }
                        }
                    } else {
                        self.focused_window = None;
                    }
                } else if self.mouse_buttons & 1 == 0 {
                    self.dragging_window = None;
                    self.resizing_window = None;
                }

                // Forward button to client if focused
                if let Some(f_idx) = self.focused_window {
                    if let WindowContent::External(s_idx) = windows[f_idx].content {
                        let pid = surfaces[s_idx as usize].pid;
                        let event = SideWindEvent {
                            event_type: SWND_EVENT_TYPE_MOUSE_BUTTON,
                            data1: ev.code as i32,
                            data2: ev.value as i32,
                            data3: self.mouse_buttons as i32,
                        };
                        let _ = send(pid, MSG_TYPE_INPUT, unsafe {
                            core::slice::from_raw_parts(&event as *const _ as *const u8, core::mem::size_of::<SideWindEvent>())
                        });
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
    base_addr: usize, // actual drawing target (back buffer)
    front_addr: usize, // physical framebuffer
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
                let addr_u64 = addr as u64;
                let base = if addr_u64 >= PHYS_MEM_OFFSET {
                    (addr_u64 - PHYS_MEM_OFFSET) as usize
                } else {
                    addr
                };
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

        // Allocate back buffer: use pitch (bytes/row) to match kernel framebuffer layout
        let pitch = if fb_info.pitch > 0 { fb_info.pitch } else { fb_info.width * 4 };
        let fb_size = (pitch as u64) * (fb_info.height as u64);
        let back_buffer = mmap(0, fb_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        
        if back_buffer == 0 || back_buffer == u64::MAX {
            println!("[SMITHAY]   - ERROR: Failed to allocate back buffer ({} bytes)", fb_size);
            return None;
        }
        
        println!("[SMITHAY]   - Back buffer allocated at address: 0x{:x}", back_buffer);
        
        let mut info = fb_info;
        info.address = fb_base as u64;
        
        Some(FramebufferState {
            info,
            base_addr: back_buffer as usize,
            front_addr: fb_base,
        })
    }

    /// Copy back buffer to front buffer (uses pitch for row stride)
    fn present(&self) {
        if self.front_addr < 0x1000 || self.base_addr < 0x1000 { return; }
        let pitch = self.info.pitch.max(self.info.width * 4);
        let size_bytes = (pitch as usize).saturating_mul(self.info.height as usize);
        // Límite defensivo: el kernel mapea pitch*height*2 (+ padding). No copiar más de un frame.
        const MAX_FB_COPY: usize = 3840 * 2160 * 4; // 4K single frame
        let size_bytes = size_bytes.min(MAX_FB_COPY);
        if size_bytes == 0 { return; }
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.base_addr as *const u8,
                self.front_addr as *mut u8,
                size_bytes
            );
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
        let max_pixels = (width as usize).saturating_mul(height as usize);
        let fb_ptr = self.base_addr as *mut u32;

        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < width && coord.y >= 0 && coord.y < height {
                let offset = (coord.y as usize).saturating_mul(width as usize).saturating_add(coord.x as usize);
                if offset >= max_pixels { continue; }
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

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        if self.base_addr < 0x1000 {
            return Ok(());
        }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let fb_ptr = self.base_addr as *mut u32;

        let intersection = area.intersection(&Rectangle::new(Point::new(0, 0), Size::new(width as u32, height as u32)));
        if intersection.is_zero_sized() {
            return Ok(());
        }

        let raw_color = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
        
        for y in intersection.top_left.y..intersection.top_left.y + intersection.size.height as i32 {
            let offset_start = (y as usize * width as usize) + intersection.top_left.x as usize;
            for x in 0..intersection.size.width as usize {
                unsafe {
                    core::ptr::write_volatile(fb_ptr.add(offset_start + x), raw_color);
                }
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        if self.base_addr < 0x1000 {
            return Ok(());
        }
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let fb_ptr = self.base_addr as *mut u32;

        let intersection = area.intersection(&Rectangle::new(Point::new(0, 0), Size::new(width as u32, height as u32)));
        if intersection.is_zero_sized() {
            return Ok(());
        }

        let mut colors_iter = colors.into_iter();

        for y in area.top_left.y..area.top_left.y + area.size.height as i32 {
            for x in area.top_left.x..area.top_left.x + area.size.width as i32 {
                if let Some(color) = colors_iter.next() {
                    // Solo dibujamos si estamos dentro de la intersección real visible
                    if x >= intersection.top_left.x && x < intersection.top_left.x + intersection.size.width as i32 &&
                       y >= intersection.top_left.y && y < intersection.top_left.y + intersection.size.height as i32 
                    {
                        let offset = (y as usize * width as usize) + x as usize;
                        let raw_color = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                        unsafe {
                            core::ptr::write_volatile(fb_ptr.add(offset), raw_color);
                        }
                    }
                } else {
                    return Ok(());
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
    
    /// Process one IPC message (if any) and return a possible `CompositorEvent`.
    fn process_messages(&mut self) -> Option<CompositorEvent> {
        let mut buffer = [0u8; IPC_BUFFER_SIZE];
        
        let (len, sender_pid) = receive(&mut buffer);
        
        if len > 0 {
            self.message_count += 1;
            
            // Check for SideWind Message
            if len >= core::mem::size_of::<SideWindMessage>() && &buffer[0..4] == b"SWND" {
                let mut sw = SideWindMessage {
                    tag: 0, op: 0, x: 0, y: 0, w: 0, h: 0, name: [0; 32]
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer.as_ptr(),
                        &mut sw as *mut SideWindMessage as *mut u8,
                        core::mem::size_of::<SideWindMessage>(),
                    );
                }
                
                // Extra validation: must have correct tag
                if sw.tag == SIDEWIND_TAG {
                    return Some(CompositorEvent::SideWind(sw, sender_pid));
                } else {
                    println!("[SMITHAY] Dropping invalid SideWind message from PID {}", sender_pid);
                }
            }
            // InputEvent DEBE ir antes del fallback Wayland: input_service también tiene sender_pid != 0,
            // y antes tratábamos todos los mensajes de input como Wayland -> panic al parsear protocolo.
            if len == core::mem::size_of::<InputEvent>() {
                let mut ev = InputEvent {
                    device_id: 0, event_type: 0, code: 0, value: 0, timestamp: 0,
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer.as_ptr(),
                        &mut ev as *mut InputEvent as *mut u8,
                        core::mem::size_of::<InputEvent>(),
                    );
                }
                return Some(CompositorEvent::Input(ev));
            }
            if sender_pid != 0 && len >= 4 && &buffer[0..4] == b"X11M" {
                return Some(CompositorEvent::X11(buffer[4..len].to_vec(), sender_pid));
            }
            if sender_pid != 0 {
                return Some(CompositorEvent::Wayland(buffer[..len].to_vec(), sender_pid));
            }

            let response = b"ACK";
            if send(sender_pid, MSG_TYPE_GRAPHICS, response) != 0 {
                // println!("[SMITHAY] WARNING: Failed to send ACK to PID {}", sender_pid);
            }
        }

        None
    }
}

/// Dibuja las ventanas (focused = borde más brillante). Omitimos minimizadas.
/// Dibuja las ventanas (focused = borde más brillante). Omitimos minimizadas.
fn draw_shell_windows(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, focused_window: Option<usize>, surfaces: &[ExternalSurface]) {
    for (i, w) in windows.iter().take(window_count).enumerate() {
        if w.content == WindowContent::None || w.minimized { continue; }
        let is_focused = focused_window == Some(i);
        
        // 1. Draw Frame
        draw_window_decoration(fb, w, is_focused);
        
        // 2. Draw Content
        match w.content {
            WindowContent::InternalDemo => {
                // Internal Demo is already handled by the body background in decoration for now
                let text_style = MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE);
                let _ = Text::new("Internal Window", Point::new(w.x + 10, w.y + ShellWindow::TITLE_H + 30), text_style)
                    .draw(fb);
            }
            WindowContent::External(idx) => {
                if (idx as usize) < surfaces.len() && surfaces[idx as usize].active {
                    draw_surface_content(fb, w, &surfaces[idx as usize]);
                }
            }
            _ => {}
        }
    }
}

fn draw_window_decoration(fb: &mut FramebufferState, w: &ShellWindow, is_focused: bool) {
    let stroke = if is_focused {
        Rgb888::new(140, 160, 255)
    } else {
        Rgb888::new(80, 80, 150)
    };
    
    // Title Bar
    let title_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(50, 50, 90))
        .stroke_color(stroke)
        .stroke_width(if is_focused { 2 } else { 1 })
        .build();
    let _ = Rectangle::new(Point::new(w.x, w.y), Size::new(w.w as u32, ShellWindow::TITLE_H as u32))
        .into_styled(title_style)
        .draw(fb);
    
    // Body Background
    let body_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(30, 30, 40))
        .stroke_color(Rgb888::new(80, 80, 80))
        .stroke_width(1)
        .build();
    let body_h = (w.h - ShellWindow::TITLE_H).max(0);
    let _ = Rectangle::new(Point::new(w.x, w.y + ShellWindow::TITLE_H), Size::new(w.w as u32, body_h as u32))
        .into_styled(body_style)
        .draw(fb);

    // Resize Handle (bottom-right)
    let handle_style = PrimitiveStyleBuilder::new()
        .fill_color(if is_focused { Rgb888::new(80, 80, 150) } else { Rgb888::new(40, 40, 60) })
        .build();
    let _ = Rectangle::new(
        Point::new(w.x + w.w - ShellWindow::RESIZE_HANDLE_SIZE, w.y + w.h - ShellWindow::RESIZE_HANDLE_SIZE),
        Size::new(ShellWindow::RESIZE_HANDLE_SIZE as u32, ShellWindow::RESIZE_HANDLE_SIZE as u32)
    ).into_styled(handle_style).draw(fb);
}

fn draw_surface_content(fb: &mut FramebufferState, w: &ShellWindow, s: &ExternalSurface) {
    if s.vaddr == 0 || s.buffer_size == 0 { return; }
    // Reject kernel addresses (0xffff8000...) - user space cannot access them
    if (s.vaddr as u64) >= PHYS_MEM_OFFSET { return; }
    let src_ptr = s.vaddr as *const u32;
    let fb_w = fb.info.width as i32;
    let fb_h = fb.info.height as i32;
    let pitch_px = (fb.info.pitch / 4).max(1) as i32; // stride in pixels (u32)
    let max_dst_offset = (pitch_px * fb_h) as usize; // use pitch instead of width
    let max_src_offset = s.buffer_size / 4; // buffer size in u32 pixels

    let content_y = w.y + ShellWindow::TITLE_H;
    let content_w = w.w;
    let content_h = (w.h - ShellWindow::TITLE_H).max(0);

    let dst_ptr = fb.base_addr as *mut u32;

    for py in 0..content_h {
        let dy = content_y + py;
        if dy < 0 || dy >= fb_h { continue; }
        let mut start_x = 0;
        let mut end_x = content_w;
        if w.x + start_x < 0 { start_x = -w.x; }
        if w.x + end_x > fb_w { end_x = fb_w - w.x; }
        if start_x >= end_x { continue; }
        let dst_offset = (dy as usize * pitch_px as usize) + (w.x + start_x) as usize;
        let copy_len = (end_x - start_x) as usize;
        if dst_offset >= max_dst_offset || dst_offset + copy_len > max_dst_offset { continue; }
        
        let src_offset = (py as usize * content_w as usize) + start_x as usize;
        if src_offset >= max_src_offset || src_offset + copy_len > max_src_offset { continue; }
        
        unsafe {
            core::ptr::copy_nonoverlapping(
                src_ptr.add(src_offset),
                dst_ptr.add(dst_offset),
                copy_len,
            );
        }
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

    // Phase 1: ask init (PID 1) for the input_service PID
    let sent_ok = send(INIT_PID, MSG_TYPE_INPUT, REQUEST) == 0;
    if sent_ok {
        let mut buffer = [0u8; IPC_BUFFER_SIZE];
        for _ in 0..2000 {
            let (len, sender_pid) = receive(&mut buffer);
            if len >= 8 && sender_pid == INIT_PID && &buffer[0..4] == b"INPT" {
                let mut id_bytes = [0u8; 4];
                id_bytes.copy_from_slice(&buffer[4..8]);
                let pid = u32::from_le_bytes(id_bytes);
                if pid != 0 {
                    println!("[SMITHAY] input_service PID from init: {}", pid);
                    return Some(pid);
                }
            }
            yield_cpu();
        }
    }

    println!("[SMITHAY] WARNING: Could not get input_service PID from init, using broadcast fallback");
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

fn handle_sidewind_message(msg: SideWindMessage, sender_pid: u32, surfaces: &mut [ExternalSurface], windows: &mut [ShellWindow], window_count: &mut usize) {
    match msg.op {
        SWND_OP_CREATE => { // Create
            // Construct path "/tmp/" + name
            let mut path = [0u8; 64];
            path[0..5].copy_from_slice(b"/tmp/");
            let mut name_len = 0;
            while name_len < 32 && msg.name[name_len] != 0 {
                path[5 + name_len] = msg.name[name_len];
                name_len += 1;
            }
            let path_str = unsafe { core::str::from_utf8_unchecked(&path[..5+name_len]) };
            
            let fd = open(path_str, O_RDWR, 0);
            if fd < 0 {
                println!("[SMITHAY] SideWind: Failed to open buffer {}", path_str);
                return;
            }
            
            let buffer_size = (msg.w * msg.h * 4) as u64;
            let vaddr = mmap(0, buffer_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            close(fd);
            
            if vaddr == 0 || vaddr == u64::MAX {
                println!("[SMITHAY] SideWind: Failed to mmap buffer {}", path_str);
                return;
            }
            if vaddr >= PHYS_MEM_OFFSET {
                println!("[SMITHAY] SideWind: mmap returned kernel address 0x{:x}, rejecting", vaddr);
                return;
            }

            // Find slot in surfaces
            let surface_idx = surfaces.iter().position(|s| !s.active);
            if let Some(s_idx) = surface_idx {
                let slot = &mut surfaces[s_idx];
                slot.id = sender_pid;
                slot.pid = sender_pid;
                slot.vaddr = vaddr as usize;
                slot.buffer_size = buffer_size as usize;
                slot.active = true;
                
                // Add to windows stack
                if *window_count < windows.len() {
                    windows[*window_count] = ShellWindow {
                        x: msg.x,
                        y: msg.y,
                        w: msg.w as i32,
                        h: msg.h as i32 + ShellWindow::TITLE_H, // Add room for title bar
                        minimized: false,
                        content: WindowContent::External(s_idx as u32),
                    };
                    *window_count += 1;
                    println!("[SMITHAY] SideWind: Surface created for PID {} ({}x{})", sender_pid, msg.w, msg.h);
                }
            }
        }
        SWND_OP_DESTROY => { // Destroy
             if let Some(s_idx) = surfaces.iter().position(|s| s.active && s.pid == sender_pid) {
                 munmap(surfaces[s_idx].vaddr as u64, surfaces[s_idx].buffer_size as u64);
                 surfaces[s_idx].active = false;
                 
                 // Remove from windows stack
                 if let Some(w_idx) = windows.iter().position(|w| w.content == WindowContent::External(s_idx as u32)) {
                     // Move others down
                     for i in w_idx..(*window_count - 1) {
                         windows[i] = windows[i+1];
                     }
                     *window_count -= 1;
                 }
                 println!("[SMITHAY] SideWind: Surface destroyed for PID {}", sender_pid);
             }
        }
        SWND_OP_UPDATE | SWND_OP_COMMIT => { // Update Position or Commit (signal redraw)
             if let Some(w_idx) = windows.iter().position(|w| matches!(w.content, WindowContent::External(idx) if surfaces[idx as usize].pid == sender_pid)) {
                 if msg.op == SWND_OP_UPDATE {
                    windows[w_idx].x = msg.x;
                    windows[w_idx].y = msg.y;
                 }
                 // COMMIT could be used for damage tracking later, for now we redraw every frame anyway
             }
        }
        _ => {}
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
    } else {
        // Broadcast fallback: send SUBS to PIDs 2-16.
        // input_service will recognize the "SUBS" prefix and register us.
        // Other services receiving it will ignore the unknown message type.
        println!("[SMITHAY] Broadcast SUBS to PIDs 2-16 as fallback...");
        for candidate in 2u32..=16 {
            if candidate != pid {
                subscribe_to_input_service(candidate, pid);
            }
        }
    }

    // Initialize IPC handler
    let mut ipc = IpcHandler::new();

    // Surfaces externas (clientes Sidewind)
    let mut surfaces: [ExternalSurface; MAX_EXTERNAL_SURFACES] = [const { ExternalSurface {
        id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false
    } }; MAX_EXTERNAL_SURFACES];

    // Ventanas (Z-order stack, máx 16)
    const MAX_WINDOWS_COUNT: usize = 16;
    let mut windows: [ShellWindow; MAX_WINDOWS_COUNT] = [const { ShellWindow {
        x: 0, y: 0, w: 0, h: 0, minimized: false, content: WindowContent::None
    } }; MAX_WINDOWS_COUNT];

    // Demo inicial
    windows[0] = ShellWindow {
        x: 100, y: 180, w: 250, h: 150 + ShellWindow::TITLE_H,
        minimized: false,
        content: WindowContent::InternalDemo,
    };
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
    let mut wayland_clients: alloc::vec::Vec<WaylandClient> = alloc::vec::Vec::new();
    let mut xwm = sidewind_xwayland::XwmState::new();

    loop {
        counter = counter.wrapping_add(1);
        let mut had_events = false;
        // Procesar todos los eventos IPC pendientes (cursor más fluido)
        // 2. Dibujar UI estática (Header/Footer simplificado)
        draw_static_ui(&mut fb);

        // 3. Procesar eventos IPC
        while let Some(event) = ipc.process_messages() {
            had_events = true;
            match event {
                CompositorEvent::Input(ev) => {
                    input_state.apply_event(
                        &ev,
                        fb.info.width as i32,
                        fb.info.height as i32,
                        &mut windows,
                        &mut window_count,
                        &surfaces,
                    );
                }
                CompositorEvent::SideWind(sw, sender_pid) => {
                    handle_sidewind_message(sw, sender_pid, &mut surfaces, &mut windows, &mut window_count);
                }
                CompositorEvent::Wayland(data, sender_pid) => {
                    // Find or create client
                    let client_idx = wayland_clients.iter().position(|c| c.pid == sender_pid)
                        .unwrap_or_else(|| {
                            wayland_clients.push(WaylandClient {
                                pid: sender_pid,
                                conn: sidewind_wayland::WaylandConnection::new(),
                            });
                            wayland_clients.len() - 1
                        });
                    
                    let client = &mut wayland_clients[client_idx];
                    let _ = client.conn.process_message(&data);
                    
                    // Flush all pending events to client
                    // In a real system, we might need a way to ensure the client is ready
                    // But here we use IPC send which is usually reliable for small chunks.
                    while !client.conn.pending_events.is_empty() {
                        let event_data = client.conn.pending_events.remove(0);
                        let _ = send(sender_pid, sidewind_core::MSG_TYPE_WAYLAND, &event_data);
                    }
                }
                CompositorEvent::X11(data, _sender_pid) => {
                    // Primitive XWM handling: if it's a "MAP" command
                    if data.len() >= 4 && &data[0..4] == b"MAP " {
                        // Very simplified: MAP <id>
                        // In reality would parse X11 MapRequest
                        println!("[SMITHAY] XWM: MapRequest received");
                        xwm.handle_map_request(0); // stub
                    }
                }
            }
        }

        // Tecla Escape: cerrar ventana enfocada
        if input_state.request_close_window && window_count > 0 {
            let to_close = input_state.focused_window
                .filter(|i| *i < window_count)
                .unwrap_or(window_count - 1);
            
            // Si es externa, avisar? (No implementado aún, solo destruir localmente)
            if let WindowContent::External(s_idx) = windows[to_close].content {
                if (s_idx as usize) < surfaces.len() {
                    munmap(surfaces[s_idx as usize].vaddr as u64, surfaces[s_idx as usize].buffer_size as u64);
                    surfaces[s_idx as usize].active = false;
                }
            }

            // Remove and shift
            for i in to_close..(window_count - 1) {
                windows[i] = windows[i+1];
            }
            windows[window_count - 1] = ShellWindow { x:0, y:0, w:0, h:0, minimized: false, content: WindowContent::None };
            window_count -= 1;
            
            input_state.dragging_window = None;
            input_state.focused_window = None;
            input_state.request_close_window = false;
        } else if input_state.request_close_window {
            input_state.request_close_window = false;
        }

        // Tab / `: ciclar focus
        if input_state.request_cycle_forward && window_count > 1 {
            let current = input_state.focused_window.unwrap_or(0);
            if let Some(next) = next_visible(current, true, &windows, window_count) {
                let top = window_count - 1;
                windows.swap(next, top);
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
                if i < window_count && windows[i].content != WindowContent::None && !windows[i].minimized {
                    windows[i].minimized = true;
                    input_state.focused_window = None;
                    input_state.dragging_window = None;
                }
            }
            input_state.request_minimize = false;
        }

        // R: restaurar última ventana minimizada
        if input_state.request_restore {
            if let Some(i) = (0..window_count).rev().find(|&i| windows[i].content != WindowContent::None && windows[i].minimized) {
                windows[i].minimized = false;
                let top = window_count - 1;
                windows.swap(i, top);
                input_state.focused_window = Some(top);
            }
            input_state.request_restore = false;
        }

        // Tecla N: nueva ventana
        if input_state.request_new_window && window_count < MAX_WINDOWS_COUNT {
            windows[window_count] = ShellWindow {
                x: 60 + (window_count as i32) * 20,
                y: 160 + (window_count as i32) * 15,
                w: 240,
                h: 180,
                minimized: false,
                content: WindowContent::InternalDemo,
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

        // Dibujar ventanas (incluye decoraciones y superficies externas)
        draw_shell_windows(&mut fb, &windows, window_count, input_state.focused_window, &surfaces);

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

        // 7. Draw cursor
        if use_hw_cursor {
            if input_state.cursor_x != last_hw_cursor_x || input_state.cursor_y != last_hw_cursor_y {
                last_hw_cursor_x = input_state.cursor_x;
                last_hw_cursor_y = input_state.cursor_y;
                let _ = set_cursor_position(input_state.cursor_x as u32, input_state.cursor_y as u32);
            }
        } else {
            input_state.draw_cursor(&mut fb);
        }

        // 8. Presentar frame (Copia back -> front)
        fb.present();

        // 9. Reducir CPU cuando idle
        if !had_events {
            for _ in 0..8 {
                yield_cpu();
            }
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
