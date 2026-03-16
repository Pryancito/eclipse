use std::prelude::v1::*;
use embedded_graphics::prelude::*;
#[cfg(target_vendor = "eclipse")]
use libc::{InputEvent, eclipse_send};
#[cfg(not(target_vendor = "eclipse"))]
use eclipse_syscall::InputEvent;
#[cfg(not(target_vendor = "eclipse"))]
unsafe fn eclipse_send(_dest: u32, _msg_type: u32, _buf: *const core::ffi::c_void, _len: usize, _flags: usize) -> usize { 0 }
use sidewind::{SideWindMessage, SideWindEvent, SWND_EVENT_TYPE_MOUSE_BUTTON};
use crate::compositor::{
    ShellWindow, WindowContent, ExternalSurface, WindowButton, focus_under_cursor, MAX_SURFACE_DIM,
};


#[derive(Clone)]
pub enum CompositorEvent {
    Input(InputEvent),
    SideWind(SideWindMessage, u32), // message, sender_pid
    Wayland(heapless::Vec<u8, 256>, u32), // data, sender_pid
    X11(heapless::Vec<u8, 256>, u32), // data, sender_pid
    NetStats(u64, u64), // rx, tx
    ServiceInfo(heapless::Vec<u8, 256>),
    /// Línea de log del kernel para el HUD (cuando el logo ya está dibujado).
    KernelLog(heapless::Vec<u8, 252>),
}


#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyAction {
    None, Clear, SetColor(u8), CycleStrokeSize, SensitivityPlus, SensitivityMinus,
    InvertY, CenterCursor, NewWindow, CloseWindow, CycleForward, CycleBackward,
    Minimize, Maximize, Restore, ToggleDashboard, ToggleLock, ToggleLauncher,
    SnapLeft, SnapRight, SwitchWorkspace(u8), CycleWindowVisual, ToggleSearch, ToggleSystemCentral,
    ToggleTiling,
    ArrowUp, ArrowDown, Input(char), Enter, Backspace,
}

pub fn scancode_to_action(scancode: u16, modifiers: u32) -> KeyAction {
    let code = (scancode & 0x7FFF) as u8;
    match code {
        0x2E => KeyAction::Clear,
        0x02 => if (modifiers & 8) != 0 { KeyAction::SwitchWorkspace(0) } else { KeyAction::SetColor(0) },
        0x03 => if (modifiers & 8) != 0 { KeyAction::SwitchWorkspace(1) } else { KeyAction::SetColor(1) },
        0x04 => KeyAction::SetColor(2),
        0x05 => KeyAction::SetColor(3),
        0x06 => KeyAction::SetColor(4),
        0x0B => KeyAction::CycleStrokeSize,
        0x0D => KeyAction::SensitivityPlus,
        0x0C => KeyAction::SensitivityMinus,
        0x17 => KeyAction::InvertY,
        0x47 => KeyAction::CenterCursor,
        0x31 => KeyAction::NewWindow,
        0x01 => KeyAction::CloseWindow,
        0x0F => if (modifiers & 4) != 0 { KeyAction::CycleWindowVisual } else { KeyAction::CycleForward },
        0x29 => KeyAction::CycleBackward,
        0x32 => KeyAction::Minimize,
        0x5B => KeyAction::ToggleDashboard,
        0x26 => KeyAction::ToggleLock,
        0x1E => KeyAction::ToggleLauncher,
        0x1F => if (modifiers & 8) != 0 { KeyAction::ToggleSystemCentral } else { KeyAction::None },
        0x4B => KeyAction::SnapLeft,

        0x4D => KeyAction::SnapRight,
        0x39 => if (modifiers & 8) != 0 { KeyAction::ToggleSearch } else { KeyAction::None },
        0x14 => if (modifiers & 8) != 0 { KeyAction::ToggleTiling } else { KeyAction::None },
        0x48 => if (modifiers & 8) != 0 { KeyAction::Maximize } else { KeyAction::ArrowUp },
        0x50 => KeyAction::ArrowDown,
        0x1C => KeyAction::Enter,
        0x0E => KeyAction::Backspace,
        _ => KeyAction::None,
    }
}

