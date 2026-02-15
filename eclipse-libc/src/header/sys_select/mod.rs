//! sys/select.h - Select implementation
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn select(_nfds: c_int, _readfds: *mut fd_set, _writefds: *mut fd_set, _exceptfds: *mut fd_set, _timeout: *mut timeval) -> c_int {
    0 // Stub: no fds ready
}
