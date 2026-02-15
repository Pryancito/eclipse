//! netdb.h - Network database
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct hostent {
    pub h_name: *mut c_char,
    pub h_aliases: *mut *mut c_char,
    pub h_addrtype: c_int,
    pub h_length: c_int,
    pub h_addr_list: *mut *mut c_char,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct servent {
    pub s_name: *mut c_char,
    pub s_aliases: *mut *mut c_char,
    pub s_port: c_int,
    pub s_proto: *mut c_char,
}

#[no_mangle]
pub unsafe extern "C" fn gethostbyname(_name: *const c_char) -> *mut hostent {
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn gethostbyaddr(_addr: *const c_void, _len: socklen_t, _type: c_int) -> *mut hostent {
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn getservbyname(_name: *const c_char, _proto: *const c_char) -> *mut servent {
    core::ptr::null_mut()
}