pub fn scancode_to_char(code: u16, shift: bool) -> Option<char> {
    match (code, shift) {
        (0x1E, false) => Some('a'), (0x1E, true) => Some('A'),
        (0x30, false) => Some('b'), (0x30, true) => Some('B'),
        (0x2E, false) => Some('c'), (0x2E, true) => Some('C'),
        (0x20, false) => Some('d'), (0x20, true) => Some('D'),
        (0x12, false) => Some('e'), (0x12, true) => Some('E'),
        (0x21, false) => Some('f'), (0x21, true) => Some('F'),
        (0x22, false) => Some('g'), (0x22, true) => Some('G'),
        (0x23, false) => Some('h'), (0x23, true) => Some('H'),
        (0x17, false) => Some('i'), (0x17, true) => Some('I'),
        (0x24, false) => Some('j'), (0x24, true) => Some('J'),
        (0x25, false) => Some('k'), (0x25, true) => Some('K'),
        (0x26, false) => Some('l'), (0x26, true) => Some('L'),
        (0x32, false) => Some('m'), (0x32, true) => Some('M'),
        (0x31, false) => Some('n'), (0x31, true) => Some('N'),
        (0x18, false) => Some('o'), (0x18, true) => Some('O'),
        (0x19, false) => Some('p'), (0x19, true) => Some('P'),
        (0x10, false) => Some('q'), (0x10, true) => Some('Q'),
        (0x13, false) => Some('r'), (0x13, true) => Some('R'),
        (0x1F, false) => Some('s'), (0x1F, true) => Some('S'),
        (0x14, false) => Some('t'), (0x14, true) => Some('T'),
        (0x16, false) => Some('u'), (0x16, true) => Some('U'),
        (0x2F, false) => Some('v'), (0x2F, true) => Some('V'),
        (0x11, false) => Some('w'), (0x11, true) => Some('W'),
        (0x2D, false) => Some('x'), (0x2D, true) => Some('X'),
        (0x15, false) => Some('y'), (0x15, true) => Some('Y'),
        (0x2C, false) => Some('z'), (0x2C, true) => Some('Z'),
        (0x39, _) => Some(' '),
        (0x02, _) => Some('1'), (0x03, _) => Some('2'), (0x04, _) => Some('3'), (0x05, _) => Some('4'), (0x06, _) => Some('5'),
        _ => None,
    }
}

pub struct InputState {
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub mouse_buttons: u8,
    pub request_clear: bool,
    pub stroke_color: u8,
    pub stroke_size: i32,
    pub mouse_sensitivity: i32,
    pub invert_y: bool,
    pub request_center_cursor: bool,
    pub request_new_window: bool,
    pub request_close_window: bool,
    pub request_cycle_forward: bool,
    pub request_cycle_backward: bool,
    pub request_minimize: bool,
    pub request_maximize: bool,
    pub request_restore: bool,
    pub dragging_window: Option<usize>,
    pub resizing_window: Option<usize>,
    pub drag_offset_x: i32,
    pub drag_offset_y: i32,
    pub focused_window: Option<usize>,
    pub modifiers: u32,
    pub request_dashboard: bool,
    pub dashboard_active: bool,
    pub lock_active: bool,
    pub launcher_active: bool,
    pub quick_settings_active: bool,
    pub context_menu_active: bool,
    pub context_menu_pos: Point,
    pub launcher_curr_y: f32,
    pub current_workspace: u8,
    pub workspace_offset: f32,
    pub tiling_active: bool,
    pub request_toggle_tiling: bool,
    pub alt_tab_active: bool,
    pub search_active: bool,
    pub search_query: heapless::String<32>,
    pub search_selected_idx: usize,
    pub search_curr_y: f32,
    pub system_central_active: bool,
    pub system_central_curr_y: f32,
}


