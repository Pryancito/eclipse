//! poll.h - Poll implementation
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct pollfd {
    pub fd: c_int,
    pub events: c_short,
    pub revents: c_short,
}

#[no_mangle]
pub unsafe extern "C" fn poll(_fds: *mut pollfd, _nfds: nfds_t, _timeout: c_int) -> c_int {
    0
}
