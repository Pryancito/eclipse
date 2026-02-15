//! signal.h - Signals
use crate::types::*;

pub type OsSigHandlerPtr = unsafe extern "C" fn(c_int);

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigaction {
    pub sa_handler: Option<OsSigHandlerPtr>,
    pub sa_mask: sigset_t,
    pub sa_flags: c_int,
    pub sa_restorer: Option<unsafe extern "C" fn()>,
}

#[no_mangle]
pub unsafe extern "C" fn signal(_signum: c_int, _handler: Option<OsSigHandlerPtr>) -> Option<OsSigHandlerPtr> {
    // Stub: return SIG_DFL (0)
    None
}

#[no_mangle]
pub unsafe extern "C" fn kill(_pid: pid_t, _sig: c_int) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigaction(_signum: c_int, _act: *const sigaction, _oldact: *mut sigaction) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigemptyset(set: *mut sigset_t) -> c_int {
    if !set.is_null() {
        (*set).sig[0] = 0;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigaddset(set: *mut sigset_t, signum: c_int) -> c_int {
    if !set.is_null() && signum > 0 && signum <= 64 {
        (*set).sig[0] |= 1 << (signum - 1);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigprocmask(_how: c_int, _set: *const sigset_t, _oldset: *mut sigset_t) -> c_int {
    0 // Stub
}
