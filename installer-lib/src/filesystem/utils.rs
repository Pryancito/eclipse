//! Utilidades del sistema de archivos para Eclipse OS

use crate::filesystem::{MAX_PATH_LEN, MAX_FILENAME_LEN};

// Componente de path
#[derive(Debug, Clone, Copy)]
pub struct PathComponent {
    pub name: [u8; MAX_FILENAME_LEN],
    pub len: usize,
}

impl PathComponent {
    pub fn new() -> Self {
        Self {
            name: [0; MAX_FILENAME_LEN],
            len: 0,
        }
    }

    pub fn from_str(s: &str) -> Self {
        let mut component = Self::new();
        let bytes = s.as_bytes();
        let len = bytes.len().min(MAX_FILENAME_LEN - 1);
        
        for i in 0..len {
            component.name[i] = bytes[i];
        }
        component.len = len;
        component
    }
}

// Path
#[derive(Debug, Clone)]
pub struct Path {
    pub components: [PathComponent; 32], // Máximo 32 componentes
    pub component_count: usize,
    pub is_absolute: bool,
}

impl Path {
    pub fn new() -> Self {
        Self {
            components: [PathComponent::new(); 32],
            component_count: 0,
            is_absolute: false,
        }
    }

    pub fn from_str(s: &str) -> Self {
        let mut path = Self::new();
        path.is_absolute = s.starts_with('/');
        
        let parts: core::str::Split<char> = s.split('/');
        for part in parts {
            if !part.is_empty() {
                if path.component_count < 32 {
                    path.components[path.component_count] = PathComponent::from_str(part);
                    path.component_count += 1;
                }
            }
        }
        path
    }

    pub fn as_str(&self) -> &str {
        // Implementación simplificada
        "/"
    }

    pub fn is_empty(&self) -> bool {
        self.component_count == 0
    }

    pub fn push(&mut self, component: &str) {
        if self.component_count < 32 {
            self.components[self.component_count] = PathComponent::from_str(component);
            self.component_count += 1;
        }
    }

    pub fn pop(&mut self) -> Option<PathComponent> {
        if self.component_count > 0 {
            self.component_count -= 1;
            Some(self.components[self.component_count].clone())
        } else {
            None
        }
    }
}

// Utilidades del sistema de archivos
pub struct FileSystemUtils;

impl FileSystemUtils {
    pub fn is_valid_filename(name: &str) -> bool {
        !name.is_empty() && 
        name.len() <= MAX_FILENAME_LEN - 1 &&
        !name.contains('/') &&
        !name.contains('\0')
    }

    pub fn is_valid_path(path: &str) -> bool {
        !path.is_empty() && 
        path.len() <= MAX_PATH_LEN - 1 &&
        !path.contains('\0')
    }

    pub fn normalize_path(path: &str) -> &str {
        // Implementación simplificada
        path
    }

    pub fn get_basename(path: &str) -> &str {
        if let Some(pos) = path.rfind('/') {
            &path[pos + 1..]
        } else {
            path
        }
    }

    pub fn get_dirname(path: &str) -> &str {
        if let Some(pos) = path.rfind('/') {
            if pos == 0 {
                "/"
            } else {
                &path[0..pos]
            }
        } else {
            "."
        }
    }

    pub fn join_paths<'a>(path1: &'a str, _path2: &str) -> &'a str {
        // Implementación simplificada
        path1
    }
}
