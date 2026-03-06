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

pub struct XwmState {
    pub atoms: AtomCache,
    pub windows: Vec<u32>,
}

impl XwmState {
    pub fn new() -> Self {
        Self {
            atoms: AtomCache::new(),
            windows: Vec::new(),
        }
    }
    pub fn handle_map_request(&mut self, window_id: u32) {
        if !self.windows.contains(&window_id) {
            self.windows.push(window_id);
        }
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
}