impl InputState {
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            cursor_x: width / 2, cursor_y: height / 2, mouse_buttons: 0,
            request_clear: false, stroke_color: 0, stroke_size: 4,
            mouse_sensitivity: 100, invert_y: false, request_center_cursor: false,
            request_new_window: false, request_close_window: false,
            request_cycle_forward: false, request_cycle_backward: false,
            request_minimize: false, request_maximize: false, request_restore: false,
            dragging_window: None, resizing_window: None, drag_offset_x: 0, drag_offset_y: 0,
            focused_window: None, modifiers: 0, request_dashboard: false,
            dashboard_active: false, lock_active: false,
            launcher_active: false, quick_settings_active: false, context_menu_active: false,
            context_menu_pos: Point::new(0, 0),
            launcher_curr_y: height as f32, current_workspace: 0, workspace_offset: 0.0,
            tiling_active: false, request_toggle_tiling: false,
            alt_tab_active: false, search_active: false, search_query: heapless::String::<32>::new(),
            search_selected_idx: 0, search_curr_y: 0.0,
            system_central_active: false, system_central_curr_y: 0.0,
        }

    }

    #[inline(never)]
    pub fn apply_event(&mut self, ev: &InputEvent, fb_width: i32, fb_height: i32, windows: &mut [ShellWindow], window_count: &mut usize, surfaces: &[ExternalSurface]) {
        // Modo normal: usa toda la lógica de ventanas, HUD y atajos.

        match ev.event_type {
            0 => { // Keyboard
                let pressed = ev.value == 1;
                let code = (ev.code & 0x7FFF) as u8;
                match code {
                    0x2A | 0x36 => { if pressed { self.modifiers |= 1; } else { self.modifiers &= !1; } }
                    0x1D => { if pressed { self.modifiers |= 2; } else { self.modifiers &= !2; } }
                    0x38 => { if pressed { self.modifiers |= 4; } else { self.modifiers &= !4; } }
                    0x5B => { if pressed { self.modifiers |= 8; } else { self.modifiers &= !8; } }
                    _ => {}
                }
                let action = if self.search_active {
                    match code {
                        0x01 => KeyAction::ToggleSearch,
                        0x1C => KeyAction::Enter,
                        0x0E => KeyAction::Backspace,
                        0x48 => KeyAction::ArrowUp,
                        0x50 => KeyAction::ArrowDown,
                        _ => { if let Some(c) = scancode_to_char(code as u16, (self.modifiers & 1) != 0) { KeyAction::Input(c) } else { KeyAction::None } }
                    }
                } else if self.modifiers & (4 | 8) != 0 {
                    scancode_to_action(ev.code, self.modifiers)
                } else {
                    KeyAction::None
                };
                match action {
                    KeyAction::None => {
                        if let Some(f_idx) = self.focused_window {
                            if f_idx < *window_count {
                                if let WindowContent::External(s_idx) = windows[f_idx].content {
                                if (s_idx as usize) < surfaces.len() {
                                    let pid = surfaces[s_idx as usize].pid;
                                    let se = SideWindEvent {
                                        event_type: sidewind::SWND_EVENT_TYPE_KEY,
                                        data1: ev.code as i32,
                                        data2: ev.value as i32,
                                        data3: self.modifiers as i32
                                    };
                                    let _ = unsafe { eclipse_send(pid as u32, 0x40, &se as *const _ as *const core::ffi::c_void, core::mem::size_of::<SideWindEvent>(), 0) };
                                }
                            }
                            }
                        }
                    }
                    KeyAction::Clear => if pressed { self.request_clear = true; },
                    KeyAction::SetColor(c) => if pressed { self.stroke_color = c.min(4); },
                    KeyAction::CenterCursor => if pressed { self.request_center_cursor = true; },
                    KeyAction::NewWindow => if pressed { self.request_new_window = true; },
                    KeyAction::CloseWindow => if pressed { self.request_close_window = true; },
                    KeyAction::CycleForward => if pressed { self.request_cycle_forward = true; },
                    KeyAction::CycleBackward => if pressed { self.request_cycle_backward = true; },
                    KeyAction::CycleStrokeSize => if pressed {
                        self.stroke_size = match self.stroke_size { 2 => 4, 4 => 6, _ => 2 };
                    },
                    KeyAction::SensitivityPlus => if pressed { self.mouse_sensitivity = (self.mouse_sensitivity + 25).min(200); },
                    KeyAction::SensitivityMinus => if pressed { self.mouse_sensitivity = (self.mouse_sensitivity - 25).max(50); },
                    KeyAction::InvertY => if pressed { self.invert_y = !self.invert_y; },
                    KeyAction::Minimize => if pressed { self.request_minimize = true; },
                    KeyAction::Maximize => if pressed { self.request_maximize = true; },
                    KeyAction::Restore => if pressed { self.request_restore = true; },
                    KeyAction::ToggleDashboard => if pressed && self.modifiers == 8 { self.request_dashboard = true; },
                    KeyAction::ToggleLock => if pressed && (self.modifiers & 8 != 0) { self.lock_active = !self.lock_active; },
                    KeyAction::ToggleLauncher => if pressed && (self.modifiers & 8 != 0) { self.launcher_active = !self.launcher_active; },
                    KeyAction::ToggleSearch => if pressed { 
                        self.search_active = !self.search_active; 
                        if self.search_active { self.search_query.clear(); self.dashboard_active = false; } 
                    },
                    KeyAction::SnapLeft => if pressed && (self.modifiers & 8 != 0) {
                        if let Some(idx) = self.focused_window {
                            if idx < *window_count {
                                windows[idx].x = 0;
                                windows[idx].y = ShellWindow::TITLE_H;
                                windows[idx].w = fb_width / 2;
                                windows[idx].h = fb_height - ShellWindow::TITLE_H - 44;
                            }
                        }
                    },
                    KeyAction::SnapRight => if pressed && (self.modifiers & 8 != 0) {
                        if let Some(idx) = self.focused_window {
                            if idx < *window_count {
                                windows[idx].x = fb_width / 2;
                                windows[idx].y = ShellWindow::TITLE_H;
                                windows[idx].w = fb_width / 2;
                                windows[idx].h = fb_height - ShellWindow::TITLE_H - 44;
                            }
                        }
                    },
                    KeyAction::SwitchWorkspace(w) => if pressed && (self.modifiers & 8 != 0) { self.current_workspace = w; },
                    KeyAction::CycleWindowVisual => if pressed && (self.modifiers & 4 != 0) { self.alt_tab_active = true; self.request_cycle_forward = true; },
                    KeyAction::ArrowUp => if pressed && self.search_active { self.search_selected_idx = self.search_selected_idx.saturating_sub(1); },
                    KeyAction::ArrowDown => if pressed && self.search_active { self.search_selected_idx += 1; },
                    KeyAction::Backspace => if pressed && self.search_active { self.search_query.pop(); },
                    KeyAction::Enter => if pressed && self.search_active { self.execute_search(); self.search_active = false; },
                    KeyAction::ToggleSystemCentral => if pressed { 
                        self.system_central_active = !self.system_central_active; 
                        if self.system_central_active { self.dashboard_active = false; self.search_active = false; }
                    },
                    KeyAction::ToggleTiling => if pressed { self.request_toggle_tiling = true; },
                    KeyAction::Input(c) => if pressed && self.search_active { if self.search_query.len() < 32 { let _ = self.search_query.push(c); } },

                }
                if !pressed && ev.code == 0x0F { self.alt_tab_active = false; }
            }
            1 => { // Mouse move
                if ev.code == 0xFFFF {
                    // Coalesced dx+dy event: dx = lower i16, dy = upper i16.
                    // Clamp each axis to i8 range after unpacking so that a malformed or
                    // accumulated-overflow event cannot cause unbounded cursor jumps.
                    let dx = (ev.value as i16) as i32;
                    let dy = ((ev.value >> 16) as i16) as i32;
                    let dx = dx.clamp(i8::MIN as i32, i8::MAX as i32);
                    let dy = dy.clamp(i8::MIN as i32, i8::MAX as i32);
                    let ddx = (dx * self.mouse_sensitivity) / 100;
                    let ddy = (dy * self.mouse_sensitivity) / 100;
                    self.cursor_x = (self.cursor_x + ddx).clamp(0, (fb_width - 1).max(0));
                    let dy_effective = if self.invert_y { -ddy } else { ddy };
                    self.cursor_y = (self.cursor_y + dy_effective).clamp(0, (fb_height - 1).max(0));
                    if let Some(idx) = self.dragging_window {
                        if idx < *window_count {
                            windows[idx].x = (windows[idx].x + ddx).clamp(0, (fb_width - windows[idx].w).max(0));
                            windows[idx].y = (windows[idx].y + dy_effective).clamp(0, (fb_height - windows[idx].h).max(0));
                        }
                    }
                    if let Some(idx) = self.resizing_window {
                        if idx < *window_count {
                            windows[idx].w = (self.cursor_x - windows[idx].x + 8).max(50).min(MAX_SURFACE_DIM as i32);
                            windows[idx].h = (self.cursor_y - windows[idx].y + 8).max(ShellWindow::TITLE_H + 20).min(MAX_SURFACE_DIM as i32);
                        }
                    }
                } else if ev.code == 0 {
                    let d = (ev.value.clamp(i8::MIN as i32, i8::MAX as i32) * self.mouse_sensitivity) / 100;
                    self.cursor_x = (self.cursor_x + d).clamp(0, (fb_width - 1).max(0));
                    if let Some(idx) = self.dragging_window { 
                        if idx < *window_count {
                            windows[idx].x = (windows[idx].x + d).clamp(0, (fb_width - windows[idx].w).max(0)); 
                        }
                    }
                    if let Some(idx) = self.resizing_window { 
                        if idx < *window_count {
                            windows[idx].w = (self.cursor_x - windows[idx].x + 8).max(50).min(MAX_SURFACE_DIM as i32); 
                        }
                    }
                } else if ev.code == 1 {
                    let d = (ev.value.clamp(i8::MIN as i32, i8::MAX as i32) * self.mouse_sensitivity) / 100;
                    let dy = if self.invert_y { -d } else { d };
                    self.cursor_y = (self.cursor_y + dy).clamp(0, (fb_height - 1).max(0));
                    if let Some(idx) = self.dragging_window { 
                        if idx < *window_count {
                            windows[idx].y = (windows[idx].y + dy).clamp(0, (fb_height - windows[idx].h).max(0)); 
                        }
                    }
                    if let Some(idx) = self.resizing_window { 
                        if idx < *window_count {
                            windows[idx].h = (self.cursor_y - windows[idx].y + 8).max(ShellWindow::TITLE_H + 20).min(MAX_SURFACE_DIM as i32); 
                        }
                    }
                }
            }
            2 => { // Mouse button
                let btn = ev.code as u8;
                let pressed = ev.value != 0;
                let old = self.mouse_buttons;
                // Guard against button indices >= 8 to prevent shift overflow on u8.
                if btn < 8 {
                    if pressed { self.mouse_buttons |= 1 << btn; } else { self.mouse_buttons &= !(1 << btn); }
                }
                // Forward mouse button events to the focused external window client
                if let Some(f_idx) = self.focused_window {
                    if f_idx < *window_count {
                        if let WindowContent::External(s_idx) = windows[f_idx].content {
                        if (s_idx as usize) < surfaces.len() {
                            let pid = surfaces[s_idx as usize].pid;
                            let se = SideWindEvent {
                                event_type: SWND_EVENT_TYPE_MOUSE_BUTTON,
                                data1: btn as i32,
                                data2: ev.value,
                                data3: 0,
                            };
                            let _ = unsafe { eclipse_send(pid as u32, 0x40, &se as *const _ as *const core::ffi::c_void, core::mem::size_of::<SideWindEvent>(), 0) };
                        }
                    }
                    }
                }
                if (self.mouse_buttons & 1 != 0) && (old & 1 == 0) {
                    self.launcher_active = false; self.context_menu_active = false;
                    let sidebar_width = (fb_width / 10).clamp(140, 220);
                    if self.cursor_x <= sidebar_width {
                        let icon_slot_h = fb_height / 5; // 5 icons
                        let slot_idx = self.cursor_y / icon_slot_h;
                        match slot_idx {
                            0 => self.request_dashboard = true,
                            1 => {
                                self.system_central_active = !self.system_central_active;
                                if self.system_central_active { self.dashboard_active = false; self.search_active = false; }
                            },
                            2 => {
                                self.launcher_active = !self.launcher_active;
                                if self.launcher_active { self.dashboard_active = false; self.system_central_active = false; }
                            },
                            _ => {}
                        }
                    } else if self.system_central_active {
                        // Handle Kill/Restart buttons in System Central
                        let sidebar_width = (fb_width / 10).clamp(140, 220);
                        let col_options = sidebar_width + 490;
                        let col_prog_options = sidebar_width + 590;

                        // Services area: y=65 to half_h
                        // Programs area: y=start_y_prog to h-20
                        let half_h = (fb_height - 60) / 2;
                        let row_h = 24;
                        if self.cursor_y >= 65 && self.cursor_y < half_h + 20 {
                            let idx = (self.cursor_y - 90) / row_h; // start_y(65) + 25
                            if idx >= 0 && idx < 32 {
                                if self.cursor_x >= col_options && self.cursor_x < col_options + 90 {
                                    // Restart Service
                                    let _ = unsafe { eclipse_send(1, 0, b"RESTART_SERVICE".as_ptr() as *const core::ffi::c_void, 15, 0) }; // Mock/Simple trigger
                                } else if self.cursor_x >= col_options + 100 && self.cursor_x < col_options + 180 {
                                    // Stop Service
                                    let _ = unsafe { eclipse_send(1, 0, b"STOP_SERVICE".as_ptr() as *const core::ffi::c_void, 12, 0) };
                                }
                            }
                        } else if self.cursor_y >= half_h + 85 {
                            let idx = (self.cursor_y - (half_h + 110)) / row_h;
                            if idx >= 0 && idx < 64 {
                                if self.cursor_x >= col_prog_options && self.cursor_x < col_prog_options + 100 {
                                    // Kill process
                                }
                            }
                        }
                    }
 else if self.cursor_y >= fb_height - 40 { if self.cursor_x < 150 { self.launcher_active = true; } }
                    else if let Some(idx) = focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count) {
                        let top = *window_count - 1; if idx != top { windows.swap(idx, top); }
                        self.focused_window = Some(top);
                        let b = windows[top].check_button_click(self.cursor_x, self.cursor_y);
                        match b {
                            WindowButton::Close => self.request_close_window = true,
                            WindowButton::Minimize => self.request_minimize = true,
                            WindowButton::Maximize => self.request_maximize = true,
                            WindowButton::None => {
                                if !self.tiling_active {
                                    self.dragging_window = Some(top);
                                    self.drag_offset_x = self.cursor_x - windows[top].x;
                                    self.drag_offset_y = self.cursor_y - windows[top].y;
                                }
                            }
                        }
                    } else { self.focused_window = None; }
                } else if (self.mouse_buttons & 2 != 0) && (old & 2 == 0) {
                    self.launcher_active = false; self.quick_settings_active = false;
                    if focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count).is_none() {
                        self.context_menu_active = true;
                        self.context_menu_pos = Point::new(self.cursor_x, self.cursor_y);
                    } else { self.context_menu_active = false; }
                } else if self.mouse_buttons & 1 == 0 { self.dragging_window = None; self.resizing_window = None; }
            }
            _ => {}
        }
    }

    pub fn execute_search(&mut self) {
        if self.search_query == "term" { self.request_new_window = true; }
        else if self.search_query == "lock" { self.lock_active = true; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scancode_to_action() {
        assert_eq!(scancode_to_action(0x01, 0), KeyAction::CloseWindow);
        assert_eq!(scancode_to_action(0x32, 0), KeyAction::Minimize);
        // Test with Super (modifier 8)
        assert_eq!(scancode_to_action(0x39, 8), KeyAction::ToggleSearch);
        assert_eq!(scancode_to_action(0x02, 8), KeyAction::SwitchWorkspace(0));
    }

    #[test]
    fn test_scancode_to_char() {
        assert_eq!(scancode_to_char(0x1E, false), Some('a'));
        assert_eq!(scancode_to_char(0x1E, true), Some('A'));
        assert_eq!(scancode_to_char(0x39, false), Some(' '));
        assert_eq!(scancode_to_char(0x02, false), Some('1'));
    }

    #[test]
    fn test_input_state_mouse_move() {
        let mut state = InputState::new(100, 100);
        let ev = InputEvent {
            device_id: 1,
            event_type: 1, // Mouse move
            code: 0,      // x
            value: 10,
            timestamp: 0,
        };
        let mut windows = [];
        let surfaces = [];
        state.apply_event(&ev, 100, 100, &mut windows, &mut 0, &surfaces);
        assert_eq!(state.cursor_x, 60); // 50 (center) + 10
    }

    #[test]
    fn test_input_state_modifiers() {
        let mut state = InputState::new(100, 100);
        let ev = InputEvent {
            device_id: 1,
            event_type: 0, // Keyboard
            code: 0x2A,   // Left Shift
            value: 1,      // Pressed
            timestamp: 0,
        };
        let mut windows = [];
        let surfaces = [];
        state.apply_event(&ev, 100, 100, &mut windows, &mut 0, &surfaces);
        assert_eq!(state.modifiers & 1, 1);

        let ev_release = InputEvent {
            device_id: 1,
            event_type: 0,
            code: 0x2A,
            value: 0, // Released
            timestamp: 0,
        };
        state.apply_event(&ev_release, 100, 100, &mut windows, &mut 0, &surfaces);
        assert_eq!(state.modifiers & 1, 0);
    }

    #[test]
    fn test_scancode_to_action_more() {
        assert_eq!(scancode_to_action(0x4B, 0), KeyAction::SnapLeft);
        assert_eq!(scancode_to_action(0x4D, 0), KeyAction::SnapRight);
        assert_eq!(scancode_to_action(0x04, 0), KeyAction::SetColor(2));
        assert_eq!(scancode_to_action(0x2E, 0), KeyAction::Clear);
        assert_eq!(scancode_to_action(0x31, 0), KeyAction::NewWindow);
        assert_eq!(scancode_to_action(0x48, 0), KeyAction::ArrowUp);
        assert_eq!(scancode_to_action(0x50, 0), KeyAction::ArrowDown);
    }

    #[test]
    fn test_scancode_to_char_more() {
        assert_eq!(scancode_to_char(0x2C, false), Some('z'));
        assert_eq!(scancode_to_char(0x2C, true), Some('Z'));
        assert_eq!(scancode_to_char(0xFF, false), None);
    }

    #[test]
    fn test_input_state_new() {
        let state = InputState::new(800, 600);
        assert_eq!(state.cursor_x, 400);
        assert_eq!(state.cursor_y, 300);
        assert_eq!(state.stroke_color, 0);
        assert_eq!(state.focused_window, None);
        assert!(!state.search_active);
    }

    #[test]
    fn test_execute_search_term() {
        let mut state = InputState::new(100, 100);
        let _ = state.search_query.push_str("term");
        state.execute_search();
        assert!(state.request_new_window);
    }

    #[test]
    fn test_execute_search_lock() {
        let mut state = InputState::new(100, 100);
        let _ = state.search_query.push_str("lock");
        state.execute_search();
        assert!(state.lock_active);
    }

    /// Helper: pack (dx, dy) as i8 values into the coalesced event value field,
    /// matching the formula used by input_service.
    fn pack_mouse_delta(dx: i8, dy: i8) -> i32 {
        ((dy as i16 as i32) << 16) | (dx as i16 as u16 as i32)
    }

    #[test]
    fn test_coalesced_mouse_move_positive() {
        let mut state = InputState::new(200, 200); // cursor starts at (100, 100)
        let ev = InputEvent {
            device_id: 1,
            event_type: 1,
            code: 0xFFFF,
            value: pack_mouse_delta(10, 5),
            timestamp: 0,
        };
        let mut windows = [];
        let surfaces = [];
        state.apply_event(&ev, 200, 200, &mut windows, &mut 0, &surfaces);
        assert_eq!(state.cursor_x, 110); // 100 + 10
        assert_eq!(state.cursor_y, 105); // 100 + 5
    }

    #[test]
    fn test_coalesced_mouse_move_negative_dy() {
        // Validates that the upper-16-bit arithmetic shift extracts dy correctly
        // (a comparison `> 16` instead of right-shift `>> 16` would always yield 0 or 1).
        let mut state = InputState::new(200, 200); // cursor starts at (100, 100)
        let ev = InputEvent {
            device_id: 1,
            event_type: 1,
            code: 0xFFFF,
            value: pack_mouse_delta(0, -20),
            timestamp: 0,
        };
        let mut windows = [];
        let surfaces = [];
        state.apply_event(&ev, 200, 200, &mut windows, &mut 0, &surfaces);
        assert_eq!(state.cursor_x, 100); // unchanged
        assert_eq!(state.cursor_y, 80);  // 100 - 20
    }

    #[test]
    fn test_coalesced_mouse_move_both_negative() {
        let mut state = InputState::new(200, 200); // cursor starts at (100, 100)
        let ev = InputEvent {
            device_id: 1,
            event_type: 1,
            code: 0xFFFF,
            value: pack_mouse_delta(-30, -40),
            timestamp: 0,
        };
        let mut windows = [];
        let surfaces = [];
        state.apply_event(&ev, 200, 200, &mut windows, &mut 0, &surfaces);
        assert_eq!(state.cursor_x, 70); // 100 - 30
        assert_eq!(state.cursor_y, 60); // 100 - 40
    }

    #[test]
    fn test_coalesced_mouse_move_extreme_clamped() {
        // A malformed/accumulation-overflow value whose lower i16 is -128 and upper i16 is 127.
        let mut state = InputState::new(200, 200); // cursor at (100, 100), sensitivity=100
        let ev = InputEvent {
            device_id: 1,
            event_type: 1,
            code: 0xFFFF,
            value: pack_mouse_delta(i8::MIN, i8::MAX), // dx=-128, dy=127
            timestamp: 0,
        };
        let mut windows = [];
        let surfaces = [];
        state.apply_event(&ev, 200, 200, &mut windows, &mut 0, &surfaces);
        // cursor_x = clamp(100 + (-128), 0, 199) = 0 (100-128 = -28, clamped to 0)
        assert_eq!(state.cursor_x, 0);
        // cursor_y = clamp(100 + 127, 0, 199) = 199 (100+127 = 227, clamped to fb_height-1=199)
        assert_eq!(state.cursor_y, 199);
    }

    /// Stress: scancode_to_action y scancode_to_char en bucle.
    #[test]
    fn test_stress_scancode_tables() {
        const ITERS: u32 = 30_000;
        let codes = [0x01u16, 0x02, 0x1E, 0x32, 0x39, 0x48, 0x4B, 0x4D, 0x50];
        for i in 0..ITERS {
            let code = codes[(i as usize) % codes.len()];
            let _ = scancode_to_action(code, 0);
            let _ = scancode_to_action(code, 8);
            let _ = scancode_to_char(code, false);
            let _ = scancode_to_char(code, true);
        }
    }
}
