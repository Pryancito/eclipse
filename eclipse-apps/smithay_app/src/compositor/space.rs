//! Espacio de ventanas (stacking, animaciones, tiling).
//! Inspirado en Smithay/cosmic-comp: master+stack layout.

use std::prelude::v1::*;
use super::{ShellWindow, WindowContent, MAX_WINDOWS_COUNT, ExternalSurface};
use core::iter::Iterator;

pub struct Space {
    pub windows: [ShellWindow; MAX_WINDOWS_COUNT],
    pub window_count: usize,
}

impl Space {
    pub fn new() -> Self {
        Self {
            windows: [const { ShellWindow {
                x: 0, y: 0, w: 0, h: 0,
                curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0,
                minimized: false, maximized: false, closing: false,
                stored_rect: (0, 0, 0, 0),
                workspace: 0,
                content: WindowContent::None,
            } }; MAX_WINDOWS_COUNT],
            window_count: 0,
        }
    }

    pub fn map_window(&mut self, window: ShellWindow) {
        if self.window_count < MAX_WINDOWS_COUNT {
            self.windows[self.window_count] = window;
            self.window_count += 1;
        }
    }

    pub fn unmap_window(&mut self, index: usize, surfaces: &mut [ExternalSurface]) {
        if index < self.window_count {
            if let WindowContent::External(s_idx) = self.windows[index].content {
                if (s_idx as usize) < surfaces.len() {
                    surfaces[s_idx as usize].unmap();
                }
            }
            for i in index..(self.window_count - 1) {
                self.windows[i] = self.windows[i + 1];
            }
            self.window_count -= 1;
            self.windows[self.window_count].content = WindowContent::None;
        }
    }

    pub fn raise_window(&mut self, index: usize) {
        if index < self.window_count && index < self.window_count - 1 {
            let target = self.window_count - 1;
            self.windows.swap(index, target);
        }
    }

    pub fn window_under_cursor(&self, px: i32, py: i32) -> Option<usize> {
        for i in (0..self.window_count).rev() {
            let w = &self.windows[i];
            if !matches!(w.content, WindowContent::None) && !w.minimized && w.contains(px, py) {
                return Some(i);
            }
        }
        None
    }

    pub fn update_animations(&mut self, surfaces: &mut [ExternalSurface]) -> u16 {
        let mut animating_mask = 0u16;
        let mut min_count_anim = 0;
        let mut i = 0;
        while i < self.window_count {
            if matches!(self.windows[i].content, WindowContent::None) {
                i += 1;
                continue;
            }

            let (tx, ty, tw, th) = if self.windows[i].closing {
                (self.windows[i].curr_x + self.windows[i].curr_w / 2.0, self.windows[i].curr_y + self.windows[i].curr_h / 2.0, 0.0, 0.0)
            } else if self.windows[i].minimized {
                let px = (100 + (min_count_anim % 3) * 120) as f32;
                let py = (250 + (min_count_anim / 3) * 150) as f32;
                min_count_anim += 1;
                (px - 20.0, py - 40.0, 40.0, 40.0)
            } else {
                (self.windows[i].x as f32, self.windows[i].y as f32, self.windows[i].w as f32, self.windows[i].h as f32)
            };

            let lerp = if self.windows[i].closing { 0.32 } else { 0.22 };
            let dx = (tx - self.windows[i].curr_x).abs();
            let dy = (ty - self.windows[i].curr_y).abs();
            let dw = (tw - self.windows[i].curr_w).abs();
            let dh = (th - self.windows[i].curr_h).abs();

            if dx > 0.1 || dy > 0.1 || dw > 0.1 || dh > 0.1 {
                animating_mask |= 1 << i;
                self.windows[i].curr_x += (tx - self.windows[i].curr_x) * lerp;
                self.windows[i].curr_y += (ty - self.windows[i].curr_y) * lerp;
                self.windows[i].curr_w += (tw - self.windows[i].curr_w) * lerp;
                self.windows[i].curr_h += (th - self.windows[i].curr_h) * lerp;
            } else if (self.windows[i].curr_x - tx).abs() > 0.001 || (self.windows[i].curr_y - ty).abs() > 0.001 {
                self.windows[i].curr_x = tx;
                self.windows[i].curr_y = ty;
                self.windows[i].curr_w = tw;
                self.windows[i].curr_h = th;
                animating_mask |= 1 << i;
            }

            if self.windows[i].closing && self.windows[i].curr_w < 5.0 {
                self.unmap_window(i, surfaces);
            } else {
                i += 1;
            }
        }
        animating_mask
    }

