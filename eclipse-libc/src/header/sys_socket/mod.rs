//! sys/socket.h - Socket interface
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr {
    pub sa_family: c_ushort,
    pub sa_data: [c_char; 14],
}


#[no_mangle]
pub unsafe extern "C" fn socket(_domain: c_int, _type: c_int, _protocol: c_int) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn bind(_sockfd: c_int, _addr: *const sockaddr, _addrlen: socklen_t) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn listen(_sockfd: c_int, _backlog: c_int) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn accept(_sockfd: c_int, _addr: *mut sockaddr, _addrlen: *mut socklen_t) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn connect(_sockfd: c_int, _addr: *const sockaddr, _addrlen: socklen_t) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn shutdown(_sockfd: c_int, _how: c_int) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn send(_sockfd: c_int, _buf: *const c_void, _len: size_t, _flags: c_int) -> ssize_t {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn recv(_sockfd: c_int, _buf: *mut c_void, _len: size_t, _flags: c_int) -> ssize_t {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn getsockname(_sockfd: c_int, _addr: *mut sockaddr, _addrlen: *mut socklen_t) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn getpeername(_sockfd: c_int, _addr: *mut sockaddr, _addrlen: *mut socklen_t) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn setsockopt(_sockfd: c_int, _level: c_int, _optname: c_int, _optval: *const c_void, _optlen: socklen_t) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn getsockopt(_sockfd: c_int, _level: c_int, _optname: c_int, _optval: *mut c_void, _optlen: *mut socklen_t) -> c_int {
    -1
}
