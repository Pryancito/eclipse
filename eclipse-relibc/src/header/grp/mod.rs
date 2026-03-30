//! grp.h - Group database
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getgrnam(_name: *const c_char) -> *mut group {
    core::ptr::null_mut()
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getgrgid(_gid: gid_t) -> *mut group {
    core::ptr::null_mut()
}
