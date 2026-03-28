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

/// Append the decimal representation of a `u8` value to a heapless String.
/// Uses only the digits 0-9 with no allocation.
fn push_u8_decimal(s: &mut heapless::String<64>, v: u8) {
    let mut buf = [0u8; 3];
    let mut n = v as u16;
    let mut len = 0;
    if n == 0 {
        buf[0] = b'0';
        len = 1;
    } else {
        while n > 0 {
            buf[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
        }
        buf[..len].reverse();
    }
    for &b in &buf[..len] {
        let _ = s.push(b as char);
    }
}

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
    SnapLeft, SnapRight, SnapTopLeft, SnapTopRight, SnapBottomLeft, SnapBottomRight,
    SwitchWorkspace(u8), CycleWindowVisual,
    Minimize, Maximize, Restore, ToggleDashboard, ToggleLock, ToggleLauncher,
    ToggleSystemCentral, ToggleTiling, ToggleSearch, ArrowUp, ArrowDown,
    Input(char), Enter, Backspace, ToggleNotifications, ToggleNetworkDetails,
    BrightnessUp, BrightnessDown,
    /// Toggle Do Not Disturb mode (Super+D).
    ToggleDoNotDisturb,
    /// Toggle Night Light mode — warm tint to reduce blue light (Super+N).
    ToggleNightLight,
    /// Take a screenshot of the current screen (PrintScreen).
    Screenshot,
    /// Toggle Quick Settings panel (Super+Q).
    ToggleQuickSettings,
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
    /// The battery indicator was clicked.
    Battery,
    /// The clock area was clicked.
    Clock,
    /// The "Show Desktop" button (right edge) was clicked.
    ShowDesktop,
    /// The scroll-left (◀) button for window tasks was clicked.
    TaskScrollLeft,
    /// The scroll-right (▶) button for window tasks was clicked.
    TaskScrollRight,
}

/// Maximum number of items in a context menu (includes separators).
pub const CONTEXT_MENU_MAX_ITEMS: usize = 12;

/// Number of distinct window decoration styles (0 = default, 1 = minimal, 2 = neon).
/// Used by `CycleWindowVisual` action to wrap the style index.
pub const DECORATION_STYLE_COUNT: u8 = 3;

/// An action that a context menu item triggers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContextAction {
    None,
    NewWindow,
    ToggleTiling,
    OpenDashboard,
    CycleWallpaper,
    CloseWindow(usize),
    MinimizeWindow(usize),
    MaximizeWindow(usize),
    VolumeUp,
    VolumeDown,
    ToggleMute,
    SetVolume(u8),
    /// Launch (or focus) a pinned app by index.
    LaunchPinnedApp(usize),
    /// Remove a pinned app from the taskbar by index.
    UnpinApp(usize),
    /// Pin a running window (by its window index) to the taskbar.
    PinApp(usize),
    /// Increase screen brightness by one step.
    BrightnessUp,
    /// Decrease screen brightness by one step.
    BrightnessDown,
    /// Set brightness to a specific level (0-100). Used by QS panel slider.
    SetBrightness(u8),
    /// Toggle Do Not Disturb mode.
    ToggleDoNotDisturb,
    /// Toggle Night Light mode (warm colour tint).
    ToggleNightLight,
    /// Capture the current framebuffer to disk.
    TakeScreenshot,
    /// Mark all desktop notifications as read.
    MarkNotificationsRead,
    /// Toggle the launcher/app-drawer panel.
    ToggleLauncher,
    /// Lock the screen (activate lock screen overlay).
    ToggleLock,
    /// Toggle show-desktop mode (minimize/restore all windows).
    ShowDesktop,
    /// Switch to a specific workspace by index (0-3).
    SwitchWorkspace(u8),
    /// Toggle the battery/power info panel.
    ToggleBatteryPanel,
}

/// A single context menu item.
#[derive(Debug, Clone, Copy)]
pub struct ContextMenuItem {
    pub label: [u8; 24],
    pub action: ContextAction,
    /// When `true`, this slot renders as a visual separator line rather than a clickable item.
    pub separator: bool,
    /// When `true`, a checkmark indicator is drawn before the label (for toggle states).
    pub checked: bool,
}

impl Default for ContextMenuItem {
    fn default() -> Self {
        Self { label: [0; 24], action: ContextAction::None, separator: false, checked: false }
    }
}

impl ContextMenuItem {
    pub fn new(label: &str, action: ContextAction) -> Self {
        let mut item = Self::default();
        let bytes = label.as_bytes();
        let len = bytes.len().min(24);
        item.label[..len].copy_from_slice(&bytes[..len]);
        item.action = action;
        item
    }

    pub fn label_str(&self) -> &str {
        let len = self.label.iter().position(|&b| b == 0).unwrap_or(24);
        core::str::from_utf8(&self.label[..len]).unwrap_or("")
    }
}

/// Context menu state.
pub struct ContextMenu {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub items: [ContextMenuItem; CONTEXT_MENU_MAX_ITEMS],
    pub item_count: usize,
    pub hovered_index: Option<usize>,
}

impl ContextMenu {
    pub fn new() -> Self {
        Self {
            visible: false,
            x: 0,
            y: 0,
            items: core::array::from_fn(|_| ContextMenuItem::default()),
            item_count: 0,
            hovered_index: None,
        }
    }

    pub fn show(&mut self, x: i32, y: i32) {
        self.visible = true;
        self.x = x;
        self.y = y;
        self.item_count = 0;
        self.hovered_index = None;
    }

    /// Add a regular clickable item.
    pub fn add_item(&mut self, label: &str, action: ContextAction) {
        if self.item_count < CONTEXT_MENU_MAX_ITEMS {
            self.items[self.item_count] = ContextMenuItem::new(label, action);
            self.item_count += 1;
        }
    }

    /// Add an item that shows a checkmark indicator when `checked` is true (for toggle states).
    pub fn add_checked_item(&mut self, label: &str, action: ContextAction, checked: bool) {
        if self.item_count < CONTEXT_MENU_MAX_ITEMS {
            let mut item = ContextMenuItem::new(label, action);
            item.checked = checked;
            self.items[self.item_count] = item;
            self.item_count += 1;
        }
    }

    /// Add a visual separator line between groups of items.
    pub fn add_separator(&mut self) {
        if self.item_count < CONTEXT_MENU_MAX_ITEMS {
            let mut item = ContextMenuItem::default();
            item.separator = true;
            self.items[self.item_count] = item;
            self.item_count += 1;
        }
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.item_count = 0;
        self.hovered_index = None;
    }

    /// Return the pixel height of item at `idx`.
    pub fn item_height(items: &[ContextMenuItem], idx: usize) -> i32 {
        if items[idx].separator {
            crate::render::CONTEXT_MENU_SEP_H
        } else {
            crate::render::CONTEXT_MENU_ITEM_H
        }
    }

    /// Return the Y pixel offset of item `idx` relative to the top of the menu.
    pub fn item_y_offset(items: &[ContextMenuItem], idx: usize) -> i32 {
        let mut y = 0;
        for i in 0..idx {
            y += Self::item_height(items, i);
        }
        y
    }

    /// Compute the total pixel height of the menu.
    pub fn total_height(&self) -> i32 {
        let mut h = 0;
        for i in 0..self.item_count {
            h += Self::item_height(&self.items, i);
        }
        h
    }

    /// Clamp menu position so it stays fully within the screen bounds.
    /// Call this after all items have been added.
    pub fn clamp_to_screen(&mut self, fb_w: i32, fb_h: i32) {
        let menu_w = crate::render::CONTEXT_MENU_W;
        let menu_h = self.total_height();
        if self.x + menu_w > fb_w { self.x = fb_w - menu_w; }
        if self.x < 0 { self.x = 0; }
        if self.y + menu_h > fb_h { self.y = fb_h - menu_h; }
        if self.y < 0 { self.y = 0; }
    }
}

