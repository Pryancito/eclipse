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




use eclipse_libc::{println, getpid, send, receive, yield_cpu, get_framebuffer_info, map_framebuffer, FramebufferInfo, get_gpu_display_info, gpu_alloc_display_buffer, gpu_present, set_cursor_position, mmap, munmap, open, close, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS, MAP_SHARED, O_RDWR, InputEvent};
use sidewind_core::{SideWindMessage, SWND_OP_CREATE, SWND_OP_DESTROY, SWND_OP_UPDATE, SWND_OP_COMMIT, SideWindEvent, SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_MOUSE_MOVE, SWND_EVENT_TYPE_MOUSE_BUTTON, SWND_EVENT_TYPE_RESIZE, SIDEWIND_TAG};
use embedded_graphics::{
    pixelcolor::{Rgb888, RgbColor},
    prelude::*,
    primitives::{Rectangle, Circle, Line, Polyline, PrimitiveStyleBuilder, Arc},
    text::Text,
    mono_font::{ascii::FONT_6X10, ascii::FONT_10X20, MonoTextStyle},
    image::ImageRaw,
};
use micromath::F32Ext;

use sidewind_sdk::ui::{self, icons, colors, Notification, NotificationPanel, Taskbar, Widget};

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
    curr_x: f32, // Para animaciones
    curr_y: f32,
    curr_w: f32,
    curr_h: f32,
    minimized: bool,
    maximized: bool,
    stored_rect: (i32, i32, i32, i32),
    content: WindowContent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WindowButton {
    None,
    Minimize,
    Maximize,
    Close,
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
    
    fn check_button_click(&self, px: i32, py: i32) -> WindowButton {
        if !self.title_bar_contains(px, py) { return WindowButton::None; }
        
        let btn_y = self.y + (Self::TITLE_H - ui::BUTTON_ICON_SIZE as i32) / 2;
        let btn_margin = 5;
        let btn_size = ui::BUTTON_ICON_SIZE as i32;
        
        if py < btn_y || py >= btn_y + btn_size { return WindowButton::None; }
        
        // Close button
        let close_x = self.x + self.w - btn_size - btn_margin;
        if px >= close_x && px < close_x + btn_size { return WindowButton::Close; }
        
        // Maximize button
        let max_x = close_x - btn_size - btn_margin;
        if px >= max_x && px < max_x + btn_size { return WindowButton::Maximize; }
        
        // Minimize button
        let min_x = max_x - btn_size - btn_margin;
        if px >= min_x && px < min_x + btn_size { return WindowButton::Minimize; }
        
        WindowButton::None
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
const CURSOR_SIZE: i32 = 24;
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
    /// Alternar Dashboard (Super)
    ToggleDashboard,
    /// Alternar Bloqueo (Super+L)
    ToggleLock,
    /// Alternar Notificaciones (Super+V)
    ToggleNotifications,
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
        0x5B => KeyAction::ToggleDashboard, // Left Meta/Super
        0x26 => if true { KeyAction::ToggleLock } else { KeyAction::None }, // L (need modifiers check)
        0x2F => if true { KeyAction::ToggleNotifications } else { KeyAction::None }, // V
        _ => KeyAction::None,
    }
}

