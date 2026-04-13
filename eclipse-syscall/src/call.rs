//! High-level, type-safe syscall wrappers
use core::result::Result::{Ok, Err};
use crate::error::{cvt, cvt_unit, Error, Result};
use crate::number::*;
use crate::arch::*;

/// Longitud máxima de ruta en userspace (el kernel usa `MAX_PATH_LENGTH` 1024 y lee hasta NUL).
const MAX_USER_PATH: usize = 1023;

#[inline]
fn path_to_nul_stack(path: &str, buf: &mut [u8; 1024]) -> Result<*const u8> {
    if path.is_empty() {
        return Err(Error::new(crate::error::EINVAL));
    }
    if path.len() > MAX_USER_PATH {
        return Err(Error::new(crate::error::EINVAL));
    }
    buf[..path.len()].copy_from_slice(path.as_bytes());
    buf[path.len()] = 0;
    Ok(buf.as_ptr())
}

/// El kernel lee hasta 16 bytes buscando NUL; un `&str` corto no garantiza 16 bytes mapeados.
#[inline]
fn spawn_name_ptr(name: Option<&str>, name_buf: &mut [u8; 16]) -> usize {
    match name {
        None => 0,
        Some(n) => {
            let clen = n.len().min(15);
            name_buf[..clen].copy_from_slice(&n.as_bytes()[..clen]);
            name_buf.as_ptr() as usize
        }
    }
}

/// Write to a file descriptor
pub fn write(fd: usize, buf: &[u8]) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())) }
}

/// Read from a file descriptor
pub fn read(fd: usize, buf: &mut [u8]) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_READ, fd, buf.as_mut_ptr() as usize, buf.len())) }
}

/// Close a file descriptor
pub fn close(fd: usize) -> Result<()> {
    unsafe { cvt_unit(syscall1(SYS_CLOSE, fd)) }
}

/// Open a path with raw string and flags
pub fn open(path: &str, flags: usize) -> Result<usize> {
    let mut buf = [0u8; 1024];
    let ptr = path_to_nul_stack(path, &mut buf)?;
    unsafe { cvt(syscall3(SYS_OPEN, ptr as usize, flags, 0)) }
}

/// lseek wrapper
pub fn lseek(fd: usize, offset: isize, whence: usize) -> Result<isize> {
    unsafe {
        let ret = cvt(syscall3(SYS_LSEEK, fd, offset as usize, whence))?;
        Ok(ret as isize)
    }
}

/// Exit the current process
pub fn exit(code: i32) -> ! {
    unsafe {
        syscall1(SYS_EXIT, code as usize);
    }
    loop {}
}

/// Get current PID
pub fn getpid() -> usize {
    unsafe { syscall0(SYS_GETPID) }
}

/// Set the current process name (kernel stores up to 15 bytes; NUL not required).
pub fn set_process_name(name: &str) -> Result<()> {
    let len = name.len().min(15);
    if len == 0 {
        return Err(crate::error::Error::new(crate::error::EINVAL));
    }
    unsafe {
        cvt_unit(syscall2(
            SYS_SET_PROCESS_NAME,
            name.as_ptr() as usize,
            len,
        ))
    }
}

pub fn stop_progress() -> Result<usize> {
    unsafe { cvt(syscall0(SYS_STOP_PROGRESS)) }
}

pub fn drm_page_flip(fb_id: u32) -> Result<usize> {
    unsafe { cvt(syscall1(SYS_DRM_PAGE_FLIP, fb_id as usize)) }
}

pub fn drm_get_caps() -> Result<crate::DrmCaps> {
    let mut caps = crate::DrmCaps { has_3d: false, has_cursor: false, max_width: 0, max_height: 0 };
    unsafe {
        cvt(syscall1(SYS_DRM_GET_CAPS, &mut caps as *mut _ as usize))?;
    }
    Ok(caps)
}

pub fn drm_alloc_buffer(size: usize) -> Result<u32> {
    unsafe { cvt(syscall1(SYS_DRM_ALLOC_BUFFER, size)).map(|v| v as u32) }
}

pub fn drm_create_fb(gem_handle: u32, width: u32, height: u32, pitch: u32) -> Result<u32> {
    unsafe { cvt(syscall4(SYS_DRM_CREATE_FB, gem_handle as usize, width as usize, height as usize, pitch as usize)).map(|v| v as u32) }
}

