//! Syscalls relacionadas con archivos
//! 
//! Este módulo implementa las syscalls para operaciones de archivos y directorios.

use crate::debug::serial_write_str;
use super::{SyscallArgs, SyscallResult, SyscallError};
use super::types::*;

/// Tabla de descriptores de archivo
pub struct FileDescriptorTable {
    entries: [Option<FileDescriptorEntry>; 256],
    next_fd: i32,
}

/// Entrada en la tabla de descriptores de archivo
#[derive(Debug, Clone, Copy)]
pub struct FileDescriptorEntry {
    pub fd: i32,
    pub file_type: FileType,
    pub flags: OpenFlags,
    pub offset: u64,
    pub inode: u64,
}

/// Tipo de archivo
#[derive(Debug, Clone, Copy)]
pub enum FileType {
    Regular,
    Directory,
    Character,
    Block,
    Pipe,
    Socket,
    SymbolicLink,
}

impl FileDescriptorTable {
    /// Crear nueva tabla de descriptores
    pub fn new() -> Self {
        Self {
            entries: [None; 256],
            next_fd: 3, // Empezar después de stdin, stdout, stderr
        }
    }

    /// Abrir un archivo
    pub fn open(&mut self, path: &str, flags: OpenFlags, mode: FileMode) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: Abriendo archivo '{}'\n", path));

        // Buscar un descriptor libre
        for i in 3..256 {
            if self.entries[i].is_none() {
                let entry = FileDescriptorEntry {
                    fd: i as i32,
                    file_type: FileType::Regular, // Por defecto
                    flags,
                    offset: 0,
                    inode: i as u64, // Simulado
                };

                self.entries[i] = Some(entry);
                serial_write_str(&alloc::format!("FILE_SYSCALL: Archivo abierto con fd={}\n", i));
                return SyscallResult::Success(i as u64);
            }
        }