/// Paleta de colores para dibujar (índice 0–4)
const STROKE_COLORS: [Rgb888; 5] = [
    colors::ACCENT_BLUE,
    colors::ACCENT_RED,
    colors::ACCENT_GREEN,
    colors::ACCENT_YELLOW,
    colors::WHITE,
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
    request_maximize: bool,
    request_restore: bool,
    dragging_window: Option<usize>,
    resizing_window: Option<usize>,
    drag_offset_x: i32,
    drag_offset_y: i32,
    focused_window: Option<usize>,
    modifiers: u32,
    request_dashboard: bool,
    dashboard_active: bool,
    lock_active: bool,
    notifications_active: bool,
    notifications: [Option<sidewind_sdk::ui::Notification>; 5],
    
    // Phase 8: Overlays
    launcher_active: bool,
    quick_settings_active: bool,
    context_menu_active: bool,
    context_menu_pos: Point,
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
            request_maximize: false,
            request_restore: false,
            dragging_window: None,
            resizing_window: None,
            drag_offset_x: 0,
            drag_offset_y: 0,
            focused_window: None,
            modifiers: 0,
            request_dashboard: false,
            dashboard_active: false,
            lock_active: false,
            notifications_active: false,
            notifications: [
                Some(sidewind_sdk::ui::Notification { title: "SISTEMA", body: "Núcleos óptimos.", icon_type: 0 }),
                Some(sidewind_sdk::ui::Notification { title: "SEGURIDAD", body: "Encriptación activa.", icon_type: 0 }),
                None, None, None
            ],
            launcher_active: false,
            quick_settings_active: false,
            context_menu_active: false,
            context_menu_pos: Point::new(0, 0),
        }
    }

    fn apply_event(&mut self, ev: &InputEvent, fb_width: i32, fb_height: i32, windows: &mut [ShellWindow], window_count: &mut usize, surfaces: &[ExternalSurface]) {
        match ev.event_type {
            // Keyboard: usar KeyAction (ref: xfwl4 process_common_key_action)
            0 => {
                let pressed = ev.value == 1;

                // Update Modifiers (Make = 1, Break = 0)
                match ev.code {
                    0x2A | 0x36 => { if pressed { self.modifiers |= 1; } else { self.modifiers &= !1; } } // Shift
                    0x1D => { if pressed { self.modifiers |= 2; } else { self.modifiers &= !2; } } // Ctrl
                    0x38 => { if pressed { self.modifiers |= 4; } else { self.modifiers &= !4; } } // Alt
                    0x5B => { if pressed { self.modifiers |= 8; } else { self.modifiers &= !8; } } // Super
                    _ => {}
                }

                let action = if self.modifiers & (4 | 8) != 0 {
                    scancode_to_action(ev.code)
                } else {
                    KeyAction::None
                };
                
                match action {
                    KeyAction::None => {
                        // Forward ALL keys (Make AND Break) to client if focused
                        if let Some(f_idx) = self.focused_window {
                            if let WindowContent::External(s_idx) = windows[f_idx].content {
                                let pid = surfaces[s_idx as usize].pid;
                                let event = SideWindEvent {
                                    event_type: SWND_EVENT_TYPE_KEY,
                                    data1: ev.code as i32,
                                    data2: ev.value as i32,
                                    data3: self.modifiers as i32,
                                };
                                let _ = send(pid, MSG_TYPE_INPUT, unsafe {
                                    core::slice::from_raw_parts(&event as *const _ as *const u8, core::mem::size_of::<SideWindEvent>())
                                });
                            }
                        }
                    }
                    KeyAction::Clear => if pressed { self.request_clear = true; },
                    KeyAction::SetColor(c) => if pressed { self.stroke_color = c.min(4); },
                    KeyAction::CycleStrokeSize => if pressed {
                        self.stroke_size = match self.stroke_size {
                            2 => 4,
                            4 => 6,
                            _ => 2,
                        };
                    },
                    KeyAction::SensitivityPlus => if pressed { self.mouse_sensitivity = (self.mouse_sensitivity + 25).min(200); },
                    KeyAction::SensitivityMinus => if pressed { self.mouse_sensitivity = (self.mouse_sensitivity - 25).max(50); },
                    KeyAction::InvertY => if pressed { self.invert_y = !self.invert_y; },
                    KeyAction::CenterCursor => if pressed { self.request_center_cursor = true; },
                    KeyAction::NewWindow => if pressed { self.request_new_window = true; },
                    KeyAction::CloseWindow => if pressed { self.request_close_window = true; },
                    KeyAction::CycleForward => if pressed { self.request_cycle_forward = true; },
                    KeyAction::CycleBackward => if pressed { self.request_cycle_backward = true; },
                    KeyAction::Minimize => if pressed { self.request_minimize = true; },
                    KeyAction::Restore => if pressed { self.request_restore = true; },
                    KeyAction::ToggleDashboard => if pressed && self.modifiers == 8 { self.request_dashboard = true; },
                    KeyAction::ToggleLock => if pressed && (self.modifiers & 8 != 0) { self.lock_active = !self.lock_active; },
                    KeyAction::ToggleNotifications => if pressed && (self.modifiers & 8 != 0) { self.notifications_active = !self.notifications_active; },
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
                    // Clicks on UI elements close any open overlays
                    let mut click_handled = false;
                    
                    self.launcher_active = false;
                    self.quick_settings_active = false;
                    self.context_menu_active = false;

                    // 1. Taskbar Hit-Test (Bottom 40px)
                    if self.cursor_y >= fb_height - 40 {
                        click_handled = true;
                        if self.cursor_x < 150 {
                            // Clicked "Start/Eclipse" button
                            self.launcher_active = true;
                        } else if self.cursor_x > fb_width - 150 {
                            // Clicked Quick Settings area
                            self.quick_settings_active = true;
                        }
                    }

                    if !click_handled {
                        if let Some(idx) = focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count) {
                            // Mover al frente
                            let top = *window_count - 1;
                            if idx != top {
                                windows.swap(idx, top);
                            }
                            let win_idx = top;
                            self.focused_window = Some(win_idx);

                            // Check buttons
                            let btn = windows[win_idx].check_button_click(self.cursor_x, self.cursor_y);
                            match btn {
                                WindowButton::Close => self.request_close_window = true,
                                WindowButton::Minimize => self.request_minimize = true,
                                WindowButton::Maximize => self.request_maximize = true,
                                WindowButton::None => {
                                    // Arrastrar o Redimensionar si no es un botón
                                    if windows[win_idx].title_bar_contains(self.cursor_x, self.cursor_y) {
                                        self.dragging_window = Some(win_idx);
                                        self.drag_offset_x = self.cursor_x - windows[win_idx].x;
                                        self.drag_offset_y = self.cursor_y - windows[win_idx].y;
                                    } else if windows[win_idx].resize_handle_contains(self.cursor_x, self.cursor_y) {
                                        self.resizing_window = Some(win_idx);
                                    }
                                }
                            }
                        } else {
                            self.focused_window = None;
                            
                            // Check for desktop minimized icons click
                            let mut min_count = 0;
                            for i in 0..*window_count {
                                if windows[i].content != WindowContent::None && windows[i].minimized {
                                    let px = 100 + (min_count % 3) * 120;
                                    let py = 250 + (min_count / 3) * 150;
                                    // Simple hit detection for the icon (radius ~40)
                                    let dx = self.cursor_x - px;
                                    let dy = self.cursor_y - py;
                                    if dx*dx + dy*dy < 40*40 {
                                        // Restaurar esta ventana
                                        windows[i].minimized = false;
                                        let top = *window_count - 1;
                                        windows.swap(i, top);
                                        self.focused_window = Some(top);
                                        break;
                                    }
                                    min_count += 1;
                                }
                            }
                        }
                    }
                } else if (self.mouse_buttons & 2 != 0) && (old_buttons & 2 == 0) {
                    // Right Click
                    self.launcher_active = false;
                    self.quick_settings_active = false;
                    
                    if focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count).is_none() {
                        // Clicked on desktop background
                        self.context_menu_active = true;
                        self.context_menu_pos = Point::new(self.cursor_x, self.cursor_y);
                    } else {
                        self.context_menu_active = false;
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

        // Blit manual con "transparency key" (negro = transparente)
        let pitch_px = (fb.info.pitch / 4).max(fb.info.width as u32) as i32;
        let dst_ptr = fb.base_addr as *mut u32;
        let c_size = CURSOR_SIZE; // cursor.bin is 24x24 pixels

        for cy in 0..c_size {
            let dy = y + cy;
            if dy < 0 || dy >= height { continue; }
            for cx in 0..c_size {
                let dx = x + cx;
                if dx < 0 || dx >= width { continue; }
                let src_idx = ((cy * c_size) + cx) as usize * 3;
                let r = icons::CURSOR[src_idx] as u32;
                let g = icons::CURSOR[src_idx + 1] as u32;
                let b = icons::CURSOR[src_idx + 2] as u32;
                
                // Key de transparencia: si es muy oscuro (fondo negro de DALL-E), saltar
                if r < 20 && g < 20 && b < 20 { continue; }
                
                let raw = 0xFF000000 | (r << 16) | (g << 8) | b;
                let offset = (dy * pitch_px + dx) as usize;
                unsafe {
                    core::ptr::write_volatile(dst_ptr.add(offset), raw);
                }
            }
        }
    }
}

/// Framebuffer state
struct FramebufferState {
    info: FramebufferInfo,
    base_addr: usize,   // drawing target (back buffer or VirtIO GPU buffer)
    front_addr: usize,  // physical framebuffer (legacy FB only; 0 when VirtIO GPU)
    gpu_resource_id: Option<u32>,  // Some(resource_id) when using VirtIO GPU 2D
}

impl FramebufferState {
    /// Initialize display: prefer VirtIO GPU 2D (double buffer, hw cursor), fall back to legacy framebuffer
    fn init() -> Option<Self> {
        println!("[SMITHAY] Initializing display...");

        // Try VirtIO GPU first
        let mut dims = [0u32, 0u32];
        let has_gpu = get_gpu_display_info(&mut dims);
        println!("[SMITHAY]   - get_gpu_display_info: {} dims={}x{}", has_gpu, dims[0], dims[1]);
        if has_gpu && dims[0] > 0 && dims[1] > 0 {
            let gpu_opt = gpu_alloc_display_buffer(dims[0], dims[1]);
            println!("[SMITHAY]   - gpu_alloc_display_buffer: {}",
                if gpu_opt.is_some() { "OK" } else { "FAIL" });
            if let Some(gpu_info) = gpu_opt {
                if gpu_info.vaddr >= 0x1000 {
                    println!("[SMITHAY]   - VirtIO GPU: {}x{} pitch={} resource={}",
                        dims[0], dims[1], gpu_info.pitch, gpu_info.resource_id);
                    let info = FramebufferInfo {
                        address: 0,
                        width: dims[0],
                        height: dims[1],
                        pitch: if gpu_info.pitch > 0 { gpu_info.pitch } else { dims[0] * 4 },
                        bpp: 32,
                        red_mask_size: 8,
                        red_mask_shift: 16,
                        green_mask_size: 8,
                        green_mask_shift: 8,
                        blue_mask_size: 8,
                        blue_mask_shift: 0,
                    };
                    return Some(FramebufferState {
                        info,
                        base_addr: gpu_info.vaddr as usize,
                        front_addr: 0,
                        gpu_resource_id: Some(gpu_info.resource_id),
                    });
                } else {
                    println!("[SMITHAY]   - VirtIO GPU vaddr invalid: 0x{:x}", gpu_info.vaddr);
                }
            }
        }

        // Fall back to legacy framebuffer
        println!("[SMITHAY]   - Using legacy framebuffer");
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
                println!("[SMITHAY]   - Framebuffer mapped at 0x{:x}", base);
                base
            }
            None => {
                println!("[SMITHAY]   - ERROR: Failed to map framebuffer");
                return None;
            }
        };

        let pitch = if fb_info.pitch > 0 { fb_info.pitch } else { fb_info.width * 4 };
        let fb_size = (pitch as u64) * (fb_info.height as u64);
        let back_buffer = mmap(0, fb_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

        if back_buffer == 0 || back_buffer == u64::MAX {
            println!("[SMITHAY]   - ERROR: Failed to allocate back buffer ({} bytes)", fb_size);
            return None;
        }

        let mut info = fb_info;
        info.address = fb_base as u64;

        Some(FramebufferState {
            info,
            base_addr: back_buffer as usize,
            front_addr: fb_base,
            gpu_resource_id: None,
        })
    }

    /// Fill entire back buffer with a solid color (raw, bypasses DrawTarget).
    /// Ensures we fully overwrite any previous content including boot logo.
    fn clear_back_buffer_raw(&self, color: Rgb888) {
        if self.base_addr < 0x1000 { return; }
        let pitch = self.info.pitch.max(self.info.width * 4);
        let width_px = self.info.width as usize;
        let height = self.info.height as usize;
        let pitch_px = (pitch / 4) as usize;
        let raw = 0xFF000000
            | ((color.r() as u32) << 16)
            | ((color.g() as u32) << 8)
            | (color.b() as u32);
        let ptr = self.base_addr as *mut u32;
        for y in 0..height {
            let row_start = y * pitch_px;
            for x in 0..width_px {
                unsafe {
                    core::ptr::write_volatile(ptr.add(row_start + x), raw);
                }
            }
        }
    }

    /// Present frame: VirtIO GPU (transfer+flush) or legacy copy
    fn present(&self) {
        if self.base_addr < 0x1000 { return; }
        let w = self.info.width;
        let h = self.info.height;
        if let Some(rid) = self.gpu_resource_id {
            let _ = gpu_present(rid, 0, 0, w, h);
        } else if self.front_addr >= 0x1000 {
            let pitch = self.info.pitch.max(self.info.width * 4);
            let size_bytes = (pitch as usize).saturating_mul(self.info.height as usize);
            const MAX_FB_COPY: usize = 3840 * 2160 * 4;
            let size_bytes = size_bytes.min(MAX_FB_COPY);
            if size_bytes > 0 {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.base_addr as *const u8,
                        self.front_addr as *mut u8,
                        size_bytes,
                    );
                }
            }
        }
    }

    /// Dibuja una cruz (vertical + horizontal) con escrituras directas; evita embedded_graphics
    fn draw_cross_raw(&mut self, cx: i32, cy: i32, half: i32, raw_color: u32) {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        if self.base_addr < 0x1000 { return; }
        let ptr = self.base_addr as *mut u32;
        // Vertical: (cx, cy-half) .. (cx, cy+half)
        for py in (cy - half)..=(cy + half) {
            if py >= 0 && py < height {
                let offset = (py * pitch_px + cx) as usize;
                unsafe { core::ptr::write_volatile(ptr.add(offset), raw_color); }
            }
        }
        // Horizontal: (cx-half, cy) .. (cx+half, cy)
        for px in (cx - half)..=(cx + half) {
            if px >= 0 && px < width {
                let offset = (cy * pitch_px + px) as usize;
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
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let max_pixels = (pitch_px as usize).saturating_mul(height as usize);
        let fb_ptr = self.base_addr as *mut u32;

        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < width && coord.y >= 0 && coord.y < height {
                let offset = (coord.y as usize).saturating_mul(pitch_px as usize).saturating_add(coord.x as usize);
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
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
        let fb_ptr = self.base_addr as *mut u32;

        let intersection = area.intersection(&Rectangle::new(Point::new(0, 0), Size::new(width as u32, height as u32)));
        if intersection.is_zero_sized() {
            return Ok(());
        }

        let raw_color = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
        
        for y in intersection.top_left.y..intersection.top_left.y + intersection.size.height as i32 {
            let offset_start = (y as usize * pitch_px as usize) + intersection.top_left.x as usize;
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
        let pitch_px = (self.info.pitch / 4).max(width as u32) as i32;
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
                        let offset = (y as usize * pitch_px as usize) + x as usize;
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

/// Dibuja el Dashboard de Eclipse OS (Fase 6)
fn draw_dashboard(fb: &mut FramebufferState, counter: u64) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    
    // 1. Fondo semitransparente (Oscurecer escritorio)
    let _ = Rectangle::new(Point::new(0, 0), Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(2, 4, 10)).build())
        .draw(fb);

    // 2. Efecto de "Grid" activo
    let _ = ui::draw_grid(fb, Rgb888::new(30, 60, 120), 64);
    
    // 3. Paneles de Widgets
    use sidewind_sdk::ui::{Panel, Gauge, Widget};

    let p_w = 600;
    let p_h = 400;
    let px = (w - p_w) / 2;
    let py = (h - p_h) / 2;

    // Panel Central: System Monitor
    let main_panel = Panel {
        position: Point::new(px, py),
        size: Size::new(p_w as u32, p_h as u32),
        title: "ANALISIS DE SISTEMA // DASHBOARD",
    };
    let _ = main_panel.draw(fb);

    // Widgets dentro del panel
    let g1 = Gauge {
        center: main_panel.position + Point::new(120, 180),
        radius: 70,
        value: 0.45 + (counter as f32 / 50.0).sin() * 0.1, // Animado
        label: "CARGA CPU",
    };
    let _ = g1.draw(fb);

    let g2 = Gauge {
        center: main_panel.position + Point::new(300, 180),
        radius: 70,
        value: 0.72 + (counter as f32 / 80.0).cos() * 0.05,
        label: "MEMORIA VRAM",
    };
    let _ = g2.draw(fb);

    let g3 = Gauge {
        center: main_panel.position + Point::new(480, 180),
        radius: 70,
        value: 0.15 + (counter as f32 / 30.0).sin() * 0.05,
        label: "RED UP",
    };
    let _ = g3.draw(fb);

    // 4. Terminal mockup
    use sidewind_sdk::ui::Terminal;
    let term = Terminal {
        position: main_panel.position + Point::new(30, 240),
        size: Size::new(p_w as u32 - 60, 130),
    };
    let _ = term.draw(fb);

    // 5. Texto informativo inferior
    let label_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
    let _ = Text::new("PRESIONE 'SUPER' PARA VOLVER AL ESCRITORIO", Point::new(w / 2 - 200, h - 100), label_style).draw(fb);
}

/// Dibuja las ventanas (focused = borde más brillante). Omitimos minimizadas.
/// Dibuja la Pantalla de Bloqueo (Fase 7)
fn draw_lock_screen(fb: &mut FramebufferState, counter: u64) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let center = Point::new(w / 2, h / 2);

    // 1. Fondo negro profundo
    let _ = fb.clear(colors::BACKGROUND_DEEP);

    // 2. Logo central masivo (ECLIPSE)
    let _ = ui::draw_eclipse_logo(fb, center);

    // 3. Texto de estado bloqueado
    let label_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
    let _ = Text::new("SISTEMA BLOQUEADO", center + Point::new(-90, 220), label_style).draw(fb);
    
    // 4. Reloj
    let time_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    let _ = Text::new("20:42:15", center + Point::new(-45, -280), time_style).draw(fb);

    // 5. Ticker inferior
    if (counter / 30) % 2 == 0 {
        let _ = Text::new("> DESBLOQUEAR CON SUPER + L <", center + Point::new(-160, h / 2 - 50), label_style).draw(fb);
    }
}

/// Dibuja el Centro de Notificaciones (Fase 7)
fn draw_notifications(fb: &mut FramebufferState, notifications: &[Option<sidewind_sdk::ui::Notification>]) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    
    let p_w = 300;
    let mut active_notifs = alloc::vec::Vec::new();
    for n in notifications {
        if let Some(val) = n {
            active_notifs.push(*val);
        }
    }

    let panel = NotificationPanel {
        position: Point::new(w - p_w, 80), 
        size: Size::new(p_w as u32, h as u32 - 160),
        notifications: &active_notifs,
    };
    let _ = panel.draw(fb);
}

/// Dibuja el App Launcher (Fase 8)
fn draw_launcher(fb: &mut FramebufferState) {
    let h = fb.info.height as i32;
    // Launcher panel bottom-left
    let rect = Rectangle::new(Point::new(10, h - 350), Size::new(300, 300));
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(10, 15, 30)).stroke_color(colors::GLOW_MID).stroke_width(1).build();
    let _ = rect.into_styled(bg_style).draw(fb);

    let title_style = MonoTextStyle::new(&FONT_10X20, colors::ACCENT_BLUE);
    let item_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);

    let _ = Text::new("SISTEMA CENTRAL", Point::new(25, h - 320), title_style).draw(fb);
    let _ = Line::new(Point::new(10, h - 300), Point::new(310, h - 300)).into_styled(bg_style).draw(fb);

    let items = ["1. Terminal Local", "2. Monitor de Recursos", "3. Gestor de Archivos", "4. Configuración", "5. Desconexión"];
    for (i, item) in items.iter().enumerate() {
        let _ = ui::draw_glowing_hexagon(fb, Point::new(40, h - 260 + (i as i32 * 40)), 15, colors::ACCENT_BLUE);
        let _ = Text::new(item, Point::new(70, h - 253 + (i as i32 * 40)), item_style).draw(fb);
    }
}

