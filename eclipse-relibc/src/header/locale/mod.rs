//! locale.h - Localization
use crate::types::*;

#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn setlocale(_category: c_int, _locale: *const c_char) -> *mut c_char {
    // Stub implementation: always return "C" or NULL
    // returning NULL usually means failure or query (if locale is NULL)
    // returning "C" means successful set to "C" locale
    
    // Static string "C\0"
    b"C\0".as_ptr() as *mut c_char
}

#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn localeconv() -> *mut c_void {
    // Stub: return NULL or pointer to static lconv
    core::ptr::null_mut()
}
