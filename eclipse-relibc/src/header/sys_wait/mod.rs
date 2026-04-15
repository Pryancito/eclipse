//! sys/wait.h - Wait for process termination
use crate::types::*;

/// WNOHANG: return immediately if no child has exited.
pub const WNOHANG: c_int = 1;
/// WUNTRACED: also return for stopped children.
pub const WUNTRACED: c_int = 2;

/// Macros to decode the wait status word.
#[inline]
pub unsafe fn WIFEXITED(status: c_int) -> bool {
    (status & 0x7f) == 0
}
#[inline]
pub unsafe fn WEXITSTATUS(status: c_int) -> c_int {
    (status >> 8) & 0xff
}
#[inline]
pub unsafe fn WIFSIGNALED(status: c_int) -> bool {
    ((status & 0x7f) != 0) && ((status & 0x7f) != 0x7f)
}
#[inline]
pub unsafe fn WTERMSIG(status: c_int) -> c_int {
    status & 0x7f
}
#[inline]
pub unsafe fn WIFSTOPPED(status: c_int) -> bool {
    (status & 0xff) == 0x7f
}
#[inline]
pub unsafe fn WSTOPSIG(status: c_int) -> c_int {
    (status >> 8) & 0xff
}

#[cfg(all(not(any(test, feature = "host-testing")), any(eclipse_target, feature = "eclipse-syscall")))]
#[no_mangle]
pub unsafe extern "C" fn wait(stat_loc: *mut c_int) -> pid_t {
    waitpid(-1, stat_loc, 0)
}

#[cfg(all(not(any(test, feature = "host-testing")), any(eclipse_target, feature = "eclipse-syscall")))]
#[no_mangle]
pub unsafe extern "C" fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t {
    let mut st: u32 = 0;
    let wait_pid = if pid <= 0 { 0usize } else { pid as usize };
    let nohang = (options & WNOHANG) != 0;
    let result = if nohang {
        eclipse_syscall::call::wait_pid_nohang(&mut st as *mut u32, wait_pid)
    } else {
        eclipse_syscall::call::wait_pid(&mut st as *mut u32, wait_pid)
    };
    match result {
        Ok(child) => {
            if !stat_loc.is_null() {
                *stat_loc = st as c_int;
            }
            child as pid_t
        }
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

/// wait3 — wait with resource usage (rusage ignored).
#[cfg(all(not(any(test, feature = "host-testing")), any(eclipse_target, feature = "eclipse-syscall")))]
#[no_mangle]
pub unsafe extern "C" fn wait3(stat_loc: *mut c_int, options: c_int, _rusage: *mut c_void) -> pid_t {
    waitpid(-1, stat_loc, options)
}

/// wait4 — wait for specific pid with resource usage (rusage ignored).
#[cfg(all(not(any(test, feature = "host-testing")), any(eclipse_target, feature = "eclipse-syscall")))]
#[no_mangle]
pub unsafe extern "C" fn wait4(pid: pid_t, stat_loc: *mut c_int, options: c_int, _rusage: *mut c_void) -> pid_t {
    waitpid(pid, stat_loc, options)
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn wait(stat_loc: *mut c_int) -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
}
