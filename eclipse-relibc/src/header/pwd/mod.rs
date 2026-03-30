//! pwd.h - Password database
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getpwnam(_name: *const c_char) -> *mut passwd {
    core::ptr::null_mut()
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getpwuid(_uid: uid_t) -> *mut passwd {
    core::ptr::null_mut()
}
