//! FAT32 Module
//! Sistema de archivos FAT32

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Estructura interna de FAT32
struct FAT32Internal {
    mounted: bool,
    mount_path: String,
    files: HashMap<String, Vec<u8>>,  // Simulación simple de archivos en memoria
}

/// Handle de FAT32
pub struct FAT32Handle {
    internal: Arc<Mutex<FAT32Internal>>,
}

impl Default for FAT32Handle {
    fn default() -> Self {
        Self::new()
    }
}

impl FAT32Handle {
    fn new() -> Self {
        FAT32Handle {
            internal: Arc::new(Mutex::new(FAT32Internal {
                mounted: false,
                mount_path: String::new(),
                files: HashMap::new(),
            })),
        }
    }
}

/// Inicializar FAT32
pub fn FAT32_Initialize() {
    println!("💿 FAT32 inicializado");
}

/// Crear instancia de FAT32
pub fn create_fat32() -> FAT32Handle {
    FAT32Handle::new()
}

/// Montar FAT32
pub fn mount_fat32(handle: &mut FAT32Handle, path: &str) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        internal.mounted = true;
        internal.mount_path = path.to_string();
        println!("💿 FAT32 montado en: {}", path);
        true
    } else {
        false
    }
}

/// Desmontar FAT32
pub fn unmount_fat32(handle: &mut FAT32Handle) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        internal.mounted = false;
        internal.mount_path.clear();
        println!("💿 FAT32 desmontado");
        true
    } else {
        false
    }
}

/// Leer archivo FAT32
pub fn read_fat32_file(handle: &FAT32Handle, path: &str) -> Vec<u8> {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ FAT32 no está montado");
            return vec![];
        }
        
        // Intentar leer desde el hashmap en memoria
        if let Some(data) = internal.files.get(path) {
            return data.clone();
        }
        
        // Si no está en memoria, intentar leer del sistema de archivos real
        let full_path = format!("{}/{}", internal.mount_path, path);
        std::fs::read(&full_path).unwrap_or_else(|e| {
            eprintln!("❌ Error leyendo archivo FAT32 {}: {}", path, e);
            vec![]
        })
    } else {
        vec![]
    }
}

/// Escribir archivo FAT32
pub fn write_fat32_file(handle: &mut FAT32Handle, path: &str, data: &[u8]) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ FAT32 no está montado");
            return false;
        }
        
        // Guardar en memoria
        internal.files.insert(path.to_string(), data.to_vec());
        
        // Intentar escribir al sistema de archivos real también
        let full_path = format!("{}/{}", internal.mount_path, path);
        if let Some(parent) = std::path::Path::new(&full_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        
        std::fs::write(&full_path, data).is_ok()
    } else {
        false
    }
}

/// Listar archivos en directorio FAT32
pub fn list_fat32_directory(handle: &FAT32Handle, path: &str) -> Vec<String> {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ FAT32 no está montado");
            return vec![];
        }
        
        let full_path = format!("{}/{}", internal.mount_path, path);
        if let Ok(entries) = std::fs::read_dir(&full_path) {
            entries
                .filter_map(|entry| {
                    entry.ok().and_then(|e| {
                        e.file_name().into_string().ok()
                    })
                })
                .collect()
        } else {
            vec![]
        }
    } else {
        vec![]
    }
}

/// Crear directorio en FAT32
pub fn create_fat32_directory(handle: &FAT32Handle, path: &str) -> bool {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ FAT32 no está montado");
            return false;
        }
        
        let full_path = format!("{}/{}", internal.mount_path, path);
        std::fs::create_dir_all(&full_path).is_ok()
    } else {
        false
    }
}

/// Eliminar archivo FAT32
pub fn delete_fat32_file(handle: &mut FAT32Handle, path: &str) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ FAT32 no está montado");
            return false;
        }
        
        // Eliminar de memoria
        internal.files.remove(path);
        
        // Eliminar del sistema de archivos real
        let full_path = format!("{}/{}", internal.mount_path, path);
        std::fs::remove_file(&full_path).is_ok()
    } else {
        false
    }
}

/// Obtener información de archivo FAT32
pub fn get_fat32_file_info(handle: &FAT32Handle, path: &str) -> Option<(u64, bool)> {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            return None;
        }
        
        let full_path = format!("{}/{}", internal.mount_path, path);
        if let Ok(metadata) = std::fs::metadata(&full_path) {
            Some((metadata.len(), metadata.is_dir()))
        } else {
            None
        }
    } else {
        None
    }
}

/// Liberar FAT32
pub fn free_fat32(handle: &mut FAT32Handle) -> bool {
    unmount_fat32(handle)
}