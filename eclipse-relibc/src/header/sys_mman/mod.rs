use crate::types::*;

pub const PROT_NONE: c_int = 0x0;
pub const PROT_READ: c_int = 0x1;
pub const PROT_WRITE: c_int = 0x2;
pub const PROT_EXEC: c_int = 0x4;

pub const MAP_SHARED: c_int = 0x01;
pub const MAP_PRIVATE: c_int = 0x02;
pub const MAP_FIXED: c_int = 0x10;
pub const MAP_ANONYMOUS: c_int = 0x20;

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn mmap(addr: *mut c_void, length: size_t, prot: c_int, flags: c_int, fd: c_int, offset: off_t) -> *mut c_void {
    let res = eclipse_syscall::syscall6(
        eclipse_syscall::number::SYS_MMAP,
        addr as usize,
        length,
        prot as usize,
        flags as usize,
        fd as usize,
        offset as usize
    );
    res as *mut c_void
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn mmap(addr: *mut c_void, length: size_t, prot: c_int, flags: c_int, fd: c_int, offset: off_t) -> *mut c_void;
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn munmap(addr: *mut c_void, length: size_t) -> c_int {
    let res = eclipse_syscall::syscall2(
        eclipse_syscall::number::SYS_MUNMAP,
        addr as usize,
        length
    );
    res as c_int
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn munmap(addr: *mut c_void, length: size_t) -> c_int;
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn getpagesize() -> c_int {
    4096
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn getpagesize() -> c_int;
}
