//! grp.h - Group database
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn getgrnam(_name: *const c_char) -> *mut group {
    core::ptr::null_mut()
}

#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn getgrgid(_gid: gid_t) -> *mut group {
    core::ptr::null_mut()
}