pub fn drm_map_handle(handle_id: u32) -> Result<usize> {
    unsafe { cvt(syscall1(SYS_DRM_MAP_HANDLE, handle_id as usize)) }
}

/// Yield the CPU (cooperative scheduling hint)
pub fn sched_yield() -> Result<()> {
    unsafe { cvt_unit(syscall0(SYS_YIELD)) }
}

/// Spawn a new process from an ELF buffer with an optional name
pub fn spawn(buf: &[u8], name: Option<&str>) -> Result<usize> {
    let mut name_buf = [0u8; 16];
    let name_ptr = spawn_name_ptr(name, &mut name_buf);
    unsafe { cvt(syscall3(SYS_SPAWN, buf.as_ptr() as usize, buf.len(), name_ptr)) }
}

/// Spawn a new process from an ELF buffer, replacing stdin/stdout/stderr
pub fn spawn_with_stdio(buf: &[u8], name: Option<&str>, fd_in: usize, fd_out: usize, fd_err: usize) -> Result<usize> {
    let mut name_buf = [0u8; 16];
    let name_ptr = spawn_name_ptr(name, &mut name_buf);
    unsafe {
        cvt(syscall6(
            SYS_SPAWN_WITH_STDIO,
            buf.as_ptr() as usize,
            buf.len(),
            name_ptr,
            fd_in,
            fd_out,
            fd_err,
        ))
    }
}

/// Spawn leyendo el ejecutable desde el VFS en el kernel (recomendado para binarios grandes, p. ej. cargo).
pub fn spawn_with_stdio_path(path: &str, name: Option<&str>, fd_in: usize, fd_out: usize, fd_err: usize) -> Result<usize> {
    let mut path_buf = [0u8; 1024];
    let path_ptr = path_to_nul_stack(path, &mut path_buf)? as usize;
    let mut name_buf = [0u8; 16];
    let name_ptr = spawn_name_ptr(name, &mut name_buf);
    unsafe {
        cvt(syscall6(
            SYS_SPAWN_WITH_STDIO_PATH,
            path_ptr,
            name_ptr,
            fd_in,
            fd_out,
            fd_err,
            0,
        ))
    }
}

/// List processes and their state
pub fn get_process_list(buf: &mut [crate::ProcessInfo]) -> Result<usize> {
    unsafe { cvt(syscall2(SYS_GET_PROCESS_LIST, buf.as_mut_ptr() as usize, buf.len())) }
}

/// Kill a process by PID with a signal
pub fn kill(pid: usize, sig: usize) -> Result<()> {
    unsafe { cvt_unit(syscall2(SYS_KILL, pid, sig)) }
}

/// Fork the current process
pub fn fork() -> Result<usize> {
    unsafe { cvt(syscall0(SYS_FORK)) }
}

/// Get the parent PID
pub fn getppid() -> usize {
    unsafe { syscall0(SYS_GETPPID) }
}

// unlink definido más abajo junto con mkdir (versión con path+len)

/// rename(2): rutas terminadas en NUL (copiadas a buffer interno).
pub fn rename(old_path: &str, new_path: &str) -> Result<()> {
    let mut old_buf = [0u8; 1024];
    let mut new_buf = [0u8; 1024];
    let old_ptr = path_to_nul_stack(old_path, &mut old_buf)?;
    let new_ptr = path_to_nul_stack(new_path, &mut new_buf)?;
    unsafe { cvt_unit(syscall2(SYS_RENAME, old_ptr as usize, new_ptr as usize)) }
}

/// Crear hilo: `stack_top` es el tope del stack (alineado, crece hacia abajo), `entry` función
/// user con convención C, `arg` se pasa en **rdi**.
pub fn thread_create(stack_top: usize, entry: usize, arg: usize) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_THREAD_CREATE, stack_top, entry, arg)) }
}

/// Change signal action
pub fn sigaction(signum: usize, act: usize, oldact: usize) -> Result<()> {
    unsafe { cvt_unit(syscall3(SYS_SIGACTION, signum, act, oldact)) }
}

/// Change signal mask (blocked signals)
/// how: 0=BLOCK, 1=UNBLOCK, 2=SETMASK
pub fn sigprocmask(how: usize, set: usize, oldset: usize) -> Result<()> {
    unsafe { cvt_unit(syscall3(SYS_SIGPROCMASK, how, set, oldset)) }
}

