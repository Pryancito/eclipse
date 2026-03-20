//! dlfcn.h - Dynamic linking
use crate::types::*;

pub const RTLD_LAZY: c_int = 1;
pub const RTLD_NOW: c_int = 2;
pub const RTLD_GLOBAL: c_int = 256;
pub const RTLD_LOCAL: c_int = 0;
pub const RTLD_DEFAULT: *mut c_void = 0 as *mut c_void;

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
mod host {
    use super::*;
    extern "C" {
        pub fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        pub fn dlerror() -> *mut c_char;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        pub fn dlclose(handle: *mut c_void) -> c_int;
    }
}

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
pub use self::host::*;

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
mod target {
    use super::*;

    #[no_mangle]
    pub unsafe extern "C" fn dlopen(_filename: *const c_char, _flag: c_int) -> *mut c_void {
        core::ptr::null_mut()
    }

    #[no_mangle]
    pub unsafe extern "C" fn dlerror() -> *mut c_char {
        core::ptr::null_mut()
    }

    #[no_mangle]
    pub unsafe extern "C" fn dlsym(_handle: *mut c_void, _symbol: *const c_char) -> *mut c_void {
        core::ptr::null_mut()
    }

    #[no_mangle]
    pub unsafe extern "C" fn dlclose(_handle: *mut c_void) -> c_int {
        -1
    }
}

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
pub use self::target::*;