        SyscallResult::Error(SyscallError::TooManyOpenFiles)
    }

    /// Cerrar un descriptor de archivo
    pub fn close(&mut self, fd: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: Cerrando fd={}\n", fd));

        if fd < 0 || fd >= 256 {
            return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
        }

        if self.entries[fd as usize].is_some() {
            self.entries[fd as usize] = None;
            serial_write_str(&alloc::format!("FILE_SYSCALL: fd={} cerrado exitosamente\n", fd));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
        }
    }

    /// Leer de un descriptor de archivo
    pub fn read(&mut self, fd: i32, buf: &mut [u8]) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: Leyendo de fd={}, {} bytes\n", fd, buf.len()));

        if fd < 0 || fd >= 256 {
            return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
        }

        if let Some(entry) = &mut self.entries[fd as usize] {
            // Simular lectura
            let bytes_read = if fd == STDIN_FD {
                // Para stdin, simular entrada vacía
                0
            } else {
                // Para otros archivos, simular lectura de datos
                let to_read = buf.len().min(512);
                buf[..to_read].fill(0x41); // Llenar con 'A'
                to_read
            };

            entry.offset += bytes_read as u64;
            serial_write_str(&alloc::format!("FILE_SYSCALL: Leídos {} bytes de fd={}\n", bytes_read, fd));
            SyscallResult::Success(bytes_read as u64)
        } else {
            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
        }
    }

    /// Escribir a un descriptor de archivo
    pub fn write(&mut self, fd: i32, buf: &[u8]) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: Escribiendo {} bytes a fd={}\n", buf.len(), fd));

        if fd < 0 || fd >= 256 {
            return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
        }

        if let Some(entry) = &mut self.entries[fd as usize] {
            let bytes_written = if fd == STDOUT_FD || fd == STDERR_FD {
                // Para stdout/stderr, escribir a serial
                for &byte in buf {
                    if byte != 0 {
                        serial_write_str(&alloc::format!("{}", byte as char));
                    }
                }
                buf.len()
            } else {
                // Para otros archivos, simular escritura
                buf.len()
            };

            entry.offset += bytes_written as u64;
            serial_write_str(&alloc::format!("FILE_SYSCALL: Escritos {} bytes a fd={}\n", bytes_written, fd));
            SyscallResult::Success(bytes_written as u64)
        } else {
            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
        }
    }

    /// Buscar en un archivo
    pub fn lseek(&mut self, fd: i32, offset: i64, whence: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: lseek fd={}, offset={}, whence={}\n", fd, offset, whence));

        if fd < 0 || fd >= 256 {
            return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
        }

        if let Some(entry) = &mut self.entries[fd as usize] {
            let new_offset = match whence {
                0 => offset as u64, // SEEK_SET
                1 => entry.offset.wrapping_add(offset as u64), // SEEK_CUR
                2 => entry.offset.wrapping_add(offset as u64), // SEEK_END (simulado)
                _ => return SyscallResult::Error(SyscallError::InvalidArgument),
            };

            entry.offset = new_offset;
            serial_write_str(&alloc::format!("FILE_SYSCALL: Offset cambiado a {}\n", new_offset));
            SyscallResult::Success(new_offset)
        } else {
            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
        }
    }

    /// Obtener información de archivo
    pub fn fstat(&self, fd: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: fstat fd={}\n", fd));

        if fd < 0 || fd >= 256 {
            return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
        }

        if let Some(_entry) = &self.entries[fd as usize] {
            // Crear información de archivo simulada
            let mut info = FileInfo::new();
            info.st_mode = S_IFREG | S_IRUSR | S_IWUSR | S_IRGRP | S_IROTH;
            info.st_size = 1024; // Tamaño simulado
            info.st_blocks = 2;
            info.st_blksize = 512;

            // TODO: Escribir a buffer de usuario
            serial_write_str("FILE_SYSCALL: Información de archivo obtenida\n");
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
        }
    }

    /// Duplicar descriptor de archivo
    pub fn dup(&mut self, oldfd: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: dup oldfd={}\n", oldfd));

        if oldfd < 0 || oldfd >= 256 {
            return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
        }

        if let Some(entry) = &self.entries[oldfd as usize] {
            // Buscar un descriptor libre
            for i in 3..256 {
                if self.entries[i].is_none() {
                    let new_entry = FileDescriptorEntry {
                        fd: i as i32,
                        file_type: entry.file_type.clone(),
                        flags: entry.flags,
                        offset: entry.offset,
                        inode: entry.inode,
                    };

                    self.entries[i] = Some(new_entry);
                    serial_write_str(&alloc::format!("FILE_SYSCALL: fd duplicado: {} -> {}\n", oldfd, i));
                    return SyscallResult::Success(i as u64);
                }
            }

            SyscallResult::Error(SyscallError::TooManyOpenFiles)
        } else {
            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
        }
    }

    /// Duplicar descriptor de archivo con número específico
    pub fn dup2(&mut self, oldfd: i32, newfd: i32) -> SyscallResult {
        serial_write_str(&alloc::format!("FILE_SYSCALL: dup2 oldfd={}, newfd={}\n", oldfd, newfd));

        if oldfd < 0 || oldfd >= 256 || newfd < 0 || newfd >= 256 {
            return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
        }

        if let Some(entry) = &self.entries[oldfd as usize] {
            let new_entry = FileDescriptorEntry {
                fd: newfd,
                file_type: entry.file_type.clone(),
                flags: entry.flags,
                offset: entry.offset,
                inode: entry.inode,
            };

            self.entries[newfd as usize] = Some(new_entry);
            serial_write_str(&alloc::format!("FILE_SYSCALL: fd duplicado: {} -> {}\n", oldfd, newfd));
            SyscallResult::Success(newfd as u64)
        } else {
            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
        }
    }

    /// Verificar si un descriptor es válido
    pub fn is_valid_fd(&self, fd: i32) -> bool {
        fd >= 0 && fd < 256 && self.entries[fd as usize].is_some()
    }

    /// Obtener entrada de descriptor
    pub fn get_entry(&self, fd: i32) -> Option<&FileDescriptorEntry> {
        if fd >= 0 && fd < 256 {
            self.entries[fd as usize].as_ref()
        } else {
            None
        }
    }

    /// Obtener entrada mutable de descriptor
    pub fn get_entry_mut(&mut self, fd: i32) -> Option<&mut FileDescriptorEntry> {
        if fd >= 0 && fd < 256 {
            self.entries[fd as usize].as_mut()
        } else {
            None
        }
    }
}

