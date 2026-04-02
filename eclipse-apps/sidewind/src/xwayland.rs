//! X window manager state for tracking X11 window properties.
use alloc::collections::BTreeMap;

#[derive(Clone)]
pub struct X11WindowProps {
    pub title: [u8; 64],
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

impl Default for X11WindowProps {
    fn default() -> Self {
        Self { title: [0u8; 64], x: 0, y: 0, width: 0, height: 0 }
    }
}

pub struct XwmState {
    pub window_props: BTreeMap<u32, X11WindowProps>,
}

impl XwmState {
    pub fn new() -> Self {
        Self { window_props: BTreeMap::new() }
    }

    pub fn handle_map_request(&mut self, window_id: u32, x: i16, y: i16, width: u16, height: u16, title: [u8; 64]) {
        let props = self.window_props.entry(window_id).or_default();
        props.x = x;
        props.y = y;
        props.width = width;
        props.height = height;
        props.title = title;
    }

    pub fn handle_unmap_request(&mut self, _window_id: u32) {}

    pub fn handle_destroy(&mut self, window_id: u32) {
        self.window_props.remove(&window_id);
    }

    pub fn set_window_geometry(&mut self, window_id: u32, x: i16, y: i16, width: u16, height: u16) {
        let props = self.window_props.entry(window_id).or_default();
        props.x = x;
        props.y = y;
        props.width = width;
        props.height = height;
    }

    pub fn set_window_title(&mut self, window_id: u32, title: [u8; 64]) {
        let props = self.window_props.entry(window_id).or_default();
        props.title = title;
    }

    pub fn get_window_title(&self, window_id: u32) -> Option<[u8; 64]> {
        self.window_props.get(&window_id).map(|p| p.title)
    }

    pub fn get_window_geometry(&self, window_id: u32) -> Option<(i16, i16, u16, u16)> {
        self.window_props.get(&window_id).map(|p| (p.x, p.y, p.width, p.height))
    }
}
