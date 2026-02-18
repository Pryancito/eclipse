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

/// Yield the CPU (cooperative scheduling hint)
pub fn sched_yield() -> Result<()> {
    unsafe { cvt_unit(syscall0(SYS_YIELD)) }
}

/// Spawn a new process from an ELF buffer
pub fn spawn(buf: &[u8]) -> Result<usize> {
    unsafe { cvt(syscall2(SYS_SPAWN, buf.as_ptr() as usize, buf.len())) }
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

