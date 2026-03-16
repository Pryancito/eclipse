//! High-level, type-safe syscall wrappers
use crate::error::{cvt, cvt_unit, Result};
use crate::number::*;
use crate::arch::*;

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
    unsafe {
        cvt(syscall3(
            SYS_OPEN,
            path.as_ptr() as usize,
            path.len(),
            flags,
        ))
    }
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
    let name_ptr = name.map(|s| s.as_ptr() as usize).unwrap_or(0);
    unsafe { cvt(syscall3(SYS_SPAWN, buf.as_ptr() as usize, buf.len(), name_ptr)) }
}

/// List processes and their state
pub fn get_process_list(buf: &mut [crate::ProcessInfo]) -> Result<usize> {
    unsafe { cvt(syscall2(SYS_GET_PROCESS_LIST, buf.as_mut_ptr() as usize, buf.len())) }
}

/// Kill a process by PID
pub fn kill(pid: usize) -> Result<()> {
    unsafe { cvt_unit(syscall1(SYS_KILL, pid)) }
}


/// mkdir(path, mode)
pub fn mkdir(path: &str, mode: usize) -> Result<()> {
    unsafe {
        cvt_unit(syscall3(
            SYS_MKDIR,
            path.as_ptr() as usize,
            path.len(),
            mode,
        ))
    }
}

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
    unsafe {
        cvt_unit(syscall4(
            SYS_FSTATAT,
            dirfd,
            path.as_ptr() as usize,
            path.len(),
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

