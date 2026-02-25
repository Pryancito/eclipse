use std::path::{Path, PathBuf};
use std::env;

/// Encuentra el directorio raíz del proyecto Eclipse OS.
/// Busca hacia arriba en el árbol de directorios la carpeta que contiene 'installer/' o 'build.sh'.
pub fn find_project_root() -> PathBuf {
    let mut current = env::current_dir().expect("Failed to get current directory");
    
    loop {
        // Verificar si estamos en la raíz (contiene installer/ y eclipse_kernel/)
        if current.join("installer").is_dir() && current.join("eclipse_kernel").is_dir() {
            return current;
        }
        
        // Si no, subir un nivel
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            // Si llegamos a la raíz del sistema sin encontrar nada, 
            // asumimos el directorio actual pero esto probablemente fallará más adelante.
            return env::current_dir().expect("Failed to get current directory");
        }
    }
}

/// Resuelve una ruta relativa al directorio raíz del proyecto.
/// Si la ruta ya empieza con el prefijo correcto de profundidad, se usa tal cual,
/// de lo contrario se normaliza a la raíz.
pub fn resolve_path(rel_path: &str) -> PathBuf {
    let root = find_project_root();
    
    // Limpiar el prefijo "../" si rel_path lo tiene, ya que resolvemos desde la raíz
    let clean_path = if rel_path.starts_with("../") {
        &rel_path[3..]
    } else {
        rel_path
    };
    
    root.join(clean_path)
}
