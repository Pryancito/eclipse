//! sys/select.h - Select implementation
use crate::types::*;

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn select(_nfds: c_int, _readfds: *mut fd_set, _writefds: *mut fd_set, _exceptfds: *mut fd_set, _timeout: *mut timeval) -> c_int {
    0 // Stub: no fds ready
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn __fdelt_chk(fd: c_int) -> c_long {
    (fd / 64) as c_long
}
