//! Tabla de descriptores de archivo para procesos
//!
//! Este módulo maneja los file descriptors (fd) para cada proceso,
//! incluyendo stdin, stdout, stderr y archivos abiertos.

use alloc::string::String;
use core::fmt;
use super::pipe::PipeEnd;

/// Número máximo de file descriptors por proceso
pub const MAX_FDS_PER_PROCESS: usize = 256;

/// Descriptores estándar
pub const STDIN_FD: i32 = 0;
pub const STDOUT_FD: i32 = 1;
pub const STDERR_FD: i32 = 2;

/// Tipo de file descriptor
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FileDescriptorType {
    /// Entrada estándar (teclado)
    Stdin,
    /// Salida estándar (pantalla/framebuffer)
    Stdout,
    /// Error estándar (pantalla/framebuffer)
    Stderr,
    /// Archivo regular
    File,
    /// Directorio
    Directory,
    /// Pipe
    Pipe,
    /// Socket
    Socket,
    /// Dispositivo de caracteres
    CharDevice,
    /// Dispositivo de bloques
    BlockDevice,
}

/// Entrada de file descriptor
#[derive(Clone)]
pub struct FileDescriptor {
    /// Tipo de descriptor
    pub fd_type: FileDescriptorType,
    /// Path del archivo (si aplica)
    pub path: Option<String>,
    /// Posición actual en el archivo
    pub offset: u64,
    /// Flags de apertura
    pub flags: i32,
    /// Modo de archivo
    pub mode: u32,
    /// Referencia al inodo (si es archivo)
    pub inode: Option<u64>,
    /// Tamaño del archivo
    pub size: u64,
    /// Pipe end (si es pipe)
    pub pipe_end: Option<PipeEnd>,
}

impl FileDescriptor {
    /// Crear nuevo file descriptor
    pub fn new(fd_type: FileDescriptorType) -> Self {
        Self {
            fd_type,
            path: None,
            offset: 0,
            flags: 0,
            mode: 0,
            inode: None,
            size: 0,
            pipe_end: None,
        }
    }
    
    /// Crear file descriptor para pipe
    pub fn from_pipe(pipe_end: PipeEnd) -> Self {
        use super::pipe::PipeEndType;
        
        let (fd_type, flags) = match pipe_end.get_type() {
            PipeEndType::Read => (FileDescriptorType::Pipe, 0), // O_RDONLY
            PipeEndType::Write => (FileDescriptorType::Pipe, 1), // O_WRONLY
        };
        
        Self {
            fd_type,
            path: Some(String::from("[pipe]")),
            offset: 0,
            flags,
            mode: 0,
            inode: None,
            size: 0,
            pipe_end: Some(pipe_end),
        }
    }

    /// Crear stdin
    pub fn stdin() -> Self {
        Self {
            fd_type: FileDescriptorType::Stdin,
            path: Some(String::from("/dev/stdin")),
            offset: 0,
            flags: 0, // O_RDONLY
            mode: 0,
            inode: None,
            size: 0,
            pipe_end: None,
        }
    }

    /// Crear stdout
    pub fn stdout() -> Self {
        Self {
            fd_type: FileDescriptorType::Stdout,
            path: Some(String::from("/dev/stdout")),
            offset: 0,
            flags: 1, // O_WRONLY
            mode: 0,
            inode: None,
            size: 0,
            pipe_end: None,
        }
    }

    /// Crear stderr
    pub fn stderr() -> Self {
        Self {
            fd_type: FileDescriptorType::Stderr,
            path: Some(String::from("/dev/stderr")),
            offset: 0,
            flags: 1, // O_WRONLY
            mode: 0,
            inode: None,
            size: 0,
            pipe_end: None,
        }
    }
}

impl fmt::Debug for FileDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileDescriptor")
            .field("fd_type", &self.fd_type)
            .field("path", &self.path)
            .field("offset", &self.offset)
            .field("flags", &self.flags)
            .finish()
    }
}

/// Tabla de file descriptors para un proceso
#[derive(Clone)]
pub struct FileDescriptorTable {
    /// Array de file descriptors
    pub fds: [Option<FileDescriptor>; MAX_FDS_PER_PROCESS],
    /// Contador de descriptores abiertos
    pub open_count: usize,
}

impl FileDescriptorTable {
    /// Crear nueva tabla de file descriptors
    pub fn new() -> Self {
        // Crear array con valores None
        const INIT: Option<FileDescriptor> = None;
        let mut fds = [INIT; MAX_FDS_PER_PROCESS];

        // Inicializar descriptores estándar
        fds[STDIN_FD as usize] = Some(FileDescriptor::stdin());
        fds[STDOUT_FD as usize] = Some(FileDescriptor::stdout());
        fds[STDERR_FD as usize] = Some(FileDescriptor::stderr());

        Self {
            fds,
            open_count: 3, // stdin, stdout, stderr
        }
    }

    /// Obtener file descriptor por número
    pub fn get(&self, fd: i32) -> Option<&FileDescriptor> {
        if fd < 0 || fd >= MAX_FDS_PER_PROCESS as i32 {
            return None;
        }
        self.fds[fd as usize].as_ref()
    }

    /// Obtener file descriptor mutable por número
    pub fn get_mut(&mut self, fd: i32) -> Option<&mut FileDescriptor> {
        if fd < 0 || fd >= MAX_FDS_PER_PROCESS as i32 {
            return None;
        }
        self.fds[fd as usize].as_mut()
    }

    /// Asignar nuevo file descriptor
    pub fn allocate(&mut self, fd: FileDescriptor) -> Result<i32, &'static str> {
        // Buscar primer slot disponible (después de los descriptores estándar)
        for (i, slot) in self.fds.iter_mut().enumerate().skip(3) {
            if slot.is_none() {
                *slot = Some(fd);
                self.open_count += 1;
                return Ok(i as i32);
            }
        }
        Err("No hay file descriptors disponibles")
    }

    /// Cerrar file descriptor
    pub fn close(&mut self, fd: i32) -> Result<(), &'static str> {
        if fd < 3 {
            return Err("No se pueden cerrar descriptores estándar");
        }
        if fd < 0 || fd >= MAX_FDS_PER_PROCESS as i32 {
            return Err("File descriptor inválido");
        }

        if self.fds[fd as usize].is_some() {
            self.fds[fd as usize] = None;
            self.open_count -= 1;
            Ok(())
        } else {
            Err("File descriptor ya está cerrado")
        }
    }

    /// Duplicar file descriptor
    pub fn dup(&mut self, oldfd: i32) -> Result<i32, &'static str> {
        let fd_copy = self.get(oldfd)
            .ok_or("File descriptor inválido")?
            .clone();
        
        self.allocate(fd_copy)
    }

    /// Duplicar file descriptor con número específico
    pub fn dup2(&mut self, oldfd: i32, newfd: i32) -> Result<i32, &'static str> {
        if oldfd == newfd {
            return Ok(newfd);
        }

        let fd_copy = self.get(oldfd)
            .ok_or("File descriptor inválido")?
            .clone();
        
        // Cerrar newfd si está abierto
        if self.fds[newfd as usize].is_some() {
            let _ = self.close(newfd);
        }

        self.fds[newfd as usize] = Some(fd_copy);
        self.open_count += 1;
        Ok(newfd)
    }

    /// Obtener número de descriptores abiertos
    pub fn open_count(&self) -> usize {
        self.open_count
    }
}

impl fmt::Debug for FileDescriptorTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileDescriptorTable")
            .field("open_count", &self.open_count)
            .finish()
    }
}

impl Default for FileDescriptorTable {
    fn default() -> Self {
        Self::new()
    }
}