    pub fn apply_tiled_layout(
        &mut self,
        fb_w: i32,
        fb_h: i32,
        focused_idx: Option<usize>,
    ) {
        super::tiling::apply_master_stack(
            &mut self.windows,
            self.window_count,
            fb_w,
            fb_h,
            focused_idx,
            &super::tiling::TilingConfig::default(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_space_map_unmap() {
        let mut space = Space::new();
        let mut surfaces = [ExternalSurface { id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false }; MAX_EXTERNAL_SURFACES];
        let win = ShellWindow {
            x: 0, y: 0, w: 100, h: 100,
            curr_x: 0.0, curr_y: 0.0, curr_w: 100.0, curr_h: 100.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (0, 0, 100, 100), workspace: 0,
            content: WindowContent::InternalDemo,
        };
        space.map_window(win);
        assert_eq!(space.window_count, 1);
        space.unmap_window(0, &mut surfaces);
        assert_eq!(space.window_count, 0);
    }

    #[test]
    fn test_space_raise_window() {
        let mut space = Space::new();
        let win1 = ShellWindow { x: 1, y: 1, w: 10, h: 10, curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0, minimized: false, maximized: false, closing: false, stored_rect: (0,0,0,0), workspace: 0, content: WindowContent::InternalDemo };
        let win2 = ShellWindow { x: 2, y: 2, w: 10, h: 10, curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0, minimized: false, maximized: false, closing: false, stored_rect: (0,0,0,0), workspace: 0, content: WindowContent::InternalDemo };
        space.map_window(win1);
        space.map_window(win2);
        assert_eq!(space.windows[0].x, 1);
        space.raise_window(0);
        assert_eq!(space.windows[0].x, 2);
    }

    #[test]
    fn test_window_under_cursor() {
        let mut space = Space::new();
        let win = ShellWindow {
            x: 10, y: 10, w: 100, h: 100,
            curr_x: 10.0, curr_y: 10.0, curr_w: 100.0, curr_h: 100.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (10, 10, 100, 100), workspace: 0,
            content: WindowContent::InternalDemo,
        };
        space.map_window(win);
        assert_eq!(space.window_under_cursor(50, 50), Some(0));
        assert_eq!(space.window_under_cursor(5, 5), None);
    }

    #[test]
    fn test_window_under_cursor_skips_minimized() {
        let mut space = Space::new();
        let win = ShellWindow {
            x: 10, y: 10, w: 100, h: 100,
            curr_x: 10.0, curr_y: 10.0, curr_w: 100.0, curr_h: 100.0,
            minimized: true, maximized: false, closing: false,
            stored_rect: (10, 10, 100, 100), workspace: 0,
            content: WindowContent::InternalDemo,
        };
        space.map_window(win);
        assert_eq!(space.window_under_cursor(50, 50), None);
    }

    #[test]
    fn test_raise_window_last_is_noop() {
        let mut space = Space::new();
        let win = ShellWindow { x: 1, y: 1, w: 10, h: 10, curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0, minimized: false, maximized: false, closing: false, stored_rect: (0,0,0,0), workspace: 0, content: WindowContent::InternalDemo };
        space.map_window(win);
        space.raise_window(0);
        assert_eq!(space.windows[0].x, 1);
    }

    #[test]
    fn test_unmap_invalid_index() {
        let mut space = Space::new();
        let mut surfaces = [ExternalSurface { id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false }; MAX_EXTERNAL_SURFACES];
        space.map_window(ShellWindow { x: 0, y: 0, w: 100, h: 100, curr_x: 0.0, curr_y: 0.0, curr_w: 100.0, curr_h: 100.0, minimized: false, maximized: false, closing: false, stored_rect: (0,0,100,100), workspace: 0, content: WindowContent::InternalDemo });
        space.unmap_window(5, &mut surfaces);
        assert_eq!(space.window_count, 1);
    }

    #[test]
    fn test_map_window_at_capacity() {
        let mut space = Space::new();
        let win = ShellWindow { x: 0, y: 0, w: 1, h: 1, curr_x: 0.0, curr_y: 0.0, curr_w: 1.0, curr_h: 1.0, minimized: false, maximized: false, closing: false, stored_rect: (0,0,1,1), workspace: 0, content: WindowContent::InternalDemo };
        for _ in 0..MAX_WINDOWS_COUNT {
            space.map_window(win);
        }
        assert_eq!(space.window_count, MAX_WINDOWS_COUNT);
        space.map_window(win);
        assert_eq!(space.window_count, MAX_WINDOWS_COUNT);
    }

    #[test]
    fn test_stress_map_unmap_cycle() {
        const CYCLES: u32 = 5_000;
        let mut space = Space::new();
        let mut surfaces = [ExternalSurface { id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false }; MAX_EXTERNAL_SURFACES];
        let win = ShellWindow {
            x: 0, y: 0, w: 100, h: 100,
            curr_x: 0.0, curr_y: 0.0, curr_w: 100.0, curr_h: 100.0,
            minimized: false, maximized: false, closing: false,
            stored_rect: (0, 0, 100, 100), workspace: 0,
            content: WindowContent::InternalDemo,
        };
        for _ in 0..CYCLES {
            space.map_window(win);
            space.unmap_window(0, &mut surfaces);
        }
    }

    #[test]
    fn test_stress_raise_rotation() {
        let mut space = Space::new();
        let win = ShellWindow { x: 0, y: 0, w: 10, h: 10, curr_x: 0.0, curr_y: 0.0, curr_w: 10.0, curr_h: 10.0, minimized: false, maximized: false, closing: false, stored_rect: (0,0,10,10), workspace: 0, content: WindowContent::InternalDemo };
        for _ in 0..4 { space.map_window(win); }
        for _ in 0..10_000 {
            space.raise_window(0);
            space.raise_window(1);
            space.raise_window(2);
        }
        assert_eq!(space.window_count, 4);
    }
}