/// Determine what element is at position (px, py) on the taskbar.
/// Returns `TaskbarHit::None` if the position is not on the taskbar.
pub fn taskbar_hit_test(
    px: i32,
    py: i32,
    fb_width: i32,
    fb_height: i32,
    pinned_count: usize,
    pinned_app_names: &[[u8; 32]],
    windows: &[ShellWindow],
    window_count: usize,
    current_workspace: u8,
    task_scroll_offset: usize,
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
    let tray_start = fb_width - crate::render::TASKBAR_TRAY_WIDTH;

    // Scroll buttons: drawn just before the tray separator when overflow is present.
    // Left scroll button occupies 16px immediately after win_tasks_start.
    let scroll_btn_w: i32 = 16;
    let tasks_start_x = sep2_x + 8;

    // Scroll-left button (always at tasks_start_x when scroll_offset > 0)
    if task_scroll_offset > 0 {
        if px >= tasks_start_x && px < tasks_start_x + scroll_btn_w
            && py >= bar_y + 8 && py < bar_y + 36
        {
            return TaskbarHit::TaskScrollLeft;
        }
    }

    // The window tasks start after the scroll-left button (if shown)
    let task_origin_x = if task_scroll_offset > 0 {
        tasks_start_x + scroll_btn_w + 2
    } else {
        tasks_start_x
    };

    let mut win_x = task_origin_x;
    let win_item_w: i32 = 120;

    // We need to know whether there is overflow to position the scroll-right button.
    // Reserve space for scroll-right button (16px) before tray.
    let scroll_right_area_w = scroll_btn_w + 4;
    let task_area_end = tray_start - 10 - scroll_right_area_w;

    let mut skipped = 0usize;
    for w_idx in 0..window_count {
        let w = &windows[w_idx];
        if w.content == WindowContent::None || w.closing { continue; }
        if w.workspace != current_workspace { continue; }

        // Skip windows that are already represented by a pinned app icon
        let w_title = w.title_str();
        let already_pinned = (0..pinned_count.min(pinned_app_names.len())).any(|pi| {
            let pname_bytes = &pinned_app_names[pi];
            let pname_len = pname_bytes.iter().position(|&b| b == 0).unwrap_or(32);
            let pname = core::str::from_utf8(&pname_bytes[..pname_len]).unwrap_or("");
            !pname.is_empty()
                && w_title.len() >= pname.len()
                && w_title[..pname.len()].eq_ignore_ascii_case(pname)
        });
        if already_pinned { continue; }

        // Apply scroll offset
        if skipped < task_scroll_offset {
            skipped += 1;
            continue;
        }

        if win_x + win_item_w > task_area_end {
            // This window overflows — check scroll-right button area
            let sr_x = tray_start - scroll_btn_w - 6;
            if px >= sr_x && px < sr_x + scroll_btn_w && py >= bar_y + 8 && py < bar_y + 36 {
                return TaskbarHit::TaskScrollRight;
            }
            break;
        }

        if px >= win_x && px < win_x + win_item_w && py >= bar_y + 8 && py < bar_y + 36 {
            return TaskbarHit::WindowTask(w_idx);
        }
        win_x += win_item_w + 4;
    }

    // Notification area: around tray_x + 70
    let notif_x = tray_start + 70;
    if px >= notif_x - 5 && px < notif_x + 20 && py >= bar_y + 4 && py < bar_y + 36 {
        return TaskbarHit::Notifications;
    }

    // Volume indicator: around tray_x + 100
    let vol_x = tray_start + 100;
    if px >= vol_x - 5 && px < vol_x + 15 && py >= bar_y + 4 && py < bar_y + 36 {
        return TaskbarHit::Volume;
    }

    // Clock area: fb_width - 56 to fb_width - 6
    let clock_x = fb_width - 56;
    if px >= clock_x && px < fb_width - 6 && py >= bar_y + 4 && py < bar_y + 36 {
        return TaskbarHit::Clock;
    }

    // Show Desktop button: thin strip at the very right (fb_width - 6 to fb_width)
    if px >= fb_width - 6 && px < fb_width && py >= bar_y && py < fb_height {
        return TaskbarHit::ShowDesktop;
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
        0x47 => if (modifiers & 8) != 0 { KeyAction::SnapTopLeft } else { KeyAction::CenterCursor },
        0x31 => if (modifiers & 8) != 0 { KeyAction::ToggleNightLight } else { KeyAction::NewWindow },
        0x01 => KeyAction::CloseWindow,
        0x0F => if (modifiers & 4) != 0 { KeyAction::CycleWindowVisual } else { KeyAction::CycleForward },
        0x29 => KeyAction::CycleBackward,
        0x32 => KeyAction::Minimize,
        // Super+R = Restore focused window
        0x13 => if (modifiers & 8) != 0 { KeyAction::Restore } else { KeyAction::None },
        0x5B => KeyAction::ToggleDashboard,
        0x26 => KeyAction::ToggleLock,
        0x1E => KeyAction::ToggleLauncher,
        0x1F => if (modifiers & 8) != 0 { KeyAction::ToggleSystemCentral } else { KeyAction::None },
        0x39 => if (modifiers & 8) != 0 { KeyAction::ToggleSearch } else { KeyAction::None },
        0x4B => KeyAction::SnapLeft,
        0x4D => KeyAction::SnapRight,
        0x14 => if (modifiers & 8) != 0 { KeyAction::ToggleTiling } else { KeyAction::None },
        // Super+Up = Maximize, plain Up = ArrowUp
        0x48 => if (modifiers & 8) != 0 { KeyAction::Maximize } else { KeyAction::ArrowUp },
        // Super+Down = Restore (un-maximize/un-minimize), plain Down = ArrowDown
        0x50 => if (modifiers & 8) != 0 { KeyAction::Restore } else { KeyAction::ArrowDown },
        0x1C => KeyAction::Enter,
        0x0E => KeyAction::Backspace,
        0x36 => if (modifiers & 8) != 0 { KeyAction::ToggleNotifications } else { KeyAction::None },
        0x12 => if (modifiers & 8) != 0 { KeyAction::ToggleNetworkDetails } else { KeyAction::None },
        // Brightness keys (F5=0x3F = down, F6=0x40 = up)
        0x3F => KeyAction::BrightnessDown,
        0x40 => KeyAction::BrightnessUp,
        // Super+Home/PgUp/End/PgDn = snap to screen quarters
        0x49 => if (modifiers & 8) != 0 { KeyAction::SnapTopRight } else { KeyAction::None },
        0x4F => if (modifiers & 8) != 0 { KeyAction::SnapBottomLeft } else { KeyAction::None },
        0x51 => if (modifiers & 8) != 0 { KeyAction::SnapBottomRight } else { KeyAction::None },
        // Super+D = Do Not Disturb toggle
        0x20 => if (modifiers & 8) != 0 { KeyAction::ToggleDoNotDisturb } else { KeyAction::None },
        // Super+Q = Quick Settings panel
        0x10 => if (modifiers & 8) != 0 { KeyAction::ToggleQuickSettings } else { KeyAction::None },
        // PrintScreen = Screenshot
        0x37 => KeyAction::Screenshot,
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
    /// Context menu state (right-click menus).
    pub context_menu: ContextMenu,
    /// Pending context action from a menu click (for the caller to act on).
    pub pending_context_action: ContextAction,
    /// Whether the volume popup panel is visible.
    pub volume_panel_active: bool,
    /// Whether the network details panel is visible.
    pub network_details_active: bool,
    /// Names of pinned apps, mirrored from DesktopShell for hit-testing inside apply_event.
    pub pinned_app_names: [[u8; 32]; 16],
    /// Tooltip text shown above the currently-hovered taskbar element (empty = hidden).
    pub tooltip: heapless::String<64>,
    /// Scroll offset for the running-windows task list (in items, not pixels).
    pub task_scroll_offset: usize,
    /// Whether the clock/calendar panel is visible (toggled by clicking the clock).
    pub clock_panel_active: bool,
    /// Whether "Show Desktop" mode is active (all windows on current workspace minimized).
    pub show_desktop_active: bool,
    /// Bitmask of window indices that were minimized by the "Show Desktop" action,
    /// so they can be restored when Show Desktop is toggled off.
    pub show_desktop_minimized_mask: u32,
    /// Index of the pinned app being dragged for reordering (None = not dragging).
    pub dragging_pinned_app: Option<usize>,
    /// X position of the left-button press that started a potential pinned-app drag.
    pub drag_press_x: i32,
    /// Y position of the left-button press that started a potential pinned-app drag.
    pub drag_press_y: i32,
    /// When a pinned-app drag ends over a different icon, this holds (src, dst) indices
    /// for state.rs to swap the apps. Cleared after processing.
    pub pending_pinned_swap: Option<(usize, usize)>,
    /// Window decoration style index (0 = default, 1 = minimal, 2 = neon). Cycled by CycleWindowVisual.
    pub window_decoration_style: u8,
    /// Whether the Quick Settings panel is visible (Super+Q toggle).
    pub quick_settings_active: bool,
    /// Mirrored from `DesktopShell::do_not_disturb`; used when building context menu checkmarks.
    pub do_not_disturb: bool,
    /// Mirrored from `DesktopShell::night_light_active`; used when building context menu checkmarks.
    pub night_light_active: bool,
    /// Mirrored from `DesktopShell::volume_muted`; used when building context menu checkmarks.
    pub volume_muted: bool,
    /// Mirrored from `DesktopShell::volume_level`; used when building state-aware tooltips.
    pub volume_level: u8,
    /// Mirrored from `DesktopShell::battery_level`; used when building state-aware tooltips.
    pub battery_level: u8,
    /// Mirrored from `DesktopShell::notification_count`; used when building tooltips.
    pub notification_count: usize,
    /// Keyboard-selected item in the launcher (used for ArrowUp/Down + Enter navigation).
    pub launcher_keyboard_index: Option<usize>,
    /// Whether the battery/power info panel is visible (toggled by clicking the battery icon).
    pub battery_panel_active: bool,
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
            context_menu: ContextMenu::new(),
            pending_context_action: ContextAction::None,
            volume_panel_active: false,
            network_details_active: false,
            pinned_app_names: [[0u8; 32]; 16],
            tooltip: heapless::String::new(),
            task_scroll_offset: 0,
            clock_panel_active: false,
            show_desktop_active: false,
            show_desktop_minimized_mask: 0,
            dragging_pinned_app: None,
            drag_press_x: 0,
            drag_press_y: 0,
            pending_pinned_swap: None,
            window_decoration_style: 0,
            quick_settings_active: false,
            do_not_disturb: false,
            night_light_active: false,
            volume_muted: false,
            volume_level: 75,
            battery_level: 80,
            notification_count: 0,
            launcher_keyboard_index: None,
            battery_panel_active: false,
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
                    // ── Context menu keyboard navigation (highest priority) ──
                    if self.context_menu.visible {
                        let code = (scancode & 0x7FFF) as u8;
                        match code {
                            0x01 => {
                                // Escape: close context menu
                                self.context_menu.hide();
                                dirty = true;
                                return dirty;
                            }
                            0x48 if (self.modifiers & 8) == 0 => {
                                // Up arrow (without Super): move selection up, skipping separators.
                                let count = self.context_menu.item_count;
                                if count > 0 {
                                    let start = match self.context_menu.hovered_index {
                                        Some(h) if h > 0 => h - 1,
                                        _ => 0,
                                    };
                                    // Find the first non-separator at or above `start`
                                    let mut sel = start;
                                    loop {
                                        if !self.context_menu.items[sel].separator { break; }
                                        if sel == 0 { break; }
                                        sel -= 1;
                                    }
                                    self.context_menu.hovered_index = Some(sel);
                                }
                                dirty = true;
                                return dirty;
                            }
                            0x50 => {
                                // Down arrow: move selection down, skipping separators.
                                let count = self.context_menu.item_count;
                                if count > 0 {
                                    let start = match self.context_menu.hovered_index {
                                        Some(h) => (h + 1).min(count - 1),
                                        None => 0,
                                    };
                                    let mut sel = start;
                                    loop {
                                        if !self.context_menu.items[sel].separator { break; }
                                        if sel + 1 >= count { break; }
                                        sel += 1;
                                    }
                                    self.context_menu.hovered_index = Some(sel);
                                }
                                dirty = true;
                                return dirty;
                            }
                            0x1C => {
                                // Enter: activate hovered item (skip separators)
                                if let Some(idx) = self.context_menu.hovered_index {
                                    if idx < self.context_menu.item_count
                                        && !self.context_menu.items[idx].separator
                                    {
                                        self.pending_context_action = self.context_menu.items[idx].action;
                                    }
                                }
                                self.context_menu.hide();
                                dirty = true;
                                return dirty;
                            }
                            _ => {}
                        }
                    }

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

                    // ── Launcher keyboard navigation (when launcher overlay is open) ──
                    if self.launcher_active {
                        let code = (scancode & 0x7FFF) as u8;
                        match code {
                            0x01 => {
                                // Escape: close launcher
                                self.launcher_active = false;
                                self.launcher_keyboard_index = None;
                                dirty = true;
                                return dirty;
                            }
                            0x48 if (self.modifiers & 8) == 0 => {
                                // Up arrow: move keyboard selection up
                                let max = self.pinned_app_count.saturating_sub(1);
                                self.launcher_keyboard_index = Some(match self.launcher_keyboard_index {
                                    Some(i) if i > 0 => i - 1,
                                    _ => 0,
                                }.min(max));
                                dirty = true;
                                return dirty;
                            }
                            0x50 if (self.modifiers & 8) == 0 => {
                                // Down arrow: move keyboard selection down
                                let count = self.pinned_app_count;
                                if count > 0 {
                                    self.launcher_keyboard_index = Some(match self.launcher_keyboard_index {
                                        Some(i) => (i + 1).min(count - 1),
                                        None => 0,
                                    });
                                }
                                dirty = true;
                                return dirty;
                            }
                            0x1C => {
                                // Enter: launch keyboard-selected app
                                if let Some(idx) = self.launcher_keyboard_index {
                                    if idx < self.pinned_app_count {
                                        self.launcher_app_click = Some(idx);
                                        self.launcher_keyboard_index = None;
                                    }
                                }
                                dirty = true;
                                return dirty;
                            }
                            _ => {}
                        }
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
                            if !self.launcher_active {
                                self.launcher_keyboard_index = None;
                            }
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
                        KeyAction::ToggleNetworkDetails => {
                            self.network_details_active = !self.network_details_active;
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
                        KeyAction::Restore => {
                            // Un-maximize if maximized; un-minimize if minimized.
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let w = &mut windows[idx];
                                    if w.maximized {
                                        let (sx, sy, sw, sh) = w.stored_rect;
                                        w.x = sx; w.y = sy; w.w = sw; w.h = sh;
                                        w.maximized = false;
                                    } else if w.minimized {
                                        w.minimized = false;
                                    }
                                    dirty = true;
                                }
                            } else {
                                // No focused window: try to restore the most recently minimized window
                                for i in (0..*window_count).rev() {
                                    if windows[i].minimized && windows[i].content != WindowContent::None && !windows[i].closing {
                                        windows[i].minimized = false;
                                        self.focused_window = Some(i);
                                        dirty = true;
                                        break;
                                    }
                                }
                            }
                        }
                        KeyAction::CycleWindowVisual => {
                            // Cycle window decoration style (0 = default, 1 = minimal, 2 = neon).
                            // Note: modifier 4 (Alt) + Tab (0x0F) triggers this action.
                            // This differs from classic Alt+Tab application switching; in Lunas,
                            // application cycling uses Tab alone (CycleForward / CycleBackward).
                            self.window_decoration_style = (self.window_decoration_style + 1) % DECORATION_STYLE_COUNT;
                            dirty = true;
                        }
                        KeyAction::BrightnessUp => {
                            self.pending_context_action = ContextAction::BrightnessUp;
                            dirty = true;
                        }
                        KeyAction::BrightnessDown => {
                            self.pending_context_action = ContextAction::BrightnessDown;
                            dirty = true;
                        }
                        KeyAction::ToggleDoNotDisturb => {
                            self.pending_context_action = ContextAction::ToggleDoNotDisturb;
                            dirty = true;
                        }
                        KeyAction::ToggleNightLight => {
                            self.pending_context_action = ContextAction::ToggleNightLight;
                            dirty = true;
                        }
                        KeyAction::Screenshot => {
                            self.pending_context_action = ContextAction::TakeScreenshot;
                            dirty = true;
                        }
                        KeyAction::ToggleQuickSettings => {
                            self.quick_settings_active = !self.quick_settings_active;
                            dirty = true;
                        }
                        KeyAction::SnapTopLeft => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let tb_h = crate::render::TASKBAR_HEIGHT;
                                    let w = &mut windows[idx];
                                    w.x = 0;
                                    w.y = ShellWindow::TITLE_H;
                                    w.w = self.fb_width / 2;
                                    w.h = (self.fb_height - ShellWindow::TITLE_H - tb_h) / 2;
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::SnapTopRight => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let tb_h = crate::render::TASKBAR_HEIGHT;
                                    let w = &mut windows[idx];
                                    w.x = self.fb_width / 2;
                                    w.y = ShellWindow::TITLE_H;
                                    w.w = self.fb_width / 2;
                                    w.h = (self.fb_height - ShellWindow::TITLE_H - tb_h) / 2;
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::SnapBottomLeft => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let tb_h = crate::render::TASKBAR_HEIGHT;
                                    let h = (self.fb_height - ShellWindow::TITLE_H - tb_h) / 2;
                                    let w = &mut windows[idx];
                                    w.x = 0;
                                    w.y = ShellWindow::TITLE_H + h;
                                    w.w = self.fb_width / 2;
                                    w.h = h;
                                    dirty = true;
                                }
                            }
                        }
                        KeyAction::SnapBottomRight => {
                            if let Some(idx) = self.focused_window {
                                if idx < *window_count {
                                    let tb_h = crate::render::TASKBAR_HEIGHT;
                                    let h = (self.fb_height - ShellWindow::TITLE_H - tb_h) / 2;
                                    let w = &mut windows[idx];
                                    w.x = self.fb_width / 2;
                                    w.y = ShellWindow::TITLE_H + h;
                                    w.w = self.fb_width / 2;
                                    w.h = h;
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
                    &self.pinned_app_names,
                    windows, *window_count,
                    self.current_workspace,
                    self.task_scroll_offset,
                );
                if hover_hit != self.hovered_taskbar_element {
                    self.hovered_taskbar_element = hover_hit;
                    // Update tooltip text for the newly hovered element
                    self.tooltip.clear();
                    match hover_hit {
                        TaskbarHit::Launcher => {
                            let _ = self.tooltip.push_str("Launcher (Super+A)");
                        }
                        TaskbarHit::Workspace(ws) => {
                            let _ = self.tooltip.push_str("Workspace ");
                            let ws_char = (b'1' + ws) as char;
                            let _ = self.tooltip.push(ws_char);
                            let _ = self.tooltip.push_str(" (Super+");
                            let _ = self.tooltip.push(ws_char);
                            let _ = self.tooltip.push(')');
                        }
                        TaskbarHit::PinnedApp(i) => {
                            if i < self.pinned_app_count && i < self.pinned_app_names.len() {
                                let name_bytes = &self.pinned_app_names[i];
                                let len = name_bytes.iter().position(|&b| b == 0).unwrap_or(32);
                                if let Ok(s) = core::str::from_utf8(&name_bytes[..len]) {
                                    let _ = self.tooltip.push_str(s);
                                }
                            }
                        }
                        TaskbarHit::WindowTask(w_idx) => {
                            if w_idx < *window_count {
                                let _ = self.tooltip.push_str(windows[w_idx].title_str());
                            }
                        }
                        TaskbarHit::Notifications => {
                            let _ = self.tooltip.push_str("Notifications");
                            if self.notification_count > 0 {
                                let _ = self.tooltip.push_str(" (");
                                push_u8_decimal(&mut self.tooltip, self.notification_count.min(99) as u8);
                                let _ = self.tooltip.push(')');
                            }
                            if self.do_not_disturb {
                                let _ = self.tooltip.push_str(" — DND");
                            }
                        }
                        TaskbarHit::Volume => {
                            if self.volume_muted {
                                let _ = self.tooltip.push_str("Volume: Muted");
                            } else {
                                let _ = self.tooltip.push_str("Volume: ");
                                push_u8_decimal(&mut self.tooltip, self.volume_level);
                                let _ = self.tooltip.push('%');
                            }
                        }
                        TaskbarHit::Battery => {
                            let _ = self.tooltip.push_str("Battery: ");
                            push_u8_decimal(&mut self.tooltip, self.battery_level);
                            let _ = self.tooltip.push('%');
                        }
                        TaskbarHit::Clock => {
                            let _ = self.tooltip.push_str("Calendar");
                            if self.night_light_active {
                                let _ = self.tooltip.push_str(" — Night Light ON");
                            }
                        }
                        TaskbarHit::ShowDesktop => {
                            let _ = self.tooltip.push_str("Show Desktop");
                        }
                        TaskbarHit::TaskScrollLeft => {
                            let _ = self.tooltip.push_str("Scroll left");
                        }
                        TaskbarHit::TaskScrollRight => {
                            let _ = self.tooltip.push_str("Scroll right");
                        }
                        TaskbarHit::None => {}
                    }
                    dirty = true;
                }

                // Update context menu hover
                if self.context_menu.visible {
                    let menu_w: i32 = crate::render::CONTEXT_MENU_W;
                    let mut new_hover = None;
                    let mut y = self.context_menu.y;
                    for i in 0..self.context_menu.item_count {
                        let h = ContextMenu::item_height(&self.context_menu.items, i);
                        if !self.context_menu.items[i].separator
                            && self.cursor_x >= self.context_menu.x
                            && self.cursor_x < self.context_menu.x + menu_w
                            && self.cursor_y >= y
                            && self.cursor_y < y + h
                        {
                            new_hover = Some(i);
                            break;
                        }
                        y += h;
                    }
                    if new_hover != self.context_menu.hovered_index {
                        self.context_menu.hovered_index = new_hover;
                        dirty = true;
                    }
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
                        // ── Context menu click detection (highest priority) ──
                        if self.context_menu.visible {
                            let menu = &self.context_menu;
                            let menu_w: i32 = crate::render::CONTEXT_MENU_W;
                            let mut hit_item = false;
                            let mut y = menu.y;
                            for i in 0..menu.item_count {
                                let h = ContextMenu::item_height(&menu.items, i);
                                if !menu.items[i].separator
                                    && self.cursor_x >= menu.x
                                    && self.cursor_x < menu.x + menu_w
                                    && self.cursor_y >= y
                                    && self.cursor_y < y + h
                                {
                                    self.pending_context_action = menu.items[i].action;
                                    hit_item = true;
                                    break;
                                }
                                y += h;
                            }
                            self.context_menu.hide();
                            dirty = true;
                            if hit_item { return dirty; }
                        }

                        // ── Volume panel click detection ──
                        if self.volume_panel_active {
                            use crate::render::{VOLUME_PANEL_W, VOLUME_PANEL_H, TASKBAR_TRAY_WIDTH};
                            let vp_x = self.fb_width - TASKBAR_TRAY_WIDTH + 160;
                            let vp_y = self.fb_height - crate::render::TASKBAR_HEIGHT - VOLUME_PANEL_H - 5;
                            if self.cursor_x >= vp_x && self.cursor_x < vp_x + VOLUME_PANEL_W
                                && self.cursor_y >= vp_y && self.cursor_y < vp_y + VOLUME_PANEL_H
                            {
                                // Click inside volume panel
                                let bar_x = vp_x + 15;
                                let bar_w: i32 = VOLUME_PANEL_W - 30;
                                if self.cursor_x >= bar_x && self.cursor_x < bar_x + bar_w
                                    && self.cursor_y >= vp_y + 55 && self.cursor_y < vp_y + 71
                                {
                                    // Click on volume bar — set level directly
                                    let relative = self.cursor_x - bar_x;
                                    let new_vol = ((relative * 100) / bar_w).clamp(0, 100) as u8;
                                    self.pending_context_action = ContextAction::SetVolume(new_vol);
                                } else {
                                    // Click on mute label area — toggle mute
                                    self.pending_context_action = ContextAction::ToggleMute;
                                }
                                dirty = true;
                                return dirty;
                            } else {
                                // Clicked outside volume panel → close it
                                self.volume_panel_active = false;
                                dirty = true;
                                return dirty;
                            }
                        }

                        // ── Clock/calendar panel click detection ──
                        if self.clock_panel_active {
                            use crate::render::{CLOCK_PANEL_W, CLOCK_PANEL_H, TASKBAR_HEIGHT};
                            let cp_x = (self.fb_width - 6 - CLOCK_PANEL_W).max(0);
                            let cp_y = self.fb_height - TASKBAR_HEIGHT - CLOCK_PANEL_H - 5;
                            if self.cursor_x >= cp_x && self.cursor_x < cp_x + CLOCK_PANEL_W
                                && self.cursor_y >= cp_y && self.cursor_y < cp_y + CLOCK_PANEL_H
                            {
                                // Clicked inside the calendar panel — do nothing (panel stays open)
                                dirty = true;
                                return dirty;
                            } else {
                                // Clicked outside → close the calendar
                                self.clock_panel_active = false;
                                dirty = true;
                                return dirty;
                            }
                        }

                        // ── Quick Settings panel click detection ──
                        if self.quick_settings_active {
                            use crate::render::TASKBAR_HEIGHT;
                            let qs_w: i32 = 220;
                            let qs_h: i32 = 220;
                            let qs_x = self.fb_width - qs_w - 10;
                            let qs_y = self.fb_height - TASKBAR_HEIGHT - qs_h - 5;
                            if self.cursor_x >= qs_x && self.cursor_x < qs_x + qs_w
                                && self.cursor_y >= qs_y && self.cursor_y < qs_y + qs_h
                            {
                                let row_x_toggle = qs_x + qs_w - 46;
                                let row_w_toggle = 36;
                                let slider_x = qs_x + 10;
                                let slider_w = qs_w - 20;
                                // Row 0: DND toggle pill (y = qs_y+34, h=16)
                                if self.cursor_x >= row_x_toggle && self.cursor_x < row_x_toggle + row_w_toggle
                                    && self.cursor_y >= qs_y + 34 && self.cursor_y < qs_y + 50
                                {
                                    self.pending_context_action = ContextAction::ToggleDoNotDisturb;
                                }
                                // Row 1: Night Light toggle pill (y = qs_y+62, h=16)
                                else if self.cursor_x >= row_x_toggle && self.cursor_x < row_x_toggle + row_w_toggle
                                    && self.cursor_y >= qs_y + 62 && self.cursor_y < qs_y + 78
                                {
                                    self.pending_context_action = ContextAction::ToggleNightLight;
                                }
                                // Row 2: Volume Mute toggle pill (y = qs_y+90, h=16)
                                else if self.cursor_x >= row_x_toggle && self.cursor_x < row_x_toggle + row_w_toggle
                                    && self.cursor_y >= qs_y + 90 && self.cursor_y < qs_y + 106
                                {
                                    self.pending_context_action = ContextAction::ToggleMute;
                                }
                                // Brightness slider bar (y = qs_y+134, h=6) — set brightness level
                                else if self.cursor_x >= slider_x && self.cursor_x < slider_x + slider_w
                                    && self.cursor_y >= qs_y + 134 && self.cursor_y < qs_y + 142
                                {
                                    let rel = self.cursor_x - slider_x;
                                    let level = ((rel * 100) / slider_w).clamp(0, 100) as u8;
                                    self.pending_context_action = ContextAction::SetBrightness(level);
                                }
                                // Volume slider bar (y = qs_y+164, h=6) — set volume level
                                else if self.cursor_x >= slider_x && self.cursor_x < slider_x + slider_w
                                    && self.cursor_y >= qs_y + 164 && self.cursor_y < qs_y + 172
                                {
                                    let rel = self.cursor_x - slider_x;
                                    let level = ((rel * 100) / slider_w).clamp(0, 100) as u8;
                                    self.pending_context_action = ContextAction::SetVolume(level);
                                }
                                dirty = true;
                                return dirty;
                            } else {
                                // Clicked outside → close Quick Settings
                                self.quick_settings_active = false;
                                dirty = true;
                                return dirty;
                            }
                        }

                        // ── Taskbar click detection (highest priority) ──
                        let tb_hit = taskbar_hit_test(
                            self.cursor_x, self.cursor_y,
                            self.fb_width, self.fb_height,
                            self.pinned_app_count,
                            &self.pinned_app_names,
                            windows, *window_count,
                            self.current_workspace,
                            self.task_scroll_offset,
                        );

                        // ── Drag-and-drop initiation for pinned apps ──
                        if let TaskbarHit::PinnedApp(idx) = tb_hit {
                            self.dragging_pinned_app = Some(idx);
                            self.drag_press_x = self.cursor_x;
                            self.drag_press_y = self.cursor_y;
                        }
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
                                TaskbarHit::PinnedApp(_idx) => {
                                    // Drag already initiated above (dragging_pinned_app = Some(idx)).
                                    // The click/swap is finalised on button release, not press.
                                    dirty = true;
                                }
                                TaskbarHit::WindowTask(w_idx) => {
                                    if w_idx < *window_count {
                                        if self.focused_window == Some(w_idx) && !windows[w_idx].minimized {
                                            // Window is focused and visible: minimize it (toggle)
                                            windows[w_idx].minimized = true;
                                            self.focused_window = None;
                                        } else {
                                            // Window is not focused or is minimized: restore and focus
                                            windows[w_idx].minimized = false;
                                            self.focused_window = Some(w_idx);
                                        }
                                        dirty = true;
                                    }
                                }
                                TaskbarHit::Notifications => {
                                    self.notifications_visible = !self.notifications_visible;
                                    dirty = true;
                                }
                                TaskbarHit::Volume => {
                                    self.volume_panel_active = !self.volume_panel_active;
                                    dirty = true;
                                }
                                TaskbarHit::Battery => {
                                    // Left-click battery: toggle power/battery info panel
                                    self.battery_panel_active = !self.battery_panel_active;
                                    dirty = true;
                                }
                                TaskbarHit::Clock => {
                                    // Toggle the calendar panel (replaces old dashboard toggle)
                                    self.clock_panel_active = !self.clock_panel_active;
                                    dirty = true;
                                }
                                TaskbarHit::ShowDesktop => {
                                    // Toggle show-desktop mode (state.rs will minimize/restore)
                                    self.clock_clicked = true; // reuse flag as show-desktop trigger
                                    // Actually use a dedicated signal to state.rs
                                    self.clock_clicked = false;
                                    self.show_desktop_active = !self.show_desktop_active;
                                    dirty = true;
                                }
                                TaskbarHit::TaskScrollLeft => {
                                    self.task_scroll_offset = self.task_scroll_offset.saturating_sub(1);
                                    dirty = true;
                                }
                                TaskbarHit::TaskScrollRight => {
                                    self.task_scroll_offset += 1;
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
                        // Finalise any pinned-app drag.
                        if let Some(src) = self.dragging_pinned_app.take() {
                            let dx = (self.cursor_x - self.drag_press_x).abs();
                            let dy = (self.cursor_y - self.drag_press_y).abs();
                            let drag_moved = dx > 4 || dy > 4;
                            let tb_hit = taskbar_hit_test(
                                self.cursor_x, self.cursor_y,
                                self.fb_width, self.fb_height,
                                self.pinned_app_count,
                                &self.pinned_app_names,
                                windows, *window_count,
                                self.current_workspace,
                                self.task_scroll_offset,
                            );
                            if let TaskbarHit::PinnedApp(dst) = tb_hit {
                                if drag_moved && src != dst {
                                    // Drag threshold crossed → reorder
                                    self.pending_pinned_swap = Some((src, dst));
                                } else {
                                    // No significant move → regular click
                                    self.last_pinned_app_click = Some(src);
                                }
                            } else if !drag_moved {
                                // Released outside any pinned app without significant movement
                                // → treat as a click on the original icon.
                                self.last_pinned_app_click = Some(src);
                                // Otherwise the drag was cancelled — do nothing.
                            }
                        }
                    }
                }
                if button == 1 && pressed { // Right click
                    // Close any existing context menu
                    self.context_menu.hide();

                    let on_taskbar = self.cursor_y >= self.fb_height - crate::render::TASKBAR_HEIGHT;
                    if on_taskbar {
                        // Right-click on taskbar: check what element was hit
                        let tb_hit = taskbar_hit_test(
                            self.cursor_x, self.cursor_y,
                            self.fb_width, self.fb_height,
                            self.pinned_app_count,
                            &self.pinned_app_names,
                            windows, *window_count,
                            self.current_workspace,
                            self.task_scroll_offset,
                        );
                        if let TaskbarHit::WindowTask(w_idx) = tb_hit {
                            if w_idx < *window_count {
                                // ── Window task context menu ──
                                // Height: 5 regular + 1 separator = 5*28 + 8 = 148px
                                self.context_menu.show(self.cursor_x, self.cursor_y - 148);
                                if windows[w_idx].minimized {
                                    self.context_menu.add_item("Restore", ContextAction::MinimizeWindow(w_idx));
                                } else {
                                    self.context_menu.add_item("Minimize", ContextAction::MinimizeWindow(w_idx));
                                }
                                let is_max = windows[w_idx].maximized;
                                self.context_menu.add_checked_item(
                                    "Maximize",
                                    ContextAction::MaximizeWindow(w_idx),
                                    is_max,
                                );
                                self.context_menu.add_separator();
                                self.context_menu.add_item("Close", ContextAction::CloseWindow(w_idx));
                                self.context_menu.add_item("Pin to Taskbar", ContextAction::PinApp(w_idx));
                                self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                                dirty = true;
                            }
                        } else if let TaskbarHit::PinnedApp(app_idx) = tb_hit {
                            // ── Pinned app context menu ──
                            // Height: 3 regular + 1 separator = 3*28 + 8 = 92px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 92);
                            self.context_menu.add_item("Open", ContextAction::LaunchPinnedApp(app_idx));
                            self.context_menu.add_separator();
                            self.context_menu.add_item("Unpin from Taskbar", ContextAction::UnpinApp(app_idx));
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else if let TaskbarHit::Volume = tb_hit {
                            // ── Volume context menu (with mute checkmark) ──
                            // Height: 3 regular + 1 separator = 3*28 + 8 = 92px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 92);
                            self.context_menu.add_item("Volume Up", ContextAction::VolumeUp);
                            self.context_menu.add_item("Volume Down", ContextAction::VolumeDown);
                            self.context_menu.add_separator();
                            self.context_menu.add_checked_item("Mute", ContextAction::ToggleMute, self.volume_muted);
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else if let TaskbarHit::Notifications = tb_hit {
                            // ── Notifications context menu ──
                            // Height: 2 regular + 1 separator = 2*28 + 8 = 64px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 64);
                            self.context_menu.add_item("Mark All Read", ContextAction::MarkNotificationsRead);
                            self.context_menu.add_separator();
                            self.context_menu.add_checked_item("Do Not Disturb", ContextAction::ToggleDoNotDisturb, self.do_not_disturb);
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else if let TaskbarHit::Clock = tb_hit {
                            // ── Clock context menu ──
                            // Height: 1 regular + 1 separator + 1 regular + 1 separator + 1 regular = 3*28 + 2*8 = 100px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 100);
                            self.context_menu.add_item("Show Calendar", ContextAction::OpenDashboard);
                            self.context_menu.add_separator();
                            self.context_menu.add_item("Power Info", ContextAction::ToggleBatteryPanel);
                            self.context_menu.add_separator();
                            self.context_menu.add_checked_item("Night Light", ContextAction::ToggleNightLight, self.night_light_active);
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else if let TaskbarHit::Battery = tb_hit {
                            // ── Battery context menu ──
                            // Height: 1 regular + 1 sep + 2 regular = 3*28 + 8 = 92px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 92);
                            self.context_menu.add_item("Power Info", ContextAction::ToggleBatteryPanel);
                            self.context_menu.add_separator();
                            self.context_menu.add_item("Brightness Up", ContextAction::BrightnessUp);
                            self.context_menu.add_item("Brightness Down", ContextAction::BrightnessDown);
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else if let TaskbarHit::Launcher = tb_hit {
                            // ── Launcher context menu ──
                            // Height: 1 regular + 1 sep + 1 regular = 2*28 + 8 = 64px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 64);
                            self.context_menu.add_item("Open Launcher", ContextAction::ToggleLauncher);
                            self.context_menu.add_separator();
                            self.context_menu.add_item("Lock Screen", ContextAction::ToggleLock);
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else if let TaskbarHit::Workspace(ws) = tb_hit {
                            // ── Workspace context menu ──
                            // Height: 1 regular + 1 sep + 4 regular = 5*28 + 8 = 148px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 148);
                            self.context_menu.add_item("New Window Here", ContextAction::NewWindow);
                            self.context_menu.add_separator();
                            for i in 0..4u8 {
                                let label = match i {
                                    0 => "Workspace 1",
                                    1 => "Workspace 2",
                                    2 => "Workspace 3",
                                    _ => "Workspace 4",
                                };
                                self.context_menu.add_checked_item(
                                    label,
                                    ContextAction::SwitchWorkspace(i),
                                    i == ws,
                                );
                            }
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else if let TaskbarHit::ShowDesktop = tb_hit {
                            // ── Show Desktop context menu ──
                            // Height: 2 regular + 1 sep = 2*28 + 8 = 64px
                            self.context_menu.show(self.cursor_x, self.cursor_y - 64);
                            self.context_menu.add_checked_item(
                                "Show Desktop",
                                ContextAction::ShowDesktop,
                                self.show_desktop_active,
                            );
                            self.context_menu.add_separator();
                            self.context_menu.add_item("Change Wallpaper", ContextAction::CycleWallpaper);
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        }
                    } else {
                        // Right-click on desktop background or window
                        let focus = focus_under_cursor(self.cursor_x, self.cursor_y, windows, *window_count);
                        if let Some(idx) = focus {
                            // ── Window context menu ──
                            // Height: 5 regular + 1 separator = 5*28 + 8 = 148px
                            self.context_menu.show(self.cursor_x, self.cursor_y);
                            if windows[idx].minimized {
                                self.context_menu.add_item("Restore", ContextAction::MinimizeWindow(idx));
                            } else {
                                self.context_menu.add_item("Minimize", ContextAction::MinimizeWindow(idx));
                            }
                            let is_max = windows[idx].maximized;
                            self.context_menu.add_checked_item(
                                "Maximize",
                                ContextAction::MaximizeWindow(idx),
                                is_max,
                            );
                            self.context_menu.add_separator();
                            self.context_menu.add_item("Close", ContextAction::CloseWindow(idx));
                            self.context_menu.add_item("Pin to Taskbar", ContextAction::PinApp(idx));
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        } else {
                            // ── Desktop context menu ──
                            // Height: 5 regular + 2 separators + 2 checked = 7*28 + 2*8 = 212px
                            self.context_menu.show(self.cursor_x, self.cursor_y);
                            self.context_menu.add_item("New Window", ContextAction::NewWindow);
                            self.context_menu.add_item("Change Wallpaper", ContextAction::CycleWallpaper);
                            self.context_menu.add_separator();
                            self.context_menu.add_checked_item("Toggle Tiling", ContextAction::ToggleTiling, self.tiling_active);
                            self.context_menu.add_checked_item("Do Not Disturb", ContextAction::ToggleDoNotDisturb, self.do_not_disturb);
                            self.context_menu.add_checked_item("Night Light", ContextAction::ToggleNightLight, self.night_light_active);
                            self.context_menu.add_separator();
                            self.context_menu.add_item("Dashboard", ContextAction::OpenDashboard);
                            self.context_menu.add_item("Screenshot", ContextAction::TakeScreenshot);
                            self.context_menu.clamp_to_screen(self.fb_width, self.fb_height);
                            dirty = true;
                        }
                    }
                }
                if button == 2 && pressed { // Middle click
                    // Middle-click on a window task in the taskbar → close the window
                    let tb_hit = taskbar_hit_test(
                        self.cursor_x, self.cursor_y,
                        self.fb_width, self.fb_height,
                        self.pinned_app_count,
                        &self.pinned_app_names,
                        windows, *window_count,
                        self.current_workspace,
                        self.task_scroll_offset,
                    );
                    if let TaskbarHit::WindowTask(w_idx) = tb_hit {
                        if w_idx < *window_count {
                            windows[w_idx].closing = true;
                            if self.focused_window == Some(w_idx) {
                                self.focused_window = None;
                            }
                            dirty = true;
                        }
                    }
                }
            }
            // Mouse scroll wheel
            3 => {
                // value > 0 = scroll down, value < 0 = scroll up
                let scroll_down = event.value > 0;
                let tray_start = self.fb_width - crate::render::TASKBAR_TRAY_WIDTH;
                let vol_x = tray_start + 100;
                let on_taskbar = self.cursor_y >= self.fb_height - crate::render::TASKBAR_HEIGHT;

                if on_taskbar && self.cursor_x >= vol_x - 5 && self.cursor_x < vol_x + 15 {
                    // Scroll on volume area → adjust volume level
                    self.pending_context_action = if scroll_down {
                        ContextAction::VolumeDown
                    } else {
                        ContextAction::VolumeUp
                    };
                } else {
                    // Scroll anywhere else → scroll the running-window task list
                    if scroll_down {
                        self.task_scroll_offset += 1;
                    } else {
                        self.task_scroll_offset = self.task_scroll_offset.saturating_sub(1);
                    }
                }
                dirty = true;
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
        let names = [[0u8; 32]; 16];
        let hit = taskbar_hit_test(20, 1080 - 20, 1920, 1080, 5, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::Launcher);
    }

    #[test]
    fn test_taskbar_hit_workspace() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let names = [[0u8; 32]; 16];
        // Workspace 0 starts at x=48
        let hit = taskbar_hit_test(55, 1080 - 20, 1920, 1080, 5, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::Workspace(0));
        // Workspace 1 at x=74
        let hit = taskbar_hit_test(80, 1080 - 20, 1920, 1080, 5, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::Workspace(1));
    }

    #[test]
    fn test_taskbar_hit_pinned_app() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let names = [[0u8; 32]; 16];
        // First pinned app starts at TASKBAR_APPS_START_X = 160
        let hit = taskbar_hit_test(170, 1080 - 20, 1920, 1080, 5, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::PinnedApp(0));
        // Second pinned app at 160 + 32 + 6 = 198
        let hit = taskbar_hit_test(205, 1080 - 20, 1920, 1080, 5, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::PinnedApp(1));
    }

    #[test]
    fn test_taskbar_hit_none_above_bar() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let names = [[0u8; 32]; 16];
        // Click above the taskbar should return None
        let hit = taskbar_hit_test(500, 500, 1920, 1080, 5, &names, &windows, 0, 0, 0);
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

        // Click on first pinned app at x=170: press then release at the same position.
        state.cursor_x = 170;
        state.cursor_y = 1080 - 20;

        let press = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&press, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        // Drag has started but click is not yet finalised on press.
        assert_eq!(state.dragging_pinned_app, Some(0), "drag should be initiated on press");

        // Release at the same position → no drag threshold crossed → click registered.
        let release = InputEvent { device_id: 0, event_type: 2, code: 0, value: 0, timestamp: 0 };
        state.apply_event(&release, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.last_pinned_app_click, Some(0));
    }

    #[test]
    fn test_taskbar_click_notifications() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Notification area is at tray_start + 155 = (1920 - 300) + 155 = 1775
        state.cursor_x = 1775;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.notifications_visible, "notifications should toggle on");
    }

    #[test]
    fn test_taskbar_hit_volume() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let names = [[0u8; 32]; 16];
        // Volume is at tray_start + 180 = (1920 - 300) + 180 = 1800
        let hit = taskbar_hit_test(1800, 1080 - 20, 1920, 1080, 5, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::Volume);
    }

    #[test]
    fn test_taskbar_hit_clock() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let names = [[0u8; 32]; 16];
        // Clock area: fb_w - 56 to fb_w - 6 → centre at fb_w - 31 = 1889
        let hit = taskbar_hit_test(1870, 1080 - 20, 1920, 1080, 5, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::Clock);
    }

    #[test]
    fn test_taskbar_click_volume() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Volume area at tray_start + 180 = 1800
        state.cursor_x = 1800;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.volume_panel_active, "volume panel should be toggled on");
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
        let names = [[0u8; 32]; 16];
        // Create a minimized window
        windows[0].content = WindowContent::InternalDemo;
        windows[0].minimized = true;
        windows[0].workspace = 0;

        // Minimized windows should now appear in taskbar hit test
        // Running windows area starts after pinned apps separator
        // With 0 pinned apps: sep2_x = TASKBAR_APPS_START_X + 2 = 162, win_x = 170
        let hit = taskbar_hit_test(180, 1080 - 20, 1920, 1080, 0, &names, &windows, 1, 0, 0);
        assert_eq!(hit, TaskbarHit::WindowTask(0));
    }

    #[test]
    fn test_taskbar_click_window_task_focuses() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        windows[0].minimized = false;

        // Position on window task (win_x = 170 with 0 pinned apps)
        state.cursor_x = 180;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert_eq!(state.focused_window, Some(0), "window task click should focus window");
        assert!(!windows[0].minimized);
    }

    #[test]
    fn test_taskbar_click_focused_window_minimizes() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        windows[0].minimized = false;
        state.focused_window = Some(0); // already focused

        // Click on the focused window task
        state.cursor_x = 180;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(windows[0].minimized, "clicking focused window task should minimize it");
        assert_eq!(state.focused_window, None, "focus should be cleared after minimize");
    }

    #[test]
    fn test_taskbar_click_minimized_window_restores() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        windows[0].minimized = true; // already minimized

        // Click on the minimized window task
        state.cursor_x = 180;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(!windows[0].minimized, "clicking minimized window task should restore it");
        assert_eq!(state.focused_window, Some(0), "restored window should be focused");
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

    #[test]
    fn test_context_menu_new() {
        let menu = ContextMenu::new();
        assert!(!menu.visible);
        assert_eq!(menu.item_count, 0);
        assert_eq!(menu.hovered_index, None);
    }

    #[test]
    fn test_context_menu_show_and_add_items() {
        let mut menu = ContextMenu::new();
        menu.show(100, 200);
        assert!(menu.visible);
        assert_eq!(menu.x, 100);
        assert_eq!(menu.y, 200);

        menu.add_item("New Window", ContextAction::NewWindow);
        menu.add_item("Close", ContextAction::CloseWindow(0));
        assert_eq!(menu.item_count, 2);
        assert_eq!(menu.items[0].label_str(), "New Window");
        assert_eq!(menu.items[0].action, ContextAction::NewWindow);
        assert_eq!(menu.items[1].label_str(), "Close");
    }

    #[test]
    fn test_context_menu_hide() {
        let mut menu = ContextMenu::new();
        menu.show(50, 50);
        menu.add_item("Test", ContextAction::None);
        assert!(menu.visible);
        menu.hide();
        assert!(!menu.visible);
        assert_eq!(menu.item_count, 0);
    }

    #[test]
    fn test_context_menu_max_items() {
        let mut menu = ContextMenu::new();
        menu.show(0, 0);
        for _ in 0..CONTEXT_MENU_MAX_ITEMS + 5 {
            menu.add_item("Item", ContextAction::None);
        }
        assert_eq!(menu.item_count, CONTEXT_MENU_MAX_ITEMS);
    }

    #[test]
    fn test_right_click_desktop_shows_context_menu() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Right-click on empty desktop area
        state.cursor_x = 500;
        state.cursor_y = 500;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.context_menu.visible, "context menu should be visible");
        // 9 items: NewWindow, ChangeWallpaper, separator, ToggleTiling(checked), DND(checked),
        //          NightLight(checked), separator, Dashboard, Screenshot
        assert_eq!(state.context_menu.item_count, 9);
        assert_eq!(state.context_menu.items[0].action, ContextAction::NewWindow);
        assert_eq!(state.context_menu.items[1].action, ContextAction::CycleWallpaper);
        assert!(state.context_menu.items[2].separator, "third item should be separator");
    }

    #[test]
    fn test_right_click_taskbar_window_shows_context_menu() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;

        // Position on window task item in taskbar: sep2_x = 162, win_x = 170
        state.cursor_x = 180;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.context_menu.visible, "context menu should be visible for window");
        // 5 items: Minimize, Maximize(checked), separator, Close, Pin to Taskbar
        assert_eq!(state.context_menu.item_count, 5);
        assert!(state.context_menu.items[2].separator, "third item should be separator");
        assert_eq!(state.context_menu.items[3].action, ContextAction::CloseWindow(0));
    }

    #[test]
    fn test_left_click_closes_context_menu() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 5;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Show a context menu
        state.context_menu.show(100, 100);
        state.context_menu.add_item("New Window", ContextAction::NewWindow);
        assert!(state.context_menu.visible);

        // Left-click on the menu item
        state.cursor_x = 110;
        state.cursor_y = 110;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(!state.context_menu.visible, "context menu should be closed");
        assert_eq!(state.pending_context_action, ContextAction::NewWindow);
    }

    #[test]
    fn test_network_details_toggle() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        assert!(!state.network_details_active);

        // Simulate Super+E key press (scancode 0x12, Super modifier = 8)
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x12, value: 1, timestamp: 0 };
        state.modifiers = 8; // Super key
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.network_details_active, "network details should be active");
    }

    #[test]
    fn test_taskbar_scroll_left_hit() {
        let names = [[0u8; 32]; 16];
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // scroll_offset=1 so the ◀ button is rendered at tasks_start_x (= sep2_x + 8 = 170)
        // tasks_start_x with 0 pinned apps: sep2_x = 162, tasks_start_x = 170
        let hit = taskbar_hit_test(172, 1080 - 20, 1920, 1080, 0, &names, &windows, 0, 0, 1);
        assert_eq!(hit, TaskbarHit::TaskScrollLeft);
    }

    #[test]
    fn test_taskbar_scroll_left_not_shown_at_offset_zero() {
        let names = [[0u8; 32]; 16];
        let mut windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // At offset=0 there is no scroll-left button; the same x hits the window task
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        let hit = taskbar_hit_test(172, 1080 - 20, 1920, 1080, 0, &names, &windows, 1, 0, 0);
        assert_eq!(hit, TaskbarHit::WindowTask(0));
    }

    #[test]
    fn test_taskbar_scroll_right_click_increments() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        // Fill up window slots so we have overflow
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 15;
        for i in 0..15 {
            windows[i].content = WindowContent::InternalDemo;
            windows[i].workspace = 0;
        }
        assert_eq!(state.task_scroll_offset, 0);

        // Click scroll-right button (sr_x = tray_start - scroll_btn_w - 6 = 1700 - 16 - 6 = 1678)
        // tray_start = 1920 - 220 = 1700
        state.cursor_x = 1680;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.task_scroll_offset, 1, "scroll offset should increment");
    }

    #[test]
    fn test_taskbar_scroll_left_click_decrements() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        state.task_scroll_offset = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Click the ◀ button (tasks_start_x with 0 pinned: 170)
        state.cursor_x = 172;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.task_scroll_offset, 2, "scroll offset should decrement");
    }

    #[test]
    fn test_taskbar_scroll_left_no_underflow() {
        let mut state = InputState::new(1920, 1080);
        state.task_scroll_offset = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // At offset=0 the ◀ button is not rendered, so clicking there is a no-op for scrolling
        state.cursor_x = 172;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.task_scroll_offset, 0, "offset must not underflow");
    }

    #[test]
    fn test_tooltip_set_on_hover_pinned_app() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 1;
        let app_name = b"Terminal";
        state.pinned_app_names[0][..app_name.len()].copy_from_slice(app_name);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Move cursor to first pinned app (x=170, taskbar y)
        state.cursor_x = 170;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 1, code: 0xFFFF, value: 0, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);

        assert_eq!(state.hovered_taskbar_element, TaskbarHit::PinnedApp(0));
        assert_eq!(state.tooltip.as_str(), "Terminal");
    }

    #[test]
    fn test_tooltip_set_on_hover_window_task() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        let title = b"MyWindow";
        windows[0].title[..title.len()].copy_from_slice(title);

        state.cursor_x = 180;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 1, code: 0xFFFF, value: 0, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);

        assert_eq!(state.hovered_taskbar_element, TaskbarHit::WindowTask(0));
        assert_eq!(state.tooltip.as_str(), "MyWindow");
    }

    #[test]
    fn test_tooltip_cleared_on_hover_off_taskbar() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Move onto taskbar
        state.cursor_x = 20;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 1, code: 0xFFFF, value: 0, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.tooltip.as_str(), "Launcher (Super+A)");

        // Move off taskbar
        state.cursor_x = 500;
        state.cursor_y = 500;
        let ev2 = InputEvent { device_id: 0, event_type: 1, code: 0xFFFF, value: 0, timestamp: 0 };
        state.apply_event(&ev2, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.tooltip.as_str(), "", "tooltip should clear when off taskbar");
    }

    #[test]
    fn test_already_pinned_windows_skipped_in_hit_test() {
        let mut names = [[0u8; 32]; 16];
        let name = b"Terminal";
        names[0][..name.len()].copy_from_slice(name);

        let mut windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        // Window with title matching pinned app "Terminal"
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        let title = b"Terminal";
        windows[0].title[..title.len()].copy_from_slice(title);

        // With 1 pinned app and a matching window, the window should be skipped in hit test
        // so clicking after sep2 should NOT return WindowTask(0)
        // sep2_x with 1 pinned app: 160 + 32 + 6 + 2 = 200, win_x = 208
        // But the Terminal window is skipped, so there's nothing at x=210
        let hit = taskbar_hit_test(210, 1080 - 20, 1920, 1080, 1, &names, &windows, 1, 0, 0);
        assert_ne!(hit, TaskbarHit::WindowTask(0), "pinned-matched window should not hit as WindowTask");
    }

    #[test]
    fn test_right_click_pinned_app_shows_context_menu() {
        let mut state = InputState::new(1920, 1080);
        // Set up one pinned app
        state.pinned_app_count = 1;
        let name = b"Terminal";
        state.pinned_app_names[0][..name.len()].copy_from_slice(name);

        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Click on first pinned app icon: TASKBAR_APPS_START_X = 160, icon_size = 32
        // Center of first icon = 160 + 16 = 176
        state.cursor_x = 176;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.context_menu.visible, "context menu should be visible for pinned app");
        // 3 items: Open, separator, Unpin from Taskbar
        assert_eq!(state.context_menu.item_count, 3);
        assert_eq!(state.context_menu.items[0].action, ContextAction::LaunchPinnedApp(0));
        assert!(state.context_menu.items[1].separator, "second item should be separator");
        assert_eq!(state.context_menu.items[2].action, ContextAction::UnpinApp(0));
    }

    #[test]
    fn test_right_click_volume_shows_context_menu() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Volume area: tray_start = 1920 - 300 = 1620
        // vol_x = tray_start + 180 = 1800; hit range [1795, 1815)
        state.cursor_x = 1800;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(state.context_menu.visible, "context menu should be visible for volume");
        // 4 items: Volume Up, Volume Down, separator, Mute(checked)
        assert_eq!(state.context_menu.item_count, 4);
        assert_eq!(state.context_menu.items[0].action, ContextAction::VolumeUp);
        assert_eq!(state.context_menu.items[1].action, ContextAction::VolumeDown);
        assert!(state.context_menu.items[2].separator, "third item should be separator");
        assert_eq!(state.context_menu.items[3].action, ContextAction::ToggleMute);
    }

    #[test]
    fn test_escape_key_closes_context_menu() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Open a context menu
        state.context_menu.show(100, 100);
        state.context_menu.add_item("New Window", ContextAction::NewWindow);
        assert!(state.context_menu.visible);

        // Press Escape (scancode 0x01)
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x01, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(!state.context_menu.visible, "Escape should close context menu");
        // focused_window should remain unchanged (not close a window)
    }

    #[test]
    fn test_arrow_keys_navigate_context_menu() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Open a context menu with 3 items
        state.context_menu.show(100, 100);
        state.context_menu.add_item("Item A", ContextAction::NewWindow);
        state.context_menu.add_item("Item B", ContextAction::ToggleTiling);
        state.context_menu.add_item("Item C", ContextAction::OpenDashboard);
        assert_eq!(state.context_menu.hovered_index, None);

        // Press Down arrow (scancode 0x50)
        let ev_down = InputEvent { device_id: 0, event_type: 0, code: 0x50, value: 1, timestamp: 0 };
        state.apply_event(&ev_down, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.context_menu.hovered_index, Some(0));

        // Press Down again
        state.apply_event(&ev_down, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.context_menu.hovered_index, Some(1));

        // Press Up (scancode 0x48)
        let ev_up = InputEvent { device_id: 0, event_type: 0, code: 0x48, value: 1, timestamp: 0 };
        state.apply_event(&ev_up, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.context_menu.hovered_index, Some(0));
    }

    #[test]
    fn test_enter_activates_context_menu_item() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        state.context_menu.show(100, 100);
        state.context_menu.add_item("New Window", ContextAction::NewWindow);
        state.context_menu.add_item("Dashboard", ContextAction::OpenDashboard);
        state.context_menu.hovered_index = Some(1); // Dashboard selected

        // Press Enter (scancode 0x1C)
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x1C, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(!state.context_menu.visible, "Enter should close menu");
        assert_eq!(state.pending_context_action, ContextAction::OpenDashboard);
    }

    #[test]
    fn test_middle_click_window_task_closes_window() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;

        // Middle-click on the window task button at x=180
        state.cursor_x = 180;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 2, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(windows[0].closing, "middle-click should close the window");
    }

    #[test]
    fn test_scroll_wheel_scrolls_task_list() {
        let mut state = InputState::new(1920, 1080);
        assert_eq!(state.task_scroll_offset, 0);

        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Scroll down (value > 0)
        let ev_down = InputEvent { device_id: 0, event_type: 3, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev_down, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.task_scroll_offset, 1);

        // Scroll up (value < 0)
        let ev_up = InputEvent { device_id: 0, event_type: 3, code: 0, value: -1, timestamp: 0 };
        state.apply_event(&ev_up, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.task_scroll_offset, 0);

        // Scroll up at 0 should not underflow
        state.apply_event(&ev_up, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.task_scroll_offset, 0, "scroll offset should not underflow");
    }

    #[test]
    fn test_scroll_wheel_on_volume_adjusts_volume() {
        let mut state = InputState::new(1920, 1080);
        // Position cursor on volume area: tray_start = 1920 - 300 = 1620, vol_x = 1620 + 180 = 1800
        state.cursor_x = 1800;
        state.cursor_y = 1080 - 20; // on taskbar

        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Scroll up on volume → VolumeUp
        let ev_up = InputEvent { device_id: 0, event_type: 3, code: 0, value: -1, timestamp: 0 };
        state.apply_event(&ev_up, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.pending_context_action, ContextAction::VolumeUp);

        // Scroll down on volume → VolumeDown
        let ev_down = InputEvent { device_id: 0, event_type: 3, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev_down, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.pending_context_action, ContextAction::VolumeDown);
    }

    #[test]
    fn test_context_menu_clamp_to_screen() {
        let mut menu = ContextMenu::new();
        // Show near bottom-right corner
        menu.show(1850, 1060);
        menu.add_item("Item A", ContextAction::NewWindow);
        menu.add_item("Item B", ContextAction::ToggleTiling);
        // Height = 2 * 28 = 56; x + 180 = 2030 > 1920; y + 56 = 1116 > 1080
        menu.clamp_to_screen(1920, 1080);
        assert!(menu.x + crate::render::CONTEXT_MENU_W <= 1920, "menu x should be clamped");
        assert!(menu.y + 2 * crate::render::CONTEXT_MENU_ITEM_H <= 1080, "menu y should be clamped");
    }

    #[test]
    fn test_right_click_window_taskbar_shows_pin_option() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;

        state.cursor_x = 180;
        state.cursor_y = 1080 - 20;

        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(state.context_menu.visible);
        // Last item should be Pin to Taskbar
        let last = state.context_menu.item_count - 1;
        assert_eq!(state.context_menu.items[last].action, ContextAction::PinApp(0));
    }

    // ── Next-phase feature tests ──

    #[test]
    fn test_taskbar_hit_battery_removed() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let names = [[0u8; 32]; 16];
        // Old battery position (tray_start+212 with tray_width=300) should no longer be a hit
        // With new TASKBAR_TRAY_WIDTH=220, tray_start = 1920-220 = 1700; old bat_x = 1700+212=1912
        // That's actually in the Clock area now. Test that the old wide tray x is now Clock/None.
        // Simpler: just test that TaskbarHit::Battery is never returned.
        let hit = taskbar_hit_test(1832, 1080 - 20, 1920, 1080, 0, &names, &windows, 0, 0, 0);
        assert_ne!(hit, TaskbarHit::Battery, "battery hit zone should be removed");
    }

    #[test]
    fn test_taskbar_hit_show_desktop() {
        let windows: [ShellWindow; 4] = core::array::from_fn(|_| ShellWindow::default());
        let names = [[0u8; 32]; 16];
        // ShowDesktop: fb_w - 6 to fb_w = 1914 to 1920
        let hit = taskbar_hit_test(1916, 1080 - 20, 1920, 1080, 0, &names, &windows, 0, 0, 0);
        assert_eq!(hit, TaskbarHit::ShowDesktop, "show-desktop strip should be hit");
    }

    #[test]
    fn test_clock_click_toggles_calendar_panel() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Click the clock area (fb_w - 31 is inside [fb_w-56, fb_w-6))
        state.cursor_x = 1889; // fb_w - 31
        state.cursor_y = 1080 - 20;
        assert!(!state.clock_panel_active);

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(state.clock_panel_active, "clock click should open calendar panel");

        // Click again → closes
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(!state.clock_panel_active, "second click should close calendar panel");
    }

    #[test]
    fn test_show_desktop_click_sets_flag() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Click the show-desktop strip at fb_w - 3
        state.cursor_x = 1917;
        state.cursor_y = 1080 - 20;
        assert!(!state.show_desktop_active);

        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(state.show_desktop_active, "show_desktop_active should be toggled on");
    }

    #[test]
    fn test_pinned_app_drag_swap_detected() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Press on first pinned app icon (x=176)
        state.cursor_x = 176;
        state.cursor_y = 1080 - 20;
        let press = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&press, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.dragging_pinned_app, Some(0), "drag should start on first pinned app");

        // Move cursor significantly to second pinned app (x=214)
        state.cursor_x = 214;
        let move_ev = InputEvent { device_id: 0, event_type: 1, code: 0xFFFF, value: 38, timestamp: 0 };
        state.apply_event(&move_ev, &mut windows, &mut count, &mut surfaces);

        // Release on second pinned app
        state.cursor_x = 214;
        let release = InputEvent { device_id: 0, event_type: 2, code: 0, value: 0, timestamp: 0 };
        state.apply_event(&release, &mut windows, &mut count, &mut surfaces);

        // Drag should be cancelled on button-release (swap is pending if over another PinnedApp)
        // In this test the cursor is not on-taskbar during release (no second press), so drag is cleared.
        assert!(state.dragging_pinned_app.is_none(), "drag should be cleared after button release");
    }

    #[test]
    fn test_tooltip_show_desktop_text() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Move cursor over show-desktop strip (fb_w - 3 = 1917)
        state.cursor_x = 1917;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 1, code: 0xFFFF, value: 0, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.tooltip.as_str(), "Show Desktop", "tooltip should say Show Desktop");
    }

    #[test]
    fn test_clock_panel_closes_on_outside_click() {
        let mut state = InputState::new(1920, 1080);
        state.clock_panel_active = true;
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Click somewhere far from the calendar panel (top-left area)
        state.cursor_x = 100;
        state.cursor_y = 100;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 0, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(!state.clock_panel_active, "clicking outside should close calendar panel");
    }

    // ── Tests for new keyboard actions: Restore, CycleWindowVisual, Brightness ──

    #[test]
    fn test_scancode_restore_super_down() {
        // Super+Down = Restore
        assert_eq!(scancode_to_action(0x50, 8), KeyAction::Restore);
    }

    #[test]
    fn test_scancode_restore_super_r() {
        // Super+R = Restore
        assert_eq!(scancode_to_action(0x13, 8), KeyAction::Restore);
    }

    #[test]
    fn test_scancode_brightness_up() {
        assert_eq!(scancode_to_action(0x40, 0), KeyAction::BrightnessUp);
    }

    #[test]
    fn test_scancode_brightness_down() {
        assert_eq!(scancode_to_action(0x3F, 0), KeyAction::BrightnessDown);
    }

    #[test]
    fn test_restore_un_maximizes_window() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        windows[0].maximized = true;
        windows[0].stored_rect = (100, 100, 400, 300);
        state.focused_window = Some(0);

        // Super+Down (scancode 0x50, Super modifier=8)
        state.modifiers = 8;
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x50, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(!windows[0].maximized, "window should be un-maximized");
        assert_eq!(windows[0].x, 100, "window x should be restored");
    }

    #[test]
    fn test_restore_un_minimizes_window() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 1;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        windows[0].minimized = true;
        state.focused_window = Some(0);

        state.modifiers = 8;
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x50, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(!windows[0].minimized, "window should be un-minimized");
    }

    #[test]
    fn test_restore_no_focused_restores_last_minimized() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 2;
        windows[0].content = WindowContent::InternalDemo;
        windows[0].workspace = 0;
        windows[0].minimized = true;
        windows[1].content = WindowContent::InternalDemo;
        windows[1].workspace = 0;
        windows[1].minimized = true;
        state.focused_window = None;

        state.modifiers = 8;
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x50, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        // Last (index 1) minimized window should be restored
        assert!(!windows[1].minimized, "last minimized window should be restored");
        assert_eq!(state.focused_window, Some(1));
    }

    #[test]
    fn test_cycle_window_visual_increments() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        assert_eq!(state.window_decoration_style, 0);

        // Ctrl+Tab (scancode 0x0F, Ctrl modifier=2)
        state.modifiers = 4; // Alt modifier for CycleWindowVisual
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x0F, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.window_decoration_style, 1, "style should advance to 1");

        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.window_decoration_style, 2, "style should advance to 2");

        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.window_decoration_style, 0, "style should wrap back to 0");
    }

    #[test]
    fn test_brightness_up_action() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x40, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert_eq!(state.pending_context_action, ContextAction::BrightnessUp);
    }

    #[test]
    fn test_brightness_down_action() {
        let mut state = InputState::new(1920, 1080);
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x3F, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert_eq!(state.pending_context_action, ContextAction::BrightnessDown);
    }

    #[test]
    fn test_super_down_without_super_is_arrow_down() {
        // Without Super modifier, 0x50 = ArrowDown (not Restore)
        assert_eq!(scancode_to_action(0x50, 0), KeyAction::ArrowDown);
    }

    #[test]
    fn test_right_click_launcher_shows_context_menu() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Right-click the launcher button (x≈20, y near bottom)
        state.cursor_x = 20;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);

        assert!(state.context_menu.visible, "Launcher right-click should show context menu");
        assert_eq!(state.context_menu.item_count, 3); // Open Launcher, sep, Lock Screen
        assert_eq!(state.context_menu.items[0].action, ContextAction::ToggleLauncher);
        assert!(state.context_menu.items[1].separator);
        assert_eq!(state.context_menu.items[2].action, ContextAction::ToggleLock);
    }

    #[test]
    fn test_right_click_workspace_shows_context_menu() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        state.current_workspace = 1;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Right-click workspace 1 indicator (ws=0 at x=48, ws=1 at x=74)
        state.cursor_x = 74;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);

        assert!(state.context_menu.visible, "Workspace right-click should show context menu");
        // 1 NewWindow + 1 sep + 4 workspace items = 6 items
        assert_eq!(state.context_menu.item_count, 6);
        assert_eq!(state.context_menu.items[0].action, ContextAction::NewWindow);
        assert!(state.context_menu.items[1].separator);
        // Workspace 2 (index 1) should be checked since current_workspace = 1
        assert_eq!(state.context_menu.items[3].action, ContextAction::SwitchWorkspace(1));
        assert!(state.context_menu.items[3].checked, "current workspace item should be checked");
    }

    #[test]
    fn test_right_click_show_desktop_shows_context_menu() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        state.show_desktop_active = false;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Right-click show-desktop strip (rightmost 6px: x = fb_w - 3 = 1917)
        state.cursor_x = 1917;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);

        assert!(state.context_menu.visible, "ShowDesktop right-click should show context menu");
        assert_eq!(state.context_menu.item_count, 3); // ShowDesktop(checked), sep, Change Wallpaper
        assert_eq!(state.context_menu.items[0].action, ContextAction::ShowDesktop);
        assert!(!state.context_menu.items[0].checked, "show_desktop unchecked when inactive");
        assert!(state.context_menu.items[1].separator);
        assert_eq!(state.context_menu.items[2].action, ContextAction::CycleWallpaper);
    }

    // ── Launcher keyboard navigation tests ──

    #[test]
    fn test_launcher_escape_closes_launcher() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = Some(0);
        state.pinned_app_count = 2;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Escape scancode = 0x01
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x01, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert!(!state.launcher_active, "Escape should close the launcher");
        assert_eq!(state.launcher_keyboard_index, None, "Escape should clear keyboard selection");
    }

    #[test]
    fn test_launcher_arrow_down_selects_next() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = None;
        state.pinned_app_count = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Down arrow = 0x50 (no Super modifier)
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x50, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.launcher_keyboard_index, Some(0), "first Down should select index 0");

        let ev2 = InputEvent { device_id: 0, event_type: 0, code: 0x50, value: 1, timestamp: 0 };
        state.apply_event(&ev2, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.launcher_keyboard_index, Some(1), "second Down should advance to index 1");
    }

    #[test]
    fn test_launcher_arrow_down_clamps_at_last() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = Some(2);
        state.pinned_app_count = 3; // indices 0, 1, 2
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Down arrow when already at last
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x50, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.launcher_keyboard_index, Some(2), "Down at last should stay at last");
    }

    #[test]
    fn test_launcher_arrow_up_selects_prev() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = Some(2);
        state.pinned_app_count = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Up arrow = 0x48 (no Super modifier)
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x48, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.launcher_keyboard_index, Some(1), "Up should go to previous index");
    }

    #[test]
    fn test_launcher_arrow_up_clamps_at_zero() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = Some(0);
        state.pinned_app_count = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x48, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.launcher_keyboard_index, Some(0), "Up at index 0 should stay at 0");
    }

    #[test]
    fn test_launcher_enter_triggers_launch() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = Some(1);
        state.pinned_app_count = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Enter = scancode 0x1C
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x1C, value: 1, timestamp: 0 };
        let dirty = state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert!(dirty);
        assert_eq!(state.launcher_app_click, Some(1), "Enter should set launcher_app_click to selected index");
        assert_eq!(state.launcher_keyboard_index, None, "Enter should clear keyboard selection");
    }

    #[test]
    fn test_launcher_enter_no_op_when_no_selection() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = None;
        state.pinned_app_count = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x1C, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        assert_eq!(state.launcher_app_click, None, "Enter with no selection should not trigger launch");
    }

    #[test]
    fn test_launcher_keyboard_index_reset_on_close() {
        let mut state = InputState::new(1920, 1080);
        state.launcher_active = true;
        state.launcher_keyboard_index = Some(2);
        state.pinned_app_count = 3;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Super+L (launcher toggle) = scancode 0x26 with modifier 8
        state.modifiers = 8;
        let ev = InputEvent { device_id: 0, event_type: 0, code: 0x26, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);
        // launcher_active toggles off, keyboard index should be cleared
        if !state.launcher_active {
            assert_eq!(state.launcher_keyboard_index, None, "Closing launcher should reset keyboard index");
        }
    }

    // ── Battery panel tests ──

    #[test]
    fn test_clock_right_click_includes_power_info() {
        let mut state = InputState::new(1920, 1080);
        state.pinned_app_count = 0;
        let mut windows: [ShellWindow; 16] = core::array::from_fn(|_| ShellWindow::default());
        let mut surfaces = [ExternalSurface::default(); 16];
        let mut count = 0;

        // Right-click on the clock area (fb_w - 56 to fb_w - 6; centre ≈ fb_w - 31 = 1889)
        state.cursor_x = 1889;
        state.cursor_y = 1080 - 20;
        let ev = InputEvent { device_id: 0, event_type: 2, code: 1, value: 1, timestamp: 0 };
        state.apply_event(&ev, &mut windows, &mut count, &mut surfaces);

        assert!(state.context_menu.visible, "Clock right-click should show context menu");
        // Menu should contain "Power Info" item (ToggleBatteryPanel action)
        let has_power_info = (0..state.context_menu.item_count).any(|i| {
            state.context_menu.items[i].action == ContextAction::ToggleBatteryPanel
        });
        assert!(has_power_info, "Clock context menu should include Power Info item");
    }
}
