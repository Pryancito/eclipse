//! NTFS Module
//! Sistema de archivos NTFS

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Estructura interna de NTFS
struct NTFSInternal {
    mounted: bool,
    mount_path: String,
    files: HashMap<String, Vec<u8>>,  // Simulación simple de archivos en memoria
    attributes: HashMap<String, u32>, // Atributos de archivos
}

/// Handle de NTFS
pub struct NTFSHandle {
    internal: Arc<Mutex<NTFSInternal>>,
}

impl Default for NTFSHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl NTFSHandle {
    fn new() -> Self {
        NTFSHandle {
            internal: Arc::new(Mutex::new(NTFSInternal {
                mounted: false,
                mount_path: String::new(),
                files: HashMap::new(),
                attributes: HashMap::new(),
            })),
        }
    }
}

/// Inicializar NTFS
pub fn NTFS_Initialize() {
    println!("💿 NTFS inicializado");
}

/// Crear instancia de NTFS
pub fn create_ntfs() -> NTFSHandle {
    NTFSHandle::new()
}

/// Montar NTFS
pub fn mount_ntfs(handle: &mut NTFSHandle, path: &str) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        internal.mounted = true;
        internal.mount_path = path.to_string();
        println!("💿 NTFS montado en: {}", path);
        true
    } else {
        false
    }
}

/// Desmontar NTFS
pub fn unmount_ntfs(handle: &mut NTFSHandle) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        internal.mounted = false;
        internal.mount_path.clear();
        println!("💿 NTFS desmontado");
        true
    } else {
        false
    }
}

/// Leer archivo NTFS
pub fn read_ntfs_file(handle: &NTFSHandle, path: &str) -> Vec<u8> {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ NTFS no está montado");
            return vec![];
        }
        
        // Intentar leer desde el hashmap en memoria
        if let Some(data) = internal.files.get(path) {
            return data.clone();
        }
        
        // Si no está en memoria, intentar leer del sistema de archivos real
        let full_path = format!("{}/{}", internal.mount_path, path);
        std::fs::read(&full_path).unwrap_or_else(|e| {
            eprintln!("❌ Error leyendo archivo NTFS {}: {}", path, e);
            vec![]
        })
    } else {
        vec![]
    }
}

/// Escribir archivo NTFS
pub fn write_ntfs_file(handle: &mut NTFSHandle, path: &str, data: &[u8]) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ NTFS no está montado");
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

/// Obtener atributos de archivo NTFS
pub fn get_ntfs_attributes(handle: &NTFSHandle, path: &str) -> u32 {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            return 0;
        }
        
        // Retornar atributos almacenados o calcular desde metadata
        if let Some(&attrs) = internal.attributes.get(path) {
            return attrs;
        }
        
        let full_path = format!("{}/{}", internal.mount_path, path);
        if let Ok(metadata) = std::fs::metadata(&full_path) {
            let mut attrs = 0u32;
            if metadata.is_dir() {
                attrs |= 0x10; // FILE_ATTRIBUTE_DIRECTORY
            }
            if metadata.permissions().readonly() {
                attrs |= 0x01; // FILE_ATTRIBUTE_READONLY
            }
            attrs
        } else {
            0
        }
    } else {
        0
    }
}

/// Establecer atributos de archivo NTFS
pub fn set_ntfs_attributes(handle: &mut NTFSHandle, path: &str, attributes: u32) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        if !internal.mounted {
            return false;
        }
        
        internal.attributes.insert(path.to_string(), attributes);
        true
    } else {
        false
    }
}

/// Listar archivos en directorio NTFS
pub fn list_ntfs_directory(handle: &NTFSHandle, path: &str) -> Vec<String> {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ NTFS no está montado");
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

/// Crear directorio en NTFS
pub fn create_ntfs_directory(handle: &NTFSHandle, path: &str) -> bool {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ NTFS no está montado");
            return false;
        }
        
        let full_path = format!("{}/{}", internal.mount_path, path);
        std::fs::create_dir_all(&full_path).is_ok()
    } else {
        false
    }
}

/// Eliminar archivo NTFS
pub fn delete_ntfs_file(handle: &mut NTFSHandle, path: &str) -> bool {
    if let Ok(mut internal) = handle.internal.lock() {
        if !internal.mounted {
            eprintln!("❌ NTFS no está montado");
            return false;
        }
        
        // Eliminar de memoria
        internal.files.remove(path);
        internal.attributes.remove(path);
        
        // Eliminar del sistema de archivos real
        let full_path = format!("{}/{}", internal.mount_path, path);
        std::fs::remove_file(&full_path).is_ok()
    } else {
        false
    }
}

/// Obtener información de archivo NTFS
pub fn get_ntfs_file_info(handle: &NTFSHandle, path: &str) -> Option<(u64, bool, u32)> {
    if let Ok(internal) = handle.internal.lock() {
        if !internal.mounted {
            return None;
        }
        
        let full_path = format!("{}/{}", internal.mount_path, path);
        if let Ok(metadata) = std::fs::metadata(&full_path) {
            let attrs = get_ntfs_attributes(handle, path);
            Some((metadata.len(), metadata.is_dir(), attrs))
        } else {
            None
        }
    } else {
        None
    }
}

/// Liberar NTFS
pub fn free_ntfs(handle: &mut NTFSHandle) -> bool {
    unmount_ntfs(handle)
}