// Tabla global de descriptores de archivo
static mut FD_TABLE: Option<FileDescriptorTable> = None;

/// Inicializar la tabla de descriptores de archivo
pub fn init_fd_table() {
    unsafe {
        FD_TABLE = Some(FileDescriptorTable::new());
        serial_write_str("FILE_SYSCALL: Tabla de descriptores de archivo inicializada\n");
    }
}

/// Obtener referencia a la tabla de descriptores
pub fn get_fd_table() -> &'static mut FileDescriptorTable {
    unsafe {
        FD_TABLE.as_mut().expect("Tabla de descriptores no inicializada")
    }
}

/// Syscall open implementada
pub fn sys_open_impl(path: &str, flags: i32, mode: u32) -> SyscallResult {
    let open_flags = OpenFlags::from_bits(flags);
    get_fd_table().open(path, open_flags, mode)
}

/// Syscall close implementada
pub fn sys_close_impl(fd: i32) -> SyscallResult {
    get_fd_table().close(fd)
}

/// Syscall read implementada
pub fn sys_read_impl(fd: i32, buf: &mut [u8]) -> SyscallResult {
    get_fd_table().read(fd, buf)
}

/// Syscall write implementada
pub fn sys_write_impl(fd: i32, buf: &[u8]) -> SyscallResult {
    get_fd_table().write(fd, buf)
}

/// Syscall lseek implementada
pub fn sys_lseek_impl(fd: i32, offset: i64, whence: i32) -> SyscallResult {
    get_fd_table().lseek(fd, offset, whence)
}

/// Syscall fstat implementada
pub fn sys_fstat_impl(fd: i32) -> SyscallResult {
    get_fd_table().fstat(fd)
}

/// Syscall dup implementada
pub fn sys_dup_impl(oldfd: i32) -> SyscallResult {
    get_fd_table().dup(oldfd)
}

/// Syscall dup2 implementada
pub fn sys_dup2_impl(oldfd: i32, newfd: i32) -> SyscallResult {
    get_fd_table().dup2(oldfd, newfd)
}

/// Crear pipe
pub fn sys_pipe_impl(pipefd: *mut i32) -> SyscallResult {
    serial_write_str("FILE_SYSCALL: Creando pipe\n");

    // TODO: Implementar pipes reales
    // Por ahora simular
    unsafe {
        if !pipefd.is_null() {
            *pipefd = 3; // read end
            *(pipefd.add(1)) = 4; // write end
        }
    }

    serial_write_str("FILE_SYSCALL: Pipe creado (simulado)\n");
    SyscallResult::Success(0)
}

/// Cambiar permisos de archivo
pub fn sys_chmod_impl(path: &str, mode: u32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: chmod '{}' mode={:o}\n", path, mode));
    
    // TODO: Implementar cambio de permisos real
    SyscallResult::Success(0)
}

/// Cambiar propietario de archivo
pub fn sys_chown_impl(path: &str, owner: u32, group: u32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: chown '{}' owner={} group={}\n", path, owner, group));
    
    // TODO: Implementar cambio de propietario real
    SyscallResult::Success(0)
}

/// Crear directorio
pub fn sys_mkdir_impl(path: &str, mode: u32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: mkdir '{}' mode={:o}\n", path, mode));
    
    // TODO: Implementar creación de directorio real
    SyscallResult::Success(0)
}

/// Eliminar directorio
pub fn sys_rmdir_impl(path: &str) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: rmdir '{}'\n", path));
    
    // TODO: Implementar eliminación de directorio real
    SyscallResult::Success(0)
}

