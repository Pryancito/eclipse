//! Input handling for Lunas desktop — keyboard, mouse, and system events.

use std::prelude::v1::*;
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
use eclipse_ipc::types::{NetExtendedStats, NetStaticConfig};

#[derive(Clone)]
pub enum CompositorEvent {
    Input(InputEvent),
    SideWind(SideWindMessage, u32),
    Wayland(heapless::Vec<u8, 512>, u32),
    X11(heapless::Vec<u8, 512>, u32),
    NetStats(u64, u64),
    NetExtendedStats(NetExtendedStats),
    ServiceInfo(heapless::Vec<u8, 512>),
    KernelLog(heapless::Vec<u8, 252>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyAction {
    None, Clear, SetColor(u8), CycleStrokeSize, SensitivityPlus, SensitivityMinus,
    InvertY, CenterCursor, NewWindow, CloseWindow, CycleForward, CycleBackward,
    SnapLeft, SnapRight, SwitchWorkspace(u8), CycleWindowVisual,
    Minimize, Maximize, Restore, ToggleDashboard, ToggleLock, ToggleLauncher,
    ToggleSystemCentral, ToggleTiling, ToggleSearch, ArrowUp, ArrowDown,
    Input(char), Enter, Backspace, ToggleNotifications,
}

/// Represents what element was clicked on the taskbar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskbarHit {
    /// No taskbar element was hit.
    None,
    /// The launcher button (grid icon) was clicked.
    Launcher,
    /// A workspace indicator was clicked (workspace index 0-3).
    Workspace(u8),
    /// A pinned app was clicked (index into desktop.pinned_apps).
    PinnedApp(usize),
    /// A running window task item was clicked (window index).
    WindowTask(usize),
    /// The notification area was clicked.
    Notifications,
    /// The volume indicator was clicked.
    Volume,
    /// The clock area was clicked.
    Clock,
}

/// Determine what element is at position (px, py) on the taskbar.
/// Returns `TaskbarHit::None` if the position is not on the taskbar.
pub fn taskbar_hit_test(
    px: i32,
    py: i32,
    fb_width: i32,
    fb_height: i32,
    pinned_count: usize,
    windows: &[ShellWindow],
    window_count: usize,
    current_workspace: u8,
) -> TaskbarHit {
    use crate::render::{TASKBAR_HEIGHT, TASKBAR_APPS_START_X};

    let bar_y = fb_height - TASKBAR_HEIGHT;
    if py < bar_y || py >= fb_height {
        return TaskbarHit::None;
    }

    // Launcher button: (4, bar_y+6) to (40, bar_y+38)
    if px >= 4 && px < 40 && py >= bar_y + 6 && py < bar_y + 38 {
        return TaskbarHit::Launcher;
    }

    // Workspace indicators: 4 workspaces starting at x=48, each 20px wide, spacing 26px
    for ws in 0..4u8 {
        let ws_x = 48 + (ws as i32) * 26;
        if px >= ws_x && px < ws_x + 20 && py >= bar_y + 12 && py < bar_y + 32 {
            return TaskbarHit::Workspace(ws);
        }
    }

    // Pinned apps: starting at TASKBAR_APPS_START_X, using shared layout constants
    use crate::render::{TASKBAR_ICON_SIZE, TASKBAR_ICON_SPACING};
    let icon_size: i32 = TASKBAR_ICON_SIZE;
    let icon_spacing: i32 = TASKBAR_ICON_SPACING;
    let mut app_x = TASKBAR_APPS_START_X;
    for i in 0..pinned_count {
        if px >= app_x && px < app_x + icon_size && py >= bar_y + 6 && py < bar_y + 38 {
            return TaskbarHit::PinnedApp(i);
        }
        app_x += icon_size + icon_spacing;
    }

    // Running windows area (after separator): start at sep2_x + 8
    let sep2_x = app_x + 2;
    let mut win_x = sep2_x + 8;
    let win_item_w: i32 = 120;
    let tray_start = fb_width - crate::render::TASKBAR_TRAY_WIDTH;

    for w_idx in 0..window_count {
        if win_x + win_item_w > tray_start - 10 { break; }
        let w = &windows[w_idx];
        if w.content == WindowContent::None || w.closing { continue; }
        if w.workspace != current_workspace { continue; }

        if px >= win_x && px < win_x + win_item_w && py >= bar_y + 8 && py < bar_y + 36 {
            return TaskbarHit::WindowTask(w_idx);
        }
        win_x += win_item_w + 4;
    }

    // Notification area: around tray_x + 155
    let notif_x = tray_start + 155;
    if px >= notif_x - 5 && px < notif_x + 20 && py >= bar_y + 4 && py < bar_y + 36 {
        return TaskbarHit::Notifications;
    }

    // Volume indicator: around tray_x + 180
    let vol_x = tray_start + 180;
    if px >= vol_x - 5 && px < vol_x + 15 && py >= bar_y + 4 && py < bar_y + 36 {
        return TaskbarHit::Volume;
    }

    // Clock area: far right
    let clock_x = fb_width - 50;
    if px >= clock_x && px < fb_width && py >= bar_y + 4 && py < bar_y + 36 {
        return TaskbarHit::Clock;
    }

    TaskbarHit::None
}

/// Determine which launcher item is at position (px, py).
/// Returns `Some(pinned_app_index)` if a launcher item was hit, `None` otherwise.
/// This accounts for search filtering when the search is active.
pub fn launcher_hit_test(
    px: i32,
    py: i32,
    fb_height: i32,
    pinned_count: usize,
    pinned_apps: &[crate::desktop::PinnedApp],
    search_active: bool,
    search_query: &str,
) -> Option<usize> {
    use crate::render::{
        launcher_panel_bounds,
        LAUNCHER_ITEM_H, LAUNCHER_ITEMS_Y_OFFSET, LAUNCHER_MAX_VISIBLE,
    };

    let (panel_x, panel_y, panel_w, panel_h) = launcher_panel_bounds(fb_height);

    // Check if click is within the launcher panel
    if px < panel_x || px >= panel_x + panel_w || py < panel_y || py >= panel_y + panel_h {
        return None;
    }

    // Iterate visible items (applying the same filter as draw_launcher)
    let mut visible_idx: i32 = 0;
    for i in 0..pinned_count {
        if visible_idx >= LAUNCHER_MAX_VISIBLE as i32 { break; }
        let app_name = pinned_apps[i].name_str();

        // Filter by search query
        if search_active && !search_query.is_empty() {
            let name_lower_matches = app_name.len() >= search_query.len()
                && app_name[..search_query.len()].eq_ignore_ascii_case(search_query);
            if !name_lower_matches { continue; }
        }

        let item_y = panel_y + LAUNCHER_ITEMS_Y_OFFSET + visible_idx * LAUNCHER_ITEM_H;
        if py >= item_y - 10 && py < item_y - 10 + LAUNCHER_ITEM_H {
            return Some(i);
        }

        visible_idx += 1;
    }

    None
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
        0x39 => if (modifiers & 8) != 0 { KeyAction::ToggleSearch } else { KeyAction::None },
        0x4B => KeyAction::SnapLeft,
        0x4D => KeyAction::SnapRight,
        0x14 => if (modifiers & 8) != 0 { KeyAction::ToggleTiling } else { KeyAction::None },
        0x48 => if (modifiers & 8) != 0 { KeyAction::Maximize } else { KeyAction::ArrowUp },
        0x50 => KeyAction::ArrowDown,
        0x1C => KeyAction::Enter,
        0x0E => KeyAction::Backspace,
        0x36 => if (modifiers & 8) != 0 { KeyAction::ToggleNotifications } else { KeyAction::None },
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
        (0x02, _) => Some('1'), (0x03, _) => Some('2'), (0x04, _) => Some('3'),
        (0x05, _) => Some('4'), (0x06, _) => Some('5'), (0x07, _) => Some('6'),
        (0x08, _) => Some('7'), (0x09, _) => Some('8'), (0x0A, _) => Some('9'),
        (0x0B, _) => Some('0'),
        (0x39, _) => Some(' '),
        (0x34, false) => Some('.'), (0x34, true) => Some('>'),
        (0x33, false) => Some(','), (0x33, true) => Some('<'),
        (0x35, false) => Some('/'), (0x35, true) => Some('?'),
        (0x27, false) => Some(';'), (0x27, true) => Some(':'),
        (0x28, false) => Some('\''), (0x28, true) => Some('"'),
        (0x0C, false) => Some('-'), (0x0C, true) => Some('_'),
        (0x0D, false) => Some('='), (0x0D, true) => Some('+'),
        _ => None,
    }
}

