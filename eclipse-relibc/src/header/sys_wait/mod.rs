//! sys/wait.h - Wait for process termination
use crate::types::*;

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn wait(stat_loc: *mut c_int) -> pid_t {
    waitpid(-1, stat_loc, 0)
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn waitpid(_pid: pid_t, _stat_loc: *mut c_int, _options: c_int) -> pid_t {
    // Stub
    -1
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn wait(stat_loc: *mut c_int) -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
}
