//! Gestión de directorios para Eclipse OS

use crate::filesystem::{MAX_DIRECTORY_ENTRIES, MAX_FILENAME_LEN};
use alloc::string::ToString;

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

    /// Buscar entrada por nombre
    pub fn find_entry(&self, name: &str) -> Option<&DirectoryEntry> {
        for entry in &self.entries {
            if let Some(ref dir_entry) = entry {
                let entry_name =
                    core::str::from_utf8(&dir_entry.name[..dir_entry.name_len as usize]).ok()?;
                if entry_name == name {
                    return Some(dir_entry);
                }
            }
        }
        None
    }

    /// Buscar entrada por inodo
    pub fn find_entry_by_inode(&self, inode: u32) -> Option<&DirectoryEntry> {
        for entry in &self.entries {
            if let Some(ref dir_entry) = entry {
                if dir_entry.inode == inode {
                    return Some(dir_entry);
                }
            }
        }
        None
    }

    /// Eliminar entrada por nombre
    pub fn remove_entry(&mut self, name: &str) -> bool {
        for i in 0..self.entry_count {
            if let Some(ref dir_entry) = self.entries[i] {
                let entry_name =
                    core::str::from_utf8(&dir_entry.name[..dir_entry.name_len as usize]).ok();
                if entry_name == Some(name) {
                    // Mover entradas hacia atrás
                    for j in i..self.entry_count - 1 {
                        self.entries[j] = self.entries[j + 1];
                    }
                    self.entries[self.entry_count - 1] = None;
                    self.entry_count -= 1;
                    return true;
                }
            }
        }
        false
    }

    /// Obtener lista de nombres de archivos
    pub fn list_files(&self) -> alloc::vec::Vec<alloc::string::String> {
        let mut files = alloc::vec::Vec::new();
        for entry in &self.entries {
            if let Some(ref dir_entry) = entry {
                if let Ok(name) =
                    core::str::from_utf8(&dir_entry.name[..dir_entry.name_len as usize])
                {
                    files.push(name.to_string());
                }
            }
        }
        files
    }

    /// Verificar si el directorio está vacío
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    /// Obtener número de entradas
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }
}
