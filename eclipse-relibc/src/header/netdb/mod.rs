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

#[repr(C)]
pub struct addrinfo {
    pub ai_flags: c_int,
    pub ai_family: c_int,
    pub ai_socktype: c_int,
    pub ai_protocol: c_int,
    pub ai_addrlen: socklen_t,
    pub ai_addr: *mut crate::header::sys_socket::sockaddr,
    pub ai_canonname: *mut c_char,
    pub ai_next: *mut addrinfo,
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

#[no_mangle]
pub unsafe extern "C" fn getaddrinfo(
    _node: *const c_char,
    _service: *const c_char,
    _hints: *const addrinfo,
    _res: *mut *mut addrinfo,
) -> c_int {
    -1 // Failure for stub
}

#[no_mangle]
pub unsafe extern "C" fn freeaddrinfo(_res: *mut addrinfo) {
    // Stub
}

#[no_mangle]
pub unsafe extern "C" fn gai_strerror(_errcode: c_int) -> *const c_char {
    b"Unknown error\0".as_ptr() as *const c_char
}
