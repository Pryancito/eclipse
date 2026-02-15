//! sys/stat.h - Data returned by the stat() function
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn stat(_path: *const c_char, _buf: *mut crate::types::stat) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn fstat(_fd: c_int, _buf: *mut crate::types::stat) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn lstat(_path: *const c_char, _buf: *mut crate::types::stat) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn chmod(_path: *const c_char, _mode: mode_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn fchmod(_fd: c_int, _mode: mode_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn mkdir(path: *const c_char, mode: mode_t) -> c_int {
    let path_len = crate::header::string::strlen(path);
    let res = eclipse_syscall::syscall3(
        35, // SYS_MKDIR
        path as u64 as usize,
        path_len as u64 as usize,
        mode as u64 as usize,
    );
    if res == usize::MAX {
        -1
    } else {
        res as c_int
    }
}

#[no_mangle]
pub unsafe extern "C" fn umask(_mask: mode_t) -> mode_t {
    0
}
