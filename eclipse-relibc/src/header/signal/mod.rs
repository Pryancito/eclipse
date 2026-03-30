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

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn signal(_signum: c_int, _handler: Option<OsSigHandlerPtr>) -> Option<OsSigHandlerPtr> {
    // Stub: return SIG_DFL (0)
    None
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn kill(pid: pid_t, sig: c_int) -> c_int {
    use eclipse_syscall::call::kill;
    match kill(pid as usize, sig as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigaction(signum: c_int, act: *const sigaction, oldact: *mut sigaction) -> c_int {
    use eclipse_syscall::call::sigaction;
    match sigaction(signum as usize, act as usize, oldact as usize) {
        Ok(_) => 0,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigemptyset(set: *mut sigset_t) -> c_int {
    if !set.is_null() {
        (*set).sig[0] = 0;
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigaddset(set: *mut sigset_t, signum: c_int) -> c_int {
    if !set.is_null() && signum > 0 && signum <= 64 {
        (*set).sig[0] |= 1 << (signum - 1);
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sigprocmask(_how: c_int, _set: *const sigset_t, _oldset: *mut sigset_t) -> c_int {
    0 // Stub
}
