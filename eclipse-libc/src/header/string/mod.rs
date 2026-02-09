//! string.h - String operations
use crate::types::*;
use core::ptr;

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, n: size_t) -> *mut c_void {
    ptr::copy_nonoverlapping(src as *const u8, dest as *mut u8, n);
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut c_void, c: c_int, n: size_t) -> *mut c_void {
    ptr::write_bytes(s as *mut u8, c as u8, n);
    s
}

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> size_t {
    crate::c_str::strlen(s)
}
