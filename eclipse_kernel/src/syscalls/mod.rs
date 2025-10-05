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

use crate::debug::serial_write_str;

/// Número de syscalls implementadas
pub const SYSCALL_COUNT: usize = 64;

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
    registry.register(11, sys_dup2);
    registry.register(12, sys_pipe);
    registry.register(13, sys_alarm);
    registry.register(14, sys_brk);
    registry.register(15, sys_mmap);
    registry.register(16, sys_munmap);
    registry.register(17, sys_mprotect);
    registry.register(18, sys_msync);
    registry.register(19, sys_madvise);
    registry.register(20, sys_shmget);
    registry.register(21, sys_shmat);
    registry.register(22, sys_shmdt);
    registry.register(23, sys_fork);
    registry.register(24, sys_execve);
    registry.register(25, sys_wait4);
    registry.register(26, sys_nanosleep);
    registry.register(27, sys_gettimeofday);
    registry.register(28, sys_getrusage);
    registry.register(29, sys_sysinfo);
    registry.register(30, sys_getuid);
    registry.register(31, sys_getgid);
    registry.register(32, sys_setuid);
    registry.register(33, sys_setgid);
    registry.register(34, sys_geteuid);
    registry.register(35, sys_getegid);
    registry.register(36, sys_setreuid);
    registry.register(37, sys_setregid);
    registry.register(38, sys_chdir);
    registry.register(39, sys_fchdir);
    registry.register(40, sys_mkdir);
    registry.register(41, sys_rmdir);
    registry.register(42, sys_unlink);
    registry.register(43, sys_symlink);
    registry.register(44, sys_readlink);
    registry.register(45, sys_chmod);
    registry.register(46, sys_fchmod);
    registry.register(47, sys_chown);
    registry.register(48, sys_fchown);
    registry.register(49, sys_lchown);
    registry.register(50, sys_stat);
    registry.register(51, sys_lstat);
    registry.register(52, sys_fstat);
    registry.register(53, sys_statfs);
    registry.register(54, sys_fstatfs);
    registry.register(55, sys_getdents);
    registry.register(56, sys_fcntl);
    registry.register(57, sys_flock);
    registry.register(58, sys_fsync);
    registry.register(59, sys_fdatasync);
    registry.register(60, sys_truncate);
    registry.register(61, sys_ftruncate);
    registry.register(62, sys_umask);
    registry.register(63, sys_getcwd);
    
    serial_write_str("SYSCALL: Sistema de syscalls inicializado\n");
    registry
}

// Syscalls básicas (implementaciones mínimas por ahora)

/// Syscall exit - Terminar proceso
fn sys_exit(args: &SyscallArgs) -> SyscallResult {
    let exit_code = args.arg0 as i32;
    serial_write_str(&alloc::format!("SYSCALL: exit({})\n", exit_code));
    
    // TODO: Implementar terminación de proceso
    // Por ahora solo logueamos
    SyscallResult::Success(0)
}

/// Syscall write - Escribir a descriptor de archivo
fn sys_write(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let buf = args.arg1 as *const u8;
    let count = args.arg2 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: write(fd={}, count={})\n", fd, count));
    
    // TODO: Implementar escritura real
    // Por ahora solo retornamos el count
    SyscallResult::Success(count as u64)
}

/// Syscall open - Abrir archivo
fn sys_open(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let flags = args.arg1 as i32;
    let mode = args.arg2 as u32;
    
    serial_write_str(&alloc::format!("SYSCALL: open(flags={}, mode={})\n", flags, mode));
    
    // TODO: Implementar apertura real de archivos
    // Por ahora retornamos un fd simulado
    SyscallResult::Success(3) // stdout
}

/// Syscall close - Cerrar descriptor de archivo
fn sys_close(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: close(fd={})\n", fd));
    
    // TODO: Implementar cierre real
    SyscallResult::Success(0)
}

/// Syscall read - Leer de descriptor de archivo
fn sys_read(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let buf = args.arg1 as *mut u8;
    let count = args.arg2 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: read(fd={}, count={})\n", fd, count));
    
    // TODO: Implementar lectura real
    // Por ahora retornamos 0 (EOF)
    SyscallResult::Success(0)
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
    
    serial_write_str(&alloc::format!("SYSCALL: ioctl(fd={}, request={})\n", fd, request));
    
    // TODO: Implementar ioctl real
    SyscallResult::Success(0)
}

/// Syscall access - Verificar permisos de archivo
fn sys_access(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let mode = args.arg1 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: access(mode={})\n", mode));
    
    // TODO: Implementar verificación de acceso real
    SyscallResult::Success(0)
}

/// Syscall kill - Enviar señal a proceso
fn sys_kill(args: &SyscallArgs) -> SyscallResult {
    let pid = args.arg0 as i32;
    let sig = args.arg1 as i32;
    
    serial_write_str(&alloc::format!("SYSCALL: kill(pid={}, sig={})\n", pid, sig));
    
    // TODO: Implementar envío de señales real
    SyscallResult::Success(0)
}

