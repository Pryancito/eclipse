//! sys/timerfd.h - Timer file descriptors
use crate::types::*;

pub const TFD_TIMER_ABSTIME: c_int = 1;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn timerfd_create(_clockid: clockid_t, _flags: c_int) -> c_int {
    // Return a dummy FD that will be handled by our poll/read shims
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn timerfd_settime(_fd: c_int, _flags: c_int, _new_value: *const itimerspec, _old_value: *mut itimerspec) -> c_int {
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn timerfd_gettime(_fd: c_int, _curr_value: *mut itimerspec) -> c_int {
    0
}
