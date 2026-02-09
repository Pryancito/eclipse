//! High-level syscall wrappers

use crate::number::*;
use crate::error::*;
use crate::flag::*;

/// Exit the current process
pub fn exit(status: i32) -> ! {
    unsafe {
        crate::syscall1(SYS_EXIT, status as usize);
    }
    unreachable!()
}

/// Write to a file descriptor
pub fn write(fd: usize, buf: &[u8]) -> Result<usize> {
    cvt(unsafe {
        crate::syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
    })
}

/// Read from a file descriptor
pub fn read(fd: usize, buf: &mut [u8]) -> Result<usize> {
    cvt(unsafe {
        crate::syscall3(SYS_READ, fd, buf.as_mut_ptr() as usize, buf.len())
    })
}

/// Yield CPU to scheduler
pub fn sched_yield() -> Result<()> {
    cvt_unit(unsafe { crate::syscall0(SYS_YIELD) })
}

/// Get current process ID
pub fn getpid() -> usize {
    unsafe { crate::syscall0(SYS_GETPID) }
}

/// Get parent process ID
pub fn getppid() -> usize {
    unsafe { crate::syscall0(SYS_GETPPID) }
}

/// Open a file
pub fn open(path: &str, flags: usize) -> Result<usize> {
    cvt(unsafe {
        crate::syscall3(SYS_OPEN, path.as_ptr() as usize, path.len(), flags)
    })
}

/// Close a file descriptor
pub fn close(fd: usize) -> Result<()> {
    cvt_unit(unsafe { crate::syscall1(SYS_CLOSE, fd) })
}

/// Map memory
pub fn mmap(
    addr: usize,
    length: usize,
    prot: usize,
    flags: usize,
    fd: isize,
    offset: usize
) -> Result<usize> {
    cvt(unsafe {
        crate::syscall6(
            SYS_MMAP,
            addr,
            length,
            prot,
            flags,
            fd as usize,
            offset
        )
    })
}

/// Unmap memory
pub fn munmap(addr: usize, length: usize) -> Result<()> {
    cvt_unit(unsafe { crate::syscall2(SYS_MUNMAP, addr, length) })
}