/// InputState tracks cursor position, modifiers, focus, overlays, and desktop shell state.
pub struct InputState {
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub fb_width: i32,
    pub fb_height: i32,
    pub modifiers: u32,
    pub mouse_sensitivity: i32,
    pub invert_y: bool,
    pub focused_window: Option<usize>,
    pub dragging_window: Option<usize>,
    pub drag_offset_x: i32,
    pub drag_offset_y: i32,
    pub resizing_window: Option<usize>,
    pub left_button_down: bool,
    pub current_workspace: u8,
    pub dashboard_active: bool,
    pub system_central_active: bool,
    pub network_panel_active: bool,
    pub lock_screen_active: bool,
    pub launcher_active: bool,
    pub search_active: bool,
    pub search_query: heapless::String<64>,
    pub tiling_active: bool,
    pub notifications_visible: bool,
    /// Number of pinned apps (synced from DesktopShell for taskbar hit detection).
    pub pinned_app_count: usize,
    /// Index of the last pinned app that was clicked (for the caller to act on).
    pub last_pinned_app_click: Option<usize>,
    /// Taskbar element currently under the cursor (for hover highlight).
    pub hovered_taskbar_element: TaskbarHit,
    /// Set when volume indicator is clicked (for the caller to act on).
    pub volume_clicked: bool,
    /// Set when clock area is clicked (for the caller to act on).
    pub clock_clicked: bool,
    /// Index of the launcher item currently hovered (for highlight rendering).
    pub launcher_hovered_index: Option<usize>,
    /// Index of the launcher app that was clicked (for the caller to launch).
    pub launcher_app_click: Option<usize>,
    /// Cursor position at click time when launcher is open (x, y), for hit-test in state.rs.
    pub launcher_click_pos: Option<(i32, i32)>,
    /// Set when notification panel is closed by clicking on it (for the caller to mark all read).
    pub notifications_mark_read: bool,
}