/// Dibuja el Quick Settings (Fase 8)
fn draw_quick_settings(fb: &mut FramebufferState) {
    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    // Quick settings bottom-right
    let rect = Rectangle::new(Point::new(w - 260, h - 210), Size::new(250, 160));
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(15, 20, 35)).stroke_color(colors::GLOW_MID).stroke_width(1).build();
    let _ = rect.into_styled(bg_style).draw(fb);

    let text_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    let _ = Text::new("RED: [ESTABLE]", Point::new(w - 240, h - 170), text_style).draw(fb);
    let _ = Text::new("VOL: [|||||   ] 60%", Point::new(w - 240, h - 130), text_style).draw(fb);
    let _ = Text::new("ENRG:[OPTIMO ] 92%", Point::new(w - 240, h - 90), text_style).draw(fb);
}

/// Dibuja el Desktop Context Menu (Fase 8)
fn draw_context_menu(fb: &mut FramebufferState, pos: Point) {
    let rect = Rectangle::new(pos, Size::new(200, 150));
    let bg_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(5, 5, 10)).stroke_color(colors::GLOW_DIM).stroke_width(1).build();
    let _ = rect.into_styled(bg_style).draw(fb);

    let text_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    let items = ["Nueva Ventana", "Configurar Fondo", "Cambiar Tema", "Propiedades"];
    for (i, item) in items.iter().enumerate() {
        let _ = Text::new(item, pos + Point::new(15, 30 + (i as i32 * 35)), text_style).draw(fb);
    }
}