/// Create an anonymous pipe.
/// On return, fds[0] is the read end and fds[1] is the write end.
pub fn pipe(fds: &mut [u32; 2]) -> Result<()> {
    unsafe { cvt_unit(syscall1(SYS_PIPE, fds.as_mut_ptr() as usize)) }
}

/// List directory children.
/// Writes newline-separated filenames into `buf` and returns the number of bytes written.
pub fn readdir(path: &str, buf: &mut [u8]) -> Result<usize> {
    let mut path_buf = [0u8; 1024];
    let ptr = path_to_nul_stack(path, &mut path_buf)?;
    unsafe {
        cvt(syscall3(SYS_READDIR, ptr as usize, buf.as_mut_ptr() as usize, buf.len()))
    }
}

/// Registrar argv para un proceso hijo justo después de spawn.
/// Debe llamarse inmediatamente después de spawn_with_stdio, antes de yield.
pub fn set_child_args(child_pid: usize, args: &[u8]) -> Result<()> {
    unsafe {
        cvt_unit(syscall3(SYS_SET_CHILD_ARGS, child_pid, args.as_ptr() as usize, args.len()))
    }
}

/// Obtener los argumentos del proceso actual. Devuelve los bytes escritos.
/// Formato: argv[0]\0argv[1]\0... (NUL-separados).
pub fn get_process_args(buf: &mut [u8]) -> usize {
    unsafe { syscall2(SYS_GET_PROCESS_ARGS, buf.as_mut_ptr() as usize, buf.len()) }
}

/// Remove a file. Only /tmp/* paths supported currently.
pub fn unlink(path: &str) -> Result<()> {
    let mut buf = [0u8; 1024];
    let ptr = path_to_nul_stack(path, &mut buf)?;
    unsafe { cvt_unit(syscall1(SYS_UNLINK, ptr as usize)) }
}

/// Create a directory. Only /tmp/* paths supported currently.
pub fn mkdir(path: &str, mode: usize) -> Result<()> {
    let mut buf = [0u8; 1024];
    let ptr = path_to_nul_stack(path, &mut buf)?;
    unsafe { cvt_unit(syscall2(SYS_MKDIR, ptr as usize, mode)) }
}

/// Wait for a child process to exit (cualquier hijo).
pub fn waitpid(status: *mut u32) -> Result<usize> {
    unsafe { cvt(syscall1(SYS_WAIT, status as usize)) }
}

/// Esperar a que termine el hijo `pid` (0 = cualquier hijo, igual que [`waitpid`]).
/// Espera bloqueante al hijo con PID dado.
pub fn wait_pid(status: *mut u32, pid: usize) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_WAIT_PID, status as usize, pid, 0)) }
}

/// Espera no-bloqueante (WNOHANG=1).
/// Devuelve Ok(0) si ningún hijo ha terminado todavía,
/// Ok(pid) si uno ha terminado, Err si no hay hijos.
pub fn wait_pid_nohang(status: *mut u32, pid: usize) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_WAIT_PID, status as usize, pid, 1)) }
}


// mkdir definido más arriba junto con unlink

/// Stat structure used by fstat/fstat_at
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
}

/// fstat(fd, stat)
pub fn fstat(fd: usize, stat: &mut Stat) -> Result<()> {
    unsafe {
        cvt_unit(syscall2(
            SYS_FSTAT,
            fd,
            stat as *mut Stat as usize,
        ))
    }
}

/// fstatat(dirfd, path, stat, flags)
pub fn fstat_at(dirfd: usize, path: &str, stat: &mut Stat, flags: usize) -> Result<()> {
    let mut buf = [0u8; 1024];
    let ptr = path_to_nul_stack(path, &mut buf)?;
    unsafe {
        cvt_unit(syscall4(
            SYS_FSTATAT,
            dirfd,
            ptr as usize,
            stat as *mut Stat as usize,
            flags,
        ))
    }
}

/// mmap(addr, length, prot, flags, fd, offset)
pub fn mmap(
    addr: usize,
    length: usize,
    prot: usize,
    flags: usize,
    fd: isize,
    offset: usize,
) -> Result<usize> {
    unsafe {
        cvt(syscall6(
            SYS_MMAP,
            addr,
            length,
            prot,
            flags,
            fd as usize,
            offset,
        ))
    }
}

/// munmap(addr, length)
pub fn munmap(addr: usize, length: usize) -> Result<()> {
    unsafe { cvt_unit(syscall2(SYS_MUNMAP, addr, length)) }
}