/// Eliminar archivo
pub fn sys_unlink_impl(path: &str) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: unlink '{}'\n", path));
    
    // TODO: Implementar eliminación de archivo real
    SyscallResult::Success(0)
}

/// Crear enlace simbólico
pub fn sys_symlink_impl(target: &str, linkpath: &str) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: symlink '{}' -> '{}'\n", target, linkpath));
    
    // TODO: Implementar creación de enlace simbólico real
    SyscallResult::Success(0)
}

/// Leer enlace simbólico
pub fn sys_readlink_impl(path: &str, buf: &mut [u8]) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: readlink '{}'\n", path));
    
    // TODO: Implementar lectura de enlace simbólico real
    SyscallResult::Success(0)
}

/// Obtener información de archivo
pub fn sys_stat_impl(path: &str) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: stat '{}'\n", path));
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
}

/// Obtener información de enlace simbólico
pub fn sys_lstat_impl(path: &str) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: lstat '{}'\n", path));
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
}

/// Control de descriptor de archivo
pub fn sys_fcntl_impl(fd: i32, cmd: i32, arg: u64) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: fcntl fd={}, cmd={}\n", fd, cmd));
    
    // TODO: Implementar control real
    SyscallResult::Success(0)
}

/// Bloquear archivo
pub fn sys_flock_impl(fd: i32, operation: i32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: flock fd={}, operation={}\n", fd, operation));
    
    // TODO: Implementar bloqueo real
    SyscallResult::Success(0)
}

/// Sincronizar archivo
pub fn sys_fsync_impl(fd: i32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: fsync fd={}\n", fd));
    
    // TODO: Implementar sincronización real
    SyscallResult::Success(0)
}

/// Sincronizar datos de archivo
pub fn sys_fdatasync_impl(fd: i32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: fdatasync fd={}\n", fd));
    
    // TODO: Implementar sincronización real
    SyscallResult::Success(0)
}

/// Truncar archivo
pub fn sys_truncate_impl(path: &str, length: i64) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: truncate '{}' length={}\n", path, length));
    
    // TODO: Implementar truncamiento real
    SyscallResult::Success(0)
}

/// Truncar archivo por fd
pub fn sys_ftruncate_impl(fd: i32, length: i64) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: ftruncate fd={}, length={}\n", fd, length));
    
    // TODO: Implementar truncamiento real
    SyscallResult::Success(0)
}

/// Obtener entradas de directorio
pub fn sys_getdents_impl(fd: i32, buf: &mut [u8]) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: getdents fd={}, {} bytes\n", fd, buf.len()));
    
    // TODO: Implementar obtención de entradas real
    SyscallResult::Success(0)
}

/// Cambiar directorio de trabajo
pub fn sys_chdir_impl(path: &str) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: chdir '{}'\n", path));
    
    // TODO: Implementar cambio de directorio real
    SyscallResult::Success(0)
}

/// Cambiar directorio de trabajo por fd
pub fn sys_fchdir_impl(fd: i32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: fchdir fd={}\n", fd));
    
    // TODO: Implementar cambio de directorio real
    SyscallResult::Success(0)
}

/// Obtener directorio de trabajo actual
pub fn sys_getcwd_impl(buf: &mut [u8]) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: getcwd {} bytes\n", buf.len()));
    
    // TODO: Implementar obtención real
    let cwd = b"/";
    let to_copy = buf.len().min(cwd.len());
    buf[..to_copy].copy_from_slice(&cwd[..to_copy]);
    
    SyscallResult::Success(to_copy as u64)
}

/// Establecer máscara de creación de archivos
pub fn sys_umask_impl(mask: u32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: umask {:o}\n", mask));
    
    // TODO: Implementar máscara real
    SyscallResult::Success(0)
}

/// Verificar acceso a archivo
pub fn sys_access_impl(path: &str, mode: i32) -> SyscallResult {
    serial_write_str(&alloc::format!("FILE_SYSCALL: access '{}' mode={}\n", path, mode));
    
    // TODO: Implementar verificación real
    SyscallResult::Success(0)
}
