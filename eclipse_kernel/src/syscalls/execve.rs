//! Implementación completa de execve() para Eclipse OS
//!
//! Este módulo implementa la carga y ejecución de binarios ELF
//! desde el filesystem.

use alloc::vec::Vec;
use alloc::string::String;
use crate::debug::serial_write_str;
use crate::elf_loader::{ElfLoader, LoadedProcess};
use crate::process::manager::get_process_manager;
use crate::process::context_switch::prepare_initial_context;

/// Resultado de execve
pub type ExecveResult = Result<(), ExecveError>;

/// Errores de execve
#[derive(Debug)]
pub enum ExecveError {
    InvalidPath,
    FileNotFound,
    NotExecutable,
    InvalidElf,
    OutOfMemory,
    PermissionDenied,
    IoError,
}

impl ExecveError {
    pub fn to_errno(&self) -> i64 {
        match self {
            ExecveError::InvalidPath => -22,      // EINVAL
            ExecveError::FileNotFound => -2,      // ENOENT
            ExecveError::NotExecutable => -8,     // ENOEXEC
            ExecveError::InvalidElf => -8,        // ENOEXEC
            ExecveError::OutOfMemory => -12,      // ENOMEM
            ExecveError::PermissionDenied => -13, // EACCES
            ExecveError::IoError => -5,           // EIO
        }
    }
}

/// Leer archivo completo desde el VFS
fn read_file_from_vfs(path: &str) -> Result<Vec<u8>, ExecveError> {
    use crate::filesystem::vfs::{get_vfs, VfsError};
    
    serial_write_str(&alloc::format!("EXECVE: Leyendo archivo: {}\n", path));
    
    // Obtener el VFS (ya es un guard)
    let vfs_guard = get_vfs();
    
    if let Some(ref vfs) = *vfs_guard {
        // Determinar el punto de montaje
        let mount_point = if path.starts_with("/boot/") {
            "/boot"
        } else {
            "/"
        };
        
        // Obtener el filesystem montado
        let fs = vfs.get_mount(mount_point)
            .ok_or(ExecveError::FileNotFound)?;
        
        let fs_guard = fs.lock();
    
    // Intentar leer el archivo completo usando el método del trait
    match fs_guard.read_file_path(path) {
        Ok(data) => {
            serial_write_str(&alloc::format!(
                "EXECVE: Archivo leído exitosamente ({} bytes)\n",
                data.len()
            ));
            Ok(data)
        }
        Err(VfsError::InvalidOperation) => {
            // Si el filesystem no implementa read_file_path, usar resolve + read
            serial_write_str("EXECVE: Usando método alternativo de lectura...\n");
            
            match fs_guard.resolve_path(path) {
                Ok(inode) => {
                    // Obtener tamaño del archivo
                    let stat = fs_guard.stat(inode)
                        .map_err(|_| ExecveError::FileNotFound)?;
                    
                    // Leer el archivo completo
                    let mut buffer = alloc::vec![0u8; stat.size as usize];
                    let bytes_read = fs_guard.read(inode, 0, &mut buffer)
                        .map_err(|_| ExecveError::IoError)?;
                    
                    buffer.truncate(bytes_read);
                    
                    serial_write_str(&alloc::format!(
                        "EXECVE: Archivo leído (método alternativo, {} bytes)\n",
                        bytes_read
                    ));
                    
                    Ok(buffer)
                }
                Err(_) => Err(ExecveError::FileNotFound)
            }
        }
        Err(_) => Err(ExecveError::FileNotFound)
        }
    } else {
        Err(ExecveError::FileNotFound)
    }
}

/// Implementación completa de execve
pub fn do_execve(
    path: &str,
    argv: &[&str],
    envp: &[&str],
) -> ExecveResult {
    serial_write_str(&alloc::format!("EXECVE: Ejecutando: {}\n", path));
    
    // 1. Leer el archivo binario desde el VFS
    let elf_data = read_file_from_vfs(path)?;
    
    serial_write_str(&alloc::format!(
        "EXECVE: Binario cargado, tamaño: {} bytes\n",
        elf_data.len()
    ));
    
    // 2. Parsear el ELF
    let mut elf_loader = ElfLoader::new();
    let loaded_process = elf_loader.load_elf(&elf_data)
        .map_err(|e| {
            serial_write_str(&alloc::format!("EXECVE: Error parseando ELF: {}\n", e));
            ExecveError::InvalidElf
        })?;
    
    serial_write_str(&alloc::format!(
        "EXECVE: ELF parseado, entry point: 0x{:016x}\n",
        loaded_process.entry_point
    ));
    
    // 3. Obtener el proceso actual
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.ok_or(ExecveError::InvalidPath)?;
        
        serial_write_str(&alloc::format!(
            "EXECVE: Reemplazando proceso {} con nuevo binario\n",
            current_pid
        ));
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // 4. Limpiar estado anterior del proceso
            // (En un sistema real, aquí se liberaría la memoria vieja)
            
            // 5. Configurar nuevo contexto de ejecución
            let new_context = prepare_initial_context(
                loaded_process.entry_point,
                loaded_process.stack_pointer,
                false, // userland
            );
            
            process.cpu_context = new_context;
            
            // 6. Configurar argc/argv/envp
            // TODO: Copiar argumentos al stack
            process.argc = argv.len() as u32;
            
            serial_write_str(&alloc::format!(
                "EXECVE: Proceso {} configurado para ejecutar en 0x{:016x}\n",
                current_pid,
                loaded_process.entry_point
            ));
            
            // 7. Saltar al nuevo programa
            // En un sistema real, esto haría un context switch inmediato
            // Por ahora, el proceso ejecutará en el próximo time slice
            
            drop(manager_guard); // Liberar lock
            
            serial_write_str("EXECVE: Proceso listo para ejecutar\n");
            
            // execve() no retorna si tiene éxito
            // Aquí deberíamos hacer un context switch inmediato
            // Por ahora, retornamos Ok y el timer hará el switch
            
            Ok(())
        } else {
            Err(ExecveError::InvalidPath)
        }
    } else {
        Err(ExecveError::InvalidPath)
    }
}

/// Wrapper para llamada desde syscall
pub fn execve_syscall(
    filename_ptr: *const u8,
    argv_ptr: *const *const u8,
    envp_ptr: *const *const u8,
) -> ExecveResult {
    // Leer el path del filename
    let mut path_bytes = [0u8; 256];
    let mut path_len = 0;
    
    if filename_ptr.is_null() {
        return Err(ExecveError::InvalidPath);
    }
    
    unsafe {
        for i in 0..256 {
            let byte = *filename_ptr.add(i);
            if byte == 0 {
                break;
            }
            path_bytes[i] = byte;
            path_len = i + 1;
        }
    }
    
    let path = core::str::from_utf8(&path_bytes[..path_len])
        .map_err(|_| ExecveError::InvalidPath)?;
    
    // TODO: Parsear argv y envp
    // Por ahora, usamos arrays vacíos
    let argv: &[&str] = &[];
    let envp: &[&str] = &[];
    
    do_execve(path, argv, envp)
}

