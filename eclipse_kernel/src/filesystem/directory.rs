//! Gestión de directorios para Eclipse OS

use crate::filesystem::{MAX_FILENAME_LEN, MAX_DIRECTORY_ENTRIES};

// Entrada de directorio
#[derive(Debug, Clone, Copy)]
pub struct DirectoryEntry {
    pub inode: u32,
    pub name: [u8; MAX_FILENAME_LEN],
    pub name_len: u8,
    pub entry_type: u8,
}

impl DirectoryEntry {
    pub fn new() -> Self {
        Self {
            inode: 0,
            name: [0; MAX_FILENAME_LEN],
            name_len: 0,
            entry_type: 0,
        }
    }

    pub fn set_name(&mut self, name: &str) {
        let name_bytes = name.as_bytes();
        let len = name_bytes.len().min(MAX_FILENAME_LEN - 1);
        
        for i in 0..MAX_FILENAME_LEN {
            if i < len {
                self.name[i] = name_bytes[i];
            } else {
                self.name[i] = 0;
            }
        }
        self.name_len = len as u8;
    }
}

// Directorio
#[derive(Debug, Clone)]
pub struct Directory {
    pub entries: [Option<DirectoryEntry>; MAX_DIRECTORY_ENTRIES],
    pub entry_count: usize,
}

impl Directory {
    pub fn new() -> Self {
        Self {
            entries: [None; MAX_DIRECTORY_ENTRIES],
            entry_count: 0,
        }
    }

    pub fn add_entry(&mut self, entry: DirectoryEntry) -> bool {
        if self.entry_count < MAX_DIRECTORY_ENTRIES {
            self.entries[self.entry_count] = Some(entry);
            self.entry_count += 1;
            true
        } else {
            false
        }
    }
}