/// socket(domain, type, protocol)
pub fn socket(domain: usize, ty: usize, protocol: usize) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_SOCKET, domain, ty, protocol)) }
}

/// bind(fd, addr_ptr, addr_len)
pub fn bind(fd: usize, addr_ptr: usize, addr_len: usize) -> Result<()> {
    unsafe { cvt_unit(syscall3(SYS_BIND, fd, addr_ptr, addr_len)) }
}

/// listen(fd, backlog)
pub fn listen(fd: usize, backlog: usize) -> Result<()> {
    unsafe { cvt_unit(syscall2(SYS_LISTEN, fd, backlog)) }
}

/// accept(fd, addr_ptr, addr_len_ptr)
pub fn accept(fd: usize, addr_ptr: usize, addr_len_ptr: usize) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_ACCEPT, fd, addr_ptr, addr_len_ptr)) }
}

/// connect(fd, addr_ptr, addr_len)
pub fn connect(fd: usize, addr_ptr: usize, addr_len: usize) -> Result<()> {
    unsafe { cvt_unit(syscall3(SYS_CONNECT, fd, addr_ptr, addr_len)) }
}

/// gettid() - get thread ID
pub fn gettid() -> usize {
    unsafe { syscall0(SYS_GETTID) }
}

/// sched_setaffinity(pid, cpu_id) - set CPU affinity for process
/// pid=0 means current process. cpu_id=u32::MAX means any CPU (clear affinity).
pub fn sched_setaffinity(pid: usize, cpu_id: u32) -> Result<()> {
    unsafe { cvt_unit(syscall2(SYS_SCHED_SETAFFINITY, pid, cpu_id as usize)) }
}

/// register_log_hud(pid) - Registrar PID que recibirá líneas de log del kernel por IPC (HUD).
/// pid=0 para desregistrar. Llamar desde smithay_app cuando esté listo para mostrar el HUD.
pub fn register_log_hud(pid: u32) -> Result<()> {
    unsafe { cvt_unit(syscall1(SYS_REGISTER_LOG_HUD, pid as usize)) }
}

const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;

/// futex_wait(uaddr, val) - wait on a futex if *uaddr == val
pub fn futex_wait(uaddr: *const core::sync::atomic::AtomicI32, val: i32) -> Result<()> {
    unsafe { cvt_unit(syscall3(SYS_FUTEX, uaddr as usize, FUTEX_WAIT, val as usize)) }
}

/// futex_wake(uaddr, count) - wake up to `count` waiters on a futex
pub fn futex_wake(uaddr: *const core::sync::atomic::AtomicI32, count: u32) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_FUTEX, uaddr as usize, FUTEX_WAKE, count as usize)) }
}

/// ftruncate(fd, length) - change the size of a file
pub fn ftruncate(fd: usize, length: usize) -> Result<()> {
    unsafe { cvt_unit(syscall2(SYS_FTRUNCATE, fd, length)) }
}

/// Send a generic command to the GPU backend (VirtIO or NVIDIA)
pub fn gpu_command(kind: usize, command: usize, payload: &[u8]) -> Result<usize> {
    unsafe {
        cvt(syscall4(
            SYS_GPU_COMMAND,
            kind,
            command,
            payload.as_ptr() as usize,
            payload.len(),
        ))
    }
}

/// ioctl(fd, request, arg) - device control
pub fn ioctl(fd: usize, request: usize, arg: usize) -> Result<usize> {
    unsafe { cvt(syscall3(SYS_IOCTL, fd, request, arg)) }
}
/// Get the active GPU backend type (0=VirtIO, 1=NVIDIA, 2=Software)
pub fn gpu_get_backend() -> Result<usize> {
    unsafe { cvt(syscall0(SYS_GET_GPU_BACKEND)) }
}
/// Get system-wide statistics (uptime, memory, CPU load, etc.)
pub fn get_system_stats(stats: &mut crate::SystemStats) -> Result<usize> {
    unsafe { cvt(syscall1(SYS_GET_SYSTEM_STATS, stats as *mut _ as usize)) }
}

/// Permite habilitar o deshabilitar el rastreo de syscalls para un proceso.
/// Si pid == 0, se aplica al proceso actual.
pub fn strace(pid: u32, enable: bool) -> Result<()> {
    unsafe { cvt_unit(syscall2(SYS_STRACE, pid as usize, if enable { 1 } else { 0 })) }
}
