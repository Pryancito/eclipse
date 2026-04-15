//! sys/uio.h - Vectored I/O
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct iovec {
    pub iov_base: *mut c_void,
    pub iov_len: size_t,
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn readv(_fd: c_int, _iov: *const iovec, _iovcnt: c_int) -> ssize_t {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn writev(_fd: c_int, _iov: *const iovec, _iovcnt: c_int) -> ssize_t {
    -1
}
