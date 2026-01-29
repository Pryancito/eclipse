//! Window Manager for COSMIC Desktop

use heapless::Vec;

/// Maximum windows
pub const MAX_WINDOWS: usize = 64;

/// Window state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowState {
    Normal,
    Maximized,
    Minimized,
    Fullscreen,
}

/// Window representation
pub struct Window {
    pub surface_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub state: WindowState,
    pub focused: bool,
    pub visible: bool,
}

impl Window {
    pub fn new(surface_id: u32) -> Self {
        Self {
            surface_id,
            x: 100,
            y: 100,
            width: 800,
            height: 600,
            state: WindowState::Normal,
            focused: false,
            visible: true,
        }
    }

    pub fn maximize(&mut self) {
        self.state = WindowState::Maximized;
        // In real implementation, resize to screen size
    }

    pub fn minimize(&mut self) {
        self.state = WindowState::Minimized;
        self.visible = false;
    }

    pub fn restore(&mut self) {
        self.state = WindowState::Normal;
        self.visible = true;
    }

    pub fn move_to(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }
}

/// Window manager
pub struct WindowManager {
    pub windows: Vec<Window, MAX_WINDOWS>,
    pub focused_window: Option<u32>,
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            focused_window: None,
        }
    }

    pub fn add_window(&mut self, window: Window) -> Result<(), &'static str> {
        let surface_id = window.surface_id;
        self.windows.push(window).map_err(|_| "Too many windows")?;
        self.focus_window(surface_id);
        Ok(())
    }

    pub fn remove_window(&mut self, surface_id: u32) -> bool {
        if let Some(pos) = self.windows.iter().position(|w| w.surface_id == surface_id) {
            self.windows.swap_remove(pos);
            
            // Update focus
            if self.focused_window == Some(surface_id) {
                self.focused_window = self.windows.first().map(|w| w.surface_id);
            }
            
            true
        } else {
            false
        }
    }

    pub fn get_window(&self, surface_id: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.surface_id == surface_id)
    }

    pub fn get_window_mut(&mut self, surface_id: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.surface_id == surface_id)
    }

    pub fn focus_window(&mut self, surface_id: u32) {
        // Unfocus all windows
        for window in self.windows.iter_mut() {
            window.focused = false;
        }

        // Focus the specified window
        if let Some(window) = self.get_window_mut(surface_id) {
            window.focused = true;
            self.focused_window = Some(surface_id);
        }
    }

    pub fn get_focused_window(&self) -> Option<&Window> {
        self.focused_window.and_then(|id| self.get_window(id))
    }

    pub fn tile_windows(&mut self) {
        // Simple tiling algorithm
        let count = self.windows.len();
        if count == 0 {
            return;
        }

        let screen_width = 1920;
        let screen_height = 1080 - 48; // Minus panel
        
        // For simplicity, split screen in grid
        // Simple square root approximation for small numbers
        let cols = match count {
            1 => 1,
            2..=4 => 2,
            5..=9 => 3,
            10..=16 => 4,
            _ => 4, // Max 4 columns
        };
        let rows = (count + cols - 1) / cols;
        
        let tile_width = screen_width / cols as u32;
        let tile_height = screen_height / rows as u32;

        for (i, window) in self.windows.iter_mut().enumerate() {
            let row = i / cols;
            let col = i % cols;
            
            window.x = (col as u32 * tile_width) as i32;
            window.y = (row as u32 * tile_height) as i32;
            window.width = tile_width;
            window.height = tile_height;
        }
    }
}
