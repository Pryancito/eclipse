//! sys/socket.h - Socket interface
use crate::types::*;
use crate::header::sys_uio::iovec;
use core::ffi::c_int;
use core::mem::size_of;
use crate::eclipse_syscall::call::{socket as sys_socket, bind as sys_bind, listen as sys_listen, accept as sys_accept, connect as sys_connect};

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr {
    pub sa_family: c_ushort,
    pub sa_data: [c_char; 14],
}

/// Message header para sendmsg/recvmsg (compatible con Linux/POSIX para rustix).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct msghdr {
    pub msg_name: *mut c_void,
    pub msg_namelen: socklen_t,
    pub msg_iov: *mut iovec,
    pub msg_iovlen: size_t,
    pub msg_control: *mut c_void,
    pub msg_controllen: size_t,
    pub msg_flags: c_int,
}

/// Control message header (datos auxiliares en recvmsg/sendmsg).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct cmsghdr {
    pub cmsg_len: size_t,
    pub cmsg_level: c_int,
    pub cmsg_type: c_int,
}

// Constantes MSG_* (valores Linux, compatibles con rustix)
pub const MSG_OOB: c_int = 1;
pub const MSG_PEEK: c_int = 2;
pub const MSG_DONTROUTE: c_int = 4;
pub const MSG_CTRUNC: c_int = 8;
pub const MSG_TRUNC: c_int = 0x20;
pub const MSG_DONTWAIT: c_int = 0x40;
pub const MSG_EOR: c_int = 0x80;
pub const MSG_WAITALL: c_int = 0x100;
pub const MSG_CONFIRM: c_int = 0x800;
pub const MSG_ERRQUEUE: c_int = 0x2000;
pub const MSG_NOSIGNAL: c_int = 0x4000;
pub const MSG_MORE: c_int = 0x8000;
pub const MSG_CMSG_CLOEXEC: c_int = 0x40000000;

/// Alineación para datos de control (CMSG), como en libc Linux.
#[inline]
const fn cmsg_align(len: usize) -> usize {
    (len + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

/// Primer header de control en un msghdr (nombre C para compatibilidad con libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub unsafe fn CMSG_FIRSTHDR(mhdr: *const msghdr) -> *mut cmsghdr {
    if !mhdr.is_null() && (*mhdr).msg_controllen as usize >= size_of::<cmsghdr>() {
        (*mhdr).msg_control.cast::<cmsghdr>()
    } else {
        core::ptr::null_mut::<cmsghdr>()
    }
}

/// Puntero a los datos de un cmsghdr (nombre C para compatibilidad con libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub unsafe fn CMSG_DATA(cmsg: *const cmsghdr) -> *mut c_uchar {
    cmsg.offset(1) as *mut c_uchar
}

/// Espacio necesario para un mensaje de control de longitud `length` (nombre C para libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub const fn CMSG_SPACE(length: c_uint) -> c_uint {
    (cmsg_align(length as usize) + cmsg_align(size_of::<cmsghdr>())) as c_uint
}

/// Longitud de un cmsghdr con payload de `length` bytes (nombre C para libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub const fn CMSG_LEN(length: c_uint) -> c_uint {
    cmsg_align(size_of::<cmsghdr>()) as c_uint + length
}

/// Siguiente header de control (nombre C para libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub unsafe fn CMSG_NXTHDR(mhdr: *const msghdr, cmsg: *const cmsghdr) -> *mut cmsghdr {
    if mhdr.is_null() || cmsg.is_null() {
        return core::ptr::null_mut::<cmsghdr>();
    }
    let len = (*cmsg).cmsg_len as usize;
    if len < size_of::<cmsghdr>() {
        return core::ptr::null_mut::<cmsghdr>();
    }
    let next = (cmsg as *const u8).add(cmsg_align(len)) as *mut cmsghdr;
    let max = (*mhdr).msg_control as *const u8;
    let max = max.add((*mhdr).msg_controllen as usize);
    if next.add(1) as *const u8 > max {
        core::ptr::null_mut::<cmsghdr>()
    } else {
        next
    }
}


#[cfg(any(test, feature = "host-testing", all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))]
extern "C" {
    pub fn socket(domain: c_int, type_: c_int, protocol: c_int) -> c_int;
    pub fn bind(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;
    pub fn listen(sockfd: c_int, backlog: c_int) -> c_int;
    pub fn accept(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int;
    pub fn connect(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;
    pub fn send(sockfd: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t;
    pub fn recv(sockfd: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t;
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn socket(domain: c_int, type_: c_int, protocol: c_int) -> c_int {
    match sys_socket(domain as usize, type_ as usize, protocol as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn bind(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int {
    match sys_bind(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn listen(sockfd: c_int, backlog: c_int) -> c_int {
    match sys_listen(sockfd as usize, backlog as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn accept(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int {
    // Note: addr and addrlen can be null. Our sys_accept wrapper handles it if we pass them.
    match sys_accept(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn connect(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int {
    match sys_connect(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn shutdown(_sockfd: c_int, _how: c_int) -> c_int {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn send(sockfd: c_int, buf: *const c_void, len: size_t, _flags: c_int) -> ssize_t {
    use crate::eclipse_syscall::call::write as sys_write;
    let slice = core::slice::from_raw_parts(buf as *const u8, len);
    match sys_write(sockfd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn recv(sockfd: c_int, buf: *mut c_void, len: size_t, _flags: c_int) -> ssize_t {
    use crate::eclipse_syscall::call::read as sys_read;
    let slice = core::slice::from_raw_parts_mut(buf as *mut u8, len);
    match sys_read(sockfd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getsockname(_sockfd: c_int, _addr: *mut sockaddr, _addrlen: *mut socklen_t) -> c_int {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getpeername(_sockfd: c_int, _addr: *mut sockaddr, _addrlen: *mut socklen_t) -> c_int {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn setsockopt(_sockfd: c_int, _level: c_int, _optname: c_int, _optval: *const c_void, _optlen: socklen_t) -> c_int {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getsockopt(_sockfd: c_int, _level: c_int, _optname: c_int, _optval: *mut c_void, _optlen: *mut socklen_t) -> c_int {
    -1
}

#[cfg(any(test, feature = "host-testing", all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))]
extern "C" {
    pub fn sendmsg(sockfd: c_int, msg: *const msghdr, flags: c_int) -> ssize_t;
    pub fn recvmsg(sockfd: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t;
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn sendmsg(_sockfd: c_int, _msg: *const msghdr, _flags: c_int) -> ssize_t {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn recvmsg(_sockfd: c_int, _msg: *mut msghdr, _flags: c_int) -> ssize_t {
    -1
}
