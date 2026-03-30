//! sys/wait.h - Wait for process termination
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn wait(stat_loc: *mut c_int) -> pid_t {
    waitpid(-1, stat_loc, 0)
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn waitpid(_pid: pid_t, _stat_loc: *mut c_int, _options: c_int) -> pid_t {
    // Stub
    -1
}

#[cfg(any(test, feature = "host-testing", all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))]
extern "C" {
    pub fn wait(stat_loc: *mut c_int) -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
}
