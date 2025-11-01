//! Sistema de syscalls para Eclipse OS
//! 
//! Este módulo implementa el sistema de llamadas al sistema (syscalls)
//! que permite a las aplicaciones de usuario interactuar con el kernel.

pub mod handler;
pub mod types;
pub mod file;
pub mod memory;
pub mod process;
pub mod io;
pub mod usb;
pub mod execve;

use crate::debug::serial_write_str;
use spin::Mutex;
use alloc::string::String;

/// Número de syscalls implementadas
pub const SYSCALL_COUNT: usize = 67;

/// Registro de syscalls disponibles
pub struct SyscallRegistry {
    handlers: [Option<fn(&SyscallArgs) -> SyscallResult>; SYSCALL_COUNT],
}

impl SyscallRegistry {
    /// Crear un nuevo registro de syscalls
    pub fn new() -> Self {
        Self {
            handlers: [None; SYSCALL_COUNT],
        }
    }

    /// Registrar un manejador de syscall
    pub fn register(&mut self, syscall_num: usize, handler: fn(&SyscallArgs) -> SyscallResult) {
        if syscall_num < SYSCALL_COUNT {
            self.handlers[syscall_num] = Some(handler);
            serial_write_str(&alloc::format!("SYSCALL: Registrado syscall {}\n", syscall_num));
        }
    }

    /// Ejecutar un syscall
    pub fn execute(&self, syscall_num: usize, args: &SyscallArgs) -> SyscallResult {
        if syscall_num >= SYSCALL_COUNT {
            serial_write_str(&alloc::format!("SYSCALL: Número de syscall inválido: {}\n", syscall_num));
            return SyscallResult::Error(SyscallError::InvalidSyscall);
        }

        if let Some(handler) = self.handlers[syscall_num] {
            serial_write_str(&alloc::format!("SYSCALL: Ejecutando syscall {}\n", syscall_num));
            handler(args)
        } else {
            serial_write_str(&alloc::format!("SYSCALL: Syscall {} no implementado\n", syscall_num));
            SyscallResult::Error(SyscallError::NotImplemented)
        }
    }
}

/// Argumentos de un syscall
#[derive(Debug, Clone)]
pub struct SyscallArgs {
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    pub arg5: u64,
}

impl SyscallArgs {
    /// Crear argumentos de syscall desde registros
    pub fn from_registers(rdi: u64, rsi: u64, rdx: u64, rcx: u64, r8: u64, r9: u64) -> Self {
        Self {
            arg0: rdi,
            arg1: rsi,
            arg2: rdx,
            arg3: rcx,
            arg4: r8,
            arg5: r9,
        }
    }
}

/// Resultado de un syscall
#[derive(Debug, Clone)]
pub enum SyscallResult {
    Success(u64),
    Error(SyscallError),
}

/// Errores de syscall
#[derive(Debug, Clone)]
pub enum SyscallError {
    InvalidSyscall,
    NotImplemented,
    InvalidArgument,
    PermissionDenied,
    FileNotFound,
    OutOfMemory,
    DeviceError,
    Interrupted,
    InvalidFileDescriptor,
    BadAddress,
    FileExists,
    NotADirectory,
    IsADirectory,
    NoSpaceLeft,
    TooManyOpenFiles,
    InvalidOperation,
    AccessDenied,
}

impl SyscallError {
    /// Convertir a código de error estándar
    pub fn to_errno(&self) -> i64 {
        match self {
            SyscallError::InvalidSyscall => -1,
            SyscallError::NotImplemented => -38, // ENOSYS
            SyscallError::InvalidArgument => -22, // EINVAL
            SyscallError::PermissionDenied => -1, // EPERM
            SyscallError::FileNotFound => -2, // ENOENT
            SyscallError::OutOfMemory => -12, // ENOMEM
            SyscallError::DeviceError => -5, // EIO
            SyscallError::Interrupted => -4, // EINTR
            SyscallError::InvalidFileDescriptor => -9, // EBADF
            SyscallError::BadAddress => -14, // EFAULT
            SyscallError::FileExists => -17, // EEXIST
            SyscallError::NotADirectory => -20, // ENOTDIR
            SyscallError::IsADirectory => -21, // EISDIR
            SyscallError::NoSpaceLeft => -28, // ENOSPC
            SyscallError::TooManyOpenFiles => -24, // EMFILE
            SyscallError::InvalidOperation => -95, // EOPNOTSUPP
            SyscallError::AccessDenied => -13, // EACCES
        }
    }
}

/// Registro global de syscalls
static SYSCALL_REGISTRY: Mutex<Option<SyscallRegistry>> = Mutex::new(None);

/// Obtener el registro global de syscalls
pub fn get_syscall_registry() -> &'static Mutex<Option<SyscallRegistry>> {
    &SYSCALL_REGISTRY
}

/// Inicializar el sistema de syscalls
pub fn init_syscalls() -> SyscallRegistry {
    serial_write_str("SYSCALL: Inicializando sistema de syscalls\n");
    
    let mut registry = SyscallRegistry::new();
    
    // Registrar syscalls básicas
    registry.register(0, sys_exit);
    registry.register(1, sys_write);
    registry.register(2, sys_open);
    registry.register(3, sys_close);
    registry.register(4, sys_read);
    registry.register(5, sys_lseek);
    registry.register(6, sys_ioctl);
    registry.register(7, sys_access);
    registry.register(8, sys_kill);
    registry.register(9, sys_getpid);
    registry.register(10, sys_dup);
    registry.register(11, sys_getppid);
    registry.register(12, sys_dup2);
    registry.register(13, sys_pipe);
    registry.register(14, sys_alarm);
    registry.register(15, sys_brk);
    registry.register(16, sys_mmap);
    registry.register(17, sys_munmap);
    registry.register(18, sys_mprotect);
    registry.register(19, sys_msync);
    registry.register(20, sys_madvise);
    registry.register(21, sys_shmget);
    registry.register(22, sys_shmat);
    registry.register(23, sys_shmdt);
    registry.register(24, sys_fork);
    registry.register(25, sys_execve);
    registry.register(26, sys_wait4);
    registry.register(27, sys_nanosleep);
    registry.register(28, sys_gettimeofday);
    registry.register(29, sys_getrusage);
    registry.register(30, sys_sysinfo);
    registry.register(31, sys_getuid);
    registry.register(32, sys_getgid);
    registry.register(33, sys_setuid);
    registry.register(34, sys_setgid);
    registry.register(35, sys_geteuid);
    registry.register(36, sys_getegid);
    registry.register(37, sys_setreuid);
    registry.register(38, sys_setregid);
    registry.register(39, sys_chdir);
    registry.register(40, sys_fchdir);
    registry.register(41, sys_mkdir);
    registry.register(42, sys_rmdir);
    registry.register(43, sys_unlink);
    registry.register(44, sys_symlink);
    registry.register(45, sys_readlink);
    registry.register(46, sys_chmod);
    registry.register(47, sys_fchmod);
    registry.register(48, sys_chown);
    registry.register(49, sys_fchown);
    registry.register(50, sys_lchown);
    registry.register(51, sys_stat);
    registry.register(52, sys_lstat);
    registry.register(53, sys_fstat);
    registry.register(54, sys_statfs);
    registry.register(55, sys_fstatfs);
    registry.register(56, sys_getdents);
    registry.register(57, sys_fcntl);
    registry.register(58, sys_flock);
    registry.register(59, sys_fsync);
    registry.register(60, sys_fdatasync);
    registry.register(61, sys_truncate);
    registry.register(62, sys_ftruncate);
    registry.register(63, sys_umask);
    registry.register(64, sys_getcwd);
    registry.register(65, sys_getenv);
    registry.register(66, sys_setenv);
    
    serial_write_str("SYSCALL: Sistema de syscalls inicializado\n");
    
    // Guardar en el registro global
    *SYSCALL_REGISTRY.lock() = Some(registry.clone());
    
    registry
}

impl Clone for SyscallRegistry {
    fn clone(&self) -> Self {
        Self {
            handlers: self.handlers.clone(),
        }
    }
}

// Syscalls básicas (implementaciones mínimas por ahora)

/// Syscall exit - Terminar proceso
fn sys_exit(args: &SyscallArgs) -> SyscallResult {
    let exit_code = args.arg0 as i32;
    serial_write_str(&alloc::format!("SYSCALL: exit({})\n", exit_code));
    
    // Implementación real de exit
    // En un sistema completo, esto:
    // 1. Marcaría el proceso como terminado
    // 2. Liberaría recursos
    // 3. Notificaría al padre (SIGCHLD)
    // 4. Haría context switch a otro proceso
    
    // Por ahora solo logueamos y marcamos como terminado
    serial_write_str(&alloc::format!("PROCESO: Terminado con código {}\n", exit_code));
    
    // TODO: Marcar proceso como zombie y hacer context switch
    // Por ahora, esta syscall "exitosa" hace que el proceso termine
    SyscallResult::Success(exit_code as u64)
}

