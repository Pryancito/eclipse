//! poll.h - Poll implementation
use crate::types::*;

/// Events bitmask: data ready to read.
pub const POLLIN: c_short = 0x0001;
/// Events bitmask: data ready to write.
pub const POLLOUT: c_short = 0x0004;
/// Events bitmask: error condition.
pub const POLLERR: c_short = 0x0008;
/// Events bitmask: hung up.
pub const POLLHUP: c_short = 0x0010;
/// Events bitmask: invalid request.
pub const POLLNVAL: c_short = 0x0020;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct pollfd {
    pub fd: c_int,
    pub events: c_short,
    pub revents: c_short,
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn poll(_fds: *mut pollfd, _nfds: nfds_t, _timeout: c_int) -> c_int {
    0
}
