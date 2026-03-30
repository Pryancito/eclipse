//! sys/wait.h - Wait for process termination
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn wait(stat_loc: *mut c_int) -> pid_t {
    waitpid(-1, stat_loc, 0)
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn waitpid(pid: pid_t, stat_loc: *mut c_int, _options: c_int) -> pid_t {
    let mut st: u32 = 0;
    let wait_pid = if pid <= 0 { 0usize } else { pid as usize };
    match eclipse_syscall::call::wait_pid(&mut st as *mut u32, wait_pid) {
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

#[cfg(any(test, feature = "host-testing", all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))]
extern "C" {
    pub fn wait(stat_loc: *mut c_int) -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
}
