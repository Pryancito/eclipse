#![no_std]

extern crate alloc;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;

/// X11 Atom representation
pub type Atom = u32;

/// A simple Atom cache to intern strings and track X11 properties
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
        
        // Pre-fill with some standard X11 atoms (simplified)
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

/// XWM (X Window Manager) stub state
pub struct XwmState {
    pub atoms: AtomCache,
    // Track mapped X11 windows
    pub windows: Vec<u32>, // simplified: just PIDs/IDs
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
