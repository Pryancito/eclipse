//! fcntl.h - File control
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn fcntl(_fd: c_int, _cmd: c_int, _arg: ...) -> c_int {
    // Stub
    0
}
