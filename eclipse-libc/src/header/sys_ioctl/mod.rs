//! sys/ioctl.h - I/O Control
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn ioctl(_fd: c_int, _request: c_ulong, _arg: *mut c_void) -> c_int {
    -1
}
