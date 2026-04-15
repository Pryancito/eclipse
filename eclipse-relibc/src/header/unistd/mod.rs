//! unistd.h - POSIX OS API
use crate::types::*;
use crate::internal_alloc::{malloc, free};
use crate::eclipse_syscall::call::{write as sys_write, read as sys_read, close as sys_close, open as sys_open, lseek as sys_lseek, exit as sys_exit, getpid as sys_getpid, spawn as sys_spawn, fstat as sys_fstat, ftruncate as sys_ftruncate, Stat as sys_Stat};
#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
use crate::header::time::nanosleep;

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
static mut FORCE_KEEP: i32 = 0;

// Nota: había stubs/externs para host bajo un cfg imposible (`all(..., not())`).
// Se eliminan para evitar referencias a `target_os` y porque nunca se compilaban.

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, _args: ...) -> c_int {
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match sys_open(path_str, flags as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t {
    let slice = core::slice::from_raw_parts(buf as *const u8, count);
    match sys_write(fd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t {
    let slice = core::slice::from_raw_parts_mut(buf as *mut u8, count);
    match sys_read(fd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    match sys_close(fd as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn lseek(fd: c_int, offset: off_t, whence: c_int) -> off_t {
    match sys_lseek(fd as usize, offset as isize, whence as usize) {
        Ok(off) => off as off_t,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getpid() -> pid_t {
    sys_getpid() as pid_t
}

#[cfg(all(not(any(test, feature = "host-testing")), any(feature = "eclipse-syscall", eclipse_target)))]
#[no_mangle]
pub unsafe extern "C" fn fork() -> pid_t {
    use crate::eclipse_syscall::call::fork;
    match fork() {
        Ok(pid) => pid as pid_t,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn vfork() -> pid_t {
    -1 // Stub
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn execl(_path: *const c_char, _arg0: *const c_char, _args: ...) -> c_int {
    -1 // Stub
}

#[cfg(all(not(any(test, feature = "host-testing")), not(feature = "use_std")))]
#[no_mangle]
pub unsafe extern "C" fn execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    let envp = crate::header::stdlib::environ_ptr();
    execve(path, argv, envp)
}

#[cfg(all(not(any(test, feature = "host-testing")), not(feature = "use_std")))]
#[no_mangle]
pub unsafe extern "C" fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    let envp = crate::header::stdlib::environ_ptr();
    execvpe(file, argv, envp)
}

/// execvpe: search PATH for `file` if it contains no slash, then execve.
#[cfg(all(not(any(test, feature = "host-testing")), not(feature = "use_std")))]
#[no_mangle]
pub unsafe extern "C" fn execvpe(file: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int {
    if file.is_null() {
        *crate::header::errno::__errno_location() = 22; // EINVAL
        return -1;
    }
    let file_str = core::ffi::CStr::from_ptr(file).to_str().unwrap_or("");
    // If the file contains a slash, use it directly.
    if file_str.contains('/') {
        return execve(file, argv, envp);
    }
    // Search PATH directories.
    let path_env = crate::header::stdlib::getenv_str("PATH").unwrap_or("/bin:/usr/bin");
    // Copy to a stack buffer so we can iterate safely.
    let mut path_buf = [0u8; 4096];
    let plen = path_env.len().min(4095);
    path_buf[..plen].copy_from_slice(&path_env.as_bytes()[..plen]);
    let path_str = core::str::from_utf8(&path_buf[..plen]).unwrap_or("/bin:/usr/bin");
    let mut candidate_buf = [0u8; 1024];
    for dir in path_str.split(':') {
        let dir_len = dir.len();
        let file_len = file_str.len();
        if dir_len + 1 + file_len >= candidate_buf.len() { continue; }
        candidate_buf[..dir_len].copy_from_slice(dir.as_bytes());
        candidate_buf[dir_len] = b'/';
        candidate_buf[dir_len + 1..dir_len + 1 + file_len].copy_from_slice(file_str.as_bytes());
        candidate_buf[dir_len + 1 + file_len] = 0;
        let ret = crate::eclipse_syscall::call::execve(
            candidate_buf.as_ptr() as usize,
            argv as usize,
            envp as usize,
        );
        // execve only returns on error; ENOENT (2) means try next dir.
        match ret {
            Err(e) if e.errno == 2 => { /* ENOENT, try next dir */ }
            Err(e) => {
                *crate::header::errno::__errno_location() = e.errno as c_int;
                return -1;
            }
            Ok(_) => { return 0; } // should not happen
        }
    }
    *crate::header::errno::__errno_location() = 2; // ENOENT
    -1
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int {
    use crate::eclipse_syscall::call::execve as sys_execve;
    match sys_execve(path as usize, argv as usize, envp as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn pipe(pipefd: *mut c_int) -> c_int {
    if pipefd.is_null() {
        *crate::header::errno::__errno_location() = 14; // EFAULT
        return -1;
    }
    let mut fds = [0u32; 2];
    match crate::eclipse_syscall::call::pipe(&mut fds) {
        Ok(_) => {
            *pipefd = fds[0] as c_int;
            *pipefd.add(1) = fds[1] as c_int;
            0
        }
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn pipe2(pipefd: *mut c_int, flags: c_int) -> c_int {
    if pipefd.is_null() {
        *crate::header::errno::__errno_location() = 14; // EFAULT
        return -1;
    }
    let mut fds = [0u32; 2];
    match crate::eclipse_syscall::call::pipe2(&mut fds, flags as usize) {
        Ok(_) => {
            *pipefd = fds[0] as c_int;
            *pipefd.add(1) = fds[1] as c_int;
            0
        }
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getuid() -> uid_t {
    0 // Root
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getgid() -> gid_t {
    0 // Root
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn setuid(_uid: uid_t) -> c_int {
    0 // Stub
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn setgid(_gid: gid_t) -> c_int {
    0 // Stub
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn unlink(pathname: *const c_char) -> c_int {
    use crate::eclipse_syscall::call::unlink;
    let path_str = core::ffi::CStr::from_ptr(pathname).to_str().unwrap_or("");
    match unlink(path_str) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn sysconf(name: c_int) -> c_long {
    match name {
        4 => 1024, // _SC_OPEN_MAX
        _ => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getdtablesize() -> c_int {
    1024
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn sleep(seconds: c_uint) -> c_uint {
    let req = crate::types::timespec {
        tv_sec: seconds as time_t,
        tv_nsec: 0,
    };
    nanosleep(&req, core::ptr::null_mut());
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn usleep(usec: useconds_t) -> c_int {
    let req = crate::types::timespec {
        tv_sec: (usec / 1_000_000) as time_t,
        tv_nsec: ((usec % 1_000_000) * 1000) as c_long,
    };
    nanosleep(&req, core::ptr::null_mut())
}

#[cfg(not(eclipse_target))]
extern "C" {
    pub fn sleep(seconds: c_uint) -> c_uint;
    pub fn usleep(usec: useconds_t) -> c_int;
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn _exit(status: c_int) -> ! {
    eclipse_syscall::syscall1(eclipse_syscall::SYS_EXIT, status as usize);
    loop {}
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn dup2(old: c_int, new: c_int) -> c_int {
    match crate::eclipse_syscall::call::dup2(old as usize, new as usize) {
        Ok(fd) => fd as c_int,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn gethostname(name: *mut c_char, len: size_t) -> c_int {
    let s = b"eclipse\0";
    let copy_len = core::cmp::min(len, s.len());
    core::ptr::copy_nonoverlapping(s.as_ptr(), name as *mut u8, copy_len);
    0
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn chdir(path: *const c_char) -> c_int {
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match crate::eclipse_syscall::call::chdir(path_str) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: size_t) -> *mut c_char {
    if size < 2 { return core::ptr::null_mut(); }
    match crate::eclipse_syscall::call::getcwd(buf as usize, size) {
        Ok(_) => buf,
        Err(_) => {
            // Fallback to "/" if the kernel call fails.
            *buf = b'/' as c_char;
            *buf.add(1) = 0;
            buf
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn isatty(fd: c_int) -> c_int {
    if fd >= 0 && fd <= 2 {
        1
    } else {
        0
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn geteuid() -> uid_t {
    0
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getegid() -> gid_t {
    0
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn seteuid(_euid: uid_t) -> c_int {
    0
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn setegid(_egid: gid_t) -> c_int {
    0
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getppid() -> pid_t {
    use crate::eclipse_syscall::call::getppid;
    getppid() as pid_t
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getpgrp() -> pid_t {
    match crate::eclipse_syscall::call::getpgid(0) {
        Ok(pgid) => pgid as pid_t,
        Err(_) => getpid(),
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn setpgid(pid: pid_t, pgid: pid_t) -> c_int {
    match crate::eclipse_syscall::call::setpgid(pid as usize, pgid as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn link(_oldpath: *const c_char, _newpath: *const c_char) -> c_int {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn chown(_path: *const c_char, _owner: uid_t, _group: gid_t) -> c_int {
    0
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn fchown(_fd: c_int, _owner: uid_t, _group: gid_t) -> c_int {
    0
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn spawn(path: *const c_char, _argv: *const *const c_char, _envp: *const *const c_char) -> pid_t {
    // For now, our sys_spawn only takes an ELF buffer. 
    // We need to read the file first.
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match sys_open(path_str, 0) { // O_RDONLY
        Ok(fd) => {
             // In eclipse-libc we don't have a good way to read the whole file to a buffer easily without fstat/malloc
             // But fstat is implemented. Let's try.
             let mut st = sys_Stat::default();
             if sys_fstat(fd, &mut st).is_ok() {
                  let size = st.size as usize;
                  let ptr = malloc(size);
                  if !ptr.is_null() {
                       let buf = core::slice::from_raw_parts_mut(ptr as *mut u8, size);
                       if sys_read(fd, buf).is_ok() {
                            let res = match sys_spawn(buf, None) {
                                 Ok(pid) => pid as pid_t,
                                 Err(_) => -1,
                            };
                            free(ptr);
                            sys_close(fd).ok();
                            return res;
                       }
                  }
             }
             sys_close(fd).ok();
             -1
        },
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn ftruncate(fd: c_int, length: off_t) -> c_int {
    match sys_ftruncate(fd as usize, length as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn readlink(_path: *const c_char, _buf: *mut c_char, _bufsiz: size_t) -> ssize_t {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn dup(oldfd: c_int) -> c_int {
    match crate::eclipse_syscall::call::dup(oldfd as usize) {
        Ok(fd) => fd as c_int,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn dup3(oldfd: c_int, newfd: c_int, _flags: c_int) -> c_int {
    dup2(oldfd, newfd)
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match crate::eclipse_syscall::call::access(path_str, mode as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn setsid() -> pid_t {
    match crate::eclipse_syscall::call::setsid() {
        Ok(sid) => sid as pid_t,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getpgid(pid: pid_t) -> pid_t {
    match crate::eclipse_syscall::call::getpgid(pid as usize) {
        Ok(pgid) => pgid as pid_t,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn setpgrp() -> c_int {
    setpgid(0, 0)
}

/// tcgetpgrp — get foreground process group of terminal fd.
#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn tcgetpgrp(fd: c_int) -> pid_t {
    let mut pgid: pid_t = 0;
    // TIOCGPGRP = 0x540F
    let ret = crate::header::sys_ioctl::ioctl(fd, 0x540F, &mut pgid as *mut pid_t as *mut c_void);
    if ret < 0 {
        // Not a real terminal — return current pgrp as fallback.
        getpgrp()
    } else {
        pgid
    }
}

/// tcsetpgrp — set foreground process group of terminal fd.
#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn tcsetpgrp(fd: c_int, pgrp: pid_t) -> c_int {
    // TIOCSPGRP = 0x5410
    crate::header::sys_ioctl::ioctl(fd, 0x5410, &pgrp as *const pid_t as *mut c_void)
}

/// truncate(path, length) — set file size.
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn truncate(path: *const c_char, length: off_t) -> c_int {
    let path_str = core::ffi::CStr::from_ptr(path).to_str().unwrap_or("");
    match sys_open(path_str, 1) { // O_WRONLY
        Ok(fd) => {
            let ret = match sys_ftruncate(fd as usize, length as usize) {
                Ok(_) => 0,
                Err(e) => {
                    *crate::header::errno::__errno_location() = e.errno as c_int;
                    -1
                }
            };
            sys_close(fd as usize).ok();
            ret
        }
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

/// symlink — create a symbolic link (stub, Eclipse doesn't support symlinks yet).
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn symlink(_target: *const c_char, _linkpath: *const c_char) -> c_int {
    *crate::header::errno::__errno_location() = 38; // ENOSYS
    -1
}

/// fsync — flush file to disk (stub).
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn fsync(_fd: c_int) -> c_int {
    0
}

/// fdatasync — flush file data to disk (stub).
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn fdatasync(_fd: c_int) -> c_int {
    0
}

/// getlogin_r — get login name (always "root" on Eclipse OS).
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getlogin_r(buf: *mut c_char, bufsize: size_t) -> c_int {
    let name = b"root\0";
    if buf.is_null() || bufsize < name.len() { return 34; } // ERANGE
    core::ptr::copy_nonoverlapping(name.as_ptr(), buf as *mut u8, name.len());
    0
}

/// confstr — get configuration-defined string values (stub).
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn confstr(_name: c_int, _buf: *mut c_char, _len: size_t) -> size_t {
    0
}

/// getgroups — get supplementary group IDs (Eclipse OS: always just group 0).
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn getgroups(size: c_int, list: *mut gid_t) -> c_int {
    if size >= 1 && !list.is_null() {
        *list = 0;
    }
    1
}

/// setgroups — set supplementary group IDs (stub).
#[cfg(all(not(any(test, feature = "host-testing"))))]
#[no_mangle]
pub unsafe extern "C" fn setgroups(_size: size_t, _list: *const gid_t) -> c_int {
    0
}
