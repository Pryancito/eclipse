//! sys/eventfd.h - Event file descriptors
use crate::types::*;

pub const EFD_SEMAPHORE: c_int = 0o0000001;
pub const EFD_CLOEXEC: c_int = 0o2000000;
pub const EFD_NONBLOCK: c_int = 0o0004000;

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn eventfd(_initval: c_uint, _flags: c_int) -> c_int {
    // For now, return -1 as it's not yet implemented in kernel
    // but the symbol must exist to link.
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn eventfd_read(_fd: c_int, _value: *mut u64) -> c_int {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn eventfd_write(_fd: c_int, _value: u64) -> c_int {
    -1
}
