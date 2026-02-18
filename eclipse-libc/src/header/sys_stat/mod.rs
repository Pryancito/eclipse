use crate::types::*;
use eclipse_syscall::call::mkdir as sys_mkdir;

#[no_mangle]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut crate::types::stat) -> c_int {
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    let mut st = eclipse_syscall::call::Stat::default();
    match eclipse_syscall::call::fstat_at(0, path_str, &mut st, 0) {
        Ok(_) => {
            if !buf.is_null() {
                (*buf).st_size = st.size as off_t;
                (*buf).st_mode = st.mode as mode_t;
                // Fill other fields as needed
            }
            0
        },
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn fstat(fd: c_int, buf: *mut crate::types::stat) -> c_int {
    let mut st = eclipse_syscall::call::Stat::default();
    match eclipse_syscall::call::fstat(fd as usize, &mut st) {
        Ok(_) => {
            if !buf.is_null() {
                (*buf).st_size = st.size as off_t;
                (*buf).st_mode = st.mode as mode_t;
            }
            0
        },
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut crate::types::stat) -> c_int {
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    let mut st = eclipse_syscall::call::Stat::default();
    // AT_SYMLINK_NOFOLLOW = 0x100 usually, let's check eclipse-syscall
    match eclipse_syscall::call::fstat_at(0, path_str, &mut st, 0x100) {
        Ok(_) => {
            if !buf.is_null() {
                (*buf).st_size = st.size as off_t;
                (*buf).st_mode = st.mode as mode_t;
            }
            0
        },
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
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
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match sys_mkdir(path_str, mode as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn umask(_mask: mode_t) -> mode_t {
    0
}
