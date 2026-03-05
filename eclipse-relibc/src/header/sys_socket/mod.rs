//! sys/socket.h - Socket interface
use crate::types::*;
use core::ffi::c_int;
use eclipse_syscall::call::{socket as sys_socket, bind as sys_bind, listen as sys_listen, accept as sys_accept, connect as sys_connect};

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr {
    pub sa_family: c_ushort,
    pub sa_data: [c_char; 14],
}


#[no_mangle]
pub unsafe extern "C" fn socket(domain: c_int, type_: c_int, protocol: c_int) -> c_int {
    match sys_socket(domain as usize, type_ as usize, protocol as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn bind(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int {
    match sys_bind(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn listen(sockfd: c_int, backlog: c_int) -> c_int {
    match sys_listen(sockfd as usize, backlog as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn accept(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int {
    // Note: addr and addrlen can be null. Our sys_accept wrapper handles it if we pass them.
    match sys_accept(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn connect(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int {
    match sys_connect(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
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