impl InputState {
    pub fn new(fb_width: i32, fb_height: i32) -> Self {
        Self {
            cursor_x: fb_width / 2,
            cursor_y: fb_height / 2,
            fb_width,
            fb_height,
            modifiers: 0,
            mouse_sensitivity: 100,
            invert_y: false,
            focused_window: None,
            dragging_window: None,
            drag_offset_x: 0,
            drag_offset_y: 0,
            resizing_window: None,
            left_button_down: false,
            current_workspace: 0,
            dashboard_active: false,
            system_central_active: false,
            network_panel_active: false,
            lock_screen_active: false,
            launcher_active: false,
            search_active: false,
            search_query: heapless::String::new(),
            tiling_active: false,
            notifications_visible: false,
            pinned_app_count: 0,
            last_pinned_app_click: None,
            hovered_taskbar_element: TaskbarHit::None,
            volume_clicked: false,
            clock_clicked: false,
            launcher_hovered_index: None,
            launcher_app_click: None,
            launcher_click_pos: None,
            notifications_mark_read: false,
        }
    }

    /// Apply an input event to the desktop state (keyboard + mouse handling).
    pub fn apply_event(
        &mut self,
        event: &InputEvent,
        windows: &mut [ShellWindow],
        window_count: &mut usize,
        surfaces: &mut [ExternalSurface],
    ) -> bool {
        let mut dirty = false;
        match event.event_type {
            // Keyboard events
            0 => {
                let scancode = event.code;
                let pressed = event.value != 0;

                // Update modifier state
                match scancode & 0x7FFF {
                    0x2A | 0x36 => {
                        if pressed { self.modifiers |= 1; } else { self.modifiers &= !1; }
                    }
                    0x1D | 0x61 => {
                        if pressed { self.modifiers |= 2; } else { self.modifiers &= !2; }
                    }
                    0x38 | 0x64 => {
                        if pressed { self.modifiers |= 4; } else { self.modifiers &= !4; }
                    }
                    0x5B | 0x5C => {
                        if pressed { self.modifiers |= 8; } else { self.modifiers &= !8; }
                    }
                    _ => {}
                }

                if pressed {
                    // Handle search bar input
                    if self.search_active {
                        let action = scancode_to_action(scancode, self.modifiers);
                        match action {
                            KeyAction::Enter => {
                                self.search_active = false;
                                dirty = true;
                            }
                            KeyAction::Backspace => {
                                let _ = self.search_query.pop();
                                dirty = true;
                            }
                            KeyAction::CloseWindow => {
                                self.search_active = false;
                                self.search_query.clear();
                                dirty = true;
                            }
                            _ => {
                                let shift = (self.modifiers & 1) != 0;
                                if let Some(ch) = scancode_to_char(scancode, shift) {
                                    let _ = self.search_query.push(ch);
                                    dirty = true;
                                }
                            }
                        }
                        return dirty;
                    }

                    let action = scancode_to_action(scancode, self.modifiers);
                    match action {
                        KeyAction::ToggleDashboard => {
                            self.dashboard_active = !self.dashboard_active;
                            dirty = true;
                        }
                        KeyAction::ToggleSystemCentral => {
                            self.system_central_active = !self.system_central_active;
                            dirty = true;
                        }
                        KeyAction::ToggleLock => {
                            self.lock_screen_active = !self.lock_screen_active;
                            dirty = true;
                        }
                        KeyAction::ToggleLauncher => {
                            self.launcher_active = !self.launcher_active;
                            dirty = true;
                        }
                        KeyAction::ToggleSearch => {
                            self.search_active = !self.search_active;
                            if self.search_active { self.search_query.clear(); }
                            dirty = true;
                        }
                        KeyAction::ToggleTiling => {
                            self.tiling_active = !self.tiling_active;
                            dirty = true;
                        }
                        KeyAction::ToggleNotifications => {
                            self.notifications_visible = !self.notifications_visible;
                            dirty = true;
                        }
                        KeyAction::CloseWindow => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    windows[idx].closing = true;
                                    self.focused_window = None;
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::Minimize => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    windows[idx].minimized = true;
                                    self.focused_window = None;
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::Maximize => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let w = &mut windows[idx];
                                    if w.maximized {
                                        let (sx, sy, sw, sh) = w.stored_rect;
                                        w.x = sx; w.y = sy; w.w = sw; w.h = sh;
                                        w.maximized = false;
                                    } else {
                                        w.stored_rect = (w.x, w.y, w.w, w.h);
                                        w.x = 0;
                                        w.y = ShellWindow::TITLE_H;
                                        w.w = self.fb_width;
                                        w.h = self.fb_height - ShellWindow::TITLE_H - 44;
                                        w.maximized = true;
                                    }
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::CycleForward => {
                            if *window_count > 0 {
                                let from = self.focused_window.unwrap_or(0);
                                if let Some(next) = crate::compositor::next_visible(from, true, windows, *window_count) {
                                    self.focused_window = Some(next);
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::CycleBackward => {
                            if *window_count > 0 {
                                let from = self.focused_window.unwrap_or(0);
                                if let Some(prev) = crate::compositor::next_visible(from, false, windows, *window_count) {
                                    self.focused_window = Some(prev);
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::SnapLeft => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let w = &mut windows[idx];
                                    w.x = 0;
                                    w.y = ShellWindow::TITLE_H;
                                    w.w = self.fb_width / 2;
                                    w.h = self.fb_height - ShellWindow::TITLE_H - 44;
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::SnapRight => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let w = &mut windows[idx];
                                    w.x = self.fb_width / 2;
                                    w.y = ShellWindow::TITLE_H;
                                    w.w = self.fb_width / 2;
                                    w.h = self.fb_height - ShellWindow::TITLE_H - 44;
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::SwitchWorkspace(ws) => {
                            self.current_workspace = ws;
                            dirty = true;
                        }
                        KeyAction::SensitivityPlus => {
                            self.mouse_sensitivity = (self.mouse_sensitivity + 25).min(200);
                        }
                        KeyAction::SensitivityMinus => {
                            self.mouse_sensitivity = (self.mouse_sensitivity - 25).max(50);
                        }
                        KeyAction::InvertY => {
                            self.invert_y = !self.invert_y;
                        }
                        KeyAction::CenterCursor => {
                            self.cursor_x = self.fb_width / 2;
                            self.cursor_y = self.fb_height / 2;
                            dirty = true;
                        }
                        _ => {}
                    }
                }
            }
            // Mouse move
            1 => {
                if event.code == 0xFFFF {
                    // Coalesced dx+dy event: dx = lower i16, dy = upper i16.
                    let dx = (event.value as i16) as i32;
                    let dy = ((event.value >> 16) as i16) as i32;
                    let dx = dx.clamp(i8::MIN as i32, i8::MAX as i32);
                    let dy = dy.clamp(i8::MIN as i32, i8::MAX as i32);
                    let ddx = (dx * self.mouse_sensitivity) / 100;
                    let ddy = (dy * self.mouse_sensitivity) / 100;

                    self.cursor_x = (self.cursor_x + ddx).clamp(0, self.fb_width - 1);
                    let dy_effective = if self.invert_y { -ddy } else { ddy };
                    self.cursor_y = (self.cursor_y + dy_effective).clamp(0, self.fb_height - 1);

                    // Window dragging
                    if let Some(drag_idx) = self.dragging_window {
                        if drag_idx < *window_count {
                            windows[drag_idx].x = (windows[drag_idx].x + ddx).clamp(0, self.fb_width - windows[drag_idx].w);
                            windows[drag_idx].y = (windows[drag_idx].y + dy_effective).clamp(0, self.fb_height - windows[drag_idx].h);
                            dirty = true;
                        }
                    }

                    // Window resizing
                    if let Some(resize_idx) = self.resizing_window {
                        if resize_idx < *window_count {
                            windows[resize_idx].w = (self.cursor_x - windows[resize_idx].x + 8).max(50);
                            windows[resize_idx].h = (self.cursor_y - windows[resize_idx].y + 8).max(40);
                            dirty = true;
                        }
                    }
                } else if event.code == 0 {
                    let d = (event.value.clamp(i8::MIN as i32, i8::MAX as i32) * self.mouse_sensitivity) / 100;
                    self.cursor_x = (self.cursor_x + d).clamp(0, self.fb_width - 1);
                    if let Some(drag_idx) = self.dragging_window {
                        if drag_idx < *window_count {
                            windows[drag_idx].x = (windows[drag_idx].x + d).clamp(0, self.fb_width - windows[drag_idx].w);
                        }
                    }
                    if let Some(resize_idx) = self.resizing_window {
                        if resize_idx < *window_count {
                            windows[resize_idx].w = (self.cursor_x - windows[resize_idx].x + 8).max(50);
                        }
                    }
                } else if event.code == 1 {
                    let d = (event.value.clamp(i8::MIN as i32, i8::MAX as i32) * self.mouse_sensitivity) / 100;
                    let dy = if self.invert_y { -d } else { d };
                    self.cursor_y = (self.cursor_y + dy).clamp(0, self.fb_height - 1);
                    if let Some(drag_idx) = self.dragging_window {
                        if drag_idx < *window_count {
                            windows[drag_idx].y = (windows[drag_idx].y + dy).clamp(0, self.fb_height - windows[drag_idx].h);
                        }
                    }
                    if let Some(resize_idx) = self.resizing_window {
                        if resize_idx < *window_count {
                            windows[resize_idx].h = (self.cursor_y - windows[resize_idx].y + 8).max(40);
                        }
                    }
                }

                // Forward mouse events to SideWind external surfaces
                if let Some(focused) = self.focused_window {
                    if focused < *window_count {
                        if let WindowContent::External(s_idx) = windows[focused].content {
                            let s_idx = s_idx as usize;
                            if s_idx < surfaces.len() && surfaces[s_idx].active {
                                let local_x = self.cursor_x - windows[focused].x;
                                let local_y = self.cursor_y - windows[focused].y - ShellWindow::TITLE_H;
                                let ev = SideWindEvent {
                                    event_type: sidewind::SWND_EVENT_TYPE_MOUSE_MOVE,
                                    data1: local_x,
                                    data2: local_y,
                                    data3: self.modifiers as i32,
                                };
                                let _ = unsafe {
                                    eclipse_send(
                                        surfaces[s_idx].pid,
                                        sidewind::MSG_TYPE_INPUT,
                                        &ev as *const _ as *const core::ffi::c_void,
                                        core::mem::size_of::<SideWindEvent>(),
                                        0,
                                    )
                                };
                            }
                        }
                    }
                }

                // Update taskbar hover state
                let hover_hit = taskbar_hit_test(
                    self.cursor_x, self.cursor_y,
                    self.fb_width, self.fb_height,
                    self.pinned_app_count,
                    windows, *window_count,
                    self.current_workspace,
                );
                if hover_hit != self.hovered_taskbar_element {
                    self.hovered_taskbar_element = hover_hit;
                    dirty = true;
                }

                dirty = true;
            }
            // Mouse button
            2 => {
                let button = event.code;
                let pressed = event.value != 0;

                if button == 0 { // Left click
                    self.left_button_down = pressed;
                    if pressed {
                        // ── Taskbar click detection (highest priority) ──
                        let tb_hit = taskbar_hit_test(
                            self.cursor_x, self.cursor_y,
                            self.fb_width, self.fb_height,
                            self.pinned_app_count,
                            windows, *window_count,
                            self.current_workspace,
                        );
                        let on_taskbar = self.cursor_y >= self.fb_height - crate::render::TASKBAR_HEIGHT;
                        if on_taskbar {
                            self.last_pinned_app_click = None;
                            match tb_hit {
                                TaskbarHit::Launcher => {
                                    self.launcher_active = !self.launcher_active;
                                    dirty = true;
                                }
                                TaskbarHit::Workspace(ws) => {
                                    self.current_workspace = ws;
                                    dirty = true;
                                }
                                TaskbarHit::PinnedApp(idx) => {
                                    self.last_pinned_app_click = Some(idx);
                                    dirty = true;
                                }
                                TaskbarHit::WindowTask(w_idx) => {
                                    if w_idx < *window_count {
                                        if windows[w_idx].minimized {
                                            windows[w_idx].minimized = false;
                                        }
                                        self.focused_window = Some(w_idx);
                                        dirty = true;
                                    }
                                }
                                TaskbarHit::Notifications => {
                                    self.notifications_visible = !self.notifications_visible;
                                    dirty = true;
                                }
                                TaskbarHit::Volume => {
                                    self.volume_clicked = true;
                                    dirty = true;
                                }
                                TaskbarHit::Clock => {
                                    self.clock_clicked = true;
                                    dirty = true;
                                }
                                _ => {
                                    // Clicked on taskbar but not a specific element
                                    dirty = true;
                                }
                            }
                        } else {
                        // ── Launcher overlay click detection ──
                        if self.launcher_active {
                            // Record click position — state.rs will do the hit test
                            // since it has access to desktop.pinned_apps
                            self.launcher_click_pos = Some((self.cursor_x, self.cursor_y));
                            dirty = true;
                        }
                        // ── Notification panel click → mark all read ──
                        if self.notifications_visible {
                            // Click anywhere closes notifications and marks them as read
                            let panel_w = crate::render::NOTIF_PANEL_W;
                            let panel_h = crate::render::NOTIF_PANEL_H;
                            let panel_x = self.fb_width - panel_w - 10;
                            let panel_y = 10;
                            if self.cursor_x >= panel_x && self.cursor_x < panel_x + panel_w
                                && self.cursor_y >= panel_y && self.cursor_y < panel_y + panel_h
                            {
                                // Clicked inside notification panel — mark read
                                self.notifications_visible = false;
                                self.notifications_mark_read = true;
                                dirty = true;
                            }
                        }
                        // ── Window focus / interaction ──
                        let focus = focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count);
                        if let Some(idx) = focus {
                            self.focused_window = Some(idx);

                            // Check title bar button clicks
                            let btn = windows[idx].check_button_click(self.cursor_x, self.cursor_y);
                            match btn {
                                WindowButton::Close => {
                                    windows[idx].closing = true;
                                    self.focused_window = None;
                                }
                                WindowButton::Maximize => {
                                    let w = &mut windows[idx];
                                    if w.maximized {
                                        let (sx, sy, sw, sh) = w.stored_rect;
                                        w.x = sx; w.y = sy; w.w = sw; w.h = sh;
                                        w.maximized = false;
                                    } else {
                                        w.stored_rect = (w.x, w.y, w.w, w.h);
                                        w.x = 0;
                                        w.y = ShellWindow::TITLE_H;
                                        w.w = self.fb_width;
                                        w.h = self.fb_height - ShellWindow::TITLE_H - 44;
                                        w.maximized = true;
                                    }
                                }
                                WindowButton::Minimize => {
                                    windows[idx].minimized = true;
                                    self.focused_window = None;
                                }
                                WindowButton::None => {
                                    // Check for resize handle
                                    let w = &windows[idx];
                                    let rx = w.x + w.w - ShellWindow::RESIZE_HANDLE_SIZE;
                                    let ry = w.y + w.h - ShellWindow::RESIZE_HANDLE_SIZE;
                                    if self.cursor_x >= rx && self.cursor_y >= ry {
                                        self.resizing_window = Some(idx);
                                    } else if windows[idx].title_bar_contains(self.cursor_x, self.cursor_y) {
                                        // Start dragging
                                        self.dragging_window = Some(idx);
                                        self.drag_offset_x = self.cursor_x - windows[idx].x;
                                        self.drag_offset_y = self.cursor_y - windows[idx].y;
                                    }
                                }
                            }

                            // Forward to external surface
                            if let WindowContent::External(s_idx) = windows[idx].content {
                                let s = s_idx as usize;
                                if s < surfaces.len() && surfaces[s].active {
                                    let ev = SideWindEvent {
                                        event_type: SWND_EVENT_TYPE_MOUSE_BUTTON,
                                        data1: button as i32,
                                        data2: self.cursor_x - windows[idx].x,
                                        data3: if pressed { 1 } else { 0 },
                                    };
                                    let _ = unsafe {
                                        eclipse_send(
                                            surfaces[s].pid,
                                            sidewind::MSG_TYPE_INPUT,
                                            &ev as *const _ as *const core::ffi::c_void,
                                            core::mem::size_of::<SideWindEvent>(),
                                            0,
                                        )
                                    };
                                }
                            }
                        } else {
                            self.focused_window = None;
                        }
                        } // end else (not on taskbar)
                        dirty = true;
                    } else {
                        // Button released
                        self.dragging_window = None;
                        self.resizing_window = None;
                    }
                }
            }
            _ => {}
        }
        dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scancode_to_action_dashboard() {
        assert_eq!(scancode_to_action(0x5B, 0), KeyAction::ToggleDashboard);
    }

    #[test]
    fn test_scancode_to_action_workspace() {
        assert_eq!(scancode_to_action(0x02, 8), KeyAction::SwitchWorkspace(0));
        assert_eq!(scancode_to_action(0x03, 8), KeyAction::SwitchWorkspace(1));
    }

    #[test]
    fn test_scancode_to_char() {
        assert_eq!(scancode_to_char(0x1E, false), Some('a'));
        assert_eq!(scancode_to_char(0x1E, true), Some('A'));
        assert_eq!(scancode_to_char(0x39, false), Some(' '));
    }

    #[test]
    fn test_input_state_new() {
        let state = InputState::new(1920, 1080);
        assert_eq!(state.cursor_x, 960);
        assert_eq!(state.cursor_y, 540);
        assert!(!state.dashboard_active);
        assert!(!state.lock_screen_active);
    }

    #[test]
    fn test_apply_event_mouse_move() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Use core X-axis event
        let ev = InputEvent { device_id: 0, event_type: 1, code: 0, value: 5, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.cursor_x > 960);
    }

    #[test]
    fn test_cursor_clamping() {
        let mut state = InputState::new(100, 100);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Move cursor far right using X-axis event
        let ev = InputEvent { device_id: 0, event_type: 1, code: 0, value: 500, timestamp: 0 };
        let _ = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(state.cursor_x <= 99);
    }

    #[test]
    fn test_taskbar_hit_launcher() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let hit = taskbar_hit_test(20, 1080 - 20, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::Launcher);
    }

    #[test]
    fn test_taskbar_hit_workspace() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // Workspace 0 starts at x=48
        let hit = taskbar_hit_test(55, 1080 - 20, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::Workspace(0));
        // Workspace 1 at x=74
        let hit = taskbar_hit_test(80, 1080 - 20, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::Workspace(1));
    }

    #[test]
    fn test_taskbar_hit_pinned_app() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // First pinned app starts at TASKBAR_APPS_START_X = 160
        let hit = taskbar_hit_test(170, 1080 - 20, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::PinnedApp(0));
        // Second pinned app at 160 + 32 + 6 = 198
        let hit = taskbar_hit_test(205, 1080 - 20, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::PinnedApp(1));
    }

    #[test]
    fn test_taskbar_hit_none_above_bar() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // Click above the taskbar should return None
        let hit = taskbar_hit_test(500, 500, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::None);
    }

    #[test]
    fn test_taskbar_click_launcher_toggles() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Move cursor to taskbar launcher area
        state.cursor_x = 20;
        state.cursor_y = 1080 - 20;

        // Left mouse button press
        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.launcher_active, "launcher should toggle on");

