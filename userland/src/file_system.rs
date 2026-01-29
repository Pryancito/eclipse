//! File System Module
//! Gestión de sistemas de archivos

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

/// Handle de archivo
pub struct FileHandle {
    file: Option<File>,
    path: String,
}

/// Inicializar sistema de archivos
pub fn file_system_init() {
    println!("📁 Sistema de archivos inicializado");
}

/// Abrir archivo
pub fn open_file(path: &str, mode: &str) -> Box<FileHandle> {
    let file = match mode {
        "r" | "rb" => {
            // Modo lectura
            File::open(path).ok()
        }
        "w" | "wb" => {
            // Modo escritura (crear o truncar)
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .ok()
        }
        "a" | "ab" => {
            // Modo append
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
                .ok()
        }
        "r+" | "rb+" => {
            // Modo lectura/escritura
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .ok()
        }
        _ => None,
    };
    
    Box::new(FileHandle {
        file,
        path: path.to_string(),
    })
}

/// Leer archivo
pub fn read_file(handle: &mut FileHandle, buffer: &mut [u8]) -> usize {
    if let Some(ref mut file) = handle.file {
        file.read(buffer).unwrap_or(0)
    } else {
        0
    }
}

/// Escribir archivo
pub fn write_file(handle: &mut FileHandle, buffer: &[u8]) -> usize {
    if let Some(ref mut file) = handle.file {
        file.write(buffer).unwrap_or(0)
    } else {
        0
    }
}

/// Cerrar archivo
pub fn close_file(mut handle: Box<FileHandle>) -> bool {
    handle.file = None;
    true
}

/// Crear directorio
pub fn create_directory(path: &str) -> bool {
    fs::create_dir_all(path).is_ok()
}

/// Eliminar directorio
pub fn remove_directory(path: &str) -> bool {
    fs::remove_dir_all(path).is_ok()
}

/// Listar directorio
pub fn list_directory(path: &str) -> Vec<String> {
    if let Ok(entries) = fs::read_dir(path) {
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
}

/// Verificar si existe un archivo
pub fn file_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Verificar si es un directorio
pub fn is_directory(path: &str) -> bool {
    Path::new(path).is_dir()
}

/// Obtener tamaño de archivo
pub fn get_file_size(path: &str) -> u64 {
    fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Eliminar archivo
pub fn delete_file(path: &str) -> bool {
    fs::remove_file(path).is_ok()
}

/// Renombrar archivo
pub fn rename_file(old_path: &str, new_path: &str) -> bool {
    fs::rename(old_path, new_path).is_ok()
}

/// Copiar archivo
pub fn copy_file(src: &str, dst: &str) -> bool {
    fs::copy(src, dst).is_ok()
}

/// Inicializar sistema de archivos
pub fn init() {
    file_system_init();
}
