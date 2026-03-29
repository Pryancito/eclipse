#![no_std]

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// X11 Atom representation
pub type Atom = u32;

pub struct AtomCache {
    atoms: BTreeMap<String, Atom>,
    names: BTreeMap<Atom, String>,
    next_atom: Atom,
}

impl AtomCache {
    pub fn new() -> Self {
        let mut cache = Self {
            atoms: BTreeMap::new(),
            names: BTreeMap::new(),
            next_atom: 1,
        };
        cache.intern("WM_NAME");
        cache.intern("WM_CLASS");
        cache.intern("UTF8_STRING");
        cache.intern("WM_PROTOCOLS");
        cache.intern("WM_DELETE_WINDOW");
        cache.intern("_NET_WM_NAME");
        cache.intern("_NET_WM_STATE");
        cache.intern("_NET_WM_STATE_FULLSCREEN");
        cache
    }
    pub fn intern(&mut self, name: &str) -> Atom {
        if let Some(&atom) = self.atoms.get(name) {
            return atom;
        }
        let atom = self.next_atom;
        self.next_atom += 1;
        let name_str = String::from(name);
        self.atoms.insert(name_str.clone(), atom);
        self.names.insert(atom, name_str);
        atom
    }
    pub fn get_name(&self, atom: Atom) -> Option<&str> {
        self.names.get(&atom).map(|s| s.as_str())
    }
}

/// Properties associated with a mapped X11 window.
#[derive(Debug, Clone)]
pub struct X11WindowProps {
    /// Window title (WM_NAME or _NET_WM_NAME), up to 64 bytes.
    pub title: Vec<u8>,
    /// X position on screen.
    pub x: i16,
    /// Y position on screen.
    pub y: i16,
    /// Window width in pixels.
    pub width: u16,
    /// Window height in pixels.
    pub height: u16,
}

impl X11WindowProps {
    pub fn new() -> Self {
        Self {
            title: Vec::new(),
            x: 0, y: 0,
            width: 0, height: 0,
        }
    }
}

pub struct XwmState {
    pub atoms: AtomCache,
    /// Currently mapped X11 window IDs.
    pub windows: Vec<u32>,
    /// Per-window properties, keyed by window ID.
    pub window_props: BTreeMap<u32, X11WindowProps>,
}

impl XwmState {
    pub fn new() -> Self {
        Self {
            atoms: AtomCache::new(),
            windows: Vec::new(),
            window_props: BTreeMap::new(),
        }
    }

    /// Record that an X11 window has been mapped (made visible).
    pub fn handle_map_request(&mut self, window_id: u32) {
        if !self.windows.contains(&window_id) {
            self.windows.push(window_id);
        }
        // Ensure a props entry exists for this window.
        self.window_props.entry(window_id).or_insert_with(X11WindowProps::new);
    }

    /// Record that an X11 window has been unmapped (hidden).
    pub fn handle_unmap_request(&mut self, window_id: u32) {
        self.windows.retain(|&w| w != window_id);
    }

    /// Remove all state for a destroyed X11 window.
    pub fn handle_destroy(&mut self, window_id: u32) {
        self.windows.retain(|&w| w != window_id);
        let _ = self.window_props.remove(&window_id);
    }

    /// Update the geometry of a mapped X11 window (e.g. from a ConfigureNotify event).
    pub fn set_window_geometry(&mut self, window_id: u32, x: i16, y: i16, width: u16, height: u16) {
        let props = self.window_props.entry(window_id).or_insert_with(X11WindowProps::new);
        props.x = x;
        props.y = y;
        props.width = width;
        props.height = height;
    }

    /// Update the title of a mapped X11 window (e.g. from a PropertyNotify on WM_NAME).
    pub fn set_window_title(&mut self, window_id: u32, title: &[u8]) {
        let props = self.window_props.entry(window_id).or_insert_with(X11WindowProps::new);
        props.title.clear();
        // Store at most 64 bytes to avoid unbounded growth.
        let len = title.len().min(64);
        props.title.extend_from_slice(&title[..len]);
    }

    /// Retrieve the title of a window, or an empty slice if unknown.
    pub fn get_window_title(&self, window_id: u32) -> &[u8] {
        self.window_props.get(&window_id).map(|p| p.title.as_slice()).unwrap_or(&[])
    }

    /// Retrieve the stored geometry of a window.
    pub fn get_window_geometry(&self, window_id: u32) -> Option<(i16, i16, u16, u16)> {
        self.window_props.get(&window_id).map(|p| (p.x, p.y, p.width, p.height))
    }
}

#[cfg(test)]
mod tests {
    use super::{AtomCache, XwmState};
    #[test]
    fn test_stress_atom_cache() {
        const ITERS: u32 = 30_000;
        let mut cache = AtomCache::new();
        let names = [
            "WM_NAME",
            "WM_CLASS",
            "UTF8_STRING",
            "WM_PROTOCOLS",
            "WM_DELETE_WINDOW",
        ];
        for i in 0..ITERS {
            let name = names[(i as usize) % names.len()];
            let atom = cache.intern(name);
            let back = cache.get_name(atom).unwrap();
            assert_eq!(back, name);
        }
    }
    #[test]
    fn test_stress_xwm_handle_map_request() {
        const ITERS: u32 = 50_000;
        let mut state = XwmState::new();
        for i in 0..ITERS {
            state.handle_map_request((i % 1024) as u32);
        }
        assert!(state.windows.len() <= 1024);
    }

    #[test]
    fn test_xwm_unmap_and_destroy() {
        let mut state = XwmState::new();
        state.handle_map_request(1);
        state.handle_map_request(2);
        assert_eq!(state.windows.len(), 2);
        state.handle_unmap_request(1);
        assert_eq!(state.windows.len(), 1);
        assert!(!state.windows.contains(&1));
        state.handle_destroy(2);
        assert!(state.windows.is_empty());
        assert!(state.window_props.get(&2).is_none());
    }

    #[test]
    fn test_xwm_window_geometry() {
        let mut state = XwmState::new();
        state.handle_map_request(42);
        state.set_window_geometry(42, 100, 200, 800, 600);
        let geom = state.get_window_geometry(42).expect("geometry");
        assert_eq!(geom, (100, 200, 800, 600));
    }

    #[test]
    fn test_xwm_window_title() {
        let mut state = XwmState::new();
        state.handle_map_request(7);
        state.set_window_title(7, b"My App");
        assert_eq!(state.get_window_title(7), b"My App");
    }

    #[test]
    fn test_xwm_title_truncated_at_64_bytes() {
        let mut state = XwmState::new();
        state.handle_map_request(99);
        let long_title = [b'A'; 128];
        state.set_window_title(99, &long_title);
        assert_eq!(state.get_window_title(99).len(), 64);
    }
}