        // Click again to toggle off
        let ev2 = InputEvent { device_id: 0, event_type: 2, code: 0, value: 0, timestamp: 0 };
        state.apply_event(&ev2, &mut windows, &mut count, &mut surfaces);
        let ev3 = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev3, &mut windows, &mut count, &mut surfaces);
        assert!(!state.launcher_active, "launcher should toggle off");
    }

    #[test]
    fn test_taskbar_click_workspace_switch() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        assert_eq!(state.current_workspace, 0);

        // Move cursor to workspace indicator 1 (at x=74, y=bar_y+20)
        state.cursor_x = 80;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert_eq!(state.current_workspace, 1);
    }

    #[test]
    fn test_taskbar_click_pinned_app() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Click on first pinned app at x=170
        state.cursor_x = 170;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert_eq!(state.last_pinned_app_click, Some(0));
    }

    #[test]
    fn test_taskbar_click_notifications() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Notification area is at tray_start + 155 = (1920 - 250) + 155 = 1825
        state.cursor_x = 1825;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.notifications_visible, "notifications should toggle on");
    }

    #[test]
    fn test_taskbar_hit_volume() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // Volume is at tray_start + 180 = (1920 - 250) + 180 = 1850
        let hit = taskbar_hit_test(1850, 1080 - 20, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::Volume);
    }

    #[test]
    fn test_taskbar_hit_clock() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // Clock is at fb_w - 50 = 1870
        let hit = taskbar_hit_test(1880, 1080 - 20, 1920, 1080, 5, &windows, 0, 0);
        assert_eq!(hit, TaskbarHit::Clock);
    }

    #[test]
    fn test_taskbar_click_volume() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Volume area at tray_start + 180 = 1850
        state.cursor_x = 1850;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.volume_clicked, "volume_clicked should be set");
    }

    #[test]
    fn test_taskbar_hover_tracking() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Move cursor to first pinned app
        let ev = InputEvent { device_id: 0, event_type: 1, code: 0xFFFF, value: 0, timestamp: 0 };
        state.cursor_x = 170;
        state.cursor_y = 1080 - 20;
        let _ = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.hovered_taskbar_element, TaskbarHit::PinnedApp(0));
    }

    #[test]
    fn test_launcher_hit_test_basic() {
        use crate::desktop::PinnedApp;
        let apps: [PinnedApp; 8] = core::array::from_fn(|_| PinnedApp::default());
        let mut apps = apps;
        apps[0] = PinnedApp::with_exec("Terminal", 0, 200, 100, "/bin/terminal");
        apps[1] = PinnedApp::with_exec("Files", 100, 150, 255, "/bin/files");

        let fb_height = 1080;
        // Panel is at y = 1080 - 44 - 400 - 10 = 626, x=10, w=300
        // First item at y = 626 + 50 + 0*36 = 676, items go from y-10 to y-10+36 = 666..702
        let hit = launcher_hit_test(100, 680, fb_height, 2, &apps, false, "");
        assert_eq!(hit, Some(0));

        // Second item at y = 626 + 50 + 1*36 = 712, range 702..738
        let hit = launcher_hit_test(100, 720, fb_height, 2, &apps, false, "");
        assert_eq!(hit, Some(1));
    }

    #[test]
    fn test_launcher_hit_test_outside() {
        use crate::desktop::PinnedApp;
        let apps: [PinnedApp; 8] = core::array::from_fn(|_| PinnedApp::default());
        let mut apps = apps;
        apps[0] = PinnedApp::new("Terminal", 0, 200, 100);

        // Click outside launcher panel
        let hit = launcher_hit_test(500, 500, 1080, 1, &apps, false, "");
        assert_eq!(hit, None);
    }

    #[test]
    fn test_launcher_hit_test_search_filter() {
        use crate::desktop::PinnedApp;
        let apps: [PinnedApp; 8] = core::array::from_fn(|_| PinnedApp::default());
        let mut apps = apps;
        apps[0] = PinnedApp::new("Terminal", 0, 200, 100);
        apps[1] = PinnedApp::new("Files", 100, 150, 255);

        let fb_height = 1080;
        // With search "Fi" active, only Files matches; it becomes visible_idx=0
        // First visible item position: y = 626 + 50 + 0*36 = 676
        let hit = launcher_hit_test(100, 680, fb_height, 2, &apps, true, "Fi");
        assert_eq!(hit, Some(1)); // Files is index 1 in pinned_apps

        // Terminal is filtered out — hitting first slot should still be Files
        let hit = launcher_hit_test(100, 680, fb_height, 2, &apps, true, "Te");
        assert_eq!(hit, Some(0)); // Terminal is index 0
    }

    #[test]
    fn test_taskbar_minimized_window_hit() {
        let mut windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // Create a minimized window
        windows[0].content = WindowContent::InternalDemo;
        windows[0].minimized = true;
        windows[0].workspace = 0;

        // Minimized windows should now appear in taskbar hit test
        // Running windows area starts after pinned apps separator
        // With 0 pinned apps: sep2_x = TASKBAR_APPS_START_X + 2 = 162, win_x = 170
        let hit = taskbar_hit_test(180, 1080 - 20, 1920, 1080, 0, &windows, 1, 0);
        assert_eq!(hit, TaskbarHit::WindowTask(0));
    }

    #[test]
    fn test_launcher_click_records_position() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        state.launcher_active = true;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Click inside launcher area (not on taskbar)
        state.cursor_x = 100;
        state.cursor_y = 700;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert_eq!(state.launcher_click_pos, Some((100, 700)));
    }
}
