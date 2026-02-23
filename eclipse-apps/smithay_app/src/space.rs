use crate::compositor::{ShellWindow, WindowContent, MAX_WINDOWS_COUNT, ExternalSurface};

/// A Space represents a set of windows arranged in a 2D coordinate system.
/// This is inspired by Smithay's Space abstraction.
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

    pub fn unmap_window(&mut self, index: usize) {
        if index < self.window_count {
            for i in index..(self.window_count - 1) {
                self.windows[i] = self.windows[i + 1];
            }
            self.window_count -= 1;
            // Clear the last slot
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
            if w.content != WindowContent::None && !w.minimized && w.contains(px, py) {
                return Some(i);
            }
        }
        None
    }

    pub fn update_animations(&mut self, surfaces: &mut [ExternalSurface]) {
        let mut min_count_anim = 0;
        let mut i = 0;
        while i < self.window_count {
            if self.windows[i].content == WindowContent::None {
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
            self.windows[i].curr_x += (tx - self.windows[i].curr_x) * lerp;
            self.windows[i].curr_y += (ty - self.windows[i].curr_y) * lerp;
            self.windows[i].curr_w += (tw - self.windows[i].curr_w) * lerp;
            self.windows[i].curr_h += (th - self.windows[i].curr_h) * lerp;

            if self.windows[i].closing && self.windows[i].curr_w < 5.0 {
                if let WindowContent::External(s_idx) = self.windows[i].content {
                    if (s_idx as usize) < surfaces.len() {
                        // The actual munmap should happen in the backend/compositor level, 
                        // but for now we follow the existing pattern.
                        surfaces[s_idx as usize].active = false;
                    }
                }
                self.unmap_window(i);
            } else {
                i += 1;
            }
        }
    }
}
