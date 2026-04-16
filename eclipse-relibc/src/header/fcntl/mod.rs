//! fcntl.h - File control
use crate::types::*;

// --- fcntl commands (compatible with Linux for rustix/x11rb) ---
pub const F_DUPFD:   c_int = 0;
pub const F_GETFD:   c_int = 1;
pub const F_SETFD:   c_int = 2;
pub const F_GETFL:   c_int = 3;
pub const F_SETFL:   c_int = 4;
pub const F_SETLK:   c_int = 6;
pub const F_SETLKW:  c_int = 7;
pub const F_GETLK:   c_int = 5;

// --- File descriptor flags ---
pub const FD_CLOEXEC: c_int = 1;

// --- File access modes ---
pub const O_RDONLY:  c_int = 0x0000;
pub const O_WRONLY:  c_int = 0x0001;
pub const O_RDWR:    c_int = 0x0002;
pub const O_ACCMODE: c_int = 0x0003;

// --- Open flags ---
pub const O_CREAT:   c_int = 0x0040;
pub const O_EXCL:    c_int = 0x0080;
pub const O_NOCTTY:  c_int = 0x0100;
pub const O_TRUNC:   c_int = 0x0200;
pub const O_APPEND:  c_int = 0x0400;
pub const O_NONBLOCK:c_int = 0x0800;
pub const O_DIRECTORY:c_int = 0x10000;
pub const O_CLOEXEC: c_int = 0x80000;

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, mut ap: ...) -> c_int {
    let arg = ap.arg::<usize>();
    match crate::eclipse_syscall::call::fcntl(fd as usize, cmd as usize, arg) {
        Ok(v) => v as c_int,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mut ap: ...) -> c_int {
    let mode = if (flags & O_CREAT) != 0 {
        ap.arg::<mode_t>()
    } else {
        0
    };
    
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match crate::eclipse_syscall::call::open(path_str, flags as usize) {
        Ok(fd) => fd as c_int,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn open(path: *const c_char, flags: c_int, ...) -> c_int;
}