fn draw_shell_windows(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, focused_window: Option<usize>, surfaces: &[ExternalSurface]) {
    for (i, w) in windows.iter().take(window_count).enumerate() {
        if w.content == WindowContent::None { continue; }
        // Si está minimizado Y ya se encogió mucho (animación terminada), dejar de dibujar aquí
        // para que solo se vea el icono del escritorio
        if w.minimized && w.curr_w < 50.0 { continue; }
        
        let is_focused = focused_window == Some(i);
        
        // 1. Draw Frame
        draw_window_decoration(fb, w, is_focused);
        
        // 2. Draw Content (Solo si no es demasiado pequeña)
        if w.curr_w > 100.0 {
            match w.content {
                WindowContent::InternalDemo => {
                    let text_style = MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE);
                    let _ = Text::new("Internal Window", Point::new(w.curr_x as i32 + 10, w.curr_y as i32 + ShellWindow::TITLE_H + 30), text_style)
                        .draw(fb);
                }
                WindowContent::External(idx) => {
                    if (idx as usize) < surfaces.len() && surfaces[idx as usize].active {
                        draw_surface_content(fb, w, &surfaces[idx as usize]);
                    }
                }
                WindowContent::None => {}
            }
        }
    }
}

fn draw_window_decoration(fb: &mut FramebufferState, w: &ShellWindow, is_focused: bool) {
    let wx = w.curr_x as i32;
    let wy = w.curr_y as i32;
    let ww = w.curr_w as i32;
    let wh = w.curr_h as i32;
    
    // Window Border/Glow
    let accent = if is_focused { colors::ACCENT_BLUE } else { Rgb888::new(40, 60, 100) };
    let border_style = PrimitiveStyleBuilder::new()
        .stroke_color(accent)
        .stroke_width(2)
        .fill_color(colors::PANEL_BG)
        .build();
    
    let _ = Rectangle::new(Point::new(wx, wy), Size::new(ww as u32, wh as u32))
        .into_styled(border_style)
        .draw(fb);

    // Title Bar
    let title_bg = colors::TITLE_BAR_BG;
    let title_style = PrimitiveStyleBuilder::new().fill_color(title_bg).build();
    let _ = Rectangle::new(Point::new(wx, wy), Size::new(ww as u32, ShellWindow::TITLE_H as u32))
        .into_styled(title_style)
        .draw(fb);

    // Title text (Solo si hay espacio)
    if ww > 100 {
        let text_style = MonoTextStyle::new(&FONT_6X10, colors::WHITE);
        let _ = Text::new("ECLIPSE // TERMINAL", Point::new(wx + 10, wy + 20), text_style).draw(fb);
    }

    // Window Buttons (Right aligned in title bar - solo si hay espacio)
    if ww > 80 {
        let btn_y = wy + (ShellWindow::TITLE_H - ui::BUTTON_ICON_SIZE as i32) / 2;
        let btn_margin = 5;
        
        // Close button
        let close_x = wx + ww - ui::BUTTON_ICON_SIZE as i32 - btn_margin;
        let _ = ui::draw_button_icon(fb, Point::new(close_x, btn_y), icons::BTN_CLOSE);
        
        // Maximize button
        let max_x = close_x - ui::BUTTON_ICON_SIZE as i32 - btn_margin;
        let _ = ui::draw_button_icon(fb, Point::new(max_x, btn_y), icons::BTN_MAX);
        
        // Minimize button
        let min_x = max_x - ui::BUTTON_ICON_SIZE as i32 - btn_margin;
        let _ = ui::draw_button_icon(fb, Point::new(min_x, btn_y), icons::BTN_MIN);
    }

    // Resize Handle (bottom-right)
    let handle_style = PrimitiveStyleBuilder::new()
        .stroke_color(accent)
        .stroke_width(1)
        .build();
    let _ = Rectangle::new(
        Point::new(wx + ww - ShellWindow::RESIZE_HANDLE_SIZE, wy + wh - ShellWindow::RESIZE_HANDLE_SIZE),
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

    let content_y = w.curr_y as i32 + ShellWindow::TITLE_H;
    let content_w = w.curr_w as i32;
    let content_h = (w.curr_h as i32 - ShellWindow::TITLE_H).max(0);

    let dst_ptr = fb.base_addr as *mut u32;

    for py in 0..content_h {
        let dy = content_y + py;
        if dy < 0 || dy >= fb_h { continue; }
        let mut start_x = 0;
        let mut end_x = content_w;
        let wx = w.curr_x as i32;
        if wx + start_x < 0 { start_x = -wx; }
        if wx + end_x > fb_w { end_x = fb_w - wx; }
        if start_x >= end_x { continue; }
        let dst_offset = (dy as usize * pitch_px as usize) + (wx + start_x) as usize;
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


/// Dibuja el fondo futurista de Eclipse OS y las apps minimizadas
fn draw_static_ui(fb: &mut FramebufferState, windows: &[ShellWindow], window_count: usize, counter: u64) {
    // 1. Fondo cósmico: gradiente + nebulosas
    let _ = ui::draw_cosmic_background(fb);

    // 2. Campo de estrellas (cosmic enhanced)
    let mut star_seed = 0xACE1u32;
    let _ = ui::draw_starfield_cosmic(fb, &mut star_seed);

    let w = fb.info.width as i32;
    let h = fb.info.height as i32;
    let center = Point::new(w / 2, h / 2);

    // 3. Grid sutil (cosmic)
    let _ = ui::draw_grid(fb, Rgb888::new(18, 28, 55), 48);

    // 3. LOGO CENTRAL "ECLIPSE OS" (SDK)
    let _ = ui::draw_eclipse_logo(fb, center);

    // 4. ICONOS HEXAGONALES (Sistema, Apps, Archivos, Red)
    let hex_color = Rgb888::new(100, 200, 255);
    let hex_size = 50;
    
    // System
    let p_sys = center + Point::new(-380, -120);
    let _ = ui::draw_glowing_hexagon(fb, p_sys, hex_size, hex_color);
    let _ = ui::draw_standard_icon(fb, p_sys, icons::SYSTEM);
    
    // Apps
    let p_apps = center + Point::new(-380, 120);
    let _ = ui::draw_glowing_hexagon(fb, p_apps, hex_size, hex_color);
    let _ = ui::draw_standard_icon(fb, p_apps, icons::APPS);
    
    // Files
    let p_files = center + Point::new(380, -120);
    let _ = ui::draw_glowing_hexagon(fb, p_files, hex_size, hex_color);
    let _ = ui::draw_standard_icon(fb, p_files, icons::FILES);
    
    // Network
    let p_net = center + Point::new(380, 120);
    let _ = ui::draw_glowing_hexagon(fb, p_net, hex_size, hex_color);
    let _ = ui::draw_standard_icon(fb, p_net, icons::NETWORK);
    
    // Etiquetas (Calidad HUD - Nombres en Español + Tamaño Mayor)
    let label_style = MonoTextStyle::new(&FONT_10X20, Rgb888::new(180, 220, 255));
    let _ = Text::new("SISTEMA", p_sys + Point::new(-35, 85), label_style).draw(fb);
    let _ = Text::new("APLICACIONES", p_apps + Point::new(-60, 85), label_style).draw(fb);
    let _ = Text::new("ARCHIVOS", p_files + Point::new(-40, 85), label_style).draw(fb);
    let _ = Text::new("RED", p_net + Point::new(-15, 85), label_style).draw(fb);

    // 5. HUD SUPERIOR (glass panels)
    let hud_line_style = PrimitiveStyleBuilder::new().stroke_color(colors::GLASS_BORDER).stroke_width(1).build();
    let hud_bg = colors::GLASS_PANEL;
    
    // Panel Izquierdo
    let _ = Rectangle::new(Point::new(15, 15), Size::new(240, 50)).into_styled(PrimitiveStyleBuilder::new().fill_color(hud_bg).build()).draw(fb);
    // Brackets
    let _ = Line::new(Point::new(15, 15), Point::new(35, 15)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(15, 15), Point::new(15, 35)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(255, 65), Point::new(235, 65)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(255, 65), Point::new(255, 45)).into_styled(hud_line_style).draw(fb);
    let _ = Text::new("APLICACIONES ACTIVAS", Point::new(30, 45), label_style).draw(fb);

    // Panel Derecho
    let rx = w - 255;
    let _ = Rectangle::new(Point::new(rx, 15), Size::new(240, 50)).into_styled(PrimitiveStyleBuilder::new().fill_color(hud_bg).build()).draw(fb);
    let _ = Line::new(Point::new(w - 15, 15), Point::new(w - 35, 15)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(w - 15, 15), Point::new(w - 15, 35)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(rx, 65), Point::new(rx + 20, 65)).into_styled(hud_line_style).draw(fb);
    let _ = Line::new(Point::new(rx, 65), Point::new(rx, 45)).into_styled(hud_line_style).draw(fb);
    // Dynamic status text (Phase 5)
    let _seconds = counter / 30; // Approx 30 FPS
    let _h_ = (_seconds / 3600) % 24;
    let _m_ = (_seconds / 60) % 60;
    let _s_ = _seconds % 60;
    let mut _time_buf = [0u8; 32];
    let _time_str = {
        let mut _idx = 0;
        let _labels = ["TIEMPO: ", "0", "0", ":", "0", "0", ":", "0", "0"];
        let _vals = [_h_ / 10, _h_ % 10, _m_ / 10, _m_ % 10, _s_ / 10, _s_ % 10];
        // Hardcoded simple formatter for no_std
        unsafe { core::slice::from_raw_parts(b"TIEMPO: 00:42:15".as_ptr(), 16) }
    };
    let dot = if (counter / 15) % 2 == 0 { "*" } else { " " };
    let mut _hud_text = [0u8; 24];
    let hud_str = "SISTEMA ONLINE ";
    let _ = Text::new(hud_str, Point::new(rx + 20, 45), label_style).draw(fb);
    let _ = Text::new(dot, Point::new(rx + 200, 45), label_style).draw(fb);

    // 6. TASKBAR (SDK Widget) - cosmic dock
    let taskbar = Taskbar {
        width: fb.info.width as u32,
        y: h - 44,
        active_app: None,
    };
    let _ = taskbar.draw(fb);
    
    let help_style = MonoTextStyle::new(&FONT_10X20, colors::WHITE);
    let _ = Text::new("SUPER: Dash | SUPER+L: Lock | SUPER+V: Notifs", Point::new(w - 450, h - 15), help_style).draw(fb);

    // 7. Iconos de apps minimizadas
    let mut min_count = 0;
    for i in 0..window_count {
        if windows[i].content != WindowContent::None && windows[i].minimized {
            // Dibujar icono pequeño en el escritorio
            let p = Point::new(100 + (min_count % 3) * 120, 250 + (min_count / 3) * 150);
            let _ = ui::draw_glowing_hexagon(fb, p, 35, colors::ACCENT_BLUE);
            
            // Icono genérico o según contenido
            match windows[i].content {
                WindowContent::External(_) => { let _ = ui::draw_standard_icon(fb, p, icons::APPS); },
                _ => { let _ = ui::draw_standard_icon(fb, p, icons::SYSTEM); },
            }
            
            let label = if let WindowContent::External(_) = windows[i].content { "APP" } else { "DEMO" };
            let _ = Text::new(label, p + Point::new(-15, 60), label_style).draw(fb);
            
            min_count += 1;
        }
    }
}

fn draw_icon_small(fb: &mut FramebufferState, center: Point, raw_data: &[u8], size: u32) {
    let raw = ImageRaw::<Rgb888>::new(raw_data, ui::STANDARD_ICON_SIZE); // original is 64x64
    let top_left = center - Point::new(size as i32 / 2, size as i32 / 2);
    // embedded-graphics doesn't scale ImageRaw easily without extra crates or manual loop
    // For now we'll just draw it; if it's too big, we'd need a scaled version
    let _ = embedded_graphics::image::Image::new(&raw, top_left).draw(fb);
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
                        curr_x: msg.x as f32,
                        curr_y: msg.y as f32,
                        curr_w: msg.w as f32,
                        curr_h: (msg.h as i32 + ShellWindow::TITLE_H) as f32,
                        minimized: false,
                        maximized: false,
                        stored_rect: (msg.x, msg.y, msg.w as i32, msg.h as i32 + ShellWindow::TITLE_H),
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
        x:0, y:0, w:0, h:0, curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0, minimized: false, maximized: false, stored_rect: (0,0,0,0), content: WindowContent::None
    } }; MAX_WINDOWS_COUNT];

    // Demo inicial
    windows[0] = ShellWindow {
        x: 100, y: 100, w: 400, h: 300,
        curr_x: 100.0, curr_y: 100.0, curr_w: 400.0, curr_h: 300.0,
        minimized: false,
        maximized: false,
        stored_rect: (100, 100, 400, 300),
        content: WindowContent::InternalDemo,
    };
    let mut window_count: usize = 1;

    // Cursor: con VirtIO GPU el hw cursor no se dibuja correctamente en QEMU,
    // usar siempre cursor software (draw_cursor) que sí funciona.
    let use_hw_cursor = false;

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
        // Eliminado de aquí, se dibuja después de las animaciones en el loop

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

        // Tecla Escape o Botón Close: cerrar ventana enfocada
        if input_state.request_close_window && window_count > 0 {
            let to_close = input_state.focused_window
                .filter(|i| *i < window_count)
                .unwrap_or(window_count - 1);
            
            if let WindowContent::External(s_idx) = windows[to_close].content {
                if (s_idx as usize) < surfaces.len() {
                    munmap(surfaces[s_idx as usize].vaddr as u64, surfaces[s_idx as usize].buffer_size as u64);
                    surfaces[s_idx as usize].active = false;
                }
            }

            for i in to_close..(window_count - 1) {
                windows[i] = windows[i+1];
            }
            windows[window_count - 1] = ShellWindow { 
                x:0, y:0, w:0, h:0, 
                curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
                minimized: false, maximized: false, stored_rect: (0,0,0,0), content: WindowContent::None 
            };
            window_count -= 1;
            
            input_state.dragging_window = None;
            input_state.focused_window = None;
            input_state.request_close_window = false;
        } else if input_state.request_close_window {
            input_state.request_close_window = false;
        }

        // Restaurar pantalla (manual o por clic en escritorio)
        if input_state.request_restore {
            if let Some(i) = (0..window_count).rev().find(|&i| windows[i].content != WindowContent::None && windows[i].minimized) {
                windows[i].minimized = false;
                let top = window_count - 1;
                windows.swap(i, top);
                input_state.focused_window = Some(top);
            }
            input_state.request_restore = false;
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

        // Maximizar / Restaurar tamaño
        if input_state.request_maximize && window_count > 0 {
            if let Some(f_idx) = input_state.focused_window.filter(|i| *i < window_count) {
                let win = &mut windows[f_idx];
                if win.maximized {
                    let (x, y, w, h) = win.stored_rect;
                    win.x = x; win.y = y; win.w = w; win.h = h;
                    win.maximized = false;
                } else {
                    win.stored_rect = (win.x, win.y, win.w, win.h);
                    win.x = 0; win.y = 0;
                    win.w = fb.info.width as i32;
                    win.h = fb.info.height as i32 - 45;
                    win.maximized = true;
                }
                if let WindowContent::External(s_idx) = win.content {
                    let pid = surfaces[s_idx as usize].pid;
                    let event = SideWindEvent {
                        event_type: SWND_EVENT_TYPE_RESIZE,
                        data1: win.w,
                        data2: win.h - ShellWindow::TITLE_H,
                        data3: 0,
                    };
                    let _ = send(pid, MSG_TYPE_INPUT, unsafe {
                        core::slice::from_raw_parts(&event as *const _ as *const u8, core::mem::size_of::<SideWindEvent>())
                    });
                }
            }
            input_state.request_maximize = false;
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

        // Tecla N: nueva ventana
        if input_state.request_new_window && window_count < MAX_WINDOWS_COUNT {
            windows[window_count] = ShellWindow {
                x: 60 + (window_count as i32) * 20,
                y: 160 + (window_count as i32) * 15,
                w: 240,
                h: 180,
                curr_x: (60 + (window_count as i32) * 20) as f32,
                curr_y: (160 + (window_count as i32) * 15) as f32,
                curr_w: 240.0,
                curr_h: 180.0,
                minimized: false,
                maximized: false,
                stored_rect: (60 + (window_count as i32) * 20, 160 + (window_count as i32) * 15, 240, 180),
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

        // --- Animación de Ventanas (Fase 5) ---
        let mut min_count_anim = 0;
        for w in windows.iter_mut().take(window_count) {
            if w.content == WindowContent::None { continue; }
            
            let (tx, ty, tw, th) = if w.minimized {
                let px = (100 + (min_count_anim % 3) * 120) as f32;
                let py = (250 + (min_count_anim / 3) * 150) as f32;
                min_count_anim += 1;
                (px - 20.0, py - 40.0, 40.0, 40.0) // Fly to icon
            } else {
                (w.x as f32, w.y as f32, w.w as f32, w.h as f32)
            };

            let lerp = 0.25;
            w.curr_x += (tx - w.curr_x) * lerp;
            w.curr_y += (ty - w.curr_y) * lerp;
            w.curr_w += (tw - w.curr_w) * lerp;
            w.curr_h += (th - w.curr_h) * lerp;
        }

        // Toggle Dashboard (Super key)
        if input_state.request_dashboard {
            input_state.dashboard_active = !input_state.dashboard_active;
            input_state.request_dashboard = false;
        }

        // 2. Borrado completo del back buffer y dibujar UI estática
        // Clear raw primero para sobrescribir cualquier contenido previo (logo de boot, etc.)
        fb.clear_back_buffer_raw(colors::COSMIC_DEEP);
        if !input_state.lock_active {
            if counter <= 5 { println!("[SMITHAY] main loop 1: draw_static_ui"); }
            draw_static_ui(&mut fb, &windows, window_count, counter);

            // Dibujar ventanas (solo si el dashboard NO está activo)
            if !input_state.dashboard_active {
                if counter <= 5 { println!("[SMITHAY] main loop 2: draw_shell_windows"); }
                draw_shell_windows(&mut fb, &windows, window_count, input_state.focused_window, &surfaces);
            } else {
                draw_dashboard(&mut fb, counter);
            }

            // Phase 8 Overlays
            if input_state.launcher_active {
                if counter <= 5 { println!("[SMITHAY] main loop 4: draw_launcher"); }
                draw_launcher(&mut fb);
            }
            if input_state.quick_settings_active {
                if counter <= 5 { println!("[SMITHAY] main loop 5: draw_quick_settings"); }
                draw_quick_settings(&mut fb);
            }
            if input_state.context_menu_active {
                if counter <= 5 { println!("[SMITHAY] main loop 6: draw_context_menu"); }
                draw_context_menu(&mut fb, input_state.context_menu_pos);
            }

            // Dibujar notificaciones si modo activo
            if input_state.notifications_active {
                draw_notifications(&mut fb, &input_state.notifications);
            }
        } else {
            draw_lock_screen(&mut fb, counter);
        }

        if counter <= 5 { println!("[SMITHAY] main loop 7: draw_mouse_and_present"); }
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
