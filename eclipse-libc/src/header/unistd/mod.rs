//! unistd.h - POSIX OS API
use crate::types::*;
use eclipse_syscall::call::{write as sys_write, read as sys_read, close as sys_close, open as sys_open, lseek as sys_lseek, exit as sys_exit, getpid as sys_getpid};

#[no_mangle]
static mut FORCE_KEEP: i32 = 0;

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, _mode: mode_t) -> c_int {
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match sys_open(path_str, flags as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

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
pub unsafe extern "C" fn lseek(fd: c_int, offset: off_t, whence: c_int) -> off_t {
    match sys_lseek(fd as usize, offset as isize, whence as usize) {
        Ok(off) => off as off_t,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn getpid() -> pid_t {
    sys_getpid() as pid_t
}

    #[no_mangle]
    pub unsafe extern "C" fn fork() -> pid_t {
        -1 // Stub
    }

    #[no_mangle]
    pub unsafe extern "C" fn vfork() -> pid_t {
        -1 // Stub
    }

    #[no_mangle]
    pub unsafe extern "C" fn execl(_path: *const c_char, _arg0: *const c_char, ...) -> c_int {
        -1 // Stub
    }

    #[no_mangle]
    pub unsafe extern "C" fn execv(_path: *const c_char, _argv: *const *const c_char) -> c_int {
        -1 // Stub
    }

    #[no_mangle]
    pub unsafe extern "C" fn execvp(_file: *const c_char, _argv: *const *const c_char) -> c_int {
        -1 // Stub
    }

    #[no_mangle]
    pub unsafe extern "C" fn pipe(pipefd: *mut c_int) -> c_int {
        if pipefd.is_null() { return -1; }
        unsafe {
            *pipefd = -1;
            *pipefd.add(1) = -1;
        }
        -1 // Stub
    }

#[no_mangle]
pub unsafe extern "C" fn pipe2(_pipefd: *mut c_int, _flags: c_int) -> c_int {
    unsafe {
        *_pipefd = -1;
        *_pipefd.add(1) = -1;
    }
    -1
}

#[no_mangle]
pub unsafe extern "C" fn getuid() -> uid_t {
    0 // Root
}

#[no_mangle]
pub unsafe extern "C" fn getgid() -> gid_t {
    0 // Root
}

#[no_mangle]
pub unsafe extern "C" fn setuid(_uid: uid_t) -> c_int {
    0 // Stub
}

#[no_mangle]
pub unsafe extern "C" fn setgid(_gid: gid_t) -> c_int {
    0 // Stub
}

#[no_mangle]
pub unsafe extern "C" fn unlink(_pathname: *const c_char) -> c_int {
    -1 // Stub
}

#[no_mangle]
pub unsafe extern "C" fn sysconf(name: c_int) -> c_long {
    match name {
        4 => 1024, // _SC_OPEN_MAX
        _ => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn getdtablesize() -> c_int {
    1024
}

#[no_mangle]
pub unsafe extern "C" fn sleep(_seconds: c_uint) -> c_uint {
    0
}

#[no_mangle]
pub unsafe extern "C" fn usleep(_usec: useconds_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn _exit(status: c_int) -> ! {
    let _ = sys_exit(status);
    loop {}
}

#[no_mangle]
pub unsafe extern "C" fn dup2(_old: c_int, _new: c_int) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn gethostname(name: *mut c_char, len: size_t) -> c_int {
    let s = b"eclipse\0";
    let copy_len = core::cmp::min(len, s.len());
    core::ptr::copy_nonoverlapping(s.as_ptr(), name as *mut u8, copy_len);
    0
}

#[no_mangle]
pub unsafe extern "C" fn chdir(_path: *const c_char) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: size_t) -> *mut c_char {
    if size < 2 { return core::ptr::null_mut(); }
    unsafe {
        *buf = b'/' as c_char;
        *buf.add(1) = 0;
    }
    buf
}

#[no_mangle]
pub unsafe extern "C" fn isatty(fd: c_int) -> c_int {
    if fd >= 0 && fd <= 2 {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn geteuid() -> uid_t {
    0
}

#[no_mangle]
pub unsafe extern "C" fn getegid() -> gid_t {
    0
}

#[no_mangle]
pub unsafe extern "C" fn seteuid(_euid: uid_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn setegid(_egid: gid_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn getppid() -> pid_t {
    1
}

#[no_mangle]
pub unsafe extern "C" fn getpgrp() -> pid_t {
    1
}

#[no_mangle]
pub unsafe extern "C" fn setpgid(_pid: pid_t, _pgid: pid_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn link(_oldpath: *const c_char, _newpath: *const c_char) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn chown(_path: *const c_char, _owner: uid_t, _group: gid_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn fchown(_fd: c_int, _owner: uid_t, _group: gid_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn spawn(_path: *const c_char, _argv: *const *const c_char, _envp: *const *const c_char) -> pid_t {
    -1
}