/// Syscall write - Escribir a descriptor de archivo
fn sys_write(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let buf = args.arg1 as *const u8;
    let count = args.arg2 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: write(fd={}, count={})\n", fd, count));
    
    // Validar puntero
    if buf.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    use crate::process::manager::get_process_manager;
    use crate::process::file_descriptor::FileDescriptorType;
    
    // Obtener el proceso actual
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            let fd_info = process.fd_table.get(fd);
            
            if let Some(fd_desc) = fd_info {
                match fd_desc.fd_type {
                    FileDescriptorType::Stdout | FileDescriptorType::Stderr => {
                        drop(manager_guard); // Liberar lock
                        
                        let data = unsafe { core::slice::from_raw_parts(buf, count) };
                        
                        if let Ok(text) = core::str::from_utf8(data) {
                            serial_write_str(text);
                            SyscallResult::Success(count as u64)
                        } else {
                            // Escribir bytes raw
                            for &byte in data {
                                unsafe {
                                    use core::arch::asm;
                                    asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
                                }
                            }
                            SyscallResult::Success(count as u64)
                        }
                    }
                    FileDescriptorType::Pipe => {
                        // Escribir a pipe
                        if let Some(ref pipe_end) = fd_desc.pipe_end {
                            let pipe_clone = pipe_end.clone();
                            drop(manager_guard);
                            
                            let data = unsafe { core::slice::from_raw_parts(buf, count) };
                            
                            match pipe_clone.write(data) {
                                Ok(bytes_written) => {
                                    serial_write_str(&alloc::format!("SYSCALL: write(pipe) - {} bytes\n", bytes_written));
                                    SyscallResult::Success(bytes_written as u64)
                                }
                                Err(e) => {
                                    serial_write_str(&alloc::format!("SYSCALL: write(pipe) error: {}\n", e));
                                    SyscallResult::Error(SyscallError::DeviceError)
                                }
                            }
                        } else {
                            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
                        }
                    }
                    _ => {
                        serial_write_str(&alloc::format!("SYSCALL: write() - fd {} tipo no soportado\n", fd));
                        SyscallResult::Error(SyscallError::InvalidFileDescriptor)
                    }
                }
            } else {
                SyscallResult::Error(SyscallError::InvalidFileDescriptor)
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall open - Abrir archivo
fn sys_open(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let flags = args.arg1 as i32;
    let mode = args.arg2 as u32;
    
    // Convertir puntero a string
    if pathname.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    let path_str = unsafe {
        let mut len = 0;
        while *pathname.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(pathname, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: open('{}', flags=0x{:x}, mode=0x{:o})\n", path_str, flags, mode));
    
    use crate::process::manager::get_process_manager;
    use crate::process::file_descriptor::{FileDescriptor, FileDescriptorType};
    use crate::filesystem::vfs::get_vfs;
    
    // Obtener proceso actual
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // Liberar lock antes de acceder VFS
            drop(manager_guard);
            
            // Intentar resolver el path en el VFS
            let vfs_guard = get_vfs();
            if let Some(ref vfs) = *vfs_guard {
                if let Some(root_fs) = vfs.get_root_fs() {
                    let fs_guard = root_fs.lock();
                    
                    // Intentar resolver el path
                    match fs_guard.resolve_path(path_str) {
                        Ok(inode) => {
                            // Obtener metadata del archivo
                            match fs_guard.stat(inode) {
                                Ok(stat_info) => {
                                    drop(fs_guard);
                                    drop(vfs_guard);
                                    
                                    // Re-adquirir lock del process manager
                                    let mut manager_guard = get_process_manager().lock();
                                    if let Some(ref mut manager) = *manager_guard {
                                        if let Some(ref mut process) = manager.processes[current_pid as usize] {
                                            // Crear file descriptor
                                            let fd = FileDescriptor {
                                                fd_type: FileDescriptorType::File,
                                                path: Some(String::from(path_str)),
                                                offset: 0,
                                                flags,
                                                mode,
                                                inode: Some(inode as u64),
                                                size: stat_info.size,
                                                pipe_end: None,
                                            };
                                            
                                            // Asignar FD
                                            match process.fd_table.allocate(fd) {
                                                Ok(fd_num) => {
                                                    serial_write_str(&alloc::format!(
                                                        "SYSCALL: open() -> fd={} (inode={}, size={})\n", 
                                                        fd_num, inode, stat_info.size
                                                    ));
                                                    return SyscallResult::Success(fd_num as u64);
                                                }
                                                Err(e) => {
                                                    serial_write_str(&alloc::format!("SYSCALL: open() - FD alloc error: {}\n", e));
                                                    return SyscallResult::Error(SyscallError::TooManyOpenFiles);
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    serial_write_str(&alloc::format!("SYSCALL: open() - stat error: {:?}\n", e));
                                    return SyscallResult::Error(SyscallError::FileNotFound);
                                }
                            }
                        }
                        Err(e) => {
                            serial_write_str(&alloc::format!("SYSCALL: open('{}') - not found: {:?}\n", path_str, e));
                            return SyscallResult::Error(SyscallError::FileNotFound);
                        }
                    }
                }
            }
        }
    }
    
    serial_write_str("SYSCALL: open() - general error\n");
    SyscallResult::Error(SyscallError::InvalidOperation)
}

/// Syscall close - Cerrar descriptor de archivo
fn sys_close(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: close(fd={})\n", fd));
    
    use crate::process::manager::get_process_manager;
    
    // No permitir cerrar stdin, stdout, stderr
    if fd < 3 {
        serial_write_str(&alloc::format!("SYSCALL: close() - no se puede cerrar fd={} (std stream)\n", fd));
        return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
    }
    
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // Cerrar el file descriptor
            match process.fd_table.close(fd) {
                Ok(_) => {
                    serial_write_str(&alloc::format!("SYSCALL: close(fd={}) -> OK\n", fd));
                    SyscallResult::Success(0)
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: close(fd={}) -> ERROR: {}\n", fd, e));
                    SyscallResult::Error(SyscallError::InvalidFileDescriptor)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall read - Leer de descriptor de archivo
fn sys_read(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let buf = args.arg1 as *mut u8;
    let count = args.arg2 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: read(fd={}, count={})\n", fd, count));
    
    // Validar puntero
    if buf.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    use crate::process::manager::get_process_manager;
    use crate::process::file_descriptor::FileDescriptorType;
    
    // Obtener el proceso actual para acceder a su fd_table
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // Obtener el file descriptor
            let fd_info = process.fd_table.get(fd);
            
            if let Some(fd_desc) = fd_info {
                match fd_desc.fd_type {
                    FileDescriptorType::Stdin => {
                        // Leer desde stdin
                        drop(manager_guard); // Liberar lock antes de I/O
                        let buffer_slice = unsafe { core::slice::from_raw_parts_mut(buf, count) };
                        
                        match crate::drivers::stdin::read_stdin(buffer_slice) {
                            Ok(bytes_read) => {
                                serial_write_str(&alloc::format!("SYSCALL: read(stdin) - {} bytes\n", bytes_read));
                                SyscallResult::Success(bytes_read as u64)
                            }
                            Err(e) => {
                                serial_write_str(&alloc::format!("SYSCALL: read(stdin) error: {}\n", e));
                                SyscallResult::Error(SyscallError::DeviceError)
                            }
                        }
                    }
                    FileDescriptorType::Pipe => {
                        // Leer desde pipe
                        if let Some(ref pipe_end) = fd_desc.pipe_end {
                            let pipe_clone = pipe_end.clone();
                            drop(manager_guard); // Liberar lock
                            
                            let buffer_slice = unsafe { core::slice::from_raw_parts_mut(buf, count) };
                            
                            match pipe_clone.read(buffer_slice) {
                                Ok(bytes_read) => {
                                    serial_write_str(&alloc::format!("SYSCALL: read(pipe) - {} bytes\n", bytes_read));
                                    SyscallResult::Success(bytes_read as u64)
                                }
                                Err(e) => {
                                    serial_write_str(&alloc::format!("SYSCALL: read(pipe) error: {}\n", e));
                                    SyscallResult::Error(SyscallError::DeviceError)
                                }
                            }
                        } else {
                            SyscallResult::Error(SyscallError::InvalidFileDescriptor)
                        }
                    }
                    _ => {
                        serial_write_str(&alloc::format!("SYSCALL: read() - tipo {} no soportado\n", fd));
                        SyscallResult::Error(SyscallError::InvalidFileDescriptor)
                    }
                }
            } else {
                SyscallResult::Error(SyscallError::InvalidFileDescriptor)
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall lseek - Reposicionar offset en archivo
fn sys_lseek(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let offset = args.arg1 as i64;
    let whence = args.arg2 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: lseek(fd={}, offset={}, whence={})\n", fd, offset, whence));
    
    // TODO: Implementar lseek real
    SyscallResult::Success(0)
}

/// Syscall ioctl - Control de dispositivo
fn sys_ioctl(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let request = args.arg1 as u64;
    let argp = args.arg2 as *mut u8;
    
    serial_write_str(&alloc::format!("SYSCALL: ioctl(fd={}, request=0x{:x})\n", fd, request));
    
    // Constantes de ioctl para terminal (Linux x86_64)
    const TCGETS: u64 = 0x5401;      // Obtener atributos de terminal
    const TCSETS: u64 = 0x5402;      // Establecer atributos
    const TIOCGWINSZ: u64 = 0x5413;  // Obtener tamaño de ventana
    const TIOCSPGRP: u64 = 0x5410;   // Establecer grupo de proceso
    const TIOCGPGRP: u64 = 0x540F;   // Obtener grupo de proceso
    const FIONREAD: u64 = 0x541B;    // Bytes disponibles para leer
    
    // Para stdin/stdout/stderr, simular terminal
    if fd >= 0 && fd <= 2 {
        match request {
            TCGETS => {
                // Retornar atributos de terminal (simular terminal raw)
                serial_write_str("SYSCALL: ioctl(TCGETS) - terminal presente\n");
                // En un sistema real, llenarías la estructura termios
                // Por ahora solo retornamos éxito
                SyscallResult::Success(0)
            }
            TCSETS => {
                serial_write_str("SYSCALL: ioctl(TCSETS) - OK\n");
                SyscallResult::Success(0)
            }
            TIOCGWINSZ => {
                // Retornar tamaño de ventana (80x25 por defecto)
                serial_write_str("SYSCALL: ioctl(TIOCGWINSZ) - 80x25\n");
                if !argp.is_null() {
                    unsafe {
                        // struct winsize { unsigned short ws_row, ws_col, ws_xpixel, ws_ypixel; }
                        *(argp as *mut u16) = 25;  // rows
                        *(argp.add(2) as *mut u16) = 80; // cols
                        *(argp.add(4) as *mut u16) = 0;  // xpixel
                        *(argp.add(6) as *mut u16) = 0;  // ypixel
                    }
                }
                SyscallResult::Success(0)
            }
            TIOCSPGRP | TIOCGPGRP => {
                serial_write_str("SYSCALL: ioctl(TIOCSPGRP/TIOCGPGRP) - simulado\n");
                SyscallResult::Success(0)
            }
            FIONREAD => {
                serial_write_str("SYSCALL: ioctl(FIONREAD) - 0 bytes\n");
                if !argp.is_null() {
                    unsafe {
                        *(argp as *mut i32) = 0;
                    }
                }
                SyscallResult::Success(0)
            }
            _ => {
                serial_write_str(&alloc::format!("SYSCALL: ioctl() - request desconocida: 0x{:x}\n", request));
                // Retornar éxito para requests desconocidas (muchos programas ignoran errores)
                SyscallResult::Success(0)
            }
        }
    } else {
        // Para otros FDs, no es un terminal
        serial_write_str(&alloc::format!("SYSCALL: ioctl(fd={}) - no es terminal\n", fd));
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall access - Verificar permisos de archivo
fn sys_access(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let mode = args.arg1 as i32;
    
    if pathname.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir pathname a string
    let path_str = unsafe {
        let mut len = 0;
        while *pathname.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(pathname, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: access('{}', mode=0x{:x})\n", path_str, mode));
    
    // Constantes de access() de Linux
    const F_OK: i32 = 0; // Verificar existencia
    const X_OK: i32 = 1; // Verificar ejecución
    const W_OK: i32 = 2; // Verificar escritura
    const R_OK: i32 = 4; // Verificar lectura
    
    use crate::filesystem::vfs::get_vfs;
    
    // Verificar que el archivo existe en el VFS
    let vfs_guard = get_vfs();
    if let Some(ref vfs) = *vfs_guard {
        if let Some(root_fs) = vfs.get_root_fs() {
            let fs_guard = root_fs.lock();
            
            // Intentar resolver el path
            match fs_guard.resolve_path(path_str) {
                Ok(inode) => {
                    // Si solo verificamos existencia (F_OK), retornar éxito
                    if mode == F_OK {
                        serial_write_str(&alloc::format!("SYSCALL: access('{}') -> OK (exists)\n", path_str));
                        return SyscallResult::Success(0);
                    }
                    
                    // Obtener permisos del archivo
                    match fs_guard.stat(inode) {
                        Ok(stat_info) => {
                            // Por ahora, asumimos que el usuario tiene todos los permisos
                            // En un sistema real, verificaríamos uid/gid y permisos
                            let file_mode = stat_info.mode;
                            
                            // Verificar permisos solicitados
                            let mut has_permission = true;
                            
                            if (mode & R_OK) != 0 {
                                // Verificar lectura (bit de lectura del usuario: 0o400)
                                if (file_mode & 0o400) == 0 {
                                    has_permission = false;
                                }
                            }
                            
                            if (mode & W_OK) != 0 {
                                // Verificar escritura (bit de escritura del usuario: 0o200)
                                if (file_mode & 0o200) == 0 {
                                    has_permission = false;
                                }
                            }
                            
                            if (mode & X_OK) != 0 {
                                // Verificar ejecución (bit de ejecución del usuario: 0o100)
                                if (file_mode & 0o100) == 0 {
                                    has_permission = false;
                                }
                            }
                            
                            if has_permission {
                                serial_write_str(&alloc::format!("SYSCALL: access('{}') -> OK\n", path_str));
                                SyscallResult::Success(0)
                            } else {
                                serial_write_str(&alloc::format!("SYSCALL: access('{}') -> EACCES\n", path_str));
                                SyscallResult::Error(SyscallError::AccessDenied)
                            }
                        }
                        Err(e) => {
                            serial_write_str(&alloc::format!("SYSCALL: access() - stat error: {:?}\n", e));
                            SyscallResult::Error(SyscallError::FileNotFound)
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: access('{}') - not found: {:?}\n", path_str, e));
                    SyscallResult::Error(SyscallError::FileNotFound)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall kill - Enviar señal a proceso
fn sys_kill(args: &SyscallArgs) -> SyscallResult {
    let pid = args.arg0 as i32;
    let sig = args.arg1 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: kill(pid={}, sig={})\n", pid, sig));
    
    // Constantes de señales (Linux)
    const SIGTERM: i32 = 15;  // Terminación normal
    const SIGKILL: i32 = 9;   // Terminación forzada
    const SIGINT: i32 = 2;    // Interrupción (Ctrl+C)
    const SIGHUP: i32 = 1;    // Hangup
    const SIGCHLD: i32 = 17;  // Hijo terminado
    
    use crate::process::manager::get_process_manager;
    
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        // Validar PID (máximo 256 procesos)
        if pid <= 0 || pid >= 256 {
            return SyscallResult::Error(SyscallError::InvalidArgument);
        }
        
        // Verificar que el proceso existe
        if let Some(ref mut target_process) = manager.processes[pid as usize] {
            match sig {
                0 => {
                    // Señal 0 = solo verificar que el proceso existe
                    serial_write_str(&alloc::format!("SYSCALL: kill(pid={}, sig=0) - proceso existe\n", pid));
                    SyscallResult::Success(0)
                }
                SIGTERM | SIGKILL => {
                    // Terminar el proceso
                    serial_write_str(&alloc::format!("SYSCALL: kill(pid={}, SIGTERM/SIGKILL) - terminando proceso\n", pid));
                    target_process.pending_signals |= 1 << sig;
                    // En el futuro, el scheduler verificaría pending_signals
                    SyscallResult::Success(0)
                }
                SIGINT => {
                    // Interrupción
                    serial_write_str(&alloc::format!("SYSCALL: kill(pid={}, SIGINT) - interrumpiendo\n", pid));
                    target_process.pending_signals |= 1 << sig;
                    SyscallResult::Success(0)
                }
                _ => {
                    // Otras señales - solo registrar
                    serial_write_str(&alloc::format!("SYSCALL: kill(pid={}, sig={}) - señal registrada\n", pid, sig));
                    target_process.pending_signals |= 1 << (sig % 32);
                    SyscallResult::Success(0)
                }
            }
        } else {
            serial_write_str(&alloc::format!("SYSCALL: kill() - proceso {} no existe\n", pid));
            SyscallResult::Error(SyscallError::InvalidArgument)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall getpid - Obtener ID del proceso
fn sys_getpid(_args: &SyscallArgs) -> SyscallResult {
    use crate::process::manager::get_process_manager;
    
    let manager_guard = get_process_manager().lock();
    
    if let Some(ref manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        serial_write_str(&alloc::format!("SYSCALL: getpid() -> {}\n", current_pid));
        SyscallResult::Success(current_pid as u64)
    } else {
        serial_write_str("SYSCALL: getpid() -> 1 (fallback)\n");
        SyscallResult::Success(1)
    }
}

/// Syscall getppid - Obtener ID del proceso padre
fn sys_getppid(_args: &SyscallArgs) -> SyscallResult {
    use crate::process::manager::get_process_manager;
    
    let manager_guard = get_process_manager().lock();
    
    if let Some(ref manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref process) = manager.processes[current_pid as usize] {
            let parent_pid = process.parent_pid.unwrap_or(0);
            serial_write_str(&alloc::format!("SYSCALL: getppid() -> {} (parent of {})\n", parent_pid, current_pid));
            return SyscallResult::Success(parent_pid as u64);
        }
    }
    
    serial_write_str("SYSCALL: getppid() -> 0 (fallback)\n");
    SyscallResult::Success(0)
}

/// Syscall dup - Duplicar descriptor de archivo
fn sys_dup(args: &SyscallArgs) -> SyscallResult {
    let oldfd = args.arg0 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: dup(oldfd={})\n", oldfd));
    
    // TODO: Implementar duplicación real
    SyscallResult::Success(oldfd as u64)
}

/// Syscall dup2 - Duplicar descriptor con número específico
fn sys_dup2(args: &SyscallArgs) -> SyscallResult {
    let oldfd = args.arg0 as i32;
    let newfd = args.arg1 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: dup2(oldfd={}, newfd={})\n", oldfd, newfd));
    
    use crate::process::manager::get_process_manager;
    
    // Obtener el proceso actual
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // Duplicar el file descriptor
            match process.fd_table.dup2(oldfd, newfd) {
                Ok(fd) => {
                    serial_write_str(&alloc::format!(
                        "SYSCALL: dup2() - fd {} duplicado a {}\n",
                        oldfd, fd
                    ));
                    SyscallResult::Success(fd as u64)
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: dup2() - Error: {}\n", e));
                    SyscallResult::Error(SyscallError::InvalidFileDescriptor)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall pipe - Crear pipe
fn sys_pipe(args: &SyscallArgs) -> SyscallResult {
    let pipefd = args.arg0 as *mut i32;
    
    serial_write_str("SYSCALL: pipe()\n");
    
    // Validar puntero
    if pipefd.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    use crate::process::manager::get_process_manager;
    use crate::process::pipe::create_pipe;
    use crate::process::file_descriptor::FileDescriptor;
    
    // Crear el pipe
    let (read_end, write_end) = create_pipe();
    
    // Obtener el proceso actual
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // Crear file descriptors para los extremos del pipe
            let read_fd = FileDescriptor::from_pipe(read_end);
            let write_fd = FileDescriptor::from_pipe(write_end);
            
            // Asignar file descriptors
            match process.fd_table.allocate(read_fd) {
                Ok(read_fd_num) => {
                    match process.fd_table.allocate(write_fd) {
                        Ok(write_fd_num) => {
                            // Escribir los FDs al array pipefd
                            unsafe {
                                *pipefd.offset(0) = read_fd_num;
                                *pipefd.offset(1) = write_fd_num;
                            }
                            
                            serial_write_str(&alloc::format!(
                                "SYSCALL: pipe() - creado pipe[{}, {}]\n",
                                read_fd_num, write_fd_num
                            ));
                            
                            SyscallResult::Success(0)
                        }
                        Err(e) => {
                            serial_write_str(&alloc::format!("SYSCALL: pipe() - Error asignando write fd: {}\n", e));
                            // Limpiar read_fd
                            let _ = process.fd_table.close(read_fd_num);
                            SyscallResult::Error(SyscallError::TooManyOpenFiles)
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: pipe() - Error asignando read fd: {}\n", e));
                    SyscallResult::Error(SyscallError::TooManyOpenFiles)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall alarm - Programar alarma
fn sys_alarm(args: &SyscallArgs) -> SyscallResult {
    let seconds = args.arg0 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: alarm(seconds={})\n", seconds));
    
    // TODO: Implementar alarma real
    SyscallResult::Success(0)
}

/// Syscall brk - Cambiar tamaño del heap
fn sys_brk(args: &SyscallArgs) -> SyscallResult {
    let new_brk = args.arg0 as u64;
    
    serial_write_str(&alloc::format!("SYSCALL: brk(0x{:x})\n", new_brk));
    
    use crate::process::manager::get_process_manager;
    
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            let heap_start = process.memory_info.heap_start;
            let heap_limit = process.memory_info.heap_limit;
            let current_brk = process.memory_info.heap_break;
            
            // Si new_brk es 0, retornar el break actual
            if new_brk == 0 {
                serial_write_str(&alloc::format!("SYSCALL: brk(0) -> 0x{:x} (current)\n", current_brk));
                return SyscallResult::Success(current_brk);
            }
            
            // Validar que el nuevo break está dentro de los límites
            if new_brk < heap_start {
                serial_write_str(&alloc::format!("SYSCALL: brk() - below heap start\n"));
                return SyscallResult::Error(SyscallError::InvalidArgument);
            }
            
            if new_brk > heap_limit {
                serial_write_str(&alloc::format!("SYSCALL: brk() - above heap limit\n"));
                return SyscallResult::Error(SyscallError::OutOfMemory);
            }
            
            // Actualizar el break
            process.memory_info.heap_break = new_brk;
            
            let heap_size = new_brk - heap_start;
            serial_write_str(&alloc::format!(
                "SYSCALL: brk() -> 0x{:x} (heap size: {} bytes)\n",
                new_brk, heap_size
            ));
            
            return SyscallResult::Success(new_brk);
        }
    }
    
    SyscallResult::Error(SyscallError::InvalidOperation)
}

/// Syscall mmap - Mapear memoria
fn sys_mmap(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as u64;
    let length = args.arg1 as usize;
    let prot = args.arg2 as i32;
    let flags = args.arg3 as i32;
    let fd = args.arg4 as i32;
    let offset = args.arg5 as i64;
    
    serial_write_str(&alloc::format!(
        "SYSCALL: mmap(addr=0x{:x}, length={}, prot=0x{:x}, flags=0x{:x}, fd={}, offset={})\n",
        addr, length, prot, flags, fd, offset
    ));
    
    // Constantes de mmap
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const PROT_EXEC: i32 = 0x4;
    const MAP_SHARED: i32 = 0x01;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANONYMOUS: i32 = 0x20;
    const MAP_FIXED: i32 = 0x10;
    
    // Validar length
    if length == 0 {
        return SyscallResult::Error(SyscallError::InvalidArgument);
    }
    
    use crate::process::manager::get_process_manager;
    
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // Por ahora, solo soportamos MAP_ANONYMOUS
            if (flags & MAP_ANONYMOUS) == 0 {
                serial_write_str("SYSCALL: mmap() - solo MAP_ANONYMOUS soportado\n");
                return SyscallResult::Error(SyscallError::InvalidArgument);
            }
            
            // Asignar memoria desde el heap
            let heap_limit = process.memory_info.heap_limit;
            let current_brk = process.memory_info.heap_break;
            
            // Alinear a página (4KB)
            let aligned_length = (length + 0xFFF) & !0xFFF;
            
            // Si addr es NULL o no es MAP_FIXED, asignar desde el break
            let mapped_addr = if addr == 0 || (flags & MAP_FIXED) == 0 {
                let new_addr = current_brk;
                let new_brk = current_brk + aligned_length as u64;
                
                if new_brk > heap_limit {
                    serial_write_str("SYSCALL: mmap() - out of memory\n");
                    return SyscallResult::Error(SyscallError::OutOfMemory);
                }
                
                process.memory_info.heap_break = new_brk;
                new_addr
            } else {
                // MAP_FIXED con addr específica
                addr
            };
            
            serial_write_str(&alloc::format!(
                "SYSCALL: mmap() -> 0x{:x} ({} bytes alineados a {} bytes)\n",
                mapped_addr, aligned_length, aligned_length
            ));
            
            return SyscallResult::Success(mapped_addr);
        }
    }
    
    SyscallResult::Error(SyscallError::InvalidOperation)
}

/// Syscall munmap - Desmapear memoria
fn sys_munmap(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as u64;
    let length = args.arg1 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: munmap(addr=0x{:x}, length={})\n", addr, length));
    
    if addr == 0 || length == 0 {
        return SyscallResult::Error(SyscallError::InvalidArgument);
    }
    
    use crate::process::manager::get_process_manager;
    
    let manager_guard = get_process_manager().lock();
    
    if let Some(ref manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref process) = manager.processes[current_pid as usize] {
            let heap_start = process.memory_info.heap_start;
            let heap_break = process.memory_info.heap_break;
            
            // Validar que está dentro del heap
            if addr < heap_start || addr >= heap_break {
                serial_write_str("SYSCALL: munmap() - addr outside heap\n");
                return SyscallResult::Error(SyscallError::InvalidArgument);
            }
            
            // Por ahora, munmap solo registra pero no libera
            // En un sistema completo, liberaría las páginas
            serial_write_str(&alloc::format!(
                "SYSCALL: munmap() -> OK (simulado, no se libera memoria)\n"
            ));
            
            return SyscallResult::Success(0);
        }
    }
    
    SyscallResult::Error(SyscallError::InvalidOperation)
}

/// Syscall mprotect - Cambiar protección de memoria
fn sys_mprotect(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as *mut u8;
    let length = args.arg1 as usize;
    let prot = args.arg2 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: mprotect(length={}, prot={})\n", length, prot));
    
    // TODO: Implementar cambio de protección real
    SyscallResult::Success(0)
}

/// Syscall msync - Sincronizar memoria mapeada
fn sys_msync(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as *mut u8;
    let length = args.arg1 as usize;
    let flags = args.arg2 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: msync(length={}, flags={})\n", length, flags));
    
    // TODO: Implementar sincronización real
    SyscallResult::Success(0)
}

/// Syscall madvise - Dar consejos sobre uso de memoria
fn sys_madvise(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as *mut u8;
    let length = args.arg1 as usize;
    let advice = args.arg2 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: madvise(length={}, advice={})\n", length, advice));
    
    // TODO: Implementar consejos de memoria real
    SyscallResult::Success(0)
}

/// Syscall shmget - Obtener segmento de memoria compartida
fn sys_shmget(args: &SyscallArgs) -> SyscallResult {
    let key = args.arg0 as i32;
    let size = args.arg1 as usize;
    let shmflg = args.arg2 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: shmget(key={}, size={})\n", key, size));
    
    // TODO: Implementar memoria compartida real
    SyscallResult::Success(0)
}

/// Syscall shmat - Adjuntar segmento de memoria compartida
fn sys_shmat(args: &SyscallArgs) -> SyscallResult {
    let shmid = args.arg0 as i32;
    let shmaddr = args.arg1 as *const u8;
    let shmflg = args.arg2 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: shmat(shmid={})\n", shmid));
    
    // TODO: Implementar adjunción real
    SyscallResult::Success(shmaddr as u64)
}

/// Syscall shmdt - Desadjuntar segmento de memoria compartida
fn sys_shmdt(args: &SyscallArgs) -> SyscallResult {
    let shmaddr = args.arg0 as *const u8;
    
    serial_write_str("SYSCALL: shmdt()\n");
    
    // TODO: Implementar desadjunción real
    SyscallResult::Success(0)
}

/// Syscall fork - Crear proceso hijo
fn sys_fork(args: &SyscallArgs) -> SyscallResult {
    serial_write_str("SYSCALL: fork()\n");
    
    use crate::process::manager::get_process_manager;
    
    // Obtener el gestor de procesos
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        // Obtener el PID del proceso actual
        // NOTA: Por ahora usamos PID 0 (kernel) como padre
        // TODO: Obtener el proceso actual real desde el scheduler/context
        let current_pid = manager.current_process.unwrap_or(0);
        
        serial_write_str(&alloc::format!("SYSCALL: fork() - parent_pid={}\n", current_pid));
        
        // Crear proceso hijo
        match manager.fork_process(current_pid) {
            Ok(child_pid) => {
                serial_write_str(&alloc::format!("SYSCALL: fork() - child_pid={}\n", child_pid));
                
                // NOTA: En un sistema real, esta syscall retornaría:
                // - 0 al proceso hijo (después del context switch)
                // - child_pid al proceso padre
                // Por ahora solo retornamos el child_pid al padre
                // ya que no tenemos context switching implementado aún
                
                SyscallResult::Success(child_pid as u64)
            }
            Err(e) => {
                serial_write_str(&alloc::format!("SYSCALL: fork() ERROR: {}\n", e));
                SyscallResult::Error(SyscallError::OutOfMemory)
            }
        }
    } else {
        serial_write_str("SYSCALL: fork() ERROR: Process manager not initialized\n");
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall execve - Ejecutar programa
fn sys_execve(args: &SyscallArgs) -> SyscallResult {
    let filename = args.arg0 as *const u8;
    let argv = args.arg1 as *const *const u8;
    let envp = args.arg2 as *const *const u8;
    
    serial_write_str("SYSCALL: execve() - Implementación completa\n");
    
    // Usar el módulo execve para la implementación completa
    match execve::execve_syscall(filename, argv, envp) {
        Ok(()) => {
            // execve() tuvo éxito
            // El proceso se reemplazó y ejecutará en el próximo time slice
            serial_write_str("SYSCALL: execve() - Proceso reemplazado exitosamente\n");
            
            // En un sistema real, execve() NO RETORNA si tiene éxito
            // El proceso actual se reemplaza completamente
            // Por ahora retornamos 0, pero el proceso ya está configurado
            // para ejecutar el nuevo binario
            SyscallResult::Success(0)
        }
        Err(e) => {
            // execve() falló, retornar código de error
            let errno = e.to_errno();
            serial_write_str(&alloc::format!(
                "SYSCALL: execve() - Error: errno={}\n",
                errno
            ));
            SyscallResult::Error(SyscallError::NotImplemented) // TODO: mejor mapeo de errores
        }
    }
}

/// Syscall wait4 - Esperar cambio de estado de proceso
fn sys_wait4(args: &SyscallArgs) -> SyscallResult {
    let pid = args.arg0 as i32;
    let wstatus = args.arg1 as *mut i32;
    let options = args.arg2 as i32;
    let rusage = args.arg3 as *mut u8;
    
    serial_write_str(&alloc::format!("SYSCALL: wait4(pid={}, options={})\n", pid, options));
    
    use crate::process::manager::get_process_manager;
    use crate::process::process::ProcessState;
    
    // Obtener el gestor de procesos
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        // Si pid == -1, esperar cualquier hijo
        // Si pid > 0, esperar ese hijo específico
        // Si pid == 0, esperar cualquier hijo del mismo grupo
        
        // Buscar hijos del proceso actual
        for i in 0..crate::process::MAX_PROCESSES {
            if let Some(ref process) = manager.processes[i] {
                // Verificar si es hijo del proceso actual
                if process.parent_pid == Some(current_pid) {
                    // Si pid > 0, solo esperar ese hijo específico
                    if pid > 0 && process.pid != pid as u32 {
                        continue;
                    }
                    
                    // Verificar si el hijo ha terminado
                    if process.state == ProcessState::Terminated || 
                       process.state == ProcessState::Zombie {
                        let child_pid = process.pid;
                        let exit_code = process.exit_code.unwrap_or(0);
                        
                        serial_write_str(&alloc::format!(
                            "SYSCALL: wait4() - child {} terminated with code {}\n",
                            child_pid, exit_code
                        ));
                        
                        // Escribir el exit status si el puntero es válido
                        if !wstatus.is_null() {
                            unsafe {
                                *wstatus = (exit_code << 8) as i32; // Linux wait format
                            }
                        }
                        
                        // Limpiar el proceso zombie (reaping)
                        manager.processes[i] = None;
                        if manager.active_processes > 0 {
                            manager.active_processes -= 1;
                        }
                        
                        // Retornar PID del hijo
                        return SyscallResult::Success(child_pid as u64);
                    }
                }
            }
        }
        
        // No hay hijos terminados
        // TODO: En un sistema real, esto bloquearía el proceso hasta que un hijo termine
        // o retornaría error si options & WNOHANG
        serial_write_str("SYSCALL: wait4() - no terminated children\n");
        SyscallResult::Error(SyscallError::Interrupted) // ECHILD = no children
    } else {
        serial_write_str("SYSCALL: wait4() ERROR: Process manager not initialized\n");
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall nanosleep - Dormir con precisión de nanosegundos
fn sys_nanosleep(args: &SyscallArgs) -> SyscallResult {
    let req = args.arg0 as *const u8;
    let rem = args.arg1 as *mut u8;
    
    serial_write_str("SYSCALL: nanosleep()\n");
    
    // TODO: Implementar sleep real
    SyscallResult::Success(0)
}

/// Syscall gettimeofday - Obtener tiempo actual
fn sys_gettimeofday(args: &SyscallArgs) -> SyscallResult {
    let tv = args.arg0 as *mut u8;
    let tz = args.arg1 as *mut u8;
    
    serial_write_str("SYSCALL: gettimeofday()\n");
    
    if tv.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Obtener tiempo del sistema (desde el timer)
    let uptime_ms = {
        let timer_guard = crate::interrupts::timer::get_system_timer().lock();
        timer_guard.get_system_time_ms()
    };
    
    // struct timeval { tv_sec: i64, tv_usec: i64 }
    unsafe {
        let tv_ptr = tv as *mut i64;
        *tv_ptr.offset(0) = (uptime_ms / 1000) as i64;  // segundos
        *tv_ptr.offset(1) = ((uptime_ms % 1000) * 1000) as i64;  // microsegundos
    }
    
    serial_write_str(&alloc::format!("SYSCALL: gettimeofday() -> {}s\n", uptime_ms / 1000));
    
    // timezone (opcional, puede ser NULL)
    if !tz.is_null() {
        // struct timezone { tz_minuteswest: i32, tz_dsttime: i32 }
        unsafe {
            let tz_ptr = tz as *mut i32;
            *tz_ptr.offset(0) = 0;  // UTC
            *tz_ptr.offset(1) = 0;  // No DST
        }
    }
    
    SyscallResult::Success(0)
}

/// Syscall getrusage - Obtener estadísticas de uso de recursos
fn sys_getrusage(args: &SyscallArgs) -> SyscallResult {
    let who = args.arg0 as i32;
    let usage = args.arg1 as *mut u8;
    
    serial_write_str(&alloc::format!("SYSCALL: getrusage(who={})\n", who));
    
    // TODO: Implementar obtención de estadísticas real
    SyscallResult::Success(0)
}

/// Syscall sysinfo - Obtener información del sistema
fn sys_sysinfo(args: &SyscallArgs) -> SyscallResult {
    let info = args.arg0 as *mut u8;
    
    serial_write_str("SYSCALL: sysinfo()\n");
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
}

/// Syscall getuid - Obtener UID del usuario
fn sys_getuid(args: &SyscallArgs) -> SyscallResult {
    serial_write_str("SYSCALL: getuid()\n");
    
    // TODO: Implementar obtención de UID real
    SyscallResult::Success(0) // root
}

/// Syscall getgid - Obtener GID del grupo
fn sys_getgid(args: &SyscallArgs) -> SyscallResult {
    serial_write_str("SYSCALL: getgid()\n");
    
    // TODO: Implementar obtención de GID real
    SyscallResult::Success(0) // root
}

/// Syscall setuid - Establecer UID del usuario
fn sys_setuid(args: &SyscallArgs) -> SyscallResult {
    let uid = args.arg0 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: setuid(uid={})\n", uid));
    
    // TODO: Implementar establecimiento de UID real
    SyscallResult::Success(0)
}

/// Syscall setgid - Establecer GID del grupo
fn sys_setgid(args: &SyscallArgs) -> SyscallResult {
    let gid = args.arg0 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: setgid(gid={})\n", gid));
    
    // TODO: Implementar establecimiento de GID real
    SyscallResult::Success(0)
}

/// Syscall geteuid - Obtener UID efectivo del usuario
fn sys_geteuid(args: &SyscallArgs) -> SyscallResult {
    serial_write_str("SYSCALL: geteuid()\n");
    
    // TODO: Implementar obtención de EUID real
    SyscallResult::Success(0) // root
}

/// Syscall getegid - Obtener GID efectivo del grupo
fn sys_getegid(args: &SyscallArgs) -> SyscallResult {
    serial_write_str("SYSCALL: getegid()\n");
    
    // TODO: Implementar obtención de EGID real
    SyscallResult::Success(0) // root
}

/// Syscall setreuid - Establecer UID real y efectivo
fn sys_setreuid(args: &SyscallArgs) -> SyscallResult {
    let ruid = args.arg0 as u32;
    let euid = args.arg1 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: setreuid(ruid={}, euid={})\n", ruid, euid));
    
    // TODO: Implementar establecimiento real
    SyscallResult::Success(0)
}

/// Syscall setregid - Establecer GID real y efectivo
fn sys_setregid(args: &SyscallArgs) -> SyscallResult {
    let rgid = args.arg0 as u32;
    let egid = args.arg1 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: setregid(rgid={}, egid={})\n", rgid, egid));
    
    // TODO: Implementar establecimiento real
    SyscallResult::Success(0)
}

/// Syscall chdir - Cambiar directorio de trabajo
fn sys_chdir(args: &SyscallArgs) -> SyscallResult {
    let path = args.arg0 as *const u8;
    
    if path.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir path a string
    let path_str = unsafe {
        let mut len = 0;
        while *path.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(path, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: chdir('{}')\n", path_str));
    
    use crate::process::manager::get_process_manager;
    use crate::filesystem::vfs::get_vfs;
    
    // Verificar que el directorio existe en el VFS
    let vfs_guard = get_vfs();
    if let Some(ref vfs) = *vfs_guard {
        if let Some(root_fs) = vfs.get_root_fs() {
            let fs_guard = root_fs.lock();
            
            // Intentar resolver el path
            match fs_guard.resolve_path(path_str) {
                Ok(inode) => {
                    // Verificar que es un directorio
                    match fs_guard.stat(inode) {
                        Ok(stat_info) => {
                            // Verificar si es directorio (modo & 0x4000 == S_IFDIR)
                            if (stat_info.mode & 0x4000) != 0 {
                                drop(fs_guard);
                                drop(vfs_guard);
                                
                                // Cambiar el working directory del proceso
                                let mut manager_guard = get_process_manager().lock();
                                if let Some(ref mut manager) = *manager_guard {
                                    let current_pid = manager.current_process.unwrap_or(0);
                                    
                                    if let Some(ref mut process) = manager.processes[current_pid as usize] {
                                        process.working_directory = String::from(path_str);
                                        serial_write_str(&alloc::format!("SYSCALL: chdir() -> OK (now '{}')\n", path_str));
                                        return SyscallResult::Success(0);
                                    }
                                }
                            } else {
                                serial_write_str(&alloc::format!("SYSCALL: chdir('{}') - not a directory\n", path_str));
                                return SyscallResult::Error(SyscallError::NotADirectory);
                            }
                        }
                        Err(e) => {
                            serial_write_str(&alloc::format!("SYSCALL: chdir() - stat error: {:?}\n", e));
                            return SyscallResult::Error(SyscallError::FileNotFound);
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: chdir('{}') - not found: {:?}\n", path_str, e));
                    return SyscallResult::Error(SyscallError::FileNotFound);
                }
            }
        }
    }
    
    SyscallResult::Error(SyscallError::InvalidOperation)
}

/// Syscall fchdir - Cambiar directorio de trabajo por fd
fn sys_fchdir(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: fchdir(fd={})\n", fd));
    
    // TODO: Implementar cambio de directorio real
    SyscallResult::Success(0)
}

/// Syscall mkdir - Crear directorio
fn sys_mkdir(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let mode = args.arg1 as u32;
    
    if pathname.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir pathname a string
    let path_str = unsafe {
        let mut len = 0;
        while *pathname.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(pathname, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: mkdir('{}', mode=0o{:o})\n", path_str, mode));
    
    // Por ahora, mkdir retorna éxito simulado
    // En un sistema completo, esto crearía el directorio en el VFS
    serial_write_str(&alloc::format!("SYSCALL: mkdir() - simulado (VFS read-only)\n"));
    
    // Simular éxito para comandos básicos
    SyscallResult::Success(0)
}

/// Syscall rmdir - Eliminar directorio
fn sys_rmdir(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    
    if pathname.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir pathname a string
    let path_str = unsafe {
        let mut len = 0;
        while *pathname.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(pathname, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: rmdir('{}')\n", path_str));
    
    use crate::filesystem::vfs::get_vfs;
    
    // Verificar que el directorio existe
    let vfs_guard = get_vfs();
    if let Some(ref vfs) = *vfs_guard {
        if let Some(root_fs) = vfs.get_root_fs() {
            let mut fs_guard = root_fs.lock();
            
            // Intentar resolver el path
            match fs_guard.resolve_path(path_str) {
                Ok(inode) => {
                    // Verificar que es un directorio
                    match fs_guard.stat(inode) {
                        Ok(stat_info) => {
                            if (stat_info.mode & 0x4000) == 0 {
                                serial_write_str(&alloc::format!("SYSCALL: rmdir('{}') - not a directory\n", path_str));
                                return SyscallResult::Error(SyscallError::NotADirectory);
                            }
                            
                            // Intentar eliminar (puede fallar si no está implementado)
                            match fs_guard.rmdir(0, path_str) {
                                Ok(_) => {
                                    serial_write_str(&alloc::format!("SYSCALL: rmdir('{}') -> OK\n", path_str));
                                    SyscallResult::Success(0)
                                }
                                Err(e) => {
                                    serial_write_str(&alloc::format!("SYSCALL: rmdir() - VFS error: {:?}\n", e));
                                    SyscallResult::Error(SyscallError::InvalidOperation)
                                }
                            }
                        }
                        Err(e) => {
                            serial_write_str(&alloc::format!("SYSCALL: rmdir() - stat error: {:?}\n", e));
                            SyscallResult::Error(SyscallError::FileNotFound)
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: rmdir('{}') - not found: {:?}\n", path_str, e));
                    SyscallResult::Error(SyscallError::FileNotFound)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall unlink - Eliminar enlace de archivo
fn sys_unlink(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    
    if pathname.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir pathname a string
    let path_str = unsafe {
        let mut len = 0;
        while *pathname.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(pathname, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: unlink('{}')\n", path_str));
    
    use crate::filesystem::vfs::get_vfs;
    
    // Verificar que el archivo existe
    let vfs_guard = get_vfs();
    if let Some(ref vfs) = *vfs_guard {
        if let Some(root_fs) = vfs.get_root_fs() {
            let mut fs_guard = root_fs.lock();
            
            // Intentar resolver el path
            match fs_guard.resolve_path(path_str) {
                Ok(inode) => {
                    // Verificar que NO es un directorio
                    match fs_guard.stat(inode) {
                        Ok(stat_info) => {
                            if (stat_info.mode & 0x4000) != 0 {
                                serial_write_str(&alloc::format!("SYSCALL: unlink('{}') - is a directory\n", path_str));
                                return SyscallResult::Error(SyscallError::IsADirectory);
                            }
                            
                            // Intentar eliminar (puede fallar si no está implementado)
                            match fs_guard.unlink(0, path_str) {
                                Ok(_) => {
                                    serial_write_str(&alloc::format!("SYSCALL: unlink('{}') -> OK\n", path_str));
                                    SyscallResult::Success(0)
                                }
                                Err(e) => {
                                    serial_write_str(&alloc::format!("SYSCALL: unlink() - VFS error: {:?}\n", e));
                                    SyscallResult::Error(SyscallError::InvalidOperation)
                                }
                            }
                        }
                        Err(e) => {
                            serial_write_str(&alloc::format!("SYSCALL: unlink() - stat error: {:?}\n", e));
                            SyscallResult::Error(SyscallError::FileNotFound)
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: unlink('{}') - not found: {:?}\n", path_str, e));
                    SyscallResult::Error(SyscallError::FileNotFound)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall symlink - Crear enlace simbólico
fn sys_symlink(args: &SyscallArgs) -> SyscallResult {
    let target = args.arg0 as *const u8;
    let linkpath = args.arg1 as *const u8;
    
    serial_write_str("SYSCALL: symlink()\n");
    
    // TODO: Implementar creación de enlace simbólico real
    SyscallResult::Success(0)
}

/// Syscall readlink - Leer enlace simbólico
fn sys_readlink(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let buf = args.arg1 as *mut u8;
    let bufsiz = args.arg2 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: readlink(bufsiz={})\n", bufsiz));
    
    // TODO: Implementar lectura de enlace simbólico real
    SyscallResult::Success(0)
}

/// Syscall chmod - Cambiar permisos de archivo
fn sys_chmod(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let mode = args.arg1 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: chmod(mode={})\n", mode));
    
    // TODO: Implementar cambio de permisos real
    SyscallResult::Success(0)
}

/// Syscall fchmod - Cambiar permisos de archivo por fd
fn sys_fchmod(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let mode = args.arg1 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: fchmod(fd={}, mode={})\n", fd, mode));
    
    // TODO: Implementar cambio de permisos real
    SyscallResult::Success(0)
}

/// Syscall chown - Cambiar propietario de archivo
fn sys_chown(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let owner = args.arg1 as u32;
    let group = args.arg2 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: chown(owner={}, group={})\n", owner, group));
    
    // TODO: Implementar cambio de propietario real
    SyscallResult::Success(0)
}

/// Syscall fchown - Cambiar propietario de archivo por fd
fn sys_fchown(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let owner = args.arg1 as u32;
    let group = args.arg2 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: fchown(fd={}, owner={}, group={})\n", fd, owner, group));
    
    // TODO: Implementar cambio de propietario real
    SyscallResult::Success(0)
}

/// Syscall lchown - Cambiar propietario de enlace simbólico
fn sys_lchown(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let owner = args.arg1 as u32;
    let group = args.arg2 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: lchown(owner={}, group={})\n", owner, group));
    
    // TODO: Implementar cambio de propietario real
    SyscallResult::Success(0)
}

/// Syscall stat - Obtener información de archivo
fn sys_stat(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let statbuf = args.arg1 as *mut u8;
    
    if pathname.is_null() || statbuf.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir pathname a string
    let path_str = unsafe {
        let mut len = 0;
        while *pathname.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(pathname, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: stat('{}')\n", path_str));
    
    use crate::filesystem::vfs::get_vfs;
    
    // Acceder al VFS
    let vfs_guard = get_vfs();
    if let Some(ref vfs) = *vfs_guard {
        if let Some(root_fs) = vfs.get_root_fs() {
            let fs_guard = root_fs.lock();
            
            // Resolver path y obtener stat
            match fs_guard.resolve_path(path_str) {
                Ok(inode) => {
                    match fs_guard.stat(inode) {
                        Ok(stat_info) => {
                            // Llenar estructura stat (formato Linux x86_64)
                            unsafe {
                                // struct stat es ~144 bytes, simplificado aquí
                                let statbuf_ptr = statbuf as *mut u64;
                                
                                // st_dev (device ID)
                                *statbuf_ptr.offset(0) = 0x101; // ID de dispositivo simulado
                                
                                // st_ino (inode number)
                                *statbuf_ptr.offset(1) = stat_info.inode as u64;
                                
                                // st_mode (file mode)
                                *statbuf_ptr.offset(2) = stat_info.mode as u64;
                                
                                // st_nlink (number of hard links)
                                *statbuf_ptr.offset(3) = stat_info.nlink as u64;
                                
                                // st_uid, st_gid
                                *statbuf_ptr.offset(4) = stat_info.uid as u64;
                                *statbuf_ptr.offset(5) = stat_info.gid as u64;
                                
                                // st_rdev (device ID if special file)
                                *statbuf_ptr.offset(6) = 0;
                                
                                // st_size (total size in bytes)
                                *statbuf_ptr.offset(7) = stat_info.size;
                                
                                // st_blksize (blocksize for filesystem I/O)
                                *statbuf_ptr.offset(8) = 4096;
                                
                                // st_blocks (number of 512B blocks allocated)
                                *statbuf_ptr.offset(9) = (stat_info.size + 511) / 512;
                                
                                // st_atime, st_mtime, st_ctime
                                *statbuf_ptr.offset(10) = stat_info.atime;
                                *statbuf_ptr.offset(11) = stat_info.mtime;
                                *statbuf_ptr.offset(12) = stat_info.ctime;
                            }
                            
                            serial_write_str(&alloc::format!(
                                "SYSCALL: stat() -> OK (inode={}, size={})\n",
                                stat_info.inode, stat_info.size
                            ));
                            SyscallResult::Success(0)
                        }
                        Err(e) => {
                            serial_write_str(&alloc::format!("SYSCALL: stat() - error: {:?}\n", e));
                            SyscallResult::Error(SyscallError::FileNotFound)
                        }
                    }
                }
                Err(e) => {
                    serial_write_str(&alloc::format!("SYSCALL: stat('{}') - not found: {:?}\n", path_str, e));
                    SyscallResult::Error(SyscallError::FileNotFound)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall lstat - Obtener información de enlace simbólico
fn sys_lstat(args: &SyscallArgs) -> SyscallResult {
    // Por ahora, lstat es igual a stat (no soportamos symlinks todavía)
    serial_write_str("SYSCALL: lstat() - usando stat()\n");
    sys_stat(args)
}

/// Syscall fstat - Obtener información de archivo por fd
fn sys_fstat(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let statbuf = args.arg1 as *mut u8;
    
    serial_write_str(&alloc::format!("SYSCALL: fstat(fd={})\n", fd));
    
    if statbuf.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    use crate::process::manager::get_process_manager;
    use crate::process::file_descriptor::FileDescriptorType;
    use crate::filesystem::vfs::get_vfs;
    
    // Obtener info del FD del proceso actual
    let manager_guard = get_process_manager().lock();
    
    if let Some(ref manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref process) = manager.processes[current_pid as usize] {
            let fd_info = process.fd_table.get(fd);
            
            if let Some(fd_desc) = fd_info {
                match fd_desc.fd_type {
                    FileDescriptorType::File => {
                        // Archivo regular - obtener stat desde VFS
                        if let Some(inode) = fd_desc.inode {
                            drop(manager_guard);
                            
                            let vfs_guard = get_vfs();
                            if let Some(ref vfs) = *vfs_guard {
                                if let Some(root_fs) = vfs.get_root_fs() {
                                    let fs_guard = root_fs.lock();
                                    
                                    match fs_guard.stat(inode as u32) {
                                        Ok(stat_info) => {
                                            // Llenar estructura stat
                                            unsafe {
                                                let statbuf_ptr = statbuf as *mut u64;
                                                *statbuf_ptr.offset(0) = 0x101;
                                                *statbuf_ptr.offset(1) = stat_info.inode as u64;
                                                *statbuf_ptr.offset(2) = stat_info.mode as u64;
                                                *statbuf_ptr.offset(3) = stat_info.nlink as u64;
                                                *statbuf_ptr.offset(4) = stat_info.uid as u64;
                                                *statbuf_ptr.offset(5) = stat_info.gid as u64;
                                                *statbuf_ptr.offset(6) = 0;
                                                *statbuf_ptr.offset(7) = stat_info.size;
                                                *statbuf_ptr.offset(8) = 4096;
                                                *statbuf_ptr.offset(9) = (stat_info.size + 511) / 512;
                                                *statbuf_ptr.offset(10) = stat_info.atime;
                                                *statbuf_ptr.offset(11) = stat_info.mtime;
                                                *statbuf_ptr.offset(12) = stat_info.ctime;
                                            }
                                            
                                            serial_write_str(&alloc::format!(
                                                "SYSCALL: fstat(fd={}) -> OK (size={})\n",
                                                fd, stat_info.size
                                            ));
                                            return SyscallResult::Success(0);
                                        }
                                        Err(e) => {
                                            serial_write_str(&alloc::format!("SYSCALL: fstat() - stat error: {:?}\n", e));
                                            return SyscallResult::Error(SyscallError::InvalidOperation);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    FileDescriptorType::Stdin | FileDescriptorType::Stdout | FileDescriptorType::Stderr => {
                        // Terminal/character device
                        unsafe {
                            let statbuf_ptr = statbuf as *mut u64;
                            *statbuf_ptr.offset(0) = 0x105; // Device ID
                            *statbuf_ptr.offset(1) = fd as u64; // Inode = fd
                            *statbuf_ptr.offset(2) = 0x2190; // S_IFCHR | 0o600
                            *statbuf_ptr.offset(3) = 1;
                            *statbuf_ptr.offset(4) = 0; // uid
                            *statbuf_ptr.offset(5) = 0; // gid
                            *statbuf_ptr.offset(6) = 0;
                            *statbuf_ptr.offset(7) = 0; // size = 0 para character device
                            *statbuf_ptr.offset(8) = 4096;
                            *statbuf_ptr.offset(9) = 0;
                            *statbuf_ptr.offset(10) = 0; // atime
                            *statbuf_ptr.offset(11) = 0; // mtime
                            *statbuf_ptr.offset(12) = 0; // ctime
                        }
                        
                        serial_write_str(&alloc::format!("SYSCALL: fstat(fd={}) -> character device\n", fd));
                        return SyscallResult::Success(0);
                    }
                    FileDescriptorType::Pipe => {
                        // Pipe
                        unsafe {
                            let statbuf_ptr = statbuf as *mut u64;
                            *statbuf_ptr.offset(0) = 0x106;
                            *statbuf_ptr.offset(1) = fd as u64;
                            *statbuf_ptr.offset(2) = 0x1180; // S_IFIFO | 0o600
                            *statbuf_ptr.offset(3) = 1;
                            *statbuf_ptr.offset(4) = 0;
                            *statbuf_ptr.offset(5) = 0;
                            *statbuf_ptr.offset(6) = 0;
                            *statbuf_ptr.offset(7) = 4096; // pipe buffer size
                            *statbuf_ptr.offset(8) = 4096;
                            *statbuf_ptr.offset(9) = 8;
                            *statbuf_ptr.offset(10) = 0;
                            *statbuf_ptr.offset(11) = 0;
                            *statbuf_ptr.offset(12) = 0;
                        }
                        
                        serial_write_str(&alloc::format!("SYSCALL: fstat(fd={}) -> pipe\n", fd));
                        return SyscallResult::Success(0);
                    }
                    _ => {
                        serial_write_str(&alloc::format!("SYSCALL: fstat(fd={}) - unsupported type\n", fd));
                        return SyscallResult::Error(SyscallError::InvalidOperation);
                    }
                }
            } else {
                return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
            }
        }
    }
    
    SyscallResult::Error(SyscallError::InvalidOperation)
}

/// Syscall statfs - Obtener información del sistema de archivos
fn sys_statfs(args: &SyscallArgs) -> SyscallResult {
    let path = args.arg0 as *const u8;
    let buf = args.arg1 as *mut u8;
    
    serial_write_str("SYSCALL: statfs()\n");
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
}

/// Syscall fstatfs - Obtener información del sistema de archivos por fd
fn sys_fstatfs(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let buf = args.arg1 as *mut u8;
    
    serial_write_str(&alloc::format!("SYSCALL: fstatfs(fd={})\n", fd));
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
}

/// Syscall getdents - Obtener entradas de directorio
fn sys_getdents(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let dirp = args.arg1 as *mut u8;
    let count = args.arg2 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: getdents(fd={}, count={})\n", fd, count));
    
    if dirp.is_null() || count == 0 {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    use crate::process::manager::get_process_manager;
    use crate::process::file_descriptor::FileDescriptorType;
    use crate::filesystem::vfs::get_vfs;
    
    // Obtener el file descriptor del proceso actual
    let manager_guard = get_process_manager().lock();
    
    if let Some(ref manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref process) = manager.processes[current_pid as usize] {
            let fd_info = process.fd_table.get(fd);
            
            if let Some(fd_desc) = fd_info {
                // Verificar que el FD es un directorio
                if fd_desc.fd_type != FileDescriptorType::File {
                    return SyscallResult::Error(SyscallError::NotADirectory);
                }
                
                if let Some(inode) = fd_desc.inode {
                    if let Some(ref path) = fd_desc.path {
                        drop(manager_guard);
                        
                        // Leer el directorio desde el VFS
                        let vfs_guard = get_vfs();
                        if let Some(ref vfs) = *vfs_guard {
                            if let Some(root_fs) = vfs.get_root_fs() {
                                let fs_guard = root_fs.lock();
                                
                                // Obtener entradas del directorio
                                match fs_guard.readdir(inode as u32) {
                                    Ok(entries) => {
                                        // Formatear entradas en formato linux_dirent64
                                        let mut offset = 0usize;
                                        let mut entries_written = 0;
                                        
                                        for entry in entries.iter() {
                                            // struct linux_dirent64 {
                                            //     u64 d_ino;      // inode number
                                            //     i64 d_off;      // offset to next
                                            //     u16 d_reclen;   // record length
                                            //     u8  d_type;     // file type
                                            //     char d_name[];  // filename (null-terminated)
                                            // }
                                            
                                            let name_bytes = entry.as_bytes();
                                            let name_len = name_bytes.len();
                                            
                                            // Calcular tamaño de la entrada (con padding)
                                            let reclen = ((19 + name_len + 1 + 7) / 8) * 8; // Alinear a 8 bytes
                                            
                                            // Verificar si hay espacio en el buffer
                                            if offset + reclen > count {
                                                break;
                                            }
                                            
                                            unsafe {
                                                let entry_ptr = dirp.add(offset);
                                                
                                                // d_ino (8 bytes)
                                                *(entry_ptr as *mut u64) = (entries_written + 1) as u64;
                                                
                                                // d_off (8 bytes) - offset a la siguiente entrada
                                                *(entry_ptr.add(8) as *mut i64) = (offset + reclen) as i64;
                                                
                                                // d_reclen (2 bytes)
                                                *(entry_ptr.add(16) as *mut u16) = reclen as u16;
                                                
                                                // d_type (1 byte) - DT_UNKNOWN=0, DT_REG=8, DT_DIR=4
                                                *(entry_ptr.add(18) as *mut u8) = 0; // DT_UNKNOWN por ahora
                                                
                                                // d_name (null-terminated string)
                                                core::ptr::copy_nonoverlapping(
                                                    name_bytes.as_ptr(),
                                                    entry_ptr.add(19),
                                                    name_len
                                                );
                                                *(entry_ptr.add(19 + name_len)) = 0; // null terminator
                                            }
                                            
                                            offset += reclen;
                                            entries_written += 1;
                                        }
                                        
                                        serial_write_str(&alloc::format!(
                                            "SYSCALL: getdents() -> {} entries, {} bytes\n",
                                            entries_written, offset
                                        ));
                                        
                                        return SyscallResult::Success(offset as u64);
                                    }
                                    Err(e) => {
                                        serial_write_str(&alloc::format!("SYSCALL: getdents() - readdir error: {:?}\n", e));
                                        return SyscallResult::Error(SyscallError::InvalidOperation);
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                return SyscallResult::Error(SyscallError::InvalidFileDescriptor);
            }
        }
    }
    
    SyscallResult::Error(SyscallError::InvalidOperation)
}

/// Syscall fcntl - Control de descriptor de archivo
fn sys_fcntl(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let cmd = args.arg1 as i32;
    let arg = args.arg2 as u64;
    
    serial_write_str(&alloc::format!("SYSCALL: fcntl(fd={}, cmd={}, arg={})\n", fd, cmd, arg));
    
    // Constantes de fcntl (Linux)
    const F_DUPFD: i32 = 0;        // Duplicar FD
    const F_GETFD: i32 = 1;        // Obtener flags de FD
    const F_SETFD: i32 = 2;        // Establecer flags de FD
    const F_GETFL: i32 = 3;        // Obtener flags de archivo
    const F_SETFL: i32 = 4;        // Establecer flags de archivo
    const FD_CLOEXEC: i32 = 1;     // Close-on-exec flag
    
    use crate::process::manager::get_process_manager;
    
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            match cmd {
                F_DUPFD => {
                    // Duplicar FD (similar a dup())
                    serial_write_str(&alloc::format!("SYSCALL: fcntl(F_DUPFD) - fd={}\n", fd));
                    SyscallResult::Success(fd as u64)
                }
                F_GETFD => {
                    // Obtener flags de FD (por ahora retornamos 0 = no close-on-exec)
                    serial_write_str(&alloc::format!("SYSCALL: fcntl(F_GETFD) - fd={}\n", fd));
                    SyscallResult::Success(0)
                }
                F_SETFD => {
                    // Establecer flags de FD
                    serial_write_str(&alloc::format!("SYSCALL: fcntl(F_SETFD) - fd={}, flags={}\n", fd, arg));
                    SyscallResult::Success(0)
                }
                F_GETFL => {
                    // Obtener flags de archivo
                    if let Some(fd_desc) = process.fd_table.get(fd) {
                        let flags = fd_desc.flags;
                        serial_write_str(&alloc::format!("SYSCALL: fcntl(F_GETFL) - fd={} -> flags=0x{:x}\n", fd, flags));
                        SyscallResult::Success(flags as u64)
                    } else {
                        SyscallResult::Error(SyscallError::InvalidFileDescriptor)
                    }
                }
                F_SETFL => {
                    // Establecer flags de archivo
                    serial_write_str(&alloc::format!("SYSCALL: fcntl(F_SETFL) - fd={}, flags=0x{:x}\n", fd, arg));
                    // Por ahora solo registramos, no actualizamos
                    SyscallResult::Success(0)
                }
                _ => {
                    serial_write_str(&alloc::format!("SYSCALL: fcntl() - cmd desconocido: {}\n", cmd));
                    SyscallResult::Success(0)
                }
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall flock - Bloquear archivo
fn sys_flock(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let operation = args.arg1 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: flock(fd={}, operation={})\n", fd, operation));
    
    // TODO: Implementar bloqueo real
    SyscallResult::Success(0)
}

/// Syscall fsync - Sincronizar archivo
fn sys_fsync(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: fsync(fd={})\n", fd));
    
    // TODO: Implementar sincronización real
    SyscallResult::Success(0)
}

/// Syscall fdatasync - Sincronizar datos de archivo
fn sys_fdatasync(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: fdatasync(fd={})\n", fd));
    
    // TODO: Implementar sincronización real
    SyscallResult::Success(0)
}

/// Syscall truncate - Truncar archivo
fn sys_truncate(args: &SyscallArgs) -> SyscallResult {
    let path = args.arg0 as *const u8;
    let length = args.arg1 as i64;
    
    serial_write_str(&alloc::format!("SYSCALL: truncate(length={})\n", length));
    
    // TODO: Implementar truncamiento real
    SyscallResult::Success(0)
}

/// Syscall ftruncate - Truncar archivo por fd
fn sys_ftruncate(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let length = args.arg1 as i64;
    
    serial_write_str(&alloc::format!("SYSCALL: ftruncate(fd={}, length={})\n", fd, length));
    
    // TODO: Implementar truncamiento real
    SyscallResult::Success(0)
}

/// Syscall umask - Establecer máscara de creación de archivos
fn sys_umask(args: &SyscallArgs) -> SyscallResult {
    let mask = args.arg0 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: umask(mask={})\n", mask));
    
    // TODO: Implementar máscara real
    SyscallResult::Success(0)
}

/// Syscall getcwd - Obtener directorio de trabajo actual
fn sys_getcwd(args: &SyscallArgs) -> SyscallResult {
    let buf = args.arg0 as *mut u8;
    let size = args.arg1 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: getcwd(size={})\n", size));
    
    if buf.is_null() || size == 0 {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    use crate::process::manager::get_process_manager;
    
    let manager_guard = get_process_manager().lock();
    
    if let Some(ref manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref process) = manager.processes[current_pid as usize] {
            let cwd = &process.working_directory;
            let cwd_bytes = cwd.as_bytes();
            
            // Verificar que el buffer sea suficientemente grande (incluyendo null terminator)
            if size < cwd_bytes.len() + 1 {
                serial_write_str(&alloc::format!("SYSCALL: getcwd() - buffer too small (need {})\n", cwd_bytes.len() + 1));
                return SyscallResult::Error(SyscallError::InvalidArgument);
            }
            
            // Copiar el path al buffer
            unsafe {
                core::ptr::copy_nonoverlapping(
                    cwd_bytes.as_ptr(),
                    buf,
                    cwd_bytes.len()
                );
                // Null terminator
                *buf.add(cwd_bytes.len()) = 0;
            }
            
            serial_write_str(&alloc::format!("SYSCALL: getcwd() -> '{}'\n", cwd));
            SyscallResult::Success(buf as u64)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall getenv - Obtener variable de entorno
fn sys_getenv(args: &SyscallArgs) -> SyscallResult {
    let name_ptr = args.arg0 as *const u8;
    let buf = args.arg1 as *mut u8;
    let size = args.arg2 as usize;
    
    if name_ptr.is_null() || buf.is_null() || size == 0 {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir name a string
    let name_str = unsafe {
        let mut len = 0;
        while *name_ptr.add(len) != 0 && len < 256 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(name_ptr, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: getenv('{}')\n", name_str));
    
    use crate::process::manager::get_process_manager;
    
    let manager_guard = get_process_manager().lock();
    
    if let Some(ref manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref process) = manager.processes[current_pid as usize] {
            // Buscar la variable en el environment
            if let Some(value) = process.environment.get(name_str) {
                let value_bytes = value.as_bytes();
                
                // Verificar que el buffer sea suficientemente grande
                if size < value_bytes.len() + 1 {
                    serial_write_str(&alloc::format!("SYSCALL: getenv() - buffer too small\n"));
                    return SyscallResult::Error(SyscallError::InvalidArgument);
                }
                
                // Copiar el valor al buffer
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        value_bytes.as_ptr(),
                        buf,
                        value_bytes.len()
                    );
                    *buf.add(value_bytes.len()) = 0; // null terminator
                }
                
                serial_write_str(&alloc::format!("SYSCALL: getenv('{}') -> '{}'\n", name_str, value));
                SyscallResult::Success(buf as u64)
            } else {
                serial_write_str(&alloc::format!("SYSCALL: getenv('{}') - variable no encontrada\n", name_str));
                SyscallResult::Error(SyscallError::FileNotFound)
            }
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

/// Syscall setenv - Establecer variable de entorno
fn sys_setenv(args: &SyscallArgs) -> SyscallResult {
    let name_ptr = args.arg0 as *const u8;
    let value_ptr = args.arg1 as *const u8;
    let overwrite = args.arg2 as i32;
    
    if name_ptr.is_null() || value_ptr.is_null() {
        return SyscallResult::Error(SyscallError::BadAddress);
    }
    
    // Convertir name a string
    let name_str = unsafe {
        let mut len = 0;
        while *name_ptr.add(len) != 0 && len < 256 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(name_ptr, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    // Convertir value a string
    let value_str = unsafe {
        let mut len = 0;
        while *value_ptr.add(len) != 0 && len < 4096 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(value_ptr, len);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return SyscallResult::Error(SyscallError::InvalidArgument),
        }
    };
    
    serial_write_str(&alloc::format!("SYSCALL: setenv('{}', '{}', overwrite={})\n", name_str, value_str, overwrite));
    
    use crate::process::manager::get_process_manager;
    
    let mut manager_guard = get_process_manager().lock();
    
    if let Some(ref mut manager) = *manager_guard {
        let current_pid = manager.current_process.unwrap_or(0);
        
        if let Some(ref mut process) = manager.processes[current_pid as usize] {
            // Si overwrite es 0 y la variable ya existe, no hacer nada
            if overwrite == 0 && process.environment.contains_key(name_str) {
                serial_write_str(&alloc::format!("SYSCALL: setenv() - variable existe, no se sobrescribe\n"));
                return SyscallResult::Success(0);
            }
            
            // Insertar o actualizar la variable
            process.environment.insert(String::from(name_str), String::from(value_str));
            
            serial_write_str(&alloc::format!("SYSCALL: setenv('{}') -> OK\n", name_str));
            SyscallResult::Success(0)
        } else {
            SyscallResult::Error(SyscallError::InvalidOperation)
        }
    } else {
        SyscallResult::Error(SyscallError::InvalidOperation)
    }
}

