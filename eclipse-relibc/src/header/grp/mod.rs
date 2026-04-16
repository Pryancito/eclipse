//! grp.h - Group database
use crate::types::*;

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn getgrnam(_name: *const c_char) -> *mut group {
    core::ptr::null_mut()
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn getgrgid(_gid: gid_t) -> *mut group {
    core::ptr::null_mut()
}
