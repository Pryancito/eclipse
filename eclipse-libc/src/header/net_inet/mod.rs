//! netinet/in.h and arpa/inet.h - Networking
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct in_addr {
    pub s_addr: c_uint,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_in {
    pub sin_family: c_ushort,
    pub sin_port: c_ushort,
    pub sin_addr: in_addr,
    pub sin_zero: [c_char; 8],
}

#[no_mangle]
pub unsafe extern "C" fn inet_ntoa(_in: in_addr) -> *mut c_char {
    // Stub: return a static string
    static mut BUF: [c_char; 16] = [0; 16];
    // "0.0.0.0"
    BUF[0] = b'0' as c_char;
    BUF[1] = b'.' as c_char;
    BUF[2] = b'0' as c_char;
    BUF[3] = b'.' as c_char;
    BUF[4] = b'0' as c_char;
    BUF[5] = b'.' as c_char;
    BUF[6] = b'0' as c_char;
    BUF[7] = 0;
    BUF.as_mut_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn htons(hostshort: u16) -> u16 {
    hostshort.to_be()
}

#[no_mangle]
pub unsafe extern "C" fn ntohs(netshort: u16) -> u16 {
    u16::from_be(netshort)
}

#[no_mangle]
pub unsafe extern "C" fn htonl(hostlong: u32) -> u32 {
    hostlong.to_be()
}

#[no_mangle]
pub unsafe extern "C" fn ntohl(netlong: u32) -> u32 {
    u32::from_be(netlong)
}

#[no_mangle]
pub unsafe extern "C" fn inet_addr(_cp: *const c_char) -> c_uint {
    0xffffffff
}

#[no_mangle]
pub unsafe extern "C" fn inet_pton(_af: c_int, _src: *const c_char, _dst: *mut c_void) -> c_int {
    0 // Stub: failure
}

#[no_mangle]
pub unsafe extern "C" fn inet_ntop(_af: c_int, _src: *const c_void, _dst: *mut c_char, _size: socklen_t) -> *const c_char {
    core::ptr::null() // Stub: failure
}
