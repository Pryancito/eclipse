//! pwd.h - Password database
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn getpwnam(_name: *const c_char) -> *mut passwd {
    core::ptr::null_mut()
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn getpwuid(_uid: uid_t) -> *mut passwd {
    core::ptr::null_mut()
}
