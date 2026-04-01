//! Window management, surfaces, and stacking logic for Lunas desktop.

pub mod space;
pub mod tiling;

pub use space::Space;
use std::vec::Vec;

pub const MAX_EXTERNAL_SURFACES: usize = 16;
pub const MAX_WINDOWS_COUNT: usize = 16;
pub const MAX_SURFACE_DIM: u32 = 8192;
pub const MAX_SURFACE_BYTES: u64 = 128 * 1024 * 1024;

use embedded_graphics::primitives::Rectangle;
use embedded_graphics::geometry::{Point, Size};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowContent {
    None,
    InternalDemo,
    External(u32),
    Snp { surface_id: u32, pid: u32 },
    X11 { window_id: u32, pid: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowButton {
    None,
    Minimize,
    Maximize,
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellWindow {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub curr_x: f32,
    pub curr_y: f32,
    pub curr_w: f32,
    pub curr_h: f32,
    pub minimized: bool,
    pub maximized: bool,
    pub closing: bool,
    pub stored_rect: (i32, i32, i32, i32),
    pub workspace: u8,
    pub content: WindowContent,
    pub damage: Vec<(i32, i32, i32, i32)>,
    pub buffer_handle: Option<u64>,
    pub is_dmabuf: bool,
    pub title: [u8; 32],
}

impl Default for ShellWindow {
    fn default() -> Self {
        Self {
            x: 0, y: 0, w: 0, h: 0,
            curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (0, 0, 0, 0),
            workspace: 0,
            content: WindowContent::None,
            damage: Vec::new(),
            buffer_handle: None,
            is_dmabuf: false,
            title: [0; 32],
        }
    }
}

impl ShellWindow {
    pub const fn new_empty() -> Self {
        Self {
            x: 0, y: 0, w: 0, h: 0,
            curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (0, 0, 0, 0),
            workspace: 0,
            content: WindowContent::None,
            damage: Vec::new(),
            buffer_handle: None,
            is_dmabuf: false,
            title: [0; 32],
        }
    }

    pub const TITLE_H: i32 = 28;

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
        let btn_y = self.y + (Self::TITLE_H - 16) / 2;
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

    pub fn curr_rect(&self) -> Rectangle {
        Rectangle::new(
            Point::new(self.curr_x as i32, self.curr_y as i32),
            Size::new(self.curr_w as u32, self.curr_h as u32),
        )
    }

    pub fn is_opaque(&self, surfaces: &[ExternalSurface]) -> bool {
        if self.minimized || self.closing { return false; }
        match self.content {
            WindowContent::InternalDemo => true,
            WindowContent::External(idx) => {
                if (idx as usize) < surfaces.len() {
                    surfaces[idx as usize].is_opaque()
                } else {
                    false
                }
            }
            WindowContent::Snp { .. } => true,
            WindowContent::X11 { .. } => true,
            WindowContent::None => false,
        }
    }

    pub fn title_str(&self) -> &str {
        let len = self.title.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.title[..len]).unwrap_or("Window")
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExternalSurface {
    pub id: u32,
    pub pid: u32,
    pub vaddr: usize,
    pub buffer_size: usize,
    /// The actual length of the mmap'd region. Stays constant after CREATE;
    /// `buffer_size` may diverge on UPDATE but the blit is clamped to this.
    pub mapped_len: usize,
    pub active: bool,
    pub ready_to_flip: bool,
}

impl Default for ExternalSurface {
    fn default() -> Self {
        Self { id: 0, pid: 0, vaddr: 0, buffer_size: 0, mapped_len: 0, active: false, ready_to_flip: false }
    }
}

impl ExternalSurface {
    pub fn unmap(&mut self) {
        if self.vaddr != 0 && self.mapped_len > 0 {
            unsafe {
                libc::munmap(self.vaddr as *mut core::ffi::c_void, self.mapped_len);
            }
        }
        self.vaddr = 0;
        self.buffer_size = 0;
        self.mapped_len = 0;
        self.active = false;
    }

    pub fn is_opaque(&self) -> bool {
        self.active && self.ready_to_flip
    }
}

pub fn focus_under_cursor(px: i32, py: i32, windows: &[ShellWindow], count: usize) -> Option<usize> {
    for i in (0..count).rev() {
        let w = &windows[i];
        if w.content != WindowContent::None && !w.minimized && !w.closing && w.contains(px, py) {
            return Some(i);
        }
    }
    None
}

/// Find the topmost focusable (non-None, non-minimized, non-closing) window in `windows[..count]`,
/// optionally excluding the window at index `exclude`.
pub fn find_next_focusable(windows: &[ShellWindow], count: usize, exclude: Option<usize>) -> Option<usize> {
    (0..count).rev().find(|&i| {
        exclude.map_or(true, |ex| i != ex)
            && windows[i].content != WindowContent::None
            && !windows[i].minimized
            && !windows[i].closing
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_contains() {
        let win = ShellWindow {
            x: 10, y: 10, w: 100, h: 100,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        assert!(win.contains(50, 50));
        assert!(!win.contains(9, 50));
    }

    #[test]
    fn test_title_bar_contains() {
        let win = ShellWindow {
            x: 10, y: 10, w: 100, h: 100,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        assert!(win.title_bar_contains(50, 20));
        assert!(!win.title_bar_contains(50, 50));
    }

    #[test]
    fn test_check_button_click() {
        let win = ShellWindow {
            x: 100, y: 100, w: 200, h: 100,
            content: WindowContent::InternalDemo,
            ..Default::default()
        };
        assert_eq!(win.check_button_click(285, 110), WindowButton::Close);
        assert_eq!(win.check_button_click(264, 110), WindowButton::Maximize);
        assert_eq!(win.check_button_click(243, 110), WindowButton::Minimize);
    }

    #[test]
    fn test_focus_under_cursor_empty() {
        let windows: [ShellWindow; 2] = core::array::from_fn(|_| ShellWindow::default());
        assert_eq!(focus_under_cursor(50, 50, &windows, 0), None);
    }

    #[test]
    fn test_focus_under_cursor_stacked() {
        let windows = [
            ShellWindow {
                x: 0, y: 0, w: 200, h: 200,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
            ShellWindow {
                x: 50, y: 50, w: 100, h: 100,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
        ];
        assert_eq!(focus_under_cursor(75, 75, &windows, 2), Some(1));
    }

    #[test]
    fn test_focus_under_cursor_skips_closing() {
        // A closing window on top should not intercept focus; the window below it should.
        let windows = [
            ShellWindow {
                x: 0, y: 0, w: 200, h: 200,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
            ShellWindow {
                x: 50, y: 50, w: 100, h: 100,
                content: WindowContent::InternalDemo,
                closing: true, // This window is being closed (invisible)
                ..Default::default()
            },
        ];
        // The closing window (index 1) overlaps at (75, 75); it must be skipped.
        // The window below (index 0) should receive focus.
        assert_eq!(focus_under_cursor(75, 75, &windows, 2), Some(0));
        // Outside both windows → None
        assert_eq!(focus_under_cursor(250, 250, &windows, 2), None);
    }

    #[test]
    fn test_next_visible_empty() {
        let windows: [ShellWindow; 1] = [ShellWindow::default()];
        assert_eq!(next_visible(0, true, &windows, 0), None);
    }

    #[test]
    fn test_next_visible_skips_minimized() {
        let windows = [
            ShellWindow {
                x: 0, y: 0, w: 100, h: 100,
                minimized: true,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
            ShellWindow {
                x: 200, y: 0, w: 100, h: 100,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
        ];
        assert_eq!(next_visible(0, true, &windows, 2), Some(1));
    }

    #[test]
    fn test_next_visible_all_minimized_returns_none() {
        let windows = [
            ShellWindow {
                minimized: true,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
            ShellWindow {
                minimized: true,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
        ];
        assert_eq!(next_visible(0, true, &windows, 2), None);
    }

    #[test]
    fn test_stress_focus_and_next_visible() {
        let windows = [
            ShellWindow {
                x: 0, y: 0, w: 100, h: 100,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
            ShellWindow {
                x: 100, y: 0, w: 100, h: 100,
                content: WindowContent::InternalDemo,
                ..Default::default()
            },
        ];
        for _ in 0..50_000 {
            assert_eq!(focus_under_cursor(50, 50, &windows, 2), Some(0));
            assert_eq!(next_visible(0, true, &windows, 2), Some(1));
        }
    }

    #[test]
    fn test_unmap_clears_mapped_len() {
        let mut surface = ExternalSurface {
            id: 1, pid: 100, vaddr: 0, buffer_size: 4096, mapped_len: 4096,
            active: true, ready_to_flip: true,
        };
        surface.unmap();
        assert_eq!(surface.vaddr, 0);
        assert_eq!(surface.buffer_size, 0);
        assert_eq!(surface.mapped_len, 0);
        assert!(!surface.active);
    }

    #[test]
    fn test_unmap_zero_vaddr_no_panic() {
        let mut surface = ExternalSurface::default();
        surface.unmap();
        assert_eq!(surface.vaddr, 0);
        assert_eq!(surface.mapped_len, 0);
    }
}
