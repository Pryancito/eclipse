//! sys/mman.h - Memory management
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn mmap(_addr: *mut c_void, _length: size_t, _prot: c_int, _flags: c_int, _fd: c_int, _offset: off_t) -> *mut c_void {
    // Stub: usually failing is safer if we don't have real mmap
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn munmap(_addr: *mut c_void, _length: size_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn getpagesize() -> c_int {
    4096
}
