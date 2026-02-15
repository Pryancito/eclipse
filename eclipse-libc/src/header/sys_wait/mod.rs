//! sys/wait.h - Wait for process termination
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn wait(stat_loc: *mut c_int) -> pid_t {
    waitpid(-1, stat_loc, 0)
}

#[no_mangle]
pub unsafe extern "C" fn waitpid(_pid: pid_t, _stat_loc: *mut c_int, _options: c_int) -> pid_t {
    // Stub
    -1
}