/// Syscall getpid - Obtener ID del proceso
fn sys_getpid(args: &SyscallArgs) -> SyscallResult {
    serial_write_str("SYSCALL: getpid()\n");
    
    // TODO: Implementar obtención de PID real
    SyscallResult::Success(1) // PID simulado
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
    
    // TODO: Implementar duplicación real
    SyscallResult::Success(newfd as u64)
}

/// Syscall pipe - Crear pipe
fn sys_pipe(args: &SyscallArgs) -> SyscallResult {
    let pipefd = args.arg0 as *mut i32;
    
    serial_write_str("SYSCALL: pipe()\n");
    
    // TODO: Implementar creación de pipe real
    SyscallResult::Success(0)
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
    let addr = args.arg0 as *mut u8;
    
    serial_write_str("SYSCALL: brk()\n");
    
    // TODO: Implementar gestión de heap real
    SyscallResult::Success(addr as u64)
}

/// Syscall mmap - Mapear memoria
fn sys_mmap(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as *mut u8;
    let length = args.arg1 as usize;
    let prot = args.arg2 as i32;
    let flags = args.arg3 as i32;
    let fd = args.arg4 as i32;
    let offset = args.arg5 as i64;
    
    serial_write_str(&alloc::format!("SYSCALL: mmap(length={}, prot={}, flags={})\n", length, prot, flags));
    
    // TODO: Implementar mapeo de memoria real
    SyscallResult::Success(addr as u64)
}

/// Syscall munmap - Desmapear memoria
fn sys_munmap(args: &SyscallArgs) -> SyscallResult {
    let addr = args.arg0 as *mut u8;
    let length = args.arg1 as usize;
    
    serial_write_str(&alloc::format!("SYSCALL: munmap(length={})\n", length));
    
    // TODO: Implementar desmapeo real
    SyscallResult::Success(0)
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
    
    // TODO: Implementar creación de proceso real
    SyscallResult::Success(0) // PID del hijo
}

/// Syscall execve - Ejecutar programa
fn sys_execve(args: &SyscallArgs) -> SyscallResult {
    let filename = args.arg0 as *const u8;
    let argv = args.arg1 as *const *const u8;
    let envp = args.arg2 as *const *const u8;
    
    serial_write_str("SYSCALL: execve()\n");
    
    // TODO: Implementar ejecución real
    SyscallResult::Success(0)
}

/// Syscall wait4 - Esperar cambio de estado de proceso
fn sys_wait4(args: &SyscallArgs) -> SyscallResult {
    let pid = args.arg0 as i32;
    let wstatus = args.arg1 as *mut i32;
    let options = args.arg2 as i32;
    let rusage = args.arg3 as *mut u8;
    
    serial_write_str(&alloc::format!("SYSCALL: wait4(pid={}, options={})\n", pid, options));
    
    // TODO: Implementar espera real
    SyscallResult::Success(pid as u64)
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
    
    // TODO: Implementar obtención de tiempo real
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
    
    serial_write_str("SYSCALL: chdir()\n");
    
    // TODO: Implementar cambio de directorio real
    SyscallResult::Success(0)
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
    
    serial_write_str(&alloc::format!("SYSCALL: mkdir(mode={})\n", mode));
    
    // TODO: Implementar creación de directorio real
    SyscallResult::Success(0)
}

/// Syscall rmdir - Eliminar directorio
fn sys_rmdir(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    
    serial_write_str("SYSCALL: rmdir()\n");
    
    // TODO: Implementar eliminación de directorio real
    SyscallResult::Success(0)
}

/// Syscall unlink - Eliminar enlace de archivo
fn sys_unlink(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    
    serial_write_str("SYSCALL: unlink()\n");
    
    // TODO: Implementar eliminación real
    SyscallResult::Success(0)
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
    
    serial_write_str("SYSCALL: stat()\n");
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
}

/// Syscall lstat - Obtener información de enlace simbólico
fn sys_lstat(args: &SyscallArgs) -> SyscallResult {
    let pathname = args.arg0 as *const u8;
    let statbuf = args.arg1 as *mut u8;
    
    serial_write_str("SYSCALL: lstat()\n");
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
}

/// Syscall fstat - Obtener información de archivo por fd
fn sys_fstat(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let statbuf = args.arg1 as *mut u8;
    
    serial_write_str(&alloc::format!("SYSCALL: fstat(fd={})\n", fd));
    
    // TODO: Implementar obtención de información real
    SyscallResult::Success(0)
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
    
    // TODO: Implementar obtención de entradas real
    SyscallResult::Success(0)
}

/// Syscall fcntl - Control de descriptor de archivo
fn sys_fcntl(args: &SyscallArgs) -> SyscallResult {
    let fd = args.arg0 as i32;
    let cmd = args.arg1 as i32;
    let arg = args.arg2 as u64;
    
    serial_write_str(&alloc::format!("SYSCALL: fcntl(fd={}, cmd={})\n", fd, cmd));
    
    // TODO: Implementar control real
    SyscallResult::Success(0)
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
    
    // TODO: Implementar obtención real
    SyscallResult::Success(0)
}

