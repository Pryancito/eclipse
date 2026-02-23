pub const MAX_EXTERNAL_SURFACES: usize = 16;
pub const MAX_WINDOWS_COUNT: usize = 16;
pub const MAX_SURFACE_DIM: u32 = 8192;
pub const MAX_SURFACE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowContent {
    None,
    InternalDemo,
    External(u32), // Index into surfaces array
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowButton {
    None,
    Minimize,
    Maximize,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellWindow {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub curr_x: f32, // Para animaciones
    pub curr_y: f32,
    pub curr_w: f32,
    pub curr_h: f32,
    pub minimized: bool,
    pub maximized: bool,
    pub closing: bool,
    pub stored_rect: (i32, i32, i32, i32),
    pub workspace: u8,
    pub content: WindowContent,
}

impl ShellWindow {
    pub const TITLE_H: i32 = 26;

    pub fn title_bar_contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w
            && py >= self.y && py < self.y + Self::TITLE_H
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w
            && py >= self.y && py < self.y + self.h
    }
    
    pub fn check_button_click(&self, px: i32, py: i32) -> WindowButton {
        if !self.title_bar_contains(px, py) { return WindowButton::None; }
        
        let btn_y = self.y + (Self::TITLE_H - 16) / 2; // ui::BUTTON_ICON_SIZE = 16
        let btn_margin = 5;
        let btn_size = 16;
        
        if py < btn_y || py >= btn_y + btn_size { return WindowButton::None; }
        
        let close_x = self.x + self.w - btn_size - btn_margin;
        if px >= close_x && px < close_x + btn_size { return WindowButton::Close; }
        
        let max_x = close_x - btn_size - btn_margin;
        if px >= max_x && px < max_x + btn_size { return WindowButton::Maximize; }
        
        let min_x = max_x - btn_size - btn_margin;
        if px >= min_x && px < min_x + btn_size { return WindowButton::Minimize; }
        
        WindowButton::None
    }

    pub const RESIZE_HANDLE_SIZE: i32 = 16;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExternalSurface {
    pub id: u32,
    pub pid: u32,
    pub vaddr: usize,
    pub buffer_size: usize,
    pub active: bool,
}

pub fn focus_under_cursor(px: i32, py: i32, windows: &[ShellWindow], count: usize) -> Option<usize> {
    for i in (0..count).rev() {
        let w = &windows[i];
        if w.content != WindowContent::None && !w.minimized && w.contains(px, py) {
            return Some(i);
        }
    }
    None
}

pub fn next_visible(from: usize, forward: bool, windows: &[ShellWindow], count: usize) -> Option<usize> {
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
