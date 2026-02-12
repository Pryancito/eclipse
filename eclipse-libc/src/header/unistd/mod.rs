//! unistd.h - POSIX OS API
use crate::types::*;
use eclipse_syscall::call::{write as sys_write, read as sys_read, close as sys_close};

#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t {
    let slice = core::slice::from_raw_parts(buf as *const u8, count);
    match sys_write(fd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t {
    let slice = core::slice::from_raw_parts_mut(buf as *mut u8, count);
    match sys_read(fd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    match sys_close(fd as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn spawn(buf: *const c_void, count: size_t) -> pid_t {
    let slice = core::slice::from_raw_parts(buf as *const u8, count);
    match eclipse_syscall::call::spawn(slice) {
        Ok(pid) => pid as pid_t,
        Err(_) => -1,
    }
}